// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：AI 技术动向的调度层（ai_trends_runner.py）—— `POST /api/trends/run` 手动触发
//       与每天 hour 点的定时触发。抓取 / 去重 / 分级 / 写文件全在
//       ai_trends_daily.py 里，这里只管「什么时候跑、跑完怎么展示」。
// 不做：GET /api/trends 的只读视图在 w5a（读的 STATE 已改指向 crate::live）。
//
// 当天已跑过（runs/<今天>.json 的 status 是 ok/partial）就跳过 —— 外部定时、
// 手动、本调度三个入口共用这一份幂等判据，一天只会真跑一次。

#![allow(dead_code)]

use super::util;
use crate::paths::Ctx;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

fn lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

fn truthy_default(v: Option<&Value>, d: bool) -> bool {
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

/// `pyexe.venv_python(venv)`
fn venv_python(venv: &Path) -> PathBuf {
    if cfg!(windows) {
        venv.join("Scripts").join("python.exe")
    } else {
        venv.join("bin").join("python")
    }
}

/// `pyexe.resolve(explicit, venv)`：显式配置 > venv > PATH 的 python3/python > 常见位置。
fn resolve_py(explicit: &str, venv: Option<&Path>) -> (Option<PathBuf>, Vec<String>) {
    let mut tried: Vec<String> = Vec::new();
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Some(v) = venv {
        cands.push(venv_python(v));
    }
    if !explicit.trim().is_empty() {
        let p = PathBuf::from(explicit);
        tried.push(p.to_string_lossy().to_string());
        if p.is_file() {
            return (Some(p), tried);
        }
    }
    for name in ["python3", "python"] {
        if let Some(w) = util::which(name) {
            cands.push(w);
        }
    }
    for d in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"] {
        for name in ["python3", "python"] {
            let p = Path::new(d).join(name);
            if p.is_file() {
                cands.push(p);
            }
        }
    }
    let mut seen: Vec<String> = Vec::new();
    for p in cands {
        let k = p.to_string_lossy().to_string();
        if seen.contains(&k) {
            continue;
        }
        seen.push(k.clone());
        tried.push(k);
        if p.is_file() {
            return (Some(p), tried);
        }
    }
    (None, tried)
}

/// `pyexe.not_found_hint`
fn not_found_hint(tried: &[String]) -> String {
    if tried.is_empty() {
        return "找不到 Python（没有任何候选路径）".into();
    }
    let head = tried.iter().take(3).cloned().collect::<Vec<_>>().join("、");
    let more = if tried.len() > 3 { format!(" 等 {} 处", tried.len()) } else { String::new() };
    format!("找不到 Python（找过 {head}{more}）")
}

struct TrendCfg {
    python: Option<PathBuf>,
    py_tried: Vec<String>,
    script: PathBuf,
    root: PathBuf,
    hour: i64,
    enabled: bool,
}

/// `_cfg(cfg)`
fn cfg_of(cfg: &Value) -> TrendCfg {
    let d = cfg.get("ai_trends").cloned().unwrap_or_else(|| json!({}));
    let script = {
        let s = d.get("script").and_then(|v| v.as_str()).unwrap_or("");
        if s.trim().is_empty() {
            util::home()
                .join(".trae-cn/assistant/2026-11-11/local-ai-workspace")
                .join("work/local-ai-workspace/scripts/ai_trends_daily.py")
        } else {
            PathBuf::from(s)
        }
    };
    // script.parents[1] / ".venv"
    let venv = script.parent().and_then(|p| p.parent()).map(|p| p.join(".venv"));
    let explicit = d.get("python").and_then(|v| v.as_str()).unwrap_or("");
    let (python, py_tried) = resolve_py(explicit, venv.as_deref());
    let root = match d.get("root").and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => PathBuf::from(s),
        _ => util::home().join("Documents").join("AI项目"),
    };
    TrendCfg {
        python,
        py_tried,
        script,
        root,
        hour: match d.get("hour") {
            Some(Value::Null) | None => 9,
            Some(v) => util::i64_of(Some(v)),
        },
        enabled: truthy_default(d.get("enabled"), true),
    }
}

/// `day_dir(cfg)`
fn day_dir(c: &TrendCfg) -> PathBuf {
    c.root.join("01-研究").join("普通").join("技术动向")
}

/// `runs_dir(cfg)`：数据根的系统记录目录
fn runs_dir(c: &TrendCfg) -> PathBuf {
    c.root.join("10-系统记录").join("新闻").join("runs")
}

/// `_legacy_runs_dir(cfg)`
fn legacy_runs_dir(c: &TrendCfg) -> PathBuf {
    c.script
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("data").join("ai-trends").join("runs"))
        .unwrap_or_else(|| PathBuf::from("runs"))
}

