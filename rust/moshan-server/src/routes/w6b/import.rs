// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：`moshan/agentscan.py`（数据面探活）+ `moshan/agent_import.py`（历史账导入）。
//       四条路由：import/detect、import/scan、import(apply)、agent/probe（在 agent.rs）。
// 不做：不解析别人家的会话正文（那是采集层 hostwork 的活，本泳道没迁），
//       所以 import/scan 里**没有 WorkBuddy / TRAE / Claude 的采集层行** ——
//       见交付说明的「已知分叉」。
//
// 扫描很慢（本机 ~/.workbuddy 12 GB），一律走 spawn_blocking，别占 tokio 工作线程。

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
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub fn router() -> Router<Arc<Ctx>> {
    Router::new()
        .route("/api/agent/import/detect", get(detect_route))
        .route("/api/agent/import/scan", get(scan_route))
        .route("/api/agent/import", post(apply_route))
}

// ---------------------------------------------------------------- agentscan

const MAX_FILES: i64 = 40000;
const SCAN_BUDGET: f64 = 2.5;

struct Probe {
    id: &'static str,
    name: &'static str,
    parser: Option<&'static str>,
    paths: &'static [(&'static str, &'static str)],
    note: &'static str,
}

const PROBES: &[Probe] = &[
    Probe {
        id: "workbuddy",
        name: "WorkBuddy",
        parser: Some("workbuddy"),
        paths: &[("home", ".workbuddy/projects")],
        note: "按会话 jsonl 解析：轮次、工具调用、token",
    },
    Probe {
        id: "cherry",
        name: "Cherry Studio",
        parser: Some("cherry"),
        paths: &[("appsupport", "CherryStudio/Data"), ("appsupport", "CherryStudio")],
        note: "读它的 sqlite（只读打开）：消息数、模型、token",
    },
    Probe {
        id: "claude",
        name: "Claude Code",
        parser: Some("claude"),
        paths: &[("home", ".claude/projects")],
        note: "按会话 jsonl 解析",
    },
    Probe {
        id: "trae",
        name: "TRAE",
        parser: Some("trae"),
        paths: &[("home", ".trae-cn/memory/projects")],
        note: "memory 目录下的会话记忆（明文）",
    },
    Probe {
        id: "codex",
        name: "Codex",
        parser: Some("codex"),
        paths: &[("home", ".codex/sessions")],
        note: "rollout-*.jsonl，按年月日分目录",
    },
    Probe {
        id: "deepseek",
        name: "DeepSeek Harness",
        parser: None,
        paths: &[
            ("home", ".deepseek"),
            ("home", ".deepseek-harness"),
            ("home", ".dsh"),
            ("appsupport", "DeepSeek Harness"),
            ("appsupport", "deepseek-harness"),
            ("appsupport", "dsh"),
        ],
        note: "dsh：桌面版或 npx，会话在本地 store",
    },
    Probe {
        id: "qwenwork",
        name: "千问办公 QwenWork",
        parser: Some("qwenwork"),
        paths: &[
            ("home", ".qwenworkcn"),
            ("home", ".qwenwork"),
            ("appsupport", "QwenWork"),
            ("appsupport", "qwenwork"),
        ],
        note: "projects/*.jsonl，格式同 Claude Code，但不写 usage（token 读不到）",
    },
    Probe {
        id: "openworker",
        name: "OpenWorker",
        parser: None,
        paths: &[
            ("home", ".openworker"),
            ("home", ".openwork"),
            ("appsupport", "OpenWorker"),
            ("appsupport", "openworker"),
            ("appsupport", "com.openworker.app"),
        ],
        note: "Tauri 壳 + Python agent server，本地 store",
    },
    Probe {
        id: "lmstudio",
        name: "LM Studio",
        parser: None,
        paths: &[("home", ".lmstudio"), ("home", ".cache/lm-studio"), ("appsupport", "LM Studio")],
        note: "会话在 ~/.lmstudio/conversations，模型另算",
    },
    Probe {
        id: "cursor",
        name: "Cursor",
        parser: None,
        paths: &[("appsupport", "Cursor/User/globalStorage"), ("home", ".cursor")],
        note: "对话在加密库里，只能看用量",
    },
];

fn root_of(kind: &str) -> Option<PathBuf> {
    match kind {
        "home" => Some(util::home()),
        "appdata" => std::env::var_os("APPDATA").map(PathBuf::from),
        "localappdata" => std::env::var_os("LOCALAPPDATA").map(PathBuf::from),
        "appsupport" => Some(util::home().join("Library").join("Application Support")),
        _ => None,
    }
}

