// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：本机工作那组读数里已经迁过来的两条 —— /api/rounds（逐轮记录）与
//       /api/sources（数据源清点）。对应 hostwork.rounds_items / sources_report。
// 不做：overview / pulse / workstat / hourly / tasks 还没迁（在 mod.rs 里没有注册，
//       请求会落到 Python 上游），它们的聚合层（token 折算、日聚合、节奏账、
//       提示词统计、任务清单）比逐轮记录厚得多，迁一半只会给出错的数。
//
// rounds 的口径只有一份：解析层在 sessions.rs（dash 的那一份），这里只负责
// 「按档位算 since、四家客户端合并、按结束时间倒排、截 80 条、补空间/会话名」。
#![allow(unused_imports, dead_code)]

use axum::extract::State;
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use super::cards;
use super::sessions;
use super::util::{now_hms, str_or, truthy};
use crate::paths::Ctx;
use crate::routes::common::{peek, q, CtxState};

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// `_range_days`：today=1 / 7d=7 / 30d=30 / all=0。
fn range_days(key: &str) -> i64 {
    match key {
        "today" => 1,
        "7d" => 7,
        "30d" => 30,
        "all" => 0,
        _ => 7,
    }
}

struct RoundsCache {
    at: f64,
    data: Value,
}

static ROUNDS: Mutex<Option<Vec<(String, RoundsCache)>>> = Mutex::new(None);

/// `_rounds_compute`：真算一遍逐轮记录。
fn rounds_compute(ctx: &Ctx, range_key: &str) -> Value {
    let days = range_days(range_key);
    let since_ms = if days == 0 {
        0.0
    } else {
        (now_secs() - days as f64 * 86400.0) * 1000.0
    };
    // 四家客户端（WorkBuddy / Cherry / Claude / TRAE）—— 与 _WB_CLIENTS + trae_work 同源
    let mut items = sessions::agent_items(&cards::root(ctx), if days == 0 { 365 } else { days });
    // agent_items 已经按 (end||start) 倒排并截了 80 条、补了 title/space/sess_title/sess_gone
    let _ = keep_none(&mut items);
    json!({"ok": true, "items": items, "at": now_hms()})
}

fn keep_none(_v: &mut Vec<Value>) {}

/// `_rounds_view`：today 档只留今天的。
fn rounds_view(data: &Value, range_key: &str) -> Value {
    if range_key != "today" {
        return data.clone();
    }
    let day = super::util::now_ymd();
    let items: Vec<Value> = match data.get("items") {
        Some(Value::Array(a)) => a
            .iter()
            .filter(|x| str_or(x.get("start")).starts_with(&day))
            .cloned()
            .collect(),
        _ => Vec::new(),
    };
    let mut m = match data {
        Value::Object(m) => m.clone(),
        _ => Map::new(),
    };
    m.insert("items".into(), Value::Array(items));
    Value::Object(m)
}

/// `GET /api/rounds`
pub async fn rounds_api(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let key = {
        let k = q(&uri, "range");
        if k.is_empty() { "7d".to_string() } else { k }
    };
    let c = ctx.clone();
    match tokio::task::spawn_blocking(move || {
        // 8 秒缓存：重算一锅要一两秒，前端 15 秒轮询一次
        {
            let g = ROUNDS.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(list) = g.as_ref() {
                if let Some((_, slot)) = list.iter().find(|(k, _)| *k == key) {
                    if now_secs() - slot.at < 8.0 {
                        return rounds_view(&slot.data, &key);
                    }
                }
            }
        }
        let data = rounds_compute(&c, &key);
        {
            let mut g = ROUNDS.lock().unwrap_or_else(|e| e.into_inner());
            let list = g.get_or_insert_with(Vec::new);
            list.retain(|(k, _)| *k != key);
            list.push((key.clone(), RoundsCache { at: now_secs(), data: data.clone() }));
        }
        rounds_view(&data, &key)
    })
    .await
    {
        Ok(v) => Json(v).into_response(),
        Err(e) => crate::routes::common::http_err(500, &format!("internal error: {e}")),
    }
}

// ------------------------------------------------------------------ 数据源清点

