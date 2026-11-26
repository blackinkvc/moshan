// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：`moshan/dedupe_runner.py` 的 3 条路由（状态 / 手动跑 / 还原）。
// 不做：判定与删移文件本身 —— Python 那边全在 `cuntu/dedupe.py` 里，
//       dedupe_runner 只负责「什么时候跑、跑完怎么给页面看」。这里照抄
//       子进程的命令行、超时、输出解析与 STATE 的变化顺序。

#![allow(dead_code)]

use super::py;
use crate::jsonio;
use crate::paths::Ctx;
use crate::routes::common::{ok_err, peek, CtxState};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

struct State0 {
    running: bool,
    started: String,
    error: String,
    last: Option<Value>,
}

impl State0 {
    fn to_value(&self) -> Value {
        json!({
            "running": self.running,
            "started": self.started,
            "error": self.error,
            "last": self.last.clone().unwrap_or(Value::Null),
        })
    }
}

fn state() -> &'static Mutex<State0> {
    static S: OnceLock<Mutex<State0>> = OnceLock::new();
    S.get_or_init(|| {
        Mutex::new(State0 {
            running: false,
            started: String::new(),
            error: String::new(),
            last: None,
        })
    })
}

fn lock_running() -> &'static Mutex<bool> {
    static L: OnceLock<Mutex<bool>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(false))
}

struct Cfg {
    python: PathBuf,
    py_found: bool,
    py_tried: Vec<String>,
    script: PathBuf,
    root: PathBuf,
    hour: i64,
    apply: bool,
    limit: i64,
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().to_string()
}

/// `Path(x)` 的字符串形态：去掉尾部多余的 `/`（根目录除外）。
fn norm_path_string(s: &str) -> String {
    if s == "/" {
        return s.to_string();
    }
    let t = s.trim_end_matches('/');
    t.to_string()
}

fn cfg_of(ctx: &Ctx) -> Cfg {
    let d = ctx.cfg.get("dedupe").cloned().unwrap_or(json!({}));
    let script = match py::s_of(d.get("script")) {
        s if !s.trim().is_empty() => PathBuf::from(s),
        _ => py::home().join("Documents/cuntu/dedupe.py"),
    };
    let venv = py::home().join("Documents/cuntu/.venv");
    let (found, tried) = py::resolve_python(
        d.get("python").and_then(|v| v.as_str()),
        &venv,
    );
    let python = found.clone().unwrap_or_else(|| {
        if cfg!(windows) {
            venv.join("Scripts").join("python.exe")
        } else {
            venv.join("bin").join("python")
        }
    });
    let root = match py::s_of(d.get("root")) {
        s if !s.trim().is_empty() => PathBuf::from(s),
        _ => py::home().join("Documents").join("AI项目").join("04-存图"),
    };
    let hour = match d.get("hour") {
        None => 3,
        Some(v) => py::i64_of(Some(v)),
    };
    let apply = match d.get("apply") {
        None => true,
        Some(v) => truthy(v),
    };
    let limit = match d.get("limit") {
        None => 100,
        Some(v) => py::i64_of(Some(v)),
    };
    Cfg { python, py_found: found.is_some(), py_tried: tried, script, root, hour, apply, limit }
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn report_dir(c: &Cfg) -> PathBuf {
    c.root.join("_去重报告")
}

fn latest_of(c: &Cfg) -> Value {
    let p = report_dir(c).join("latest.json");
    if !p.exists() {
        return json!({});
    }
    jsonio::read_json(&p).unwrap_or(json!({}))
}

fn history_of(c: &Cfg, limit: usize) -> Vec<Value> {
    let idx = report_dir(c).join("index.jsonl");
    if !idx.exists() {
        return Vec::new();
    }
    let Ok(text) = std::fs::read_to_string(&idx) else { return Vec::new() };
    let mut rows: Vec<Value> = Vec::new();
    for line in text.split('\n') {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            rows.push(v);
        }
    }
    rows.reverse();
    rows.truncate(limit);
    rows
}

