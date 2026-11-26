// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：折算（token → 点数 / 钱）。对应 hostwork.PRICE_DEFAULT / price_config /
//       _price_for / _explicit_price / _cost_of。
// 不做：不算总账（overview.money 自己 sum）；不改用户手写的折算.json 的语义。

use super::util::{self, py_dumps_compact};
use crate::paths::Ctx;
use serde_json::{json, Value};

pub fn price_path(ctx: &Ctx) -> std::path::PathBuf {
    ctx.library_dir.join("_本机工作").join("折算.json")
}

pub fn default_price() -> Value {
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
        let _ = crate::jsonio::atomic_write(&p, &py_dumps_compact(&default));
        return default;
    };
    if !cfg.is_object() || !util::truthy(cfg.get("models").unwrap_or(&Value::Null)) {
        let _ = crate::jsonio::atomic_write(&p, &py_dumps_compact(&default));
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

fn lookup(model: &str, cfg: &Value) -> Option<Value> {
    let models = cfg.get("models").and_then(|m| m.as_object())?;
    if let Some(v) = models.get(model) {
        return Some(v.clone());
    }
    let low = model.to_lowercase();
    for (k, v) in models {
        let kl = k.to_lowercase();
        if low.starts_with(&kl) || low.contains(&kl) {
            return Some(v.clone());
        }
    }
    None
}

/// `_price_for(model, cfg)`：先精确、再大小写前缀/包含，最后 default。
pub fn price_for(model: &str, cfg: &Value) -> Value {
    lookup(model, cfg)
        .or_else(|| cfg.get("default").cloned())
        .unwrap_or(json!({}))
}

/// `_explicit_price`：只认价表里点名的模型，不吃 default（自报账用）。
pub fn explicit_price(model: &str, cfg: &Value) -> Option<Value> {
    lookup(model, cfg)
}

fn num_or(v: &Value, d: f64) -> f64 {
    util::as_f64(v).unwrap_or(d)
}

/// `_cost_of(stat, price) -> (点数, 金额)`；缺价格给 None。
pub fn cost_of(stat: &Value, price: &Value) -> (Option<f64>, Option<f64>) {
    let tin = num_or(stat.get("in").unwrap_or(&Value::Null), 0.0);
    let tout = num_or(stat.get("out").unwrap_or(&Value::Null), 0.0);
    let total = tin + tout;
    let cached = num_or(stat.get("cached").unwrap_or(&Value::Null), 0.0).min(tin);
    let fresh = (tin - cached).max(0.0);
    let pm = price.get("points_per_mtok").cloned().unwrap_or(Value::Null);
    let pts = if pm.is_null() {
        None
    } else {
        Some(total / 1e6 * num_or(&pm, 0.0))
    };
    let ins = price.get("in").cloned().unwrap_or(Value::Null);
    let outs = price.get("out").cloned().unwrap_or(Value::Null);
    let cs = price.get("cached").cloned().unwrap_or(Value::Null);
    let cost = if ins.is_null() && outs.is_null() {
        let cpm = price.get("cny_per_mtok").cloned().unwrap_or(Value::Null);
        if cpm.is_null() {
            None
        } else {
            Some(total / 1e6 * num_or(&cpm, 0.0))
        }
    } else {
        let cs_v = if cs.is_null() { num_or(&ins, 0.0) } else { num_or(&cs, 0.0) };
        Some((fresh * num_or(&ins, 0.0) + cached * cs_v + tout * num_or(&outs, 0.0)) / 1e6)
    };
    (pts, cost)
}