/// `_read_run(cfg, day)`：读 `runs/<day>.json`，补 `_path` / `_dir` 两个派生字段。
fn read_run(c: &TrendCfg, day: &str) -> Value {
    let p = runs_dir(c).join(format!("{day}.json"));
    if !p.exists() {
        return json!({});
    }
    let mut d = match crate::jsonio::read_json(&p) {
        Some(v) if v.is_object() => v,
        _ => return json!({}),
    };
    if let Some(o) = d.as_object_mut() {
        o.insert("_path".into(), json!(p.to_string_lossy()));
        o.insert("_dir".into(), json!(day_dir(c).join(day).to_string_lossy()));
    }
    d
}

/// `already_done(cfg, day)`：当天是否已经产出过可用简报。
fn already_done(c: &TrendCfg, day: &str) -> bool {
    let d = read_run(c, day);
    if d.as_object().map(|o| o.is_empty()).unwrap_or(true) {
        return false;
    }
    matches!(
        d.get("status").and_then(|v| v.as_str()).unwrap_or(""),
        "ok" | "partial"
    )
}

fn today() -> String {
    util::local_fmt("%Y-%m-%d")
}

/// `finally: STATE["running"] = False` 的等价物：panic 展开时也要复位。
struct RunningFlag;
impl Drop for RunningFlag {
    fn drop(&mut self) {
        crate::live::trends_set("running", json!(false));
    }
}

/// `ai_trends_runner.run(cfg, force, timeout=900)`：跑一次。
pub fn run(ctx: &Ctx, force: bool, timeout: u64) -> Value {
    let c = cfg_of(&ctx.cfg);
    let Some(py) = c.python.clone() else {
        return json!({"ok": false, "error": not_found_hint(&c.py_tried)});
    };
    if !c.script.exists() {
        return json!({"ok": false, "error": format!("找不到脚本：{}", c.script.to_string_lossy())});
    }
    let day = today();
    if !force && already_done(&c, &day) {
        let reason = format!("{day} 已经出过简报，本次跳过");
        crate::live::trends_set(
            "last",
            json!({"ok": true, "skipped": true, "reason": reason, "date": day,
                   "status": read_run(&c, &day).get("status").cloned().unwrap_or(json!(""))}),
        );
        return json!({"ok": true, "skipped": true, "reason": reason, "run": read_run(&c, &day)});
    }
    let guard = match lock().try_lock() {
        Ok(g) => g,
        Err(_) => return json!({"ok": false, "error": "正在跑，等这次结束"}),
    };
    let _guard = guard;
    crate::live::trends_set("running", json!(true));
    let _flag = RunningFlag;
    crate::live::trends_set("started", json!(util::local_fmt("%Y-%m-%d %H:%M:%S")));
    crate::live::trends_set("error", json!(""));
    let t0 = Instant::now();
    let result = (|| -> Value {
        let argv = vec![
            py.to_string_lossy().to_string(),
            c.script.to_string_lossy().to_string(),
        ];
        let cwd = c.script.parent().and_then(|p| p.parent());
        let r = util::run_cmd(&argv, timeout, cwd);
        let Some((rc, stdout, stderr)) = r else {
            crate::live::trends_set("error", json!("超时"));
            return json!({"ok": false, "error": format!("跑了 {timeout}s 还没结束，已中止")});
        };
        let rec = read_run(&c, &day);
        if rec.as_object().map(|o| o.is_empty()).unwrap_or(true) {
            let text = if !stderr.trim().is_empty() { stderr } else { stdout };
            let text = if text.trim().is_empty() { "没有产出".to_string() } else { text };
            let chars: Vec<char> = text.chars().collect();
            let tail: String = chars[chars.len().saturating_sub(300)..].iter().collect();
            crate::live::trends_set("error", json!(tail));
            return json!({"ok": false, "error": crate::live::trends_value().get("error").cloned().unwrap_or(json!("")), "returncode": rc});
        }
        let out = json!({
            "ok": rc == 0 || rc == 1,
            "returncode": rc,
            "seconds": util::py_round(t0.elapsed().as_secs_f64(), 1),
            "date": rec.get("date").cloned().unwrap_or(Value::Null),
            "status": rec.get("status").cloned().unwrap_or(Value::Null),
            "stats": rec.get("stats").cloned().unwrap_or_else(|| json!({})),
            "llm": rec.get("llm").cloned().unwrap_or_else(|| json!({})),
            "unavailable": rec.get("unavailable").cloned().unwrap_or_else(|| json!([])),
            "dir": rec.get("_dir").cloned().unwrap_or(Value::Null),
        });
        crate::live::trends_set("last", out.clone());
        out
    })();
    crate::live::trends_set("running", json!(false));
    result
}

