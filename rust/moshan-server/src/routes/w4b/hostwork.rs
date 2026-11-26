// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：`moshan/hostwork.py` 里这一组路由真正用到的那一小块 —— 会话账（token）、
//       可选空间/项目清单、价格折算。计划卡的「host」与简报的「昨日消耗」都从这里来。
// 不做：不扫会话日志（那是 hostwork 的采集层，Python 侧已经在写缓存）、不采集进程/
//       磁盘/软件清单。Rust 侧只读 Python 落好的两份缓存：`_本机工作/缓存/会话.json`
//       和 `_本机工作/折算.json`，保证两边算的是同一份数。
//
// 为什么能这么干：这两份缓存是 hostwork 的**公共产物**（页面也读它），格式稳定、
// 带 PARSE_VERSION 版本号。Rust 自己重写一遍 jsonl 解析只会引入第二套口径。

use super::util::{self, get, int0, s_or, truthy};
use crate::paths::Ctx;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// `hostwork.PARSE_VERSION`：缓存版本不对就当没有（Python 也这么判）。
const PARSE_VERSION: i64 = 17;
const SPACE_FALLBACK: &str = "未归属空间";

fn own_dir(ctx: &Ctx) -> PathBuf {
    ctx.library_dir.join("_本机工作")
}

pub fn cache_path(ctx: &Ctx) -> PathBuf {
    own_dir(ctx).join("缓存").join("会话.json")
}

fn price_path(ctx: &Ctx) -> PathBuf {
    own_dir(ctx).join("折算.json")
}

fn wb_db() -> PathBuf {
    util::home().join(".workbuddy").join("workbuddy.db")
}

/// `_SESS["files"]` 的值（每条是一份会话聚合），按缓存里的键顺序。
fn cache_files(ctx: &Ctx) -> Vec<Value> {
    let Some(v) = util::load_json(&cache_path(ctx)) else {
        return Vec::new();
    };
    if v.get("v").and_then(|x| x.as_i64()) != Some(PARSE_VERSION) {
        return Vec::new();
    }
    match v.get("files") {
        Some(Value::Object(m)) => m.values().cloned().collect(),
        _ => Vec::new(),
    }
}

fn aggs(ctx: &Ctx) -> Vec<Value> {
    cache_files(ctx)
        .into_iter()
        .filter_map(|e| match e.get("agg") {
            Some(a) if a.is_object() => Some(a.clone()),
            _ => None,
        })
        .collect()
}

/// `sessions_snapshot()`：有 last 的会话，按 last 倒序。
pub fn sessions_snapshot(ctx: &Ctx) -> Vec<Value> {
    let mut out: Vec<Value> = aggs(ctx)
        .into_iter()
        .filter(|a| truthy(get(a, "last")))
        .collect();
    out.sort_by(|a, b| {
        let la = get(a, "last").as_i64().unwrap_or(0);
        let lb = get(b, "last").as_i64().unwrap_or(0);
        lb.cmp(&la)
    });
    out
}

/// `_session_index()`：sqlite 只读查询。库被锁/没装/表变了就回空表（Python 同口径）。
fn session_index() -> HashMap<String, String> {
    let db = wb_db();
    if !db.is_file() {
        return HashMap::new();
    }
    let mut spaces: HashMap<String, String> = HashMap::new();
    if let Some(rows) = sqlite_json(&db, "SELECT id, name FROM spaces") {
        for r in rows {
            if let Some(id) = r.get("id").and_then(|x| x.as_str()) {
                spaces.insert(
                    id.to_string(),
                    r.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                );
            }
        }
    }
    let Some(rows) = sqlite_json(&db, "SELECT id, space_id FROM sessions") else {
        return HashMap::new();
    };
    let mut out = HashMap::new();
    for r in rows {
        let Some(id) = r.get("id").and_then(|x| x.as_str()) else {
            continue;
        };
        let sp = r.get("space_id").and_then(|x| x.as_str()).unwrap_or("");
        out.insert(id.to_string(), spaces.get(sp).cloned().unwrap_or_default());
    }
    out
}

fn sqlite_json(db: &Path, sql: &str) -> Option<Vec<Value>> {
    let out = Command::new("sqlite3")
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
    let v: Value = serde_json::from_str(text.trim()).ok()?;
    v.as_array().cloned()
}

/// `hostwork.pulse(90, "")` 里的 `opts`：全量空间/项目清单 + 窗口内 token。
/// Rust 只需要这两张表（计划卡的 host 字段就是它的投影）。
pub fn pulse_opts(ctx: &Ctx) -> (Vec<Value>, Vec<Value>) {
    let since = {
        let today = util::now_local().date_naive();
        (today - chrono::Duration::days(89)).format("%Y-%m-%d").to_string()
    };
    let idx = session_index();
    let mut opt_sp: Vec<(String, i64)> = Vec::new();
    let mut opt_pj: Vec<(String, i64)> = Vec::new();

    let bump = |list: &mut Vec<(String, i64)>, key: &str, tok: i64| {
        if let Some(slot) = list.iter_mut().find(|(k, _)| k == key) {
            slot.1 += tok;
        } else {
            list.push((key.to_string(), tok));
        }
    };

    for agg in aggs(ctx) {
        let sid = s_or(get(&agg, "id"));
        let sp = idx
            .get(&sid)
            .cloned()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| SPACE_FALLBACK.to_string());
        let proj = s_or(get(&agg, "proj"));
        let mut win_tok: i64 = 0;
        if let Some(Value::Object(days)) = agg.get("days") {
            for (d, dm) in days {
                if d.as_str() >= since.as_str() {
                    win_tok += int0(get(dm, "in")) + int0(get(dm, "out"));
                }
            }
        }
        if win_tok != 0 {
            bump(&mut opt_sp, &sp, win_tok);
            if !proj.is_empty() {
                bump(&mut opt_pj, &proj, win_tok);
            }
        }
    }

    fn ranked(list: Vec<(String, i64)>, prefix: &str) -> Vec<Value> {
        let mut v = list;
        // Python: sorted(items, key=lambda kv: -kv[1]) —— 稳定排序
        v.sort_by(|a, b| b.1.cmp(&a.1));
        v.into_iter()
            .map(|(k, tok)| json!({"k": format!("{}{}", prefix, k), "n": k, "tok": tok}))
            .collect()
    }
    (ranked(opt_sp, "space:"), ranked(opt_pj, "proj:"))
}

