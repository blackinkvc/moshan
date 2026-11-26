// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：`moshan/agentconfig.py` 的 Rust 版 —— 扫各家 agent 的「配置面」：
//       分组 / 任务 / 插件 / Skill / MCP，五面一家一份。
// 不做：写任何配置（只读）；不猜读不出来的字段（照 Python 的做法留空并进 notes）。
//
// 走 `sqlite3 -readonly -json` 的原因同 store.rs：Cargo.toml 里没有 sqlite 依赖，
// 本泳道不许改它。只读打开、列名取值，语义与 Python 的 `_q()` 一致（失败回空表）。
//
// 缓存：180 秒（CACHE_TTL），命中时把 `cached` 置 true —— 与 Python 同形。

#![allow(dead_code)]

use crate::paths::Ctx;
use crate::routes::common::{peek, CtxState};
use super::util;
use axum::extract::State;
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub fn router() -> Router<Arc<Ctx>> {
    Router::new().route("/api/agent/config/scan", get(scan_route))
}

const CACHE_TTL: f64 = 180.0;
const MAX_ITEMS: usize = 60;

// ---------------------------------------------------------------- 小工具

fn home() -> PathBuf {
    util::home()
}

fn asupport() -> PathBuf {
    home().join("Library").join("Application Support")
}

fn read_json(p: &Path) -> Value {
    match std::fs::read(p) {
        Ok(b) => serde_json::from_str(&String::from_utf8_lossy(&b)).unwrap_or(Value::Null),
        Err(_) => Value::Null,
    }
}

fn read_json_or(p: &Path, default: Value) -> Value {
    let v = read_json(p);
    if v.is_null() {
        default
    } else {
        v
    }
}

fn mtime(p: &Path) -> f64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .map(|t| {
            t.duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0)
        })
        .unwrap_or(0.0)
}

fn day(ts: f64) -> String {
    if ts == 0.0 {
        return String::new();
    }
    util::strftime_secs(ts as i64, "%Y-%m-%d")
}

fn rel_of(p: &Path) -> String {
    let s = p.display().to_string();
    let h = home().display().to_string();
    if s.starts_with(&h) {
        format!("~{}", &s[h.len()..])
    } else {
        s
    }
}

/// `_front(text, limit)`：抠 SKILL.md 头部 `--- … ---` 里的短标量。
fn front(text: &str, limit: usize) -> Map<String, Value> {
    let mut out = Map::new();
    if !text.starts_with("---") {
        return out;
    }
    let Some(end) = text[3..].find("\n---").map(|i| i + 3) else {
        return out;
    };
    for line in text[3..end].lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else { continue };
        let k = k.trim().to_string();
        let mut v = v.trim().to_string();
        if matches!(v.as_str(), "|" | ">" | "|-" | ">-" | "|+" | ">+") {
            v = String::new();
        }
        if v.chars().count() >= 2 {
            let first = v.chars().next().unwrap();
            let last = v.chars().last().unwrap();
            if first == last && (first == '"' || first == '\'') {
                v = v.chars().skip(1).take(v.chars().count() - 2).collect();
            }
        }
        if !k.is_empty() && !v.is_empty() {
            out.insert(k, json!(v));
        }
    }
    if limit > 0 {
        if let Some(d) = out.get("description").cloned() {
            let s = util::s_or(&d);
            out.insert("description".into(), json!(util::cut(&s, limit)));
        }
    }
    out
}

/// `_skill(p)`：优先 SKILL.md，兜底同目录任意 .md。
fn skill_of(p: &Path) -> Option<Value> {
    if !p.is_dir() {
        return None;
    }
    let mut f = p.join("SKILL.md");
    if !f.exists() {
        let cands = files(p, "*.md");
        let first = cands.first()?;
        f = first.clone();
    }
    let bytes = std::fs::read(&f).ok()?;
    let txt: String = String::from_utf8_lossy(&bytes).to_string();
    let txt = util::cut(&txt, 6000);
    let meta = front(&txt, 160);
    let name = {
        let n = util::s_or(meta.get("name").unwrap_or(&Value::Null));
        if n.is_empty() {
            p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
        } else {
            n
        }
    };
    Some(json!({
        "name": name,
        "desc": util::s_or(meta.get("description").unwrap_or(&Value::Null)),
        "path": rel_of(p),
        "at": day(mtime(&f)),
    }))
}

/// `_dirs(p)`：直接子目录（跳过点开头），按路径排序。
fn dirs(p: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(p) else { return Vec::new() };
    let mut out: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|x| {
            x.is_dir()
                && !x.file_name()
                    .map(|s| s.to_string_lossy().starts_with('.'))
                    .unwrap_or(false)
        })
        .collect();
    out.sort();
    out
}