/// 定时回调：Python 的 `_done` 记一条 `trends_done`。
fn log_done(ctx: &Ctx, out: &Value) {
    let items = out
        .get("stats")
        .and_then(|v| v.get("day_total"))
        .cloned()
        .unwrap_or(json!(0));
    crate::storage::log(
        ctx,
        json!({
            "event": "trends_done",
            "ok": truthy_default(out.get("ok"), false),
            "skipped": truthy_default(out.get("skipped"), false),
            "status": out.get("status").and_then(|v| v.as_str()).unwrap_or(""),
            "items": items,
            "llm": out.get("llm").and_then(|v| v.get("ok")).cloned().unwrap_or(Value::Null),
            "error": out.get("error").and_then(|v| v.as_str()).unwrap_or(""),
        }),
    );
}

/// `next_run_at(cfg)`：下一次整点（今天没过就今天，过了就明天）。
fn next_run_at(c: &TrendCfg) -> String {
    use chrono::Timelike;
    let now = chrono::Local::now();
    let mut nxt = now
        .with_hour(c.hour.clamp(0, 23) as u32)
        .and_then(|t| t.with_minute(0))
        .and_then(|t| t.with_second(0))
        .and_then(|t| t.with_nanosecond(0))
        .unwrap_or(now);
    if nxt <= now {
        nxt += chrono::Duration::days(1);
    }
    nxt.format("%Y-%m-%d %H:%M").to_string()
}

/// 每天 hour 点跑一次。每分钟看一次表（Mac 睡眠时 sleep 不走，按分钟检查才有补跑）。
/// 失败不无限重试：同一天最多 3 次，两次之间至少隔 30 分钟。
pub fn start(ctx: Arc<Ctx>) {
    use chrono::{Timelike, Datelike};
    let c0 = cfg_of(&ctx.cfg);
    // Python 的 `_start_trends_scheduler`：enabled is False 时连线程都不起。
    if !c0.enabled {
        eprintln!("[moshan-rs] ai_trends.enabled=false，未启动趋势循环");
        return;
    }
    crate::storage::log(
        &ctx,
        json!({"event": "trends_scheduler", "next": next_run_at(&c0)}),
    );
    std::thread::Builder::new()
        .name("moshan-trends".into())
        .spawn(move || {
            let mut tried: std::collections::HashMap<String, (i64, f64)> =
                std::collections::HashMap::new();
            loop {
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let c = cfg_of(&ctx.cfg);
                    let now = chrono::Local::now();
                    let today = format!("{}-{:02}-{:02}", now.year(), now.month(), now.day());
                    if c.enabled && now.hour() as i64 >= c.hour && !already_done(&c, &today) {
                        let st = tried.entry(today.clone()).or_insert((0, 0.0));
                        let nowf = util::now_f();
                        if st.0 < 3 && (st.0 == 0 || nowf - st.1 >= 1800.0) {
                            st.0 += 1;
                            st.1 = nowf;
                            let out = run(&ctx, false, 900);
                            log_done(&ctx, &out);
                        }
                    }
                    if tried.len() > 7 {
                        let mut keys: Vec<String> = tried.keys().cloned().collect();
                        keys.sort();
                        let drop_n = keys.len() - 7;
                        for k in keys.into_iter().take(drop_n) {
                            tried.remove(&k);
                        }
                    }
                }));
                if r.is_err() {
                    eprintln!("[moshan-rs] trends 调度单轮异常，已跳过（下一分钟继续）");
                }
                std::thread::sleep(Duration::from_secs(60));
            }
        })
        .ok();
}

// ---------------------------------------------------------------- 路由

fn bool_422(raw: &str) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({"detail": [{
            "type": "bool_parsing",
            "loc": ["query", "force"],
            "msg": "Input should be a valid boolean, unable to interpret input",
            "input": raw,
            "url": "https://errors.pydantic.dev/2.14/v/bool_parsing",
        }]})),
    )
        .into_response()
}

/// `POST /api/trends/run?force=`（server.py:888）
pub async fn run_route(State(ctx): crate::routes::common::CtxState, headers: HeaderMap, uri: Uri) -> Response {
    use crate::routes::common::{ok_err, peek};
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let pairs = crate::routes::common::query_map(&uri);
    let force = match pairs.into_iter().find(|(k, _)| k == "force") {
        None => false,
        Some((_, raw)) => match raw.trim().to_lowercase().as_str() {
            "1" | "true" | "t" | "yes" | "y" | "on" => true,
            "0" | "false" | "f" | "no" | "n" | "off" => false,
            _ => return bool_422(&raw),
        },
    };
    if crate::live::trends_running() {
        return ok_err("正在跑，等这次结束").into_response();
    }
    let ctx2 = ctx.clone();
    std::thread::spawn(move || {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run(&ctx2, force, 900)
        }));
    });
    crate::storage::log(&ctx, json!({"event": "trends_manual", "force": force}));
    Json(json!({"ok": true, "started": true})).into_response()
}