fn fmt_bytes(n: i64) -> String {
    if n >= 1 << 30 {
        return format!("{} GB", util::f1(n as f64 / (1u64 << 30) as f64));
    }
    if n >= 1 << 20 {
        return format!("{} MB", util::f1(n as f64 / (1u64 << 20) as f64));
    }
    if n >= 1 << 10 {
        return format!("{} KB", util::f0(n as f64 / 1024.0));
    }
    format!("{} B", n)
}

fn day_of(ts: f64) -> String {
    if ts <= 0.0 {
        return String::new();
    }
    util::day_of_ts_f(ts)
}

struct Meas {
    files: i64,
    bytes: i64,
    last: f64,
    partial: bool,
    error: String,
}

/// `_measure`：数文件与字节，超预算就停（partial=True）。
fn measure(p: &Path) -> Meas {
    let mut out = Meas { files: 0, bytes: 0, last: 0.0, partial: false, error: String::new() };
    let Ok(md) = std::fs::metadata(p) else {
        out.error = format!("[Errno 2] No such file or directory: '{}'", p.display());
        return out;
    };
    if md.is_file() {
        out.files = 1;
        out.bytes = md.len() as i64;
        out.last = mtime_of(p);
        return out;
    }
    let t0 = std::time::Instant::now();
    walk_count(p, &mut out, t0);
    out
}

fn walk_count(p: &Path, out: &mut Meas, t0: std::time::Instant) {
    if out.partial {
        return;
    }
    let Ok(rd) = std::fs::read_dir(p) else {
        if out.error.is_empty() {
            out.error = format!("[Errno 13] Permission denied: '{}'", p.display());
        }
        return;
    };
    let mut files: Vec<PathBuf> = Vec::new();
    let mut dirs: Vec<PathBuf> = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let path = e.path();
        // os.walk 用 `entry.is_dir()`（**跟随**符号链接）分类：指向目录的链接进 dirs、
        // 指向文件的链接进 names。所以分类必须用 metadata；而要不要递归进去另说
        // （`followlinks=False`，链接目录只登记不进去）。
        let is_dir = std::fs::metadata(&path).map(|m| m.is_dir()).unwrap_or(false);
        if is_dir {
            if name.starts_with('.') && name != ".lmstudio" {
                continue;
            }
            let is_link = e.file_type().map(|t| t.is_symlink()).unwrap_or(false);
            if !is_link {
                dirs.push(path);
            }
        } else {
            if name.starts_with("._") {
                continue;
            }
            // 符号链接指向的文件也算文件（os.walk 把链接当文件，stat 跟随）
            files.push(path);
        }
    }
    for f in files {
        out.files += 1;
        if let Ok(md) = std::fs::metadata(&f) {
            out.bytes += md.len() as i64;
            let mt = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            if mt > out.last {
                out.last = mt;
            }
        }
        if out.files >= MAX_FILES
            || (out.files % 400 == 0 && t0.elapsed().as_secs_f64() > SCAN_BUDGET)
        {
            out.partial = true;
            return;
        }
    }
    for d in dirs {
        walk_count(&d, out, t0);
        if out.partial {
            return;
        }
    }
}

