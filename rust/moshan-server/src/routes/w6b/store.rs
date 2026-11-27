// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：`moshan/agentstore.py` 的 Rust 版 —— 墨山自己的 agent 账本
//       （一次解析 WorkBuddy 会话流水 → 一个会话一行，以后卡片查库）。
//       两条路由：store/refresh（增量折账）、store/stats（行数 / 各 agent / 库体量）。
// 不做：compact（WAL 检查点 + VACUUM，没有路由在用）；不建第二套 schema。
//
// 为什么走 `sqlite3` 命令行：Cargo.toml 里没有 sqlite 依赖，而本泳道不许改它。
// schema 与 Python 的 `SCHEMA` 逐字一致 —— 建出来的库页数一样（24576 字节），
// 所以 `/api/agent/store/stats` 的 `db_size` 能与 Python 对齐。

#![allow(dead_code)]

use super::util::{self, cut, pystr, s_or, truthy};
use crate::paths::Ctx;
use crate::routes::common::{peek, CtxState};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use axum::routing::{get, post};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub fn router() -> Router<Arc<Ctx>> {
    Router::new()
        .route("/api/agent/store/refresh", post(refresh_route))
        .route("/api/agent/store/stats", get(stats_route))
}

const DB_NAME: &str = "agentstore.sqlite";

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS sessions (
  agent      TEXT NOT NULL,
  session_id TEXT NOT NULL,
  title      TEXT DEFAULT '',
  cwd        TEXT DEFAULT '',
  project    TEXT DEFAULT '',
  model      TEXT DEFAULT '',
  started_at INTEGER DEFAULT 0,
  ended_at   INTEGER DEFAULT 0,
  turns      INTEGER DEFAULT 0,
  tool_calls INTEGER DEFAULT 0,
  tin        INTEGER DEFAULT 0,
  tout       INTEGER DEFAULT 0,
  cached     INTEGER DEFAULT 0,
  source     TEXT DEFAULT '',
  updated_at INTEGER DEFAULT 0,
  PRIMARY KEY (agent, session_id)
);
CREATE INDEX IF NOT EXISTS ix_sess_end ON sessions(agent, ended_at);
CREATE TABLE IF NOT EXISTS meta (
  k TEXT PRIMARY KEY,
  v TEXT
);";

fn db_path(ctx: &Ctx) -> PathBuf {
    ctx.runtime_dir.join("_工作台").join(DB_NAME)
}

/// `connect()`：库不在就按同一份 schema 建。
fn ensure_db(ctx: &Ctx) -> PathBuf {
    let p = db_path(ctx);
    let _ = util::ensure_parent(&p);
    if !p.exists() {
        let _ = std::process::Command::new("sqlite3")
            .arg(&p)
            .arg(SCHEMA)
            .output();
    }
    p
}

fn sqlite_json(p: &Path, sql: &str, writable: bool) -> Option<Vec<Value>> {
    let mut cmd = std::process::Command::new("sqlite3");
    if !writable {
        cmd.arg("-readonly");
    }
    let out = cmd.arg("-json").arg(p).arg(sql).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let t = text.trim();
    if t.is_empty() {
        return Some(Vec::new());
    }
    let v: Value = serde_json::from_str(t).ok()?;
    v.as_array().cloned()
}