fn batches_of(c: &Cfg) -> Vec<Value> {
    let trash = c.root.join("_回收站");
    if !trash.is_dir() {
        return Vec::new();
    }
    let Ok(rd) = std::fs::read_dir(&trash) else { return Vec::new() };
    let mut dirs: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    dirs.sort_by_key(|p| std::cmp::Reverse(p.to_string_lossy().to_lowercase()));
    let mut out = Vec::new();
    for d in dirs {
        let name = d.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        if !d.is_dir() || !name.starts_with("去重_") {
            continue;
        }
        let man = d.join("_manifest.json");
        let items: Vec<Value> = if man.exists() {
            match jsonio::read_json(&man) {
                Some(Value::Array(a)) => a,
                _ => Vec::new(),
            }
        } else {
            Vec::new()
        };
        let bytes: i64 = items
            .iter()
            .map(|i| i.get("bytes").map(|v| py::i64_of(Some(v))).unwrap_or(0))
            .sum();
        out.push(json!({"name": name, "dirs": items.len(), "bytes": bytes}));
        if out.len() >= 30 {
            break;
        }
    }
    out
}

fn next_run_at(c: &Cfg) -> String {
    let now = chrono::Local::now();
    let hour = c.hour.clamp(0, 23) as u32;
    let today = now
        .date_naive()
        .and_hms_opt(hour, 0, 0)
        .unwrap_or_else(|| now.naive_local());
    let nxt = if today <= now.naive_local() { today + chrono::Duration::days(1) } else { today };
    nxt.format("%Y-%m-%d %H:%M").to_string()
}

/// `/api/dedupe`：状态 + 最近一次 + 历史 + 可还原批次。
pub async fn state_route(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let c = cfg_of(&ctx);
    let root_str = norm_path_string(&path_str(&c.root));
    let st = state().lock().unwrap().to_value();
    Json(json!({
        "hour": c.hour,
        "apply": c.apply,
        "root": root_str,
        "next_at": next_run_at(&c),
        "state": st,
        "latest": latest_of(&c),
        "history": history_of(&c, 10),
        "batches": batches_of(&c),
    }))
    .into_response()
}

/// 真正跑一次扫描。返回 dedupe.py --json 的结果（并更新 STATE）。
fn run_once(ctx: &Ctx, apply: Option<bool>, limit: Option<i64>, timeout: u64) -> Value {
    let c = cfg_of(ctx);
    if !c.py_found {
        return json!({"ok": false, "error": py::not_found_hint(&c.py_tried)});
    }
    if !c.script.exists() {
        return json!({"ok": false, "error": format!("找不到脚本：{}", c.script.to_string_lossy())});
    }
    {
        let mut l = lock_running().lock().unwrap();
        if *l {
            return json!({"ok": false, "error": "正在扫，等这次结束"});
        }
        *l = true;
    }
    {
        let mut st = state().lock().unwrap();
        st.running = true;
        st.started = py::local_str("%Y-%m-%d %H:%M:%S");
        st.error = String::new();
    }
    let do_apply = apply.unwrap_or(c.apply);
    let mut argv = vec![
        c.python.to_string_lossy().to_string(),
        c.script.to_string_lossy().to_string(),
        "--root".to_string(),
        path_str(&c.root),
        "--json".to_string(),
    ];
    if do_apply {
        argv.push("--apply".to_string());
        argv.push("--limit".to_string());
        argv.push(limit.unwrap_or(c.limit).to_string());
    }
    let t0 = std::time::Instant::now();
    let res = py::run_cmd(&argv, timeout, None);
    let out = match res {
        None => {
            state().lock().unwrap().error = "超时".to_string();
            let mut l = lock_running().lock().unwrap();
            *l = false;
            state().lock().unwrap().running = false;
            return json!({"ok": false, "error": format!("跑了 {timeout}s 还没结束，已中止")});
        }
        Some((_code, stdout, stderr)) => {
            let mut line = String::new();
            for ln in stdout.trim().split('\n').rev() {
                if ln.starts_with('{') {
                    line = ln.to_string();
                    break;
                }
            }
            if line.is_empty() {
                let e = if stderr.is_empty() { "没有输出".to_string() } else { stderr };
                let e = py::trunc(&e, 300);
                state().lock().unwrap().error = e.clone();
                let mut l = lock_running().lock().unwrap();
                *l = false;
                state().lock().unwrap().running = false;
                return json!({"ok": false, "error": e});
            }
            match serde_json::from_str::<Value>(&line) {
                Ok(Value::Object(o)) => Value::Object(o),
                _ => {
                    let mut l = lock_running().lock().unwrap();
                    *l = false;
                    state().lock().unwrap().running = false;
                    return json!({"ok": false, "error": "输出不是合法 JSON"});
                }
            }
        }
    };
    let mut out = out;
    out["ok"] = json!(true);
    out["seconds"] = json!(py::py_round(t0.elapsed().as_secs_f64(), 1));
    {
        let mut st = state().lock().unwrap();
        st.last = Some(out.clone());
        st.running = false;
    }
    let mut l = lock_running().lock().unwrap();
    *l = false;
    out
}

