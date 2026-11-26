// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：进程内运行态（后台调度器的进度快照）。
// 放 crate 顶层是因为写的一方是调度线程、读的一方是 HTTP 处理器，跨泳道：
// `/api/dash` 读 OCR 进度与心跳、`/api/report/list` 读采样进度、`/api/heartbeat`
// 读上次心跳、`/api/trends` 读趋势任务。
// 不做：不做业务判断，只存"上次什么时候跑、跑了多久、结果如何"。
//
// 字段名与序列化形状逐字对齐 Python：
//   report    ← report_runner.STATE
//   ocr       ← ocr_runner.STATE
//   heartbeat ← heartbeat.STATE
//   trends    ← ai_trends_runner.STATE
// 这些值会原样出现在 HTTP 响应里，键顺序也必须一致（Python dict 是有序的）。

#![allow(dead_code)]

use serde_json::{json, Value};
use std::sync::{Mutex, MutexGuard, OnceLock};

/// 一轮跑批的通用快照形状（对应 Python 里各 runner 的 STATE 字典）。
#[derive(Default, Clone)]
pub struct Job {
    pub started: f64,
    pub samples: i64,
    pub last_sample: String,
    pub last_run: f64,
    pub last_made: f64,
    pub running: bool,
    pub detail: String,
}

pub fn now_ts() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn guard<T>(m: &'static Mutex<T>) -> MutexGuard<'static, T> {
    // 锁中毒（某个线程 panic 在持锁期间）时不能让读侧跟着 panic：
    // 换成内部值继续用，最差是一轮快照缺失。
    match m.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    }
}

// ---------------------------------------------------------------- 使用报告

/// `report_runner.STATE`
#[derive(Clone, Default)]
pub struct Report {
    pub started: f64,
    pub samples: i64,
    pub last_sample: String,
    pub last_run: f64,
    pub last_made: Vec<Value>,
    pub last_removed: Vec<Value>,
    pub error: String,
    pub runs: i64,
}

pub fn report() -> &'static Mutex<Report> {
    static S: OnceLock<Mutex<Report>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(Report::default()))
}

/// `report_runner.state()` —— 键顺序照 STATE 的字面量顺序。
pub fn report_value() -> Value {
    let st = guard(report()).clone();
    json!({
        "started": st.started,
        "samples": st.samples,
        "last_sample": st.last_sample,
        "last_run": st.last_run,
        "last_made": st.last_made,
        "last_removed": st.last_removed,
        "error": st.error,
        "runs": st.runs,
    })
}

pub fn report_update(f: impl FnOnce(&mut Report)) {
    f(&mut guard(report()));
}

pub fn report_get() -> Report {
    guard(report()).clone()
}

// ---------------------------------------------------------------- OCR

/// `ocr_runner.STATE`：字段与字面量同序；`img_fixed` 在 Python 里是首次跑完才
/// `update` 进去的（出现在最后），所以这里用 `Option` 控制它出不出现在 JSON 里。
pub fn ocr() -> &'static Mutex<Value> {
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

pub fn ocr_value() -> Value {
    guard(ocr()).clone()
}

pub fn ocr_update(f: impl FnOnce(&mut Value)) {
    f(&mut guard(ocr()));
}

/// `dash._ocr()`：工作台「长图 OCR」卡。`seconds` 取 `last_output.seconds`。
pub fn ocr_dash() -> Value {
    let st = ocr_value();
    let last = st.get("last_output").cloned().unwrap_or(Value::Null);
    // Python 是 `out.get("chars") or 0`：没跑过时回**整数** 0，跑过后 chars 是整数、
    // seconds 是 round(...,1) 的浮点。类型不同，不能统一成 f64。
    let num_or_zero = |v: Option<&Value>| match v {
        Some(Value::Number(_)) => v.unwrap().clone(),
        Some(Value::String(s)) => s.parse::<f64>().map(|x| json!(x)).unwrap_or(json!(0)),
        _ => json!(0),
    };
    json!({
        "at": st.get("last_at").and_then(|v| v.as_str()).unwrap_or(""),
        "done": st.get("done").cloned().unwrap_or(json!(0)),
        "failed": st.get("failed").cloned().unwrap_or(json!(0)),
        "scanned": st.get("scanned").cloned().unwrap_or(json!(0)),
        "chars": num_or_zero(last.get("chars")),
        "seconds": num_or_zero(last.get("seconds")),
        "running": st.get("running").and_then(|v| v.as_bool()).unwrap_or(false),
        "error": last.get("error").and_then(|v| v.as_str()).unwrap_or(""),
    })
}