fn mtime_of(p: &Path) -> f64 {
    std::fs::metadata(p)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// `agentscan.detect()`
pub fn detect() -> Value {
    let mut out: Vec<Value> = Vec::new();
    for spec in PROBES {
        let mut found: Vec<(PathBuf, Meas)> = Vec::new();
        for (kind, rel) in spec.paths {
            let Some(base) = root_of(kind) else { continue };
            let p = base.join(rel);
            if !p.exists() {
                continue;
            }
            let m = measure(&p);
            found.push((p, m));
        }
        if found.is_empty() {
            let mut o = Map::new();
            o.insert("id".into(), json!(spec.id));
            o.insert("name".into(), json!(spec.name));
            o.insert("installed".into(), json!(false));
            o.insert("parsable".into(), json!(spec.parser.is_some()));
            o.insert("note".into(), json!(spec.note));
            o.insert("path".into(), json!(""));
            o.insert("files".into(), json!(0));
            o.insert("bytes".into(), json!(0));
            o.insert("size".into(), json!(""));
            o.insert("last".into(), json!(""));
            out.push(Value::Object(o));
            continue;
        }
        // 多个候选路径都命中时，取文件最多的那份当主路径
        found.sort_by(|a, b| b.1.files.cmp(&a.1.files));
        let main = &found[0];
        let total_files: i64 = found.iter().map(|x| x.1.files).sum();
        let total_bytes: i64 = found.iter().map(|x| x.1.bytes).sum();
        let mut o = Map::new();
        o.insert("id".into(), json!(spec.id));
        o.insert("name".into(), json!(spec.name));
        o.insert("installed".into(), json!(true));
        o.insert("parsable".into(), json!(spec.parser.is_some()));
        o.insert("note".into(), json!(spec.note));
        o.insert("path".into(), json!(main.0.display().to_string()));
        if found.len() > 1 {
            o.insert(
                "alts".into(),
                json!(found[1..].iter().map(|x| x.0.display().to_string()).collect::<Vec<_>>()),
            );
        } else {
            o.insert("alts".into(), json!([]));
        }
        o.insert("files".into(), json!(total_files));
        o.insert("bytes".into(), json!(total_bytes));
        o.insert("size".into(), json!(fmt_bytes(total_bytes)));
        o.insert("last".into(), json!(day_of(main.1.last)));
        o.insert(
            "partial".into(),
            json!(found.iter().any(|x| x.1.partial)),
        );
        out.push(Value::Object(o));
    }
    let inst: Vec<&Value> = out
        .iter()
        .filter(|a| a.get("installed").map(truthy).unwrap_or(false))
        .collect();
    let parsable = inst
        .iter()
        .filter(|a| a.get("parsable").map(truthy).unwrap_or(false))
        .count();
    let bytes: i64 = inst.iter().map(|a| a.get("bytes").and_then(|x| x.as_i64()).unwrap_or(0)).sum();
    let mut totals = Map::new();
    totals.insert("installed".into(), json!(inst.len()));
    totals.insert("parsable".into(), json!(parsable));
    totals.insert("bytes".into(), json!(bytes));
    totals.insert("size".into(), json!(fmt_bytes(bytes)));
    let mut o = Map::new();
    o.insert("ok".into(), json!(true));
    o.insert("agents".into(), json!(out));
    o.insert("totals".into(), Value::Object(totals));
    Value::Object(o)
}

/// `agentscan.probe_manual(path)`
pub fn probe_manual(raw: &str) -> Value {
    let p = util::expanduser(raw);
    if !p.exists() {
        return json!({"ok": false, "error": "这个路径不存在"});
    }
    let m = measure(&p);
    let mut o = Map::new();
    o.insert("ok".into(), json!(true));
    o.insert("path".into(), json!(p.display().to_string()));
    o.insert("files".into(), json!(m.files));
    o.insert("bytes".into(), json!(m.bytes));
    o.insert("size".into(), json!(fmt_bytes(m.bytes)));
    o.insert("last".into(), json!(day_of(m.last)));
    Value::Object(o)
}

// ---------------------------------------------------------------- agent_import

pub const NOTE_NAME: &str = "Agent自述.jsonl";
pub const SOURCE_TAG: &str = "导入";
pub const REG_NAME: &str = "Agent数据源.json";

fn note_path(ctx: &Ctx) -> PathBuf {
    ctx.library_dir.join("_本机工作").join(NOTE_NAME)
}

fn reg_path(ctx: &Ctx) -> PathBuf {
    ctx.library_dir.join("_工作台").join(REG_NAME)
}

fn fmt_tokens(n: i64) -> String {
    if n >= 1_000_000_000 {
        return format!("{}G", util::f1(n as f64 / 1_000_000_000.0));
    }
    if n >= 1_000_000 {
        return format!("{}M", util::f1(n as f64 / 1_000_000.0));
    }
    if n >= 1000 {
        return format!("{}k", util::f0(n as f64 / 1000.0));
    }
    n.to_string()
}

/// `_cherry_stat`：Cherry Studio 的 sqlite（只读）。读不到就说读不到。
///
/// 这里复刻的是 `agent_import._cherry_stat` 用到的那几个数（calls / tokens /
/// cny / models / by_day），价表与 `cherry_usage` 同一份。
fn cherry_stat() -> Value {
    let db = util::home()
        .join("Library")
        .join("Application Support")
        .join("CherryStudio")
        .join("Data")
        .join("cherrystudio.sqlite");
    if !db.exists() {
        return json!({"name": "Cherry Studio", "ok": false,
                      "err": "本机没找到 Cherry Studio 的数据库"});
    }
    let sql = "SELECT provider_id, provider_name, model_id, model_name, input_tokens, \
               output_tokens, cache_read_tokens, cost, created_at FROM ai_usage_record";
    let Some(rows) = cr_sqlite(&db, sql) else {
        return json!({"name": "Cherry Studio", "ok": false,
                      "err": "OperationalError: 读不了它的库"});
    };
    let mut models: Vec<Map<String, Value>> = Vec::new();
    let mut t_in = 0i64;
    let mut t_out = 0i64;
    let mut t_cache = 0i64;
    let mut n = 0i64;
    let mut cny = 0.0f64;
    for r in rows {
        let pid = r.get("provider_id").and_then(|x| x.as_str()).unwrap_or("").to_string();
        if CR_LOCAL.contains(&pid.as_str()) {
            continue;
        }
        let ti = r.get("input_tokens").and_then(|x| x.as_i64()).unwrap_or(0);
        let to = r.get("output_tokens").and_then(|x| x.as_i64()).unwrap_or(0);
        let tc = r.get("cache_read_tokens").and_then(|x| x.as_i64()).unwrap_or(0);
        if ti == 0 && to == 0 {
            continue;
        }
        let mid = r.get("model_id").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let mname = r.get("model_name").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let ts = r.get("created_at").and_then(|x| x.as_i64()).unwrap_or(0);
        let busy = {
            let h = util::strftime_ms(ts, "%H").parse::<i32>().unwrap_or(0);
            (8..22).contains(&h)
        };
        let k: usize = if busy { 1 } else { 0 };
        let pr = cherry_price(&mid, &mname);
        let unc = (ti - tc).max(0);
        // 价表一条 = [in, cache, out]，每档 (闲时, 忙时)
        let pick = |t: (f64, f64)| if k == 0 { t.0 } else { t.1 };
        let mut c = pr.map(|p| {
            (unc as f64 * pick(p[0]) + tc as f64 * pick(p[1]) + to as f64 * pick(p[2])) / 1e6
        });
        if c.is_none() {
            if let Some(cost) = r.get("cost").and_then(|x| x.as_f64()) {
                c = Some(cost * 7.10);
            } else {
                continue;
            }
        }
        let c = c.unwrap_or(0.0);
        cny += c;
        n += 1;
        t_in += ti;
        t_out += to;
        t_cache += tc;
        let key = format!("{}|{}", pid, if mid.is_empty() { mname.clone() } else { mid.clone() });
        match models.iter_mut().find(|m| m.get("k").and_then(|x| x.as_str()) == Some(key.as_str())) {
            Some(m) => {
                let cur = m.get("cny").and_then(|x| x.as_f64()).unwrap_or(0.0);
                m.insert("cny".into(), json!(cur + c));
            }
            None => {
                let mut m = Map::new();
                m.insert("k".into(), json!(key));
                m.insert(
                    "model".into(),
                    json!(if mname.is_empty() { mid.clone() } else { mname.clone() }),
                );
                m.insert("name".into(), json!(mname));
                m.insert("cny".into(), json!(c));
                models.push(m);
            }
        }
    }
    // Python 侧这份数是从 cherry_usage.view(0) 里取的：models 按花费倒序、
    // 名字取「显示名优先」。首尾日期那两栏 Python 永远是空串（见下面）。
    models.sort_by(|a, b| {
        let x = b.get("cny").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let y = a.get("cny").and_then(|v| v.as_f64()).unwrap_or(0.0);
        x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal)
    });
    let names: Vec<String> = models
        .iter()
        .map(|m| {
            let s = s_or(m.get("model").unwrap_or(&Value::Null));
            if s.is_empty() {
                "?".to_string()
            } else {
                s
            }
        })
        .filter(|s| !s.is_empty())
        .take(6)
        .collect();
    // Python 的 `_cherry_stat` 取的是 `by_day[].day`，而 cherry_usage 写进去的键叫 `d`，
    // 所以 last/first 那两栏**永远是空串**。这是现有契约（页面上一直空着），照抄。
    let mut o = Map::new();
    o.insert("name".into(), json!("Cherry Studio"));
    o.insert("ok".into(), json!(true));
    o.insert("sessions".into(), json!(0));
    o.insert("calls".into(), json!(n));
    o.insert("tin".into(), json!(t_in));
    o.insert("tout".into(), json!(t_out));
    o.insert("cached".into(), json!(t_cache));
    o.insert("cny".into(), json!(util::py_round(cny, 2)));
    o.insert("models".into(), json!(names));
    o.insert("last".into(), json!(""));
    o.insert("first".into(), json!(""));
    Value::Object(o)
}