fn sqlite_exec(p: &Path, sql: &str) -> bool {
    std::process::Command::new("sqlite3")
        .arg(p)
        .arg(sql)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn size_of(p: &Path) -> i64 {
    let mut n = 0i64;
    for suffix in ["", "-wal", "-shm"] {
        let q = PathBuf::from(format!("{}{}", p.display(), suffix));
        if let Ok(md) = std::fs::metadata(&q) {
            n += md.len() as i64;
        }
    }
    n
}

/// `summary(0)`：按 agent 汇总（days=0 全部历史）。
fn summary(p: &Path) -> Vec<Value> {
    let sql = "SELECT agent, COUNT(*) AS sessions, SUM(turns) AS turns, \
               SUM(tool_calls) AS calls, SUM(tin) AS tin, SUM(tout) AS tout, \
               SUM(cached) AS cached, MAX(ended_at) AS last \
               FROM sessions GROUP BY agent ORDER BY calls DESC";
    let rows = sqlite_json(p, sql, false).unwrap_or_default();
    rows.into_iter()
        .map(|r| {
            let mut o = Map::new();
            for k in ["agent", "sessions", "turns", "calls", "tin", "tout", "cached", "last"] {
                o.insert(k.to_string(), r.get(k).cloned().unwrap_or(Value::Null));
            }
            Value::Object(o)
        })
        .collect()
}

/// `stats()`：`{"rows","by_agent","db","db_size"}`
fn stats(p: &Path) -> Value {
    let rows = sqlite_json(p, "SELECT COUNT(*) AS n FROM sessions", false)
        .and_then(|v| v.first().and_then(|x| x.get("n").and_then(|n| n.as_i64())))
        .unwrap_or(0);
    let mut by_agent = Map::new();
    for r in sqlite_json(p, "SELECT agent, COUNT(*) c FROM sessions GROUP BY agent", false)
        .unwrap_or_default()
    {
        let a = s_or(r.get("agent").unwrap_or(&Value::Null));
        let c = r.get("c").and_then(|x| x.as_i64()).unwrap_or(0);
        by_agent.insert(a, json!(c));
    }
    let mut o = Map::new();
    o.insert("rows".into(), json!(rows));
    o.insert("by_agent".into(), Value::Object(by_agent));
    o.insert("db".into(), json!(p.display().to_string()));
    o.insert("db_size".into(), json!(size_of(p)));
    Value::Object(o)
}

async fn stats_route(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let ctx2 = ctx.clone();
    let out = tokio::task::spawn_blocking(move || {
        let p = ensure_db(&ctx2);
        let mut o = Map::new();
        o.insert("ok".into(), json!(true));
        let st = stats(&p);
        if let Some(m) = st.as_object() {
            for (k, v) in m {
                o.insert(k.clone(), v.clone());
            }
        }
        o.insert("summary".into(), json!(summary(&p)));
        Value::Object(o)
    })
    .await
    .unwrap_or_else(|_| json!({"ok": false, "error": "RuntimeError: 统计失败"}));
    Json(out).into_response()
}

// ---------------------------------------------------------------- 解析

fn ms_of(v: &Value) -> i64 {
    match v {
        Value::Number(n) => {
            let x = n.as_f64().unwrap_or(0.0) as i64;
            if x > 1_000_000_000_000 {
                x
            } else if x > 1_000_000_000 {
                x * 1000
            } else {
                0
            }
        }
        _ => {
            let s = s_or(v);
            if s.trim().is_empty() {
                return 0;
            }
            match chrono::DateTime::parse_from_rfc3339(s.trim().replace('Z', "+00:00").as_str()) {
                Ok(d) => d.timestamp_millis(),
                Err(_) => 0,
            }
        }
    }
}

/// `_pd`：providerData 一般是对象；是 Python repr 字符串时放弃（回空表）。
fn pd_of(v: &Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m.clone(),
        _ => Map::new(),
    }
}

/// `_usage(pd)` → (in, out, cached)
fn usage_of(pd: &Map<String, Value>) -> (i64, i64, i64) {
    let u = pd.get("usage").and_then(|x| x.as_object()).cloned().unwrap_or_default();
    let pick = |a: &str, b: &str| -> i64 {
        let va = u.get(a).cloned().unwrap_or(Value::Null);
        if truthy(&va) {
            util::int0(&va)
        } else {
            util::int0(&u.get(b).cloned().unwrap_or(Value::Null))
        }
    };
    let tin = pick("inputTokens", "input_tokens");
    let tout = pick("outputTokens", "output_tokens");
    let mut cached = 0i64;
    let det = u
        .get("inputTokensDetails")
        .or_else(|| u.get("input_tokens_details"))
        .or_else(|| u.get("details"))
        .cloned()
        .unwrap_or(Value::Null);
    if let Some(a) = det.as_array() {
        if let Some(first) = a.first() {
            if first.is_object() {
                cached = {
                    let c = first.get("cached_tokens").cloned().unwrap_or(Value::Null);
                    if truthy(&c) {
                        util::int0(&c)
                    } else {
                        util::int0(first.get("cachedTokens").unwrap_or(&Value::Null))
                    }
                };
            }
        }
    } else if det.is_object() {
        let c = det.get("cached_tokens").cloned().unwrap_or(Value::Null);
        cached = if truthy(&c) {
            util::int0(&c)
        } else {
            util::int0(det.get("cachedTokens").unwrap_or(&Value::Null))
        };
    }
    (tin, tout, cached)
}

/// `_project_of(cwd)`：路径最后一段（跳过 "/" 与 "Users"）。
fn project_of(cwd: &str) -> String {
    if cwd.is_empty() {
        return String::new();
    }
    let parts: Vec<&str> = cwd
        .split('/')
        .filter(|p| !p.is_empty() && *p != "Users")
        .collect();
    parts.last().map(|s| s.to_string()).unwrap_or_default()
}

