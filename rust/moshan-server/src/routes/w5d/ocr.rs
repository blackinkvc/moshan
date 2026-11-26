// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：图片文章 OCR 的定时调度（ocr_runner.py）—— 启动跑一轮，之后每 interval 一轮；
//       结果写 crate::live 的 ocr 快照（/api/dash 的 ocr 段读它）。
// 不做：OCR 识别本身在 cuntu 的 ocr.py 里，这里只按 Python 的方式起子进程、
//       解析最后一行 JSON、维护 STATE。手动触发 /api/ocr、/api/ocr/run 在 w7a 泳道，
//       本文件不重复注册（axum 重复路由会 panic）。
//
// 与 Python 的差别（已知分叉，w7a 同款）：收尾的「重新分类 / 生成摘要 / 补配图」
// 依赖 article.py 与 summary.py，本泳道只跑 sweep 主路，classified/summarized/
// img_fixed 恒 0；dash 卡片不读这三项，读的 scanned/done/failed/chars/seconds 一致。

#![allow(dead_code)]

use super::util::{self, s_of};
use crate::paths::Ctx;
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

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
        if s.is_empty() {
            cuntu().join("ocr.py").to_string_lossy().to_string()
        } else {
            s
        }
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
    crate::live::ocr_update(|v| {
        if let Some(o) = v.as_object_mut() {
            for (k, val) in kv {
                o.insert((*k).to_string(), val.clone());
            }
        }
    });
}

fn state_get() -> Value {
    crate::live::ocr_value()
}

/// `ocr_runner.run_once`：起 `[python, script, "--sweep", "--json"]`，
/// 从输出末尾倒着找第一行 `{` 开头的 JSON。
pub fn run_once(cfg: &Value) -> Value {
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
        ("post_errors", json!(post_errors)),
        ("last_error", json!(s_of(rec.get("error")))),
        ("last_output", rec.clone()),
        // Python 的 `STATE.update` 里 img_fixed 是**首次运行才新增**的键，
        // 追加在 last_output 之后；已存在时插键不改位置，所以顺序能一直对上。
        ("img_fixed", json!(0)),
        ("running", json!(false)),
    ]);
    rec
}

/// 后台循环。Python 的 `scheduler_loop`：启动先跑一轮，跑完才睡 interval。
pub fn start(ctx: Arc<Ctx>) {
    let c = cfg_of(&ctx.cfg);
    // Python 的 `_start_ocr`：`ocr.enabled is False` 时连线程都不起。
    if !c.enabled {
        eprintln!("[moshan-rs] ocr.enabled=false，未启动 OCR 循环");
        return;
    }
    let raw_interval = ctx
        .cfg
        .get("ocr")
        .and_then(|v| v.get("interval_sec"))
        .map(|v| util::i64_of(Some(v)))
        .unwrap_or(300);
    crate::storage::log(&ctx, json!({"event": "ocr_scheduler", "interval_sec": raw_interval}));
    std::thread::Builder::new()
        .name("moshan-ocr".into())
        .spawn(move || loop {
            let interval = cfg_of(&ctx.cfg).interval.max(1) as u64;
            let t0 = Instant::now();
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_once(&ctx.cfg)
            }));
            match r {
                Ok(rec) => {
                    // Python 的 `_done`：skipped 不记账，其余记一条 ocr_sweep。
                    if rec.get("skipped").is_none() {
                        crate::storage::log(
                            &ctx,
                            json!({
                                "event": "ocr_sweep",
                                "ok": rec.get("ok").and_then(|v| v.as_bool()).unwrap_or(false),
                                "scanned": rec.get("scanned").cloned().unwrap_or(json!(0)),
                                "done": rec.get("done").cloned().unwrap_or(json!(0)),
                                "failed": rec.get("failed").cloned().unwrap_or(json!(0)),
                                "error": rec.get("error").and_then(|v| v.as_str()).unwrap_or(""),
                            }),
                        );
                    }
                }
                Err(_) => {
                    set_state(&[("running", json!(false)), ("last_error", json!("调度线程异常"))]);
                    eprintln!("[moshan-rs] ocr 单轮异常，已跳过（下一轮继续）");
                }
            }
            let secs = t0.elapsed().as_secs_f64();
            if secs > interval as f64 {
                eprintln!(
                    "[moshan-rs] ocr 单轮耗时 {:.1}s，超过间隔 {}s；本轮结束，跳过剩余等待",
                    secs, interval
                );
            }
            std::thread::sleep(Duration::from_secs(interval));
        })
        .ok();
}