const CR_LOCAL: [&str; 4] = [
    "lmstudio",
    "ollama",
    "cherryai",
    "72a4aecc-4dc3-43f2-82b2-82c7b4b73915",
];

/// 元/百万 token：(闲时, 忙时) 三档 —— in / cache / out
fn cherry_price(mid: &str, mname: &str) -> Option<[(f64, f64); 3]> {
    const T: &[(&str, [(f64, f64); 3])] = &[
        ("deepseek-v4-flash-0731", [(1.5, 3.0), (0.15, 0.3), (4.5, 9.0)]),
        ("deepseek-v4.1-flash", [(1.0, 2.0), (0.02, 0.04), (4.0, 8.0)]),
        ("deepseek-v4.1-flash-1017", [(1.0, 2.0), (0.02, 0.04), (4.0, 8.0)]),
        ("deepseek-v4-pro-0813", [(4.5, 9.0), (0.45, 0.9), (13.5, 27.0)]),
        ("deepseek-v4-pro", [(12.0, 12.0), (1.2, 1.2), (24.0, 24.0)]),
        ("qwen3.5-flash", [(0.2, 0.2), (0.02, 0.02), (2.0, 2.0)]),
        ("qwen3.5-plus", [(0.8, 0.8), (0.08, 0.08), (8.0, 8.0)]),
        ("qwen3.7-flash", [(0.2, 0.2), (0.04, 0.04), (0.8, 0.8)]),
        ("qwen3.8-flash", [(0.8, 0.8), (0.1, 0.1), (2.7, 2.7)]),
        ("deepseek-ai/deepseek-v3.2", [(4.0, 4.0), (0.4, 0.4), (6.0, 6.0)]),
        ("deepseek-ai/deepseek-v4-flash", [(1.5, 3.0), (0.15, 0.3), (4.5, 9.0)]),
        ("deepseek-ai/deepseek-v4.1-flash", [(1.0, 2.0), (0.02, 0.04), (4.0, 8.0)]),
    ];
    let low = if mid.is_empty() { mname } else { mid }.to_lowercase();
    for (k, p) in T {
        if *k == mid || *k == mname || k.to_lowercase() == low {
            return Some(*p);
        }
    }
    for (k, p) in T {
        if !k.is_empty() && low.contains(&k.to_lowercase()) {
            return Some(*p);
        }
    }
    None
}