/// `_files(p, pattern)`：`*` / `*.ext` 两种；跳过 `._` 开头的。
fn files(p: &Path, pattern: &str) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(p) else { return Vec::new() };
    let mut out: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|x| {
            x.is_file()
                && !x
                    .file_name()
                    .map(|s| s.to_string_lossy().starts_with("._"))
                    .unwrap_or(false)
                && match pattern.strip_prefix("*.") {
                    Some(ext) => x
                        .extension()
                        .map(|e| e == ext)
                        .unwrap_or(false),
                    None => true,
                }
        })
        .collect();
    out.sort();
    out
}

/// `p.glob("*/*/*/<name>")`：正好三层子目录下的那个文件。
fn glob_depth3_files(p: &Path, name: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for a in dirs(p) {
        for b in dirs(&a) {
            for c in dirs(&b) {
                let f = c.join(name);
                if f.is_file() {
                    out.push(f);
                }
            }
        }
    }
    out.sort();
    out
}

fn sqlite_rows(db: &Path, sql: &str) -> Vec<Value> {
    let out = std::process::Command::new("sqlite3")
        .arg("-readonly")
        .arg("-json")
        .arg(db)
        .arg(sql)
        .output();
    let Ok(out) = out else { return Vec::new() };
    if !out.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let t = text.trim();
    if t.is_empty() {
        return Vec::new();
    }
    match serde_json::from_str::<Value>(t) {
        Ok(Value::Array(a)) => a,
        _ => Vec::new(),
    }
}

fn i_of(r: &Value, k: &str) -> i64 {
    r.get(k).map(util::int_float).unwrap_or(0)
}

fn s_of(r: &Value, k: &str) -> String {
    util::s_or(r.get(k).unwrap_or(&Value::Null))
}

/// `_pack(...)`
fn pack(
    groups: Option<Vec<Value>>,
    tasks: Option<Value>,
    plugins: Option<Vec<Value>>,
    skills: Option<Vec<Value>>,
    mcp: Option<Vec<Value>>,
    notes: Vec<Value>,
    error: &str,
) -> Value {
    let empty_tasks = json!({"total": 0, "by_status": {}, "items": []});
    let groups = groups.unwrap_or_default();
    let t = match tasks {
        Some(v) if util::truthy(&v) => v,
        _ => empty_tasks,
    };
    let mut m = Map::new();
    m.insert("groups".into(), Value::Array(groups.clone()));
    m.insert("tasks".into(), t);
    m.insert("plugins".into(), Value::Array(plugins.unwrap_or_default()));
    m.insert("skills".into(), Value::Array(skills.unwrap_or_default()));
    m.insert("mcp".into(), Value::Array(mcp.unwrap_or_default()));
    m.insert("notes".into(), Value::Array(notes));
    m.insert("error".into(), json!(error));
    Value::Object(m)
}

fn empty_pack() -> Value {
    pack(None, None, None, None, None, Vec::new(), "")
}

// ---------------------------------------------------------------- WorkBuddy

fn wb_groups(root: &Path) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for d in dirs(&root.join("projects")) {
        let fs_ = files(&d, "*.jsonl");
        if fs_.is_empty() {
            continue;
        }
        let big = {
            let mut best = fs_[0].clone();
            let mut best_m = mtime(&best);
            for f in &fs_ {
                let m = mtime(f);
                if m > best_m {
                    best = f.clone();
                    best_m = m;
                }
            }
            best
        };
        let mut cwd = String::new();
        if let Ok(bytes) = std::fs::read(&big) {
            let text = String::from_utf8_lossy(&bytes).to_string();
            for (i, line) in text.lines().enumerate() {
                if i > 60 {
                    break;
                }
                let Ok(o) = serde_json::from_str::<Value>(line) else { continue };
                if o.is_object() && util::truthy(util::get(&o, "cwd")) {
                    cwd = util::pystr(util::get(&o, "cwd"));
                    break;
                }
            }
        }
        let last = fs_.iter().map(|f| mtime(f)).fold(0.0f64, f64::max);
        let d_name = d.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let mut shown = if !cwd.is_empty() {
            cwd.clone()
        } else {
            d_name.replace("Users-", "~/").replace('-', "/")
        };
        if !cwd.is_empty() {
            let h = home().display().to_string();
            if shown.starts_with(&h) {
                shown = format!("~{}", &shown[h.len()..]);
            }
        }
        out.push(json!({
            "name": shown,
            "label": d_name,
            "path": rel_of(&d),
            "count": fs_.len() as i64,
            "last": day(last),
            "from": if cwd.is_empty() { "slug" } else { "cwd" },
        }));
    }
    // Python: sort(key=last, reverse=True) —— 稳定，相等的保持原顺序
    out.sort_by(|a, b| {
        util::s_or(b.get("last").unwrap_or(&Value::Null))
            .cmp(&util::s_or(a.get("last").unwrap_or(&Value::Null)))
    });
    out
}