/// `host_projects()`：计划卡要的那一份。`at` 是 pulse 的 "HH:MM:SS"。
pub fn host_projects(ctx: &Ctx) -> Value {
    let (spaces, projs) = pulse_opts(ctx);
    json!({
        "spaces": spaces,
        "projs": projs,
        "at": util::now_hms(),
        "err": "",
    })
}

// ---------------------------------------------------------------- 折算

fn default_price() -> Value {
    json!({
        "note": "价格单位：元 / 百万 token。points_per_mtok = 每百万 token 折多少点数；\
填 null 表示这套计费没有点数概念（页面会把点数那栏画成「—」）。\
数字是估算值，不是账单 —— 想对账就改这里。",
        "currency": "¥",
        "models": {
            "deepseek-v4.1-flash": {"in": 2.0, "cached": 0.2, "out": 3.0, "points_per_mtok": null},
            "deepseek-v4.1": {"in": 2.0, "cached": 0.2, "out": 3.0, "points_per_mtok": null},
            "qwen3.5-4b": {"in": 0, "cached": 0, "out": 0, "points_per_mtok": null, "local": true},
            "claude-3-5-sonnet": {"in": 21.3, "cached": 2.1, "out": 106.5, "points_per_mtok": null},
        },
        "default": {"in": null, "cached": null, "out": null, "points_per_mtok": null},
    })
}

/// `price_config()`：默认价表 + 用户那份（models 逐个覆盖）。
pub fn price_config(ctx: &Ctx) -> Value {
    let p = price_path(ctx);
    let default = default_price();
    let Some(cfg) = util::load_json(&p) else {
        // Python 第一次会落一份默认价表；这边也落，免得两边数据目录长歪。
        let _ = crate::jsonio::atomic_write(&p, &util::dumps_flat(&default));
        return default;
    };
    if !cfg.is_object() || !truthy(get(&cfg, "models")) {
        let _ = crate::jsonio::atomic_write(&p, &util::dumps_flat(&default));
        return default;
    }
    let mut merged = default.clone();
    if let (Some(dst), Some(src)) = (merged.as_object_mut(), cfg.as_object()) {
        for (k, v) in src {
            if k != "models" {
                dst.insert(k.clone(), v.clone());
            }
        }
    }
    let mut models = default.get("models").cloned().unwrap_or(json!({}));
    if let (Some(dst), Some(src)) = (
        models.as_object_mut(),
        cfg.get("models").and_then(|m| m.as_object()),
    ) {
        for (k, v) in src {
            dst.insert(k.clone(), v.clone());
        }
    }
    if let Some(o) = merged.as_object_mut() {
        o.insert("models".to_string(), models);
    }
    merged
}

/// `_price_for(model, cfg)`：先精确、再大小写前缀/包含，最后 default。
pub fn price_for(model: &str, cfg: &Value) -> Value {
    let Some(models) = cfg.get("models").and_then(|m| m.as_object()) else {
        return cfg.get("default").cloned().unwrap_or(json!({}));
    };
    if let Some(v) = models.get(model) {
        return v.clone();
    }
    let low = model.to_lowercase();
    for (k, v) in models {
        let kl = k.to_lowercase();
        if low.starts_with(&kl) || low.contains(&kl) {
            return v.clone();
        }
    }
    cfg.get("default").cloned().unwrap_or(json!({}))
}

fn num_or(v: &Value, d: f64) -> f64 {
    util::as_f64(v).unwrap_or(d)
}

/// `_cost_of(stat, price) -> (点数, 金额)`；缺价格给 None。
pub fn cost_of(stat: &Value, price: &Value) -> (Option<f64>, Option<f64>) {
    let tin = num_or(get(stat, "in"), 0.0);
    let tout = num_or(get(stat, "out"), 0.0);
    let total = tin + tout;
    let cached = num_or(get(stat, "cached"), 0.0).min(tin);
    let fresh = (tin - cached).max(0.0);
    let pm = get(price, "points_per_mtok");
    let pts = if pm.is_null() {
        None
    } else {
        Some(total / 1e6 * num_or(pm, 0.0))
    };
    let ins = get(price, "in");
    let outs = get(price, "out");
    let cs = get(price, "cached");
    let cost = if ins.is_null() && outs.is_null() {
        let cpm = get(price, "cny_per_mtok");
        if cpm.is_null() {
            None
        } else {
            Some(total / 1e6 * num_or(cpm, 0.0))
        }
    } else {
        let cs_v = if cs.is_null() { num_or(ins, 0.0) } else { num_or(cs, 0.0) };
        Some((fresh * num_or(ins, 0.0) + cached * cs_v + tout * num_or(outs, 0.0)) / 1e6)
    };
    (pts, cost)
}