/// `_user_is_real`：这条 user 消息是不是「用户真的说了话」。
fn user_is_real(content: &Value) -> bool {
    let mut txt = String::new();
    match content {
        Value::String(s) => txt = s.clone(),
        Value::Array(a) => {
            for x in a {
                if let Some(t) = x.get("type").and_then(|v| v.as_str()) {
                    if t == "tool_result" {
                        return false;
                    }
                    if t == "input_text" || t == "text" {
                        txt.push_str(&s_or(x.get("text").unwrap_or(&Value::Null)));
                    }
                }
            }
        }
        _ => {}
    }
    if txt.is_empty() {
        return false;
    }
    let rest = sr_tag_strip(&txt);
    let rest = rest.trim();
    if rest.is_empty() {
        return false;
    }
    const NOISE: [&str; 4] = ["<task-notification", "<user_info", "cb_summary", "接着上面的摘要继续"];
    for bad in NOISE {
        if rest.starts_with(bad) {
            return false;
        }
    }
    true
}

/// 去掉 `<system-reminder ...>...</system-reminder>`（DOTALL，非贪婪）。
fn sr_tag_strip(s: &str) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"(?s)<system-reminder\b[^>]*>.*?</system-reminder>").unwrap()
    });
    re.replace_all(s, "").to_string()
}

struct Row {
    sid: String,
    title: String,
    cwd: String,
    project: String,
    model: String,
    first: i64,
    last: i64,
    turns: i64,
    tools: i64,
    tin: i64,
    tout: i64,
    cached: i64,
    source: String,
}

/// `_parse_wb(f, sid)`：一个会话文件 → 一行。
fn parse_wb(f: &Path, sid: &str) -> Option<Row> {
    let bytes = std::fs::read(f).ok()?;
    let text = String::from_utf8_lossy(&bytes).to_string();
    let mut title = String::new();
    let mut cwd = String::new();
    let mut model = String::new();
    let mut first = 0i64;
    let mut last = 0i64;
    let mut turns = 0i64;
    let mut tools = 0i64;
    let mut tin = 0i64;
    let mut tout = 0i64;
    let mut cached = 0i64;
    let mut got = false;
    for ln in text.lines() {
        let s = ln.trim();
        if s.is_empty() {
            continue;
        }
        let Ok(o) = serde_json::from_str::<Value>(s) else { continue };
        got = true;
        let t = o.get("type").and_then(|x| x.as_str()).unwrap_or("");
        let ts = ms_of(o.get("timestamp").unwrap_or(&Value::Null));
        if ts != 0 {
            if first == 0 || ts < first {
                first = ts;
            }
            if ts > last {
                last = ts;
            }
        }
        if cwd.is_empty() {
            if let Some(c) = o.get("cwd") {
                if truthy(c) {
                    cwd = pystr(c);
                }
            }
        }
        if t == "ai-title" {
            let a = o.get("aiTitle").cloned().unwrap_or(Value::Null);
            let a = if truthy(&a) { a } else { o.get("title").cloned().unwrap_or(Value::Null) };
            title = cut(&pystr(&a), 160);
        } else if t == "message" {
            if o.get("role").and_then(|x| x.as_str()) == Some("user")
                && user_is_real(o.get("content").unwrap_or(&Value::Null))
            {
                turns += 1;
            }
            let pd = pd_of(o.get("providerData").unwrap_or(&Value::Null));
            let m = {
                let r = pd.get("requestModelId").cloned().unwrap_or(Value::Null);
                if truthy(&r) { pystr(&r) } else { s_or(pd.get("model").unwrap_or(&Value::Null)) }
            };
            if !m.is_empty() && model.is_empty() {
                model = cut(&m, 60);
            }
            let (a, b, c) = usage_of(&pd);
            tin += a;
            tout += b;
            cached += c;
        } else if t == "function_call" {
            tools += 1;
            let pd = pd_of(o.get("providerData").unwrap_or(&Value::Null));
            let (a, b, c) = usage_of(&pd);
            tin += a;
            tout += b;
            cached += c;
        } else if t == "session-meta" || t == "file-history-snapshot" {
            let pd = pd_of(o.get("providerData").unwrap_or(&Value::Null));
            let (a, b, c) = usage_of(&pd);
            tin += a;
            tout += b;
            cached += c;
        }
    }
    if !got {
        return None;
    }
    let mtime_ms = f
        .metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    Some(Row {
        sid: sid.to_string(),
        title,
        project: project_of(&cwd),
        cwd,
        model,
        first: if first != 0 { first } else { mtime_ms },
        last: if last != 0 { last } else { mtime_ms },
        turns,
        tools,
        tin,
        tout,
        cached,
        source: f.parent().map(|p| p.display().to_string()).unwrap_or_default(),
    })
}

