// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：口袋自己的事件流水（hostwork.pocket_events）—— 按脚本汇总次数/耗时/失败。
// 不做：不处理别的 agent 的自述账（那是 _工作台/agents）。

use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Mutex;

use super::util::{py_int, str_or};

fn pocket_log() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".trae-cn")
        .join("assistant")
        .join("2026-11-11")
        .join("local-ai-workspace")
        .join("work")
        .join("local-ai-workspace")
        .join("pocket")
        .join("pocket.jsonl")
}

fn script_of(ev: &str) -> Option<&'static str> {
    Some(match ev {
        "link_start" | "link_done" => "存图.py",
        "article_start" | "article_done" => "文章.py",
        "ocr_sweep" | "ocr" | "refetch" => "ocr_runner.py",
        "heartbeat_done" | "heartbeat_scheduler" => "heartbeat.py",
        "report_done" => "report_runner.py",
        "dedupe_scheduler" => "dedupe_runner.py",
        "dedupe_manual" => "dedupe.py",
        "trends_scheduler" => "trends.py",
        "wechat_guard_scheduler" | "wechat_guard" => "wechat_guard.py",
        "upload" | "share" | "rename" | "zip" | "trash" => "server.py",
        "chat" | "chat_quick" => "chat（本地模型）",
        "inbox_group" | "inbox_name" => "server.py",
        _ => return None,
    })
}

static CACHE: Mutex<Option<(f64, Vec<Value>)>> = Mutex::new(None);

/// `pocket_events(days)`
pub fn pocket_events(days: i64) -> Value {
    let p = pocket_log();
    if !p.exists() {
        return json!({"scripts": [], "recent": [], "tokens": 0});
    }
    let mt = std::fs::metadata(&p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let rows: Vec<Value> = {
        let mut g = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        let need = match g.as_ref() {
            Some((m, _)) => *m != mt,
            None => true,
        };
        if need {
            let mut rows = Vec::new();
            if let Ok(text) = std::fs::read_to_string(&p) {
                for ln in text.lines() {
                    if ln.starts_with('{') {
                        if let Ok(v) = serde_json::from_str::<Value>(ln) {
                            rows.push(v);
                        }
                    }
                }
            }
            *g = Some((mt, rows));
        }
        g.as_ref().unwrap().1.clone()
    };
    let since = if days != 0 {
        (chrono::Local::now().timestamp() as f64) - days as f64 * 86400.0
    } else {
        0.0
    };
    let mut agg: serde_json::Map<String, Value> = serde_json::Map::new();
    let mut recent: Vec<Value> = Vec::new();
    let mut tokens = 0i64;
    for r in &rows {
        let ts = str_or(r.get("ts"));
        let mut when = 0.0f64;
        let head: String = ts.chars().take(19).collect();
        if let Ok(d) = chrono::NaiveDateTime::parse_from_str(&head.replace('T', " ").as_str(), "%Y-%m-%d %H:%M:%S") {
            when = d.and_local_timezone(chrono::Local).single().map(|t| t.timestamp() as f64).unwrap_or(0.0);
        }
        if since != 0.0 && when != 0.0 && when < since {
            continue;
        }
        let ev = str_or(r.get("event"));
        let Some(script) = script_of(&ev) else { continue };
        let a = agg.entry(script.to_string()).or_insert_with(|| {
            json!({"script": script, "n": 0, "ms": 0, "fail": 0, "last": ""})
        });
        let o = a.as_object_mut().unwrap();
        o.insert("n".into(), json!(py_int(o.get("n").unwrap_or(&Value::Null)) + 1));
        o.insert("ms".into(), json!(py_int(o.get("ms").unwrap_or(&Value::Null)) + py_int(r.get("ms").unwrap_or(&Value::Null))));
        let failed = r.get("ok").map(|v| *v == Value::Bool(false)).unwrap_or(false)
            || super::util::truthy(r.get("error").unwrap_or(&Value::Null));
        if failed {
            o.insert("fail".into(), json!(py_int(o.get("fail").unwrap_or(&Value::Null)) + 1));
        }
        if ts > str_or(o.get("last")) {
            o.insert("last".into(), json!(ts));
        }
        if let Some(t) = r.get("tokens") {
            if super::util::truthy(t) {
                tokens += py_int(t);
            }
        }
        if recent.len() < 40 && matches!(ev.as_str(), "link_done" | "article_done" | "chat" | "ocr_sweep") {
            let chars: Vec<char> = ts.chars().collect();
            let at = if chars.len() >= 16 {
                chars[5..16].iter().collect::<String>().replace('T', " ")
            } else {
                ts.clone()
            };
            let detail = {
                let a = str_or(r.get("title"));
                if !a.is_empty() { a } else {
                    let b = str_or(r.get("text"));
                    if !b.is_empty() { b } else { str_or(r.get("names")) }
                }
            };
            recent.push(json!({
                "at": at, "event": ev, "script": script,
                "detail": detail.chars().take(60).collect::<String>(),
                "ms": py_int(r.get("ms").unwrap_or(&Value::Null)),
                "ok": !failed,
            }));
        }
    }
    let mut scripts: Vec<Value> = agg.values().cloned().collect();
    scripts.sort_by(|a, b| {
        py_int(b.get("n").unwrap_or(&Value::Null)).cmp(&py_int(a.get("n").unwrap_or(&Value::Null)))
    });
    recent.reverse();
    let recent: Vec<Value> = recent.into_iter().take(20).collect();
    json!({"scripts": scripts, "recent": recent, "tokens": tokens})
}
