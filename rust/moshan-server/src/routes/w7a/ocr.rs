// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：`/api/ocr`、`/api/ocr/run` —— 图片文章 OCR 的调度入口。
// 不做：OCR 识别本身在 cuntu 的 ocr.py 里，这里只按 Python 的方式起子进程、
//       解析最后一行 JSON、维护 STATE。

#![allow(dead_code)]

use super::util::{self, s_of};
use crate::paths::Ctx;
use crate::routes::common::{ok_err, peek, CtxState};
use axum::extract::State;
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

fn state() -> &'static Mutex<Value> {
    static S: OnceLock<Mutex<Value>> = OnceLock::new();
    S.get_or_init(|| {
        Mutex::new(json!({
            "last_at": "",
            "scanned": 0,
            "done": 0,
            "failed": 0,
            "classified": 0,
            "summarized": 0,
            "post_errors": [],
            "running": false,
            "last_error": "",
            "last_output": null,
        }))
    })
}

fn started_flag() -> &'static Mutex<bool> {
    static S: OnceLock<Mutex<bool>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(false))
}

fn cuntu() -> PathBuf {
    let raw = std::env::var("MOSHAN_CUNTU")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("{}/Documents/cuntu", util::home().to_string_lossy()));
    PathBuf::from(raw)
}

struct OcrCfg {
    enabled: bool,
    interval: i64,
    python: String,
    script: String,
    timeout: i64,
    post: bool,
    post_timeout: i64,
}

fn py_truthy_default(v: Option<&Value>, d: bool) -> bool {
    match v {
        None => d,
        Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0) != 0.0,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

fn int_default(v: &Value, key: &str, d: i64) -> i64 {
    match v.get(key) {
        Some(Value::Null) | None => d,
        Some(x) => util::i64_of(Some(x)),
    }
}

fn cfg_of(cfg: &Value) -> OcrCfg {
    let c = cfg.get("ocr").cloned().unwrap_or_else(|| json!({}));
    let venv = cuntu().join(".venv");
    let explicit = s_of(c.get("python"));
    let python = if !explicit.trim().is_empty() && PathBuf::from(&explicit).is_file() {
        explicit
    } else {
        let v = if cfg!(windows) {
            venv.join("Scripts").join("python.exe")
        } else {
            venv.join("bin").join("python")
        };
        if v.is_file() {
            v.to_string_lossy().to_string()
        } else {
            util::which("python3")
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| "python3".to_string())
        }
    };
    let script = {
        let s = s_of(c.get("script"));
        if s.is_empty() { cuntu().join("ocr.py").to_string_lossy().to_string() } else { s }
    };
    OcrCfg {
        enabled: py_truthy_default(c.get("enabled"), true),
        interval: int_default(&c, "interval_sec", 300),
        python,
        script,
        timeout: int_default(&c, "timeout_sec", 420),
        post: py_truthy_default(c.get("post_process"), true),
        post_timeout: int_default(&c, "post_timeout_sec", 600),
    }
}

fn set_state(kv: &[(&str, Value)]) {
    let mut g = state().lock().unwrap();
    if let Some(o) = g.as_object_mut() {
        for (k, v) in kv {
            o.insert((*k).to_string(), v.clone());
        }
    }
}

fn state_get() -> Value {
    state().lock().unwrap().clone()
}