fn sql_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// `ingest_workbuddy(force)`
fn ingest(db: &Path, force: bool) -> Value {
    let wb = util::home().join(".workbuddy").join("projects");
    if !wb.exists() {
        return json!({"ok": false, "error": "本机没有 ~/.workbuddy/projects"});
    }
    ensure_db_path(db);

    let mut have: HashMap<(String, String), i64> = HashMap::new();
    if !force {
        if let Some(rows) = sqlite_json(
            db,
            "SELECT session_id, source, updated_at FROM sessions WHERE agent='WorkBuddy'",
            false,
        ) {
            for r in rows {
                let sid = s_or(r.get("session_id").unwrap_or(&Value::Null));
                let src = s_or(r.get("source").unwrap_or(&Value::Null));
                let up = r.get("updated_at").and_then(|x| x.as_i64()).unwrap_or(0);
                have.insert((sid, src), up);
            }
        }
    }
    let mut files: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&wb) {
        for e in rd.flatten() {
            let d = e.path();
            if !d.is_dir() {
                continue;
            }
            if let Ok(rd2) = std::fs::read_dir(&d) {
                for f in rd2.flatten() {
                    let p = f.path();
                    if p.is_file() && p.extension().map(|x| x == "jsonl").unwrap_or(false) {
                        files.push(p);
                    }
                }
            }
        }
    }

    let mut n_new = 0i64;
    let mut n_skip = 0i64;
    let mut n_err = 0i64;
    let mut rows: Vec<Row> = Vec::new();
    let mut mtimes: Vec<i64> = Vec::new();
    for f in &files {
        let Ok(md) = std::fs::metadata(f) else { continue };
        let mtime = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let sid = f
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let parent = f.parent().map(|p| p.display().to_string()).unwrap_or_default();
        if !force && have.get(&(sid.clone(), parent)) == Some(&mtime) {
            n_skip += 1;
            continue;
        }
        match parse_wb(f, &sid) {
            Some(r) => {
                rows.push(r);
                mtimes.push(mtime);
                n_new += 1;
            }
            None => n_err += 1,
        }
    }

    if !rows.is_empty() {
        let mut sql = String::from("BEGIN;");
        for (i, r) in rows.iter().enumerate() {
            sql.push_str(&format!(
                "INSERT INTO sessions (agent, session_id, title, cwd, project, model, \
                 started_at, ended_at, turns, tool_calls, tin, tout, cached, source, updated_at) \
                 VALUES ('WorkBuddy',{},{},{},{},{},{},{},{},{},{},{},{},{},{} ) \
                 ON CONFLICT(agent, session_id) DO UPDATE SET title=excluded.title, \
                 cwd=excluded.cwd, project=excluded.project, model=excluded.model, \
                 started_at=excluded.started_at, ended_at=excluded.ended_at, \
                 turns=excluded.turns, tool_calls=excluded.tool_calls, tin=excluded.tin, \
                 tout=excluded.tout, cached=excluded.cached, source=excluded.source, \
                 updated_at=excluded.updated_at;",
                sql_str(&r.sid),
                sql_str(&r.title),
                sql_str(&r.cwd),
                sql_str(&r.project),
                sql_str(&r.model),
                r.first,
                r.last,
                r.turns,
                r.tools,
                r.tin,
                r.tout,
                r.cached,
                sql_str(&r.source),
                mtimes[i]
            ));
        }
        sql.push_str("COMMIT;");
        sqlite_exec(db, &sql);
    }
    let mut o = Map::new();
    o.insert("ok".into(), json!(true));
    o.insert("agent".into(), json!("WorkBuddy"));
    o.insert("files".into(), json!(files.len()));
    o.insert("new".into(), json!(n_new));
    o.insert("skipped".into(), json!(n_skip));
    o.insert("bad".into(), json!(n_err));
    o.insert("db".into(), json!(db.display().to_string()));
    o.insert("db_size".into(), json!(size_of(db)));
    Value::Object(o)
}

fn ensure_db_path(p: &Path) -> bool {
    let _ = util::ensure_parent(p);
    if !p.exists() {
        return sqlite_exec(p, SCHEMA);
    }
    true
}

async fn refresh_route(State(ctx): CtxState, headers: HeaderMap, uri: Uri, body: Bytes) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let b = super::parse_json_opt(&body).unwrap_or_else(|| json!({}));
    let force = b.get("force").map(truthy).unwrap_or(false);
    let ctx2 = ctx.clone();
    let out = tokio::task::spawn_blocking(move || {
        let p = db_path(&ctx2);
        ingest(&p, force)
    })
    .await
    .unwrap_or_else(|_| json!({"ok": false, "error": "RuntimeError: 折账失败"}));
    Json(out).into_response()
}