fn cr_sqlite(db: &Path, sql: &str) -> Option<Vec<Value>> {
    let out = std::process::Command::new("sqlite3")
        .arg("-readonly")
        .arg("-json")
        .arg(db)
        .arg(sql)
        .output()
        .ok()?;
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

/// `_qwenwork_stat`：会话 jsonl 结构同 Claude Code，但不写 usage。
fn qwenwork_stat() -> Option<Value> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for d in [".qwenworkcn", ".qwenwork"] {
        let p = util::home().join(d).join("projects");
        if p.is_dir() {
            roots.push(p);
        }
    }
    if roots.is_empty() {
        return Some(json!({"name": "千问办公 QwenWork", "ok": false,
                           "err": "本机没找到 ~/.qwenworkcn/projects"}));
    }
    let mut sess = 0i64;
    let mut turns = 0i64;
    let mut tools = 0i64;
    let mut models: Vec<String> = Vec::new();
    let mut stamps: Vec<f64> = Vec::new();
    for root in &roots {
        for p in read_dir_unsorted(root) {
            if !p.is_dir() {
                continue;
            }
            for f in read_dir_unsorted(&p) {
                if !f.extension().map(|e| e == "jsonl").unwrap_or(false) {
                    continue;
                }
                sess += 1;
                let text = match std::fs::read(&f) {
                    Ok(b) => String::from_utf8_lossy(&b).to_string(),
                    Err(_) => continue,
                };
                for ln in text.lines() {
                    let s = ln.trim();
                    if s.is_empty() {
                        continue;
                    }
                    let Ok(o) = serde_json::from_str::<Value>(s) else { continue };
                    let t = o.get("type").and_then(|x| x.as_str()).unwrap_or("");
                    let m = o.get("message").cloned().unwrap_or(json!({}));
                    if t == "user" {
                        let c = m.get("content").cloned().unwrap_or(Value::Null);
                        if let Some(a) = c.as_array() {
                            if !a.is_empty()
                                && a.iter().all(|x| {
                                    x.get("type").and_then(|y| y.as_str()) == Some("tool_result")
                                })
                            {
                                continue;
                            }
                        }
                        turns += 1;
                    } else if t == "assistant" {
                        if let Some(a) = m.get("content").and_then(|x| x.as_array()) {
                            tools += a
                                .iter()
                                .filter(|x| {
                                    x.get("type").and_then(|y| y.as_str()) == Some("tool_use")
                                })
                                .count() as i64;
                        }
                        let mm = s_or(m.get("model").unwrap_or(&Value::Null)).trim().to_string();
                        if !mm.is_empty() && mm != "<synthetic>" && !models.contains(&mm) {
                            models.push(mm);
                        }
                    }
                    if let Some(ts) = o.get("timestamp") {
                        let ms = iso_ms(ts);
                        if ms != 0.0 {
                            stamps.push(ms);
                        }
                    }
                }
            }
        }
    }
    if sess == 0 {
        return Some(json!({"name": "千问办公 QwenWork", "ok": false,
                           "err": "目录在，但一个会话文件都没有"}));
    }
    stamps.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mut o = Map::new();
    o.insert("name".into(), json!("千问办公 QwenWork"));
    o.insert("ok".into(), json!(true));
    o.insert("sessions".into(), json!(sess));
    o.insert("calls".into(), json!(tools));
    o.insert("asks".into(), json!(turns));
    o.insert("tin".into(), json!(0));
    o.insert("tout".into(), json!(0));
    o.insert("cached".into(), json!(0));
    o.insert("notok".into(), json!(true));
    o.insert(
        "models".into(),
        json!(models.iter().take(6).cloned().collect::<Vec<_>>()),
    );
    let first = stamps.first().copied().unwrap_or(0.0);
    let last = stamps.last().copied().unwrap_or(0.0);
    o.insert("first".into(), json!(day_of(first / 1000.0)));
    o.insert("last".into(), json!(day_of(last / 1000.0)));
    Some(Value::Object(o))
}