/// `POST /api/dedupe/run?apply=`
pub async fn run(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let pairs = crate::routes::common::query_map(&uri);
    let apply_v = match pairs.into_iter().find(|(k, _)| k == "apply") {
        None => true,
        Some((_, raw)) => match parse_bool(&raw) {
            Some(b) => b,
            None => return bool_422(&raw),
        },
    };
    if state().lock().unwrap().running {
        return ok_err("正在扫，等这次结束").into_response();
    }
    let ctx2 = ctx.clone();
    std::thread::spawn(move || {
        let _ = run_once(&ctx2, Some(apply_v), None, 1800);
    });
    crate::storage::log(&ctx, json!({"event": "dedupe_manual", "apply": apply_v}));
    Json(json!({"ok": true, "started": true})).into_response()
}

/// FastAPI 的 bool 查询参数口径。
fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_lowercase().as_str() {
        "1" | "true" | "t" | "yes" | "y" | "on" => Some(true),
        "0" | "false" | "f" | "no" | "n" | "off" => Some(false),
        _ => None,
    }
}

fn bool_422(raw: &str) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({"detail": [{
            "type": "bool_parsing",
            "loc": ["query", "apply"],
            "msg": "Input should be a valid boolean, unable to interpret input",
            "input": raw,
            "url": "https://errors.pydantic.dev/2.14/v/bool_parsing",
        }]})),
    )
        .into_response()
}

fn missing_query(field: &str) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({"detail": [{
            "type": "missing",
            "loc": ["query", field],
            "msg": "Field required",
            "input": null,
            "url": "https://errors.pydantic.dev/2.14/v/missing",
        }]})),
    )
        .into_response()
}

fn internal_error() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        [(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "Internal Server Error",
    )
        .into_response()
}

/// `POST /api/dedupe/restore?batch=`
pub async fn restore(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let map = crate::routes::common::query_map(&uri);
    let Some((_, raw_batch)) = map.into_iter().find(|(k, _)| k == "batch") else {
        return missing_query("batch");
    };
    let batch = raw_batch.trim().trim_matches('/').to_string();
    if !batch.starts_with("去重_") {
        return Json(json!({"ok": false, "error": "批次名要以 去重_ 开头"})).into_response();
    }
    let c = cfg_of(&ctx);
    let argv = vec![
        c.python.to_string_lossy().to_string(),
        c.script.to_string_lossy().to_string(),
        "--root".to_string(),
        path_str(&c.root),
        "--restore".to_string(),
        batch.clone(),
    ];
    let ctx2 = ctx.clone();
    let batch2 = batch.clone();
    let res = tokio::task::spawn_blocking(move || py::run_cmd(&argv, 300, None)).await;
    let Some(res) = res.ok() else { return internal_error() };
    let Some((code, stdout, stderr)) = res else {
        // Python 那边是 FileNotFoundError 之类的 OSError：未捕获 → 500
        return internal_error();
    };
    let out = {
        let s = if !stdout.is_empty() { stdout } else if !stderr.is_empty() { stderr } else { String::new() };
        py::trunc(s.trim(), 500)
    };
    let ok = code == 0;
    crate::storage::log(
        &ctx2,
        json!({"event": "dedupe_restore", "batch": batch2, "ok": ok}),
    );
    Json(json!({"ok": ok, "output": out})).into_response()
}