fn wb_tasks(root: &Path) -> Value {
    let tdir = root.join("tasks");
    let all_dirs = dirs(&tdir);
    let mut by_status: Map<String, Value> = Map::new();
    let mut items: Vec<Value> = Vec::new();
    for d in &all_dirs {
        let dname = d.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        for f in files(d, "*.json") {
            let o = read_json(&f);
            if !o.is_object() {
                continue;
            }
            let st = {
                let v = util::s_or(util::get(&o, "status"));
                if v.is_empty() { "未知".to_string() } else { v }
            };
            let n = by_status.get(&st).and_then(|v| v.as_i64()).unwrap_or(0);
            by_status.insert(st.clone(), json!(n + 1));
            let stem = f.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            let name = {
                let a = util::s_or(util::get(&o, "subject"));
                if !a.is_empty() {
                    a
                } else {
                    let b = util::s_or(util::get(&o, "activeForm"));
                    if !b.is_empty() { b } else { stem }
                }
            };
            let at = if util::truthy(util::get(&o, "updatedAt")) {
                let v = util::fnum(util::get(&o, "updatedAt")).unwrap_or(0.0) / 1000.0;
                day(v)
            } else {
                day(mtime(&f))
            };
            items.push(json!({
                "name": util::cut(&name, 80),
                "status": st,
                "group": util::cut(&dname, 8),
                "at": at,
            }));
        }
    }
    items.sort_by(|a, b| {
        util::s_or(b.get("at").unwrap_or(&Value::Null))
            .cmp(&util::s_or(a.get("at").unwrap_or(&Value::Null)))
    });
    let total = items.len() as i64;
    let shown: Vec<Value> = items.into_iter().take(MAX_ITEMS).collect();
    let mut m = Map::new();
    m.insert("total".into(), json!(total));
    m.insert("by_status".into(), Value::Object(by_status));
    m.insert("items".into(), Value::Array(shown));
    m.insert("dirs".into(), json!(all_dirs.len()));
    Value::Object(m)
}

fn wb_plugins(root: &Path) -> Vec<Value> {
    let inst = read_json_or(&root.join("plugins").join("installed_plugins.json"), json!({}));
    let cfg = read_json_or(&root.join("plugins").join("configured-plugins.json"), json!({}));
    let on: Vec<String> = cfg
        .get("builtInMarketPlugins")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter(|x| x.is_object())
                .map(|x| util::pystr(util::get(x, "name")))
                .collect()
        })
        .unwrap_or_default();
    let mut out: Vec<Value> = Vec::new();
    if let Some(plugs) = inst.get("plugins").and_then(|v| v.as_object()) {
        for (key, arr) in plugs {
            let (name, market) = match key.split_once('@') {
                Some((a, b)) => (a.to_string(), b.to_string()),
                None => (key.clone(), String::new()),
            };
            let first = arr
                .as_array()
                .and_then(|a| a.first())
                .cloned()
                .filter(|v| v.is_object())
                .unwrap_or_else(|| json!({}));
            let is_on = if on.is_empty() { true } else { on.contains(&name) };
            let mut m = Map::new();
            m.insert("name".into(), json!(name));
            m.insert(
                "market".into(),
                json!(if market.is_empty() { "—".to_string() } else { market }),
            );
            m.insert(
                "ver".into(),
                json!(util::cut(&util::s_or(util::get(&first, "version")), 22)),
            );
            m.insert("on".into(), json!(is_on));
            m.insert(
                "at".into(),
                json!(util::cut(
                    &{
                        let a = util::s_or(util::get(&first, "lastUpdated"));
                        if a.is_empty() { util::s_or(util::get(&first, "installedAt")) } else { a }
                    },
                    10
                )),
            );
            let ip = util::s_or(util::get(&first, "installPath"));
            m.insert(
                "path".into(),
                json!(if ip.is_empty() { String::new() } else { rel_of(Path::new(&ip)) }),
            );
            out.push(Value::Object(m));
        }
    }
    out.sort_by(|a, b| {
        let ao = !util::truthy(a.get("on").unwrap_or(&Value::Null));
        let bo = !util::truthy(b.get("on").unwrap_or(&Value::Null));
        ao.cmp(&bo).then_with(|| {
            util::s_or(a.get("name").unwrap_or(&Value::Null))
                .cmp(&util::s_or(b.get("name").unwrap_or(&Value::Null)))
        })
    });
    out
}

fn wb_skills(root: &Path) -> Vec<Value> {
    let mut out = Vec::new();
    for d in dirs(&root.join("skills")) {
        if d.file_name().map(|s| s.to_string_lossy().starts_with('_')).unwrap_or(false) {
            continue;
        }
        if let Some(s) = skill_of(&d) {
            out.push(s);
        }
    }
    out
}