fn iso_ms(v: &Value) -> f64 {
    match v {
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        _ => {
            let s = s_or(v);
            if s.trim().is_empty() {
                return 0.0;
            }
            match chrono::DateTime::parse_from_rfc3339(s.trim().replace('Z', "+00:00").as_str()) {
                Ok(d) => d.timestamp_millis() as f64,
                Err(_) => 0.0,
            }
        }
    }
}

fn read_dir_unsorted(p: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(p) else { return Vec::new() };
    rd.filter_map(|e| e.ok().map(|x| x.path())).collect()
}

/// `agent_import.scan()`
/// `_hw_stats(hw)`：采集层认识的所有客户端（WorkBuddy / TRAE / Claude Code …），全部历史。
/// 口袋自己的账不算「别的 agent」，跳过；空 id 也跳过。
pub fn hw_stats(ctx: &Ctx) -> Vec<Value> {
    let rows = crate::hostwork::clients_view(ctx, 0);
    let mut out: Vec<Value> = Vec::new();
    for c in &rows {
        let cid = crate::hostwork::util::str_or(c.get("id"));
        if cid == "pocket" || cid.is_empty() {
            continue;
        }
        let t = c.get("tokens").cloned().unwrap_or(json!({}));
        let models: Vec<String> = c
            .get("kids")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter(|k| crate::hostwork::util::str_or(k.get("kind")) == "model")
                    .map(|k| crate::hostwork::util::str_or(k.get("name")))
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let name = {
            let n = crate::hostwork::util::str_or(c.get("name"));
            if n.is_empty() { cid.clone() } else { n }
        };
        let mut m = Map::new();
        m.insert("name".into(), json!(name));
        m.insert("id".into(), json!(cid));
        m.insert("ok".into(), json!(true));
        m.insert("sessions".into(), json!(util::int0(c.get("sessions").unwrap_or(&Value::Null))));
        m.insert("calls".into(), json!(util::int0(c.get("calls").unwrap_or(&Value::Null))));
        m.insert("asks".into(), json!(util::int0(c.get("asks").unwrap_or(&Value::Null))));
        m.insert("tin".into(), json!(util::int0(t.get("in").unwrap_or(&Value::Null))));
        m.insert("tout".into(), json!(util::int0(t.get("out").unwrap_or(&Value::Null))));
        m.insert("cached".into(), json!(util::int0(t.get("cached").unwrap_or(&Value::Null))));
        m.insert("models".into(), json!(models.into_iter().take(6).collect::<Vec<_>>()));
        m.insert("last".into(), json!(crate::hostwork::util::str_or(c.get("last"))));
        out.push(Value::Object(m));
    }
    out
}

pub fn scan(ctx: &Ctx) -> Value {
    // 顺序照 Python：采集层各客户端 → Cherry → QwenWork
    let mut out: Vec<Value> = hw_stats(ctx);
    let ch = cherry_stat();
    out.push(ch);
    if let Some(qw) = qwenwork_stat() {
        out.push(qw);
    }
    let ok_rows: Vec<&Value> = out.iter().filter(|x| x.get("ok").map(truthy).unwrap_or(false)).collect();
    let tot_calls: i64 = ok_rows
        .iter()
        .map(|x| x.get("calls").and_then(|v| v.as_i64()).unwrap_or(0))
        .sum();
    let tot_tok: i64 = ok_rows
        .iter()
        .map(|x| {
            x.get("tin").and_then(|v| v.as_i64()).unwrap_or(0)
                + x.get("tout").and_then(|v| v.as_i64()).unwrap_or(0)
        })
        .sum();
    let mut totals = Map::new();
    totals.insert("calls".into(), json!(tot_calls));
    totals.insert("tokens".into(), json!(tot_tok));
    let mut o = Map::new();
    o.insert("ok".into(), json!(true));
    o.insert("sources".into(), json!(out));
    o.insert("totals".into(), Value::Object(totals));
    o.insert("note_path".into(), json!(note_path(ctx).display().to_string()));
    Value::Object(o)
}