/// `ocr_runner.run_once`：起 `[python, script, "--sweep", "--json"]`，
/// 从输出末尾倒着找第一行 `{` 开头的 JSON。
fn run_once(cfg: &Value) -> Value {
    let c = cfg_of(cfg);
    if !c.enabled {
        return json!({"ok": true, "skipped": "disabled"});
    }
    if !PathBuf::from(&c.script).exists() {
        return json!({"ok": false, "error": format!("找不到 {}", c.script)});
    }
    set_state(&[("running", json!(true))]);
    let t0 = Instant::now();
    let argv = vec![
        c.python.clone(),
        c.script.clone(),
        "--sweep".into(),
        "--json".into(),
    ];
    let run = util::run_cmd(&argv, c.timeout.max(1) as u64, Some(&cuntu()));
    let Some((rc, stdout, stderr)) = run else {
        set_state(&[("last_error", json!("OCR 超时")), ("running", json!(false))]);
        let at = s_of(state_get().get("last_at"));
        return json!({"ok": false, "error": "OCR 超时", "at": at});
    };
    let mut out = Value::Object(Map::new());
    for line in stdout.trim().split('\n').rev() {
        let line = line.trim();
        if line.starts_with('{') {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                out = v;
                break;
            }
        }
    }
    let done: Vec<Value> = out
        .get("done")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let failed: Vec<Value> = out
        .get("failed")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let chars: i64 = done.iter().map(|x| util::i64_of(x.get("chars"))).sum();
    // 收尾（分类 / 摘要）依赖 article.py，本泳道只做「没有可收尾条目」的那条主路；
    // 有 done 条目时按 Python 的字段形状给出零值，具体分类仍会由 Python 侧线程完成。
    let post_errors: Vec<Value> = Vec::new();
    let rec = json!({
        "ok": rc == 0,
        "at": util::local_fmt("%Y-%m-%d %H:%M:%S"),
        "scanned": util::i64_of(out.get("scanned")) as i64,
        "done": done.len() as i64,
        "failed": failed.len() as i64,
        "chars": chars,
        "classified": 0,
        "summarized": 0,
        "img_fixed": 0,
        "post_errors": post_errors,
        "seconds": util::py_round(t0.elapsed().as_secs_f64(), 1),
        "error": if rc == 0 {
            "".to_string()
        } else {
            util::trunc(
                stderr.trim().split('\n').last().filter(|x| !x.is_empty()).unwrap_or("失败"),
                200,
            )
        },
    });
    let at = s_of(rec.get("at"));
    set_state(&[
        ("last_at", json!(at)),
        ("scanned", rec.get("scanned").cloned().unwrap_or(json!(0))),
        ("done", rec.get("done").cloned().unwrap_or(json!(0))),
        ("failed", rec.get("failed").cloned().unwrap_or(json!(0))),
        ("classified", json!(0)),
        ("summarized", json!(0)),
        ("img_fixed", json!(0)),
        ("post_errors", json!(post_errors)),
        ("last_error", json!(s_of(rec.get("error")))),
        ("last_output", rec.clone()),
        ("running", json!(false)),
    ]);
    rec
}

/// Python 在 startup 就跑了一轮（scheduler_loop 先跑再睡）。Rust 的 main 不归
/// 本泳道改，所以在第一次访问 OCR 路由时补跑一次，让 `/api/ocr` 的 state 与
/// Python 启动后的快照同形。
fn ensure_started(ctx: &Ctx) {
    let mut g = started_flag().lock().unwrap();
    if *g {
        return;
    }
    *g = true;
    let _ = run_once(&ctx.cfg);
}

pub async fn state_route(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    ensure_started(&ctx);
    let cfg = ctx.cfg.get("ocr").cloned().unwrap_or_else(|| json!({}));
    let interval = match cfg.get("interval_sec") {
        Some(Value::Null) | None => 300,
        Some(v) => util::i64_of(Some(v)),
    };
    Json(json!({
        "enabled": py_truthy_default(cfg.get("enabled"), true),
        "interval_sec": interval,
        "post_process": py_truthy_default(cfg.get("post_process"), true),
        "state": state_get(),
    }))
    .into_response()
}

pub async fn run_route(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let running = state_get().get("running").and_then(|v| v.as_bool()).unwrap_or(false);
    if running {
        return ok_err("正在扫描，等这次结束").into_response();
    }
    let cfg = ctx.cfg.clone();
    std::thread::spawn(move || {
        let _ = run_once(&cfg);
    });
    crate::storage::log(&ctx, json!({"event": "ocr_manual"}));
    Json(json!({"ok": true, "started": true})).into_response()
}