fn wb_mcp(root: &Path) -> Vec<Value> {
    let d = read_json_or(&root.join("mcp.json"), json!({}));
    let srv = d.get("mcpServers").cloned().unwrap_or(json!({}));
    let srv_map = srv.as_object().cloned().unwrap_or_default();
    let tools = read_json_or(&root.join("mcp-tool-list.json"), json!({}));
    let entries = tools.get("entries").cloned().unwrap_or(json!({}));
    let only: i64 = if srv_map.len() == 1 {
        entries
            .as_object()
            .map(|m| {
                m.values()
                    .map(|v| v.as_array().map(|a| a.len()).unwrap_or(0) as i64)
                    .sum()
            })
            .unwrap_or(0)
    } else {
        0
    };
    let single = srv_map.len() == 1;
    let mut out = Vec::new();
    for (name, cfg) in srv_map {
        if !cfg.is_object() {
            continue;
        }
        let kind = if util::truthy(util::get(&cfg, "url")) {
            "http"
        } else if util::truthy(util::get(&cfg, "command")) {
            "stdio"
        } else {
            "?"
        };
        let target = {
            let u = util::s_or(util::get(&cfg, "url"));
            if !u.is_empty() { u } else { util::s_or(util::get(&cfg, "command")) }
        };
        out.push(json!({
            "name": name,
            "kind": kind,
            "target": util::cut(&target, 70),
            "tools": if single { only } else { 0 },
            "on": true,
        }));
    }
    out
}

fn workbuddy() -> Value {
    let root = home().join(".workbuddy");
    if !root.is_dir() {
        return empty_pack();
    }
    let mut notes: Vec<Value> = Vec::new();
    let g = wb_groups(&root);
    let t = wb_tasks(&root);
    let p = wb_plugins(&root);
    let s = wb_skills(&root);
    let m = wb_mcp(&root);
    let _ = &mut notes;
    pack(Some(g), Some(t), Some(p), Some(s), Some(m), notes, "")
}

// ---------------------------------------------------------------- Cherry Studio

fn cherry() -> Value {
    let base = asupport().join("CherryStudio").join("Data");
    if !base.is_dir() {
        return empty_pack();
    }
    let mut notes: Vec<Value> = Vec::new();
    let mut groups: Vec<Value> = Vec::new();
    let mut tasks_items: Vec<Value> = Vec::new();
    let mut tasks_by_status: Map<String, Value> = Map::new();
    let mut tasks_runs: Map<String, Value> = Map::new();
    let mut skills: Vec<Value> = Vec::new();
    let mut mcp: Vec<Value> = Vec::new();
    let plugins: Vec<Value> = Vec::new();

    let adb = base.join("agents.db");
    if adb.exists() {
        let rows = sqlite_rows(&adb, "select id,name,type,model from agents");
        let mut sess: Map<String, Value> = Map::new();
        for r in sqlite_rows(&adb, "select agent_id,count(*) as n from sessions group by agent_id") {
            sess.insert(s_of(&r, "agent_id"), json!(i_of(&r, "n")));
        }
        for r in rows {
            let id = s_of(&r, "id");
            let name = {
                let n = s_of(&r, "name");
                if n.is_empty() { id.clone() } else { n }
            };
            groups.push(json!({
                "name": name,
                "label": s_of(&r, "type"),
                "path": id,
                "count": sess.get(&s_of(&r, "id")).and_then(|v| v.as_i64()).unwrap_or(0),
                "last": "",
                "from": "agent",
                "model": util::cut(&s_of(&r, "model"), 40),
            }));
        }
        for r in sqlite_rows(
            &adb,
            "select name,schedule_type,schedule_value,status,last_run,agent_id from scheduled_tasks",
        ) {
            let st = {
                let s = s_of(&r, "status");
                if s.is_empty() { "?".to_string() } else { s }
            };
            let n = tasks_by_status.get(&st).and_then(|v| v.as_i64()).unwrap_or(0);
            tasks_by_status.insert(st.clone(), json!(n + 1));
            let note = format!(
                "{} {}",
                s_of(&r, "schedule_type"),
                s_of(&r, "schedule_value")
            )
            .trim()
            .to_string();
            tasks_items.push(json!({
                "name": s_of(&r, "name"),
                "status": st,
                "group": s_of(&r, "schedule_type"),
                "at": util::cut(&s_of(&r, "last_run"), 10),
                "note": note,
            }));
        }
        for r in sqlite_rows(&adb, "select status,count(*) as n from task_run_logs group by status") {
            tasks_runs.insert(s_of(&r, "status"), json!(i_of(&r, "n")));
        }
        for r in sqlite_rows(&adb, "select name,description,source from skills") {
            skills.push(json!({
                "name": s_of(&r, "name"),
                "desc": util::cut(&s_of(&r, "description"), 160),
                "path": "",
                "at": "",
                "src": s_of(&r, "source"),
            }));
        }
    }

    let sdb = base.join("cherrystudio.sqlite");
    if sdb.exists() {
        let mut bind: Map<String, Value> = Map::new();
        for r in sqlite_rows(
            &sdb,
            "select mcp_server_id,count(*) as n from agent_mcp_server group by mcp_server_id",
        ) {
            bind.insert(s_of(&r, "mcp_server_id"), json!(i_of(&r, "n")));
        }
        for r in sqlite_rows(
            &sdb,
            "select id,name,type,base_url,command from mcp_server",
        ) {
            let id = s_of(&r, "id");
            let name = {
                let n = s_of(&r, "name");
                if n.is_empty() { id.clone() } else { n }
            };
            let target = {
                let b = s_of(&r, "base_url");
                if !b.is_empty() { b } else { s_of(&r, "command") }
            };
            mcp.push(json!({
                "name": name,
                "kind": s_of(&r, "type"),
                "target": util::cut(&target, 70),
                "tools": bind.get(&id).and_then(|v| v.as_i64()).unwrap_or(0),
                "on": true,
            }));
        }
    }

    let have: Vec<String> = skills
        .iter()
        .map(|s| util::s_or(util::get(s, "name")))
        .collect();
    let mut have = have;
    for d in dirs(&base.join("Skills")) {
        if let Some(mut s) = skill_of(&d) {
            let name = util::s_or(util::get(&s, "name"));
            if !have.contains(&name) {
                if let Some(m) = s.as_object_mut() {
                    if !util::truthy(m.get("src").unwrap_or(&Value::Null)) {
                        m.insert("src".into(), json!("内置"));
                    }
                }
                have.push(name);
                skills.push(s);
            }
        }
    }
    if mcp.is_empty() {
        notes.push(json!("没读到 MCP 绑定（可能这版把服务存在别处）"));
    }

    let mut tasks = Map::new();
    tasks.insert("total".into(), json!(tasks_items.len()));
    tasks.insert("by_status".into(), Value::Object(tasks_by_status));
    tasks.insert("items".into(), Value::Array(tasks_items));
    tasks.insert("runs".into(), Value::Object(tasks_runs));
    pack(
        Some(groups),
        Some(Value::Object(tasks)),
        Some(plugins),
        Some(skills),
        Some(mcp),
        notes,
        "",
    )
}