/// `_summary_of(x)`
fn summary_of(x: &Value) -> String {
    let mut bits: Vec<String> = Vec::new();
    let g = |k: &str| x.get(k).cloned().unwrap_or(Value::Null);
    let sessions = util::int0(&g("sessions"));
    if sessions != 0 {
        bits.push(format!("{} 个会话", sessions));
    }
    let calls = util::int0(&g("calls"));
    if calls != 0 {
        bits.push(format!("{} 次调用", calls));
    }
    let tok = util::int0(&g("tin")) + util::int0(&g("tout"));
    if tok != 0 {
        bits.push(format!("{} token", fmt_tokens(tok)));
    }
    let models: Vec<String> = g("models")
        .as_array()
        .map(|a| a.iter().map(pystr).collect())
        .unwrap_or_default();
    if !models.is_empty() {
        bits.push(format!("模型：{}", models.iter().take(3).cloned().collect::<Vec<_>>().join("、")));
    }
    let cny = util::fnum(&g("cny")).unwrap_or(0.0);
    if cny != 0.0 {
        bits.push(format!("实付 ¥{}", util::f2(cny)));
    }
    let span = [s_or(&g("first")), s_or(&g("last"))]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ~ ");
    if !span.is_empty() {
        bits.push(span);
    }
    if bits.is_empty() {
        "没有可导入的账".to_string()
    } else {
        bits.join(" · ")
    }
}

/// `_strip_imported`：去掉上次导入写的那几条（source == "导入"）。
fn strip_imported(lines: &[String]) -> Vec<String> {
    let mut keep: Vec<String> = Vec::new();
    for ln in lines {
        let s = ln.trim();
        if s.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(s) {
            Ok(Value::Object(m)) => {
                let src = m.get("source").map(pystr).unwrap_or_default();
                if src == SOURCE_TAG {
                    continue;
                }
                keep.push(s.to_string());
            }
            Ok(_) => keep.push(s.to_string()),
            Err(_) => keep.push(s.to_string()),
        }
    }
    keep
}

/// `_register(dirs)`：手选目录登记进 `_工作台/Agent数据源.json`。
fn register(ctx: &Ctx, dirs: &[String]) -> Vec<String> {
    let dirs: Vec<String> = dirs
        .iter()
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty())
        .collect();
    if dirs.is_empty() {
        return Vec::new();
    }
    let p = reg_path(ctx);
    let old = util::load_json(&p).unwrap_or_else(|| json!({}));
    let mut items: Vec<Value> = old
        .get("dirs")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    let mut have: Vec<String> = items
        .iter()
        .filter_map(|d| d.get("path").map(pystr))
        .collect();
    let mut added: Vec<String> = Vec::new();
    for d in dirs {
        if have.iter().any(|h| *h == d) {
            continue;
        }
        items.push(json!({"path": d, "at": util::strftime_now("%Y-%m-%d %H:%M")}));
        have.push(d.clone());
        added.push(d);
    }
    let body = json!({"dirs": items});
    let _ = util::ensure_parent(&p);
    let tmp = with_suffix(&p, ".json.tmp");
    if super::write_json_indent(&tmp, &body, 1).is_err() || std::fs::rename(&tmp, &p).is_err() {
        return Vec::new();
    }
    added
}

fn with_suffix(p: &Path, suffix: &str) -> PathBuf {
    // 对应 Python 的 `p.with_suffix(".json.tmp")`：把最后一个后缀整体换掉
    let name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let stem = match name.rfind('.') {
        Some(i) => name[..i].to_string(),
        None => name.clone(),
    };
    p.with_file_name(format!("{}{}", stem, suffix))
}