struct Spec {
    name: &'static str,
    path: PathBuf,
    pat: &'static str,
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

fn source_specs(root: &Path) -> Vec<Spec> {
    let h = home();
    let own = root.join("_本机工作");
    // macOS 的 TRAE CN 数据在 ~/Library/Application Support/Trae CN；
    // 会话记忆（MEM_ROOT）却仍在 ~/.trae-cn/memory/projects —— 两处不同源。
    let trae = h.join("Library").join("Application Support").join("Trae CN");
    let trae_mem = h.join(".trae-cn").join("memory").join("projects");
    vec![
        Spec { name: "WorkBuddy 会话流水", path: h.join(".workbuddy").join("projects"), pat: "*/*.jsonl" },
        Spec { name: "WorkBuddy 任务库", path: h.join(".workbuddy").join("workbuddy.db"), pat: "" },
        Spec { name: "TRAE 会话索引", path: trae.join("User").join("globalStorage").join("state.vscdb"), pat: "" },
        Spec { name: "TRAE 项目快照", path: trae.join("ModularData").join("ai-agent").join("snapshot"), pat: "*" },
        Spec { name: "TRAE 命令记录", path: trae.join("ModularData").join("ai-agent").join("command-execution"), pat: "*" },
        Spec { name: "TRAE 侧栏 OCR", path: own.join("TRAE任务.jsonl"), pat: "" },
        Spec { name: "TRAE 会话记忆", path: trae_mem, pat: "*/*/*.jsonl" },
        Spec { name: "TRAE 窗口采样", path: own.join("TRAE窗口.jsonl"), pat: "" },
        Spec { name: "TRAE 自述账", path: own.join("TRAE自述.jsonl"), pat: "" },
        Spec { name: "Agent 自述账", path: own.join("Agent自述.jsonl"), pat: "" },
        Spec { name: "Codex", path: h.join(".codex").join("sessions"), pat: "*/*/*/rollout-*.jsonl" },
        Spec { name: "Claude Code", path: h.join(".claude").join("projects"), pat: "*/*.jsonl" },
        Spec {
            name: "Cherry Studio 用量账本",
            path: h.join("Library").join("Application Support").join("CherryStudio").join("Data").join("cherrystudio.sqlite"),
            pat: "",
        },
        Spec { name: "Cursor", path: h.join(".cursor").join("chats"), pat: "*" },
    ]
}

/// `glob` 的等价：`*` 只在一层里匹配，`**` 不存在；目录按层展开。
fn count_glob(base: &Path, pat: &str) -> i64 {
    let segs: Vec<&str> = pat.split('/').collect();
    fn walk(dir: &Path, segs: &[&str]) -> i64 {
        if segs.is_empty() {
            return 1;
        }
        let seg = segs[0];
        let mut n = 0;
        let rd = match std::fs::read_dir(dir) {
            Ok(r) => r,
            Err(_) => return 0,
        };
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            let ok = if seg == "*" {
                true
            } else if let Some(suffix) = seg.strip_prefix("*.") {
                name.ends_with(&format!(".{}", suffix))
            } else if let Some(prefix) = seg.strip_suffix("*") {
                name.starts_with(prefix)
            } else {
                name == seg
            };
            if !ok {
                continue;
            }
            if segs.len() == 1 {
                n += 1;
            } else if p.is_dir() {
                n += walk(&p, &segs[1..]);
            }
        }
        n
    }
    walk(base, &segs)
}

/// `_root_source`：数据根从哪来。
fn root_source(state_dir: &Path) -> &'static str {
    if !std::env::var("CUNTU_ROOT").unwrap_or_default().trim().is_empty() {
        return "环境变量 CUNTU_ROOT";
    }
    // paths.CONF_FILE 是 state_dir()/config.json（交付版的等价物就是它自己的配置）
    if state_dir.join("config.json").exists() {
        return "配置文件 ~/.cuntu/config.json";
    }
    "默认位置 ~/Documents/AI项目"
}

/// `GET /api/sources`
pub async fn sources_api(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let c = ctx.clone();
    match tokio::task::spawn_blocking(move || {
        // 这里是 `paths.data_root()` = `library_dir()`，**走 resolve**：
        // macOS 上 /var 是 /private/var 的软链，Python 那边打印的是解析后的写法。
        // （dash 的 `_root(cfg)` 是另一条口径：allowed_roots[0] 原样，见 cards::root）
        let root = c.library_dir.clone();
        let mut rows: Vec<Value> = Vec::new();
        for spec in source_specs(&root) {
            let found = spec.path.exists();
            let mut count = 0i64;
            let mut hint = String::new();
            if found {
                if !spec.pat.is_empty() && spec.path.is_dir() {
                    count = count_glob(&spec.path, spec.pat);
                } else if spec.path.is_dir() {
                    count = std::fs::read_dir(&spec.path).map(|r| r.count() as i64).unwrap_or(0);
                } else {
                    count = 1;
                }
            } else {
                hint = "这台机器上没找到".to_string();
            }
            rows.push(json!({
                "name": spec.name,
                "path": spec.path.to_string_lossy(),
                "found": found,
                "count": count,
                "hint": hint,
            }));
        }
        let found = rows.iter().filter(|r| r.get("found").map(truthy).unwrap_or(false)).count();
        json!({
            "ok": true,
            "root": root.to_string_lossy(),
            "root_from": root_source(&c.state_dir),
            "items": rows,
            "found": found,
            "at": now_hms(),
        })
    })
    .await
    {
        Ok(v) => Json(v).into_response(),
        Err(e) => crate::routes::common::http_err(500, &format!("internal error: {e}")),
    }
}