// ---------------------------------------------------------------- TRAE

fn trae_slug(name: &str) -> String {
    static TAIL: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static SLASH: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = TAIL.get_or_init(|| regex::Regex::new(r"-(?:p\d+-)?[0-9a-z]{10,}$").unwrap());
    let sl = SLASH.get_or_init(|| regex::Regex::new(r"/{2,}").unwrap());
    let mut s = re.replace(name, "").trim_start_matches('-').to_string();
    let cut = s.find("---");
    let lost = cut.is_some();
    if let Some(i) = cut {
        s = s[..i].to_string();
    }
    let s = sl.replace_all(&s.replace('-', "/"), "/").to_string();
    let parts: Vec<&str> = s.split('/').collect();
    let mut out = if parts.len() > 1 && parts[0] == "Users" {
        format!("~/{}", parts.get(2..).map(|r| r.join("/")).unwrap_or_default())
    } else if parts.first() == Some(&"Volumes") {
        format!("/{}", s)
    } else {
        s
    };
    if lost {
        out.push('…');
    }
    out
}

fn mcp_name(raw: &str) -> String {
    let mut s = if let Some(r) = raw.strip_prefix("mcp_") { r } else { raw };
    if s.starts_with("plugin_") {
        s = s.rsplit_once('_').map(|(_, b)| b).unwrap_or(s);
    }
    let t = s.replace('_', " ");
    if t.is_empty() {
        raw.to_string()
    } else {
        t
    }
}