/// `agent_import.apply(names, paths_list)`
pub fn apply(ctx: &Ctx, names: Option<Vec<String>>, dirs: Vec<String>) -> Value {
    let data = scan(ctx);
    let sources = data.get("sources").cloned().unwrap_or(json!([]));
    let picked: Vec<Value> = sources
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|x| {
            if !x.get("ok").map(truthy).unwrap_or(false) {
                return false;
            }
            match &names {
                None => true,
                Some(ns) => {
                    let name = x.get("name").map(pystr).unwrap_or_default();
                    let id = x.get("id").map(pystr).unwrap_or_default();
                    ns.iter().any(|n| *n == name || *n == id)
                }
            }
        })
        .collect();
    let registered = register(ctx, &dirs);
    if picked.is_empty() {
        if !registered.is_empty() {
            return json!({
                "ok": true, "imported": [], "lines": 0,
                "path": reg_path(ctx).display().to_string(),
                "registered": registered,
                "summary": [],
                "note": "这些目录只登记了路径，账还要等解析器",
            });
        }
        let mut o = Map::new();
        o.insert("ok".into(), json!(false));
        o.insert(
            "error".into(),
            json!("没有可导入的来源（先看看本机有没有这些客户端的数据库）"),
        );
        o.insert("sources".into(), sources);
        return Value::Object(o);
    }

    let p = note_path(ctx);
    let _ = util::ensure_parent(&p);
    let old = util::read_lines(&p);
    let keep = strip_imported(&old);
    let now = util::now_secs() as f64;
    let mut added: Vec<String> = Vec::new();
    let mut summary: Vec<Value> = Vec::new();
    let mut imported: Vec<String> = Vec::new();
    for x in &picked {
        let name = s_or(x.get("name").unwrap_or(&Value::Null));
        let models: Vec<String> = x
            .get("models")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().map(pystr).collect())
            .unwrap_or_default();
        let mut rec = Map::new();
        rec.insert("at".into(), json!(util::py_round(now, 1)));
        rec.insert("agent".into(), json!(name));
        rec.insert("task".into(), json!("历史数据导入"));
        rec.insert("summary".into(), json!(summary_of(x)));
        rec.insert("project".into(), json!(""));
        rec.insert("model".into(), json!(cut(&models.join("、"), 80)));
        rec.insert("tin".into(), json!(util::int0(x.get("tin").unwrap_or(&Value::Null))));
        rec.insert("tout".into(), json!(util::int0(x.get("tout").unwrap_or(&Value::Null))));
        rec.insert("cached".into(), json!(util::int0(x.get("cached").unwrap_or(&Value::Null))));
        rec.insert("calls".into(), json!(util::int0(x.get("calls").unwrap_or(&Value::Null))));
        rec.insert("source".into(), json!(SOURCE_TAG));
        added.push(util::dumps_line(&Value::Object(rec)));
        summary.push(json!({"name": name, "text": summary_of(x)}));
        imported.push(name);
    }
    let mut body = String::new();
    for ln in keep.iter().chain(added.iter()) {
        body.push_str(ln);
        body.push('\n');
    }
    let tmp = with_suffix(&p, ".jsonl.tmp");
    if std::fs::write(&tmp, body.as_bytes()).is_err() || std::fs::rename(&tmp, &p).is_err() {
        return json!({"ok": false, "error": "写不进去（Agent自述.jsonl）"});
    }
    let mut o = Map::new();
    o.insert("ok".into(), json!(true));
    o.insert("imported".into(), json!(imported));
    o.insert("lines".into(), json!(added.len()));
    o.insert("path".into(), json!(p.display().to_string()));
    o.insert("registered".into(), json!(registered));
    o.insert("summary".into(), json!(summary));
    Value::Object(o)
}

// ---------------------------------------------------------------- 路由

async fn detect_route(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let out = tokio::task::spawn_blocking(detect)
        .await
        .unwrap_or_else(|_| json!({"ok": false, "error": "RuntimeError: 扫描失败", "agents": []}));
    Json(out).into_response()
}

async fn scan_route(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let ctx2 = ctx.clone();
    let out = tokio::task::spawn_blocking(move || scan(&ctx2))
        .await
        .unwrap_or_else(|_| json!({"ok": false, "error": "RuntimeError: 扫描失败", "sources": []}));
    Json(out).into_response()
}

async fn apply_route(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let b = match super::parse_json_opt(&body) {
        Some(v) => v,
        None => json!({}),
    };
    let names: Option<Vec<String>> = b.get("sources").and_then(|x| x.as_array()).map(|a| {
        a.iter()
            .map(pystr)
            .filter(|s| !s.trim().is_empty())
            .collect()
    });
    let dirs: Vec<String> = b
        .get("paths")
        .and_then(|x| x.as_array())
        .map(|a| a.iter().map(pystr).filter(|s| !s.trim().is_empty()).collect())
        .unwrap_or_default();
    let ctx2 = ctx.clone();
    let out = tokio::task::spawn_blocking(move || apply(&ctx2, names, dirs))
        .await
        .unwrap_or_else(|_| json!({"ok": false, "error": "RuntimeError: 导入失败"}));
    Json(out).into_response()
}

/// `await request.json()` 的宽松版：空体/坏 JSON 都当 `{}`。
pub fn body_or_empty(raw: &Bytes) -> Value {
    if raw.is_empty() {
        return json!({});
    }
    serde_json::from_slice::<Value>(raw).unwrap_or_else(|_| json!({}))
}