// ---------------------------------------------------------------- 心跳

/// `heartbeat.STATE`
pub fn heartbeat() -> &'static Mutex<Value> {
    static S: OnceLock<Mutex<Value>> = OnceLock::new();
    S.get_or_init(|| {
        Mutex::new(json!({
            "running": false,
            "last_at": "",
            "last": null,
            "restart_count": 0,
            "last_restart_at": 0.0,
            "consecutive_offline": 0,
            "needs_attention": false,
            "last_compact_at": 0.0,
        }))
    })
}

pub fn hb_get(k: &str) -> Value {
    guard(heartbeat()).get(k).cloned().unwrap_or(Value::Null)
}

pub fn hb_set(k: &str, v: Value) {
    if let Some(o) = guard(heartbeat()).as_object_mut() {
        o.insert(k.to_string(), v);
    }
}

pub fn hb_i64(k: &str) -> i64 {
    match hb_get(k) {
        Value::Number(n) => n.as_i64().unwrap_or(0),
        Value::String(s) => s.parse().unwrap_or(0),
        Value::Bool(b) => b as i64,
        _ => 0,
    }
}

pub fn hb_f64(k: &str) -> f64 {
    match hb_get(k) {
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        Value::String(s) => s.parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

pub fn hb_truthy(k: &str) -> bool {
    match hb_get(k) {
        Value::Bool(b) => b,
        Value::Null => false,
        Value::Number(n) => n.as_f64().unwrap_or(0.0) != 0.0,
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

pub fn hb_inc(k: &str) -> i64 {
    let mut g = guard(heartbeat());
    let n = match g.get(k) {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0) + 1,
        _ => 1,
    };
    if let Some(o) = g.as_object_mut() {
        o.insert(k.to_string(), json!(n));
    }
    n
}

pub fn heartbeat_value() -> Value {
    guard(heartbeat()).clone()
}

/// `dash._heartbeat()`：工作台「心跳」卡（只读投影）。
pub fn heartbeat_dash() -> Value {
    let st = heartbeat_value();
    let last = st.get("last").cloned().unwrap_or(Value::Null);
    let mut checks: Vec<Value> = Vec::new();
    if let Some(Value::Array(arr)) = last.get("checks") {
        for c in arr {
            checks.push(json!({
                "id": c.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                "name": c.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                "ok": c.get("ok").and_then(|v| v.as_bool()).unwrap_or(false),
                "level": c.get("level").and_then(|v| v.as_str()).unwrap_or("info"),
                "detail": c.get("detail").and_then(|v| v.as_str()).unwrap_or(""),
            }));
        }
    }
    let bad: Vec<Value> = checks
        .iter()
        .filter(|c| !c.get("ok").and_then(|v| v.as_bool()).unwrap_or(false))
        .cloned()
        .collect();
    let at = last
        .get("at")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| st.get("last_at").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .unwrap_or_default();
    json!({
        "ok": if last.is_null() { Value::Null } else { Value::Bool(last.get("ok").and_then(|v| v.as_bool()).unwrap_or(false)) },
        "at": at,
        "running": st.get("running").and_then(|v| v.as_bool()).unwrap_or(false),
        "heal": true,
        "checks": checks,
        "bad": bad,
    })
}

// ---------------------------------------------------------------- AI 趋势

/// `ai_trends_runner.STATE`
pub fn trends() -> &'static Mutex<Value> {
    static S: OnceLock<Mutex<Value>> = OnceLock::new();
    S.get_or_init(|| {
        Mutex::new(json!({
            "running": false,
            "started": "",
            "error": "",
            "last": null,
        }))
    })
}

pub fn trends_value() -> Value {
    guard(trends()).clone()
}

pub fn trends_set(k: &str, v: Value) {
    if let Some(o) = guard(trends()).as_object_mut() {
        o.insert(k.to_string(), v);
    }
}

pub fn trends_running() -> bool {
    guard(trends())
        .get("running")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}