fn trae() -> Value {
    let root = home().join(".trae-cn");
    if !root.is_dir() {
        return empty_pack();
    }
    let mut notes: Vec<Value> = Vec::new();
    let mut groups: Vec<Value> = Vec::new();
    for d in dirs(&root.join("memory").join("projects")) {
        let dname = d.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let fs_ = files(&d, "*.jsonl");
        let sub = dirs(&d);
        let n: i64 = if !fs_.is_empty() {
            fs_.len() as i64
        } else {
            sub.iter().map(|x| files(x, "*.jsonl").len() as i64).sum()
        };
        let mut last: f64 = 0.0;
        for f in &fs_ {
            last = last.max(mtime(f));
        }
        for x in &sub {
            last = last.max(mtime(x));
        }
        groups.push(json!({
            "name": trae_slug(&dname),
            "label": dname,
            "path": rel_of(&d),
            "count": n,
            "last": day(last),
            "from": "slug",
        }));
    }
    groups.sort_by(|a, b| {
        util::s_or(b.get("last").unwrap_or(&Value::Null))
            .cmp(&util::s_or(a.get("last").unwrap_or(&Value::Null)))
    });
    if !groups.is_empty() {
        notes.push(json!("分组目录名里的中文被 TRAE 换成了 '-'，还原不回来（按 slug 显示）"));
    }

    let mut tasks_items: Vec<Value> = Vec::new();
    let tdir = root.join("mcps");
    for d in dirs(&tdir) {
        let dname = d.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        if !dname.starts_with("s_") {
            continue;
        }
        static TAIL: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let re = TAIL.get_or_init(|| regex::Regex::new(r"-(?:p\d+-)?[0-9a-z]{10,}$").unwrap());
        let raw = &dname[2..];
        let t = re.replace(raw, "").to_string();
        let title = t.rsplit_once('-').map(|(a, _)| a.to_string()).unwrap_or(t);
        let title = if title.is_empty() { dname.clone() } else { title };
        let servers = dirs(&d.join("solo_agent"));
        tasks_items.push(json!({
            "name": util::cut(&title, 80),
            "status": "工作区",
            "group": format!("{} 个 MCP", servers.len()),
            "at": day(mtime(&d)),
        }));
    }
    let mut by_status: Map<String, Value> = Map::new();
    if !tasks_items.is_empty() {
        by_status.insert("工作区".into(), json!(tasks_items.len()));
    }

    let mut plugins: Vec<Value> = Vec::new();
    let cfg = read_json_or(&root.join("plugin-config.json"), json!({}));
    let cfg_plugins = cfg.get("plugins").cloned().unwrap_or(json!({}));
    let mut meta: Map<String, Value> = Map::new();
    let inst = read_json_or(&root.join("installed-plugins.json"), json!({}));
    if let Some(mks) = inst
        .get("data")
        .and_then(|d| d.get("marketplaces"))
        .and_then(|v| v.as_array())
    {
        for mk in mks {
            if let Some(pls) = mk.get("plugins").and_then(|v| v.as_array()) {
                for pl in pls {
                    let pid = {
                        let a = util::s_or(util::get(pl, "plugin_id"));
                        if a.is_empty() { util::s_or(util::get(pl, "name")) } else { a }
                    };
                    if !pid.is_empty() {
                        meta.insert(pid, pl.clone());
                    }
                }
            }
        }
    }
    if let Some(cm) = cfg_plugins.as_object() {
        for (pid, st) in cm {
            let m = meta.get(pid).cloned().unwrap_or_else(|| json!({}));
            let name = {
                let d = util::s_or(util::get(&m, "display_name"));
                if d.is_empty() {
                    pid.split(':').last().unwrap_or("").to_string()
                } else {
                    d
                }
            };
            let market = {
                let r = util::s_or(util::get(&m, "registry"));
                if r.is_empty() {
                    pid.split(':').next().unwrap_or("").to_string()
                } else {
                    r
                }
            };
            let on = if st.is_object() {
                truthy_or(util::get(st, "user_enabled"), true)
            } else {
                truthy_or(&Value::Null, true)
            };
            plugins.push(json!({
                "name": name,
                "market": market,
                "ver": util::s_or(util::get(&m, "version")),
                "on": on,
                "at": "",
                "desc": util::cut(&util::s_or(util::get(&m, "description")), 90),
            }));
        }
    }
    plugins.sort_by(|a, b| {
        let ao = !util::truthy(a.get("on").unwrap_or(&Value::Null));
        let bo = !util::truthy(b.get("on").unwrap_or(&Value::Null));
        ao.cmp(&bo).then_with(|| {
            util::s_or(a.get("name").unwrap_or(&Value::Null))
                .cmp(&util::s_or(b.get("name").unwrap_or(&Value::Null)))
        })
    });

    let mut skills: Vec<Value> = Vec::new();
    for d in dirs(&root.join("builtin_skills"))
        .into_iter()
        .chain(dirs(&root.join("skills")))
    {
        if let Some(mut s) = skill_of(&d) {
            let src = if util::s_or(util::get(&s, "path")).contains("builtin") {
                "内置"
            } else {
                "用户"
            };
            if let Some(m) = s.as_object_mut() {
                m.insert("src".into(), json!(src));
            }
            skills.push(s);
        }
    }

    let mut mcp: Vec<Value> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for wk in dirs(&tdir) {
        for srv in dirs(&wk.join("solo_agent")) {
            let n = srv.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            if !n.starts_with("mcp_") || seen.contains(&n) {
                continue;
            }
            seen.push(n.clone());
            let meta = read_json_or(&srv.join("SERVER_METADATA.json"), json!({}));
            let tools = dirs(&srv.join("tools"));
            let nm = {
                let a = util::s_or(util::get(&meta, "server_name"));
                if a.is_empty() { n.clone() } else { a }
            };
            mcp.push(json!({
                "name": mcp_name(&nm),
                "kind": "builtin",
                "target": rel_of(&srv),
                "tools": tools.len(),
                "on": true,
            }));
        }
    }
    for f in glob_depth3_files(&root.join("plugins"), ".mcp.json") {
        let d = read_json_or(&f, json!({}));
        if let Some(servers) = d.get("mcpServers").and_then(|v| v.as_object()) {
            for (name, cfg) in servers {
                if seen.contains(name) {
                    continue;
                }
                seen.push(name.clone());
                let cfg = if cfg.is_object() { cfg.clone() } else { json!({}) };
                let mut tgt = util::s_or(util::get(&cfg, "url"));
                if tgt.is_empty() {
                    let mut bits: Vec<String> = Vec::new();
                    let c = util::s_or(util::get(&cfg, "command"));
                    if !c.is_empty() {
                        bits.push(c);
                    }
                    if let Some(args) = cfg.get("args").and_then(|v| v.as_array()) {
                        if let Some(a0) = args.first() {
                            let s = util::pystr(a0);
                            if let Some(n) = Path::new(&s).file_name() {
                                bits.push(n.to_string_lossy().to_string());
                            }
                        }
                    }
                    tgt = bits.join(" ");
                }
                mcp.push(json!({
                    "name": name,
                    "kind": "plugin",
                    "target": util::cut(&tgt, 70),
                    "tools": 0,
                    "on": true,
                }));
            }
        }
    }
    // 同一个服务会被多个插件各带一份 .mcp.json，按名字去重（先到的算）
    let mut uniq: Vec<Value> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    for x in mcp {
        let n = util::s_or(util::get(&x, "name"));
        if !names.contains(&n) {
            names.push(n);
            uniq.push(x);
        }
    }

    let mut tasks = Map::new();
    tasks.insert("total".into(), json!(tasks_items.len()));
    tasks.insert("by_status".into(), Value::Object(by_status));
    tasks.insert("items".into(), Value::Array(tasks_items));
    pack(
        Some(groups),
        Some(Value::Object(tasks)),
        Some(plugins),
        Some(skills),
        Some(uniq),
        notes,
        "",
    )
}

fn truthy_or(v: &Value, default: bool) -> bool {
    if v.is_null() {
        default
    } else {
        util::truthy(v)
    }
}

// ---------------------------------------------------------------- 千问办公

fn qwenwork() -> Value {
    let root = home().join(".qwenworkcn");
    if !root.is_dir() {
        return empty_pack();
    }
    let mut notes: Vec<Value> = Vec::new();
    let mut groups: Vec<Value> = Vec::new();
    for d in dirs(&root.join("projects")) {
        let dname = d.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let fs_ = files(&d, "*.jsonl");
        let last = fs_.iter().map(|f| mtime(f)).fold(0.0f64, f64::max);
        groups.push(json!({
            "name": trae_slug(&dname),
            "label": dname,
            "path": rel_of(&d),
            "count": fs_.len(),
            "last": day(last),
            "from": "slug",
        }));
    }
    groups.sort_by(|a, b| {
        util::s_or(b.get("last").unwrap_or(&Value::Null))
            .cmp(&util::s_or(a.get("last").unwrap_or(&Value::Null)))
    });

    let mut tasks_items: Vec<Value> = Vec::new();
    let pl = read_json_or(&root.join("projectless-task-folder-default.json"), json!({}));
    if util::truthy(util::get(&pl, "path")) {
        tasks_items.push(json!({
            "name": "无项目任务落点",
            "status": "配置",
            "group": util::s_or(util::get(&pl, "source")),
            "at": "",
            "note": util::s_or(util::get(&pl, "path")),
        }));
    }
    let mut by_status: Map<String, Value> = Map::new();
    if !tasks_items.is_empty() {
        by_status.insert("配置".into(), json!(1));
    }

    let mut plugins: Vec<Value> = Vec::new();
    for d in dirs(&root.join("plugins").join("marketplaces")) {
        let sub = dirs(&d);
        plugins.push(json!({
            "name": d.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
            "market": "marketplace",
            "ver": sub.len().to_string(),
            "on": true,
            "at": "",
            "desc": if sub.is_empty() { String::new() } else { format!("{} 个插件", sub.len()) },
        }));
    }

    let usage = read_json_or(&root.join("skill-usage.json"), json!({}));
    let mut skills: Vec<Value> = Vec::new();
    for d in dirs(&root.join("skills")) {
        let Some(mut s) = skill_of(&d) else { continue };
        let name = util::s_or(util::get(&s, "name"));
        let dname = d.file_name().map(|x| x.to_string_lossy().to_string()).unwrap_or_default();
        let u = {
            let a = usage.get(&name).cloned().unwrap_or(Value::Null);
            if util::truthy(&a) {
                a
            } else {
                usage.get(&dname).cloned().unwrap_or_else(|| json!({}))
            }
        };
        let used = if u.is_object() {
            u.get("usageCount").map(util::int_float).unwrap_or(0)
        } else {
            0
        };
        let last_used = if u.is_object() && util::truthy(util::get(&u, "lastUsedAt")) {
            day(util::fnum(util::get(&u, "lastUsedAt")).unwrap_or(0.0) / 1000.0)
        } else {
            String::new()
        };
        if let Some(m) = s.as_object_mut() {
            m.insert("used".into(), json!(used));
            m.insert("last_used".into(), json!(last_used));
            m.insert("src".into(), json!("用户"));
        }
        skills.push(s);
    }
    skills.sort_by(|a, b| {
        let au = -a.get("used").and_then(|v| v.as_i64()).unwrap_or(0);
        let bu = -b.get("used").and_then(|v| v.as_i64()).unwrap_or(0);
        au.cmp(&bu).then_with(|| {
            util::s_or(a.get("name").unwrap_or(&Value::Null))
                .cmp(&util::s_or(b.get("name").unwrap_or(&Value::Null)))
        })
    });

    let cache = root.join("cache").join("mcp");
    if cache.is_dir() {
        notes.push(json!("MCP 只有市场缓存，没读到已配置的服务"));
    }
    let mut tasks = Map::new();
    tasks.insert("total".into(), json!(tasks_items.len()));
    tasks.insert("by_status".into(), Value::Object(by_status));
    tasks.insert("items".into(), Value::Array(tasks_items));
    pack(
        Some(groups),
        Some(Value::Object(tasks)),
        Some(plugins),
        Some(skills),
        Some(Vec::new()),
        notes,
        "",
    )
}

// ---------------------------------------------------------------- 汇总

fn count_of(cfg: &Value, facet: &str) -> i64 {
    let v = cfg.get(facet).cloned().unwrap_or(Value::Null);
    if v.is_object() {
        v.get("total").map(util::int_float).unwrap_or(0)
    } else {
        v.as_array().map(|a| a.len()).unwrap_or(0) as i64
    }
}

static CACHE: Mutex<Option<(f64, Value)>> = Mutex::new(None);

fn scan(force: bool) -> Value {
    let now = util::now_secs() as f64;
    if !force {
        if let Ok(g) = CACHE.lock() {
            if let Some((at, data)) = g.as_ref() {
                if now - at < CACHE_TTL {
                    let mut d = data.clone();
                    if let Some(m) = d.as_object_mut() {
                        m.insert("cached".into(), json!(true));
                    }
                    return d;
                }
            }
        }
    }
    let t0 = std::time::Instant::now();
    let sources: [(&str, &str, fn() -> Value); 4] = [
        ("workbuddy", "WorkBuddy", workbuddy),
        ("cherry", "Cherry Studio", cherry),
        ("trae", "TRAE", trae),
        ("qwenwork", "千问办公", qwenwork),
    ];
    let mut agents: Vec<Value> = Vec::new();
    for (aid, name, f) in sources {
        let cfg = f();
        let facets = ["groups", "tasks", "plugins", "skills", "mcp"];
        let inst = facets.iter().any(|x| count_of(&cfg, x) > 0)
            || cfg
                .get("groups")
                .map(|v| v.as_array().map(|a| !a.is_empty()).unwrap_or(false))
                .unwrap_or(false);
        let mut counts = Map::new();
        for x in facets {
            counts.insert(x.to_string(), json!(count_of(&cfg, x)));
        }
        let mut m = Map::new();
        m.insert("id".into(), json!(aid));
        m.insert("name".into(), json!(name));
        m.insert("installed".into(), json!(inst));
        m.insert("counts".into(), Value::Object(counts));
        for k in ["groups", "tasks", "plugins", "skills", "mcp", "notes", "error"] {
            m.insert(k.to_string(), cfg.get(k).cloned().unwrap_or(Value::Null));
        }
        agents.push(Value::Object(m));
    }
    let live: Vec<&Value> = agents
        .iter()
        .filter(|a| a.get("installed").and_then(|v| v.as_bool()).unwrap_or(false))
        .collect();
    let mut totals = Map::new();
    totals.insert("installed".into(), json!(live.len()));
    for f in ["groups", "tasks", "plugins", "skills", "mcp"] {
        let sum: i64 = live
            .iter()
            .map(|a| {
                a.get("counts")
                    .and_then(|c| c.get(f))
                    .map(util::int_float)
                    .unwrap_or(0)
            })
            .sum();
        totals.insert(f.to_string(), json!(sum));
    }
    let ms = t0.elapsed().as_millis() as i64;
    let mut d = Map::new();
    d.insert("ok".into(), json!(true));
    d.insert("agents".into(), Value::Array(agents));
    d.insert("totals".into(), Value::Object(totals));
    d.insert("ms".into(), json!(ms));
    d.insert("at".into(), json!(util::strftime_now("%H:%M")));
    d.insert("cached".into(), json!(false));
    let val = Value::Object(d);
    if let Ok(mut g) = CACHE.lock() {
        *g = Some((now, val.clone()));
    }
    val
}

async fn scan_route(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let force = crate::routes::common::q_int(&uri, "force", 0)
        .map(|v| v != 0)
        .unwrap_or(false);
    let out = tokio::task::spawn_blocking(move || scan(force))
        .await
        .unwrap_or_else(|_| json!({"ok": false, "error": "RuntimeError: 扫描失败", "agents": []}));
    Json(out).into_response()
}
