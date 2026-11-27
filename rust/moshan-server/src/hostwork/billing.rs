// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：计费账号（余额 / 官方账单）的读侧 —— 对齐 billing.py 的 view / _public / _delta。
// 不做：不做出网拉账（pull），也不写盘（保存账号走 Python 侧的路由）。
//
// 密钥只在本机文件里，给页面的那份一律打码，只露末四位。
#![allow(dead_code)]

use super::util::{py_float, round_n, str_or};
use crate::paths::Ctx;
use serde_json::{json, Map, Value};
use std::path::PathBuf;

struct Field {
    k: &'static str,
    secret: bool,
}

struct Provider {
    id: &'static str,
    name: &'static str,
    short: &'static str,
    bill: bool,
    usd: bool,
    fields: &'static [Field],
}

const P_BAILIAN: Provider = Provider {
    id: "bailian",
    name: "阿里百炼",
    short: "百炼",
    bill: true,
    usd: false,
    fields: &[
        Field { k: "ak", secret: false },
        Field { k: "sk", secret: true },
        Field { k: "key", secret: true },
    ],
};
const P_SILICON: Provider = Provider {
    id: "silicon",
    name: "硅基流动",
    short: "硅基",
    bill: false,
    usd: false,
    fields: &[Field { k: "key", secret: true }],
};
const P_DEEPSEEK: Provider = Provider {
    id: "deepseek",
    name: "DeepSeek",
    short: "DS",
    bill: false,
    usd: false,
    fields: &[Field { k: "key", secret: true }],
};
const P_MOONSHOT: Provider = Provider {
    id: "moonshot",
    name: "月之暗面 Kimi",
    short: "Kimi",
    bill: false,
    usd: false,
    fields: &[Field { k: "key", secret: true }],
};
const P_OPENROUTER: Provider = Provider {
    id: "openrouter",
    name: "OpenRouter",
    short: "OR",
    bill: false,
    usd: true,
    fields: &[Field { k: "key", secret: true }],
};
const P_MANUAL: Provider = Provider {
    id: "manual",
    name: "手动录入",
    short: "手录",
    bill: true,
    usd: false,
    fields: &[
        Field { k: "balance", secret: false },
        Field { k: "month", secret: false },
        Field { k: "cur", secret: false },
    ],
};

fn provider_of(id: &str) -> Option<&'static Provider> {
    match id {
        "bailian" => Some(&P_BAILIAN),
        "silicon" => Some(&P_SILICON),
        "deepseek" => Some(&P_DEEPSEEK),
        "moonshot" => Some(&P_MOONSHOT),
        "openrouter" => Some(&P_OPENROUTER),
        "manual" => Some(&P_MANUAL),
        _ => None,
    }
}

pub fn store_path(ctx: &Ctx) -> PathBuf {
    ctx.runtime_dir.join("_本机工作").join("计费账号.json")
}

/// `_read()`：缺失/损坏给 {"accounts": []}。
fn read_store(ctx: &Ctx) -> Map<String, Value> {
    let p = store_path(ctx);
    let mut d = match super::util::load_json(&p) {
        Some(Value::Object(m)) => m,
        _ => return {
            let mut m = Map::new();
            m.insert("accounts".into(), json!([]));
            m
        },
    };
    if !d.get("accounts").map(|v| v.is_array()).unwrap_or(false) {
        d.insert("accounts".into(), json!([]));
    }
    d
}

pub fn mask(v: &str) -> String {
    let chars: Vec<char> = v.chars().collect();
    if chars.is_empty() {
        return String::new();
    }
    if chars.len() <= 8 {
        let n = std::cmp::max(2, chars.len() as i64 - 2) as usize;
        let tail: String = chars[chars.len() - 2..].iter().collect();
        return "•".repeat(n) + &tail;
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{}…{}", head, tail)
}

fn num2(v: Option<&Value>) -> Option<f64> {
    match v {
        None => None,
        Some(Value::String(s)) if s.is_empty() => None,
        Some(x) => py_float(x).map(|f| round_n(f, 2)),
    }
}

fn public_of(acc: &Value) -> Value {
    let pid = str_or(acc.get("provider"));
    let prov = provider_of(&pid);
    let fields_src = acc.get("fields").and_then(|v| v.as_object()).cloned().unwrap_or_default();

    let mut f = Map::new();
    if let Some(p) = prov {
        for spec in p.fields {
            let v = fields_src.get(spec.k);
            if spec.secret {
                let s = str_or(v);
                f.insert(spec.k.into(), Value::String(if s.is_empty() { String::new() } else { mask(&s) }));
            } else {
                f.insert(
                    spec.k.into(),
                    match v {
                        Some(x) if !x.is_null() => x.clone(),
                        _ => Value::String(String::new()),
                    },
                );
            }
        }
    }

    let mut filled = Map::new();
    if let Some(p) = prov {
        for spec in p.fields {
            filled.insert(spec.k.into(), json!(super::util::truthy(fields_src.get(spec.k).unwrap_or(&Value::Null))));
        }
    }

    let hist = acc
        .get("hist")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let hist_tail: Vec<Value> = if hist.len() > 60 {
        hist[hist.len() - 60..].to_vec()
    } else {
        hist
    };

    let mut o = Map::new();
    o.insert("id".into(), Value::String(str_or(acc.get("id"))));
    o.insert("provider".into(), Value::String(pid.clone()));
    o.insert(
        "provider_name".into(),
        Value::String(match prov {
            Some(p) if !p.name.is_empty() => p.name.to_string(),
            _ if !pid.is_empty() => pid.clone(),
            _ => "?".into(),
        }),
    );
    o.insert(
        "provider_short".into(),
        Value::String(match prov {
            Some(p) if !p.short.is_empty() => p.short.to_string(),
            Some(p) => p.name.chars().take(1).collect(),
            None => "?".into(),
        }),
    );
    o.insert("name".into(), Value::String(str_or(acc.get("name"))));
    o.insert("note".into(), Value::String(str_or(acc.get("note"))));
    o.insert("fields".into(), Value::Object(f));
    o.insert("filled".into(), Value::Object(filled));
    o.insert(
        "last".into(),
        match acc.get("last") {
            Some(x) if super::util::truthy(x) => x.clone(),
            _ => json!({}),
        },
    );
    o.insert("hist".into(), Value::Array(hist_tail));
    o.insert("can_pull".into(), json!(prov.is_some()));
    o.insert("bill".into(), json!(prov.map(|p| p.bill).unwrap_or(false)));
    o.insert("usd".into(), json!(prov.map(|p| p.usd).unwrap_or(false)));
    Value::Object(o)
}

fn num_of(v: Option<&Value>) -> f64 {
    match v {
        Some(x) => py_float(x).unwrap_or(0.0),
        None => 0.0,
    }
}

/// `_delta(a)`：上次差多少、这个月掉了多少。
fn delta_of(acc: &Value) -> Value {
    let hist = acc.get("hist").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let mut out = Map::new();
    if hist.len() >= 2 {
        let prev = &hist[hist.len() - 2];
        let last = &hist[hist.len() - 1];
        out.insert(
            "d_prev".into(),
            super::util::fnum(round_n(
                num_of(last.get("b")) - num_of(prev.get("b")),
                2,
            )),
        );
        out.insert("t_prev".into(), prev.get("t").cloned().unwrap_or(Value::Null));
    }
    if !hist.is_empty() {
        let now_ym = super::util::now_ymd().get(..7).unwrap_or("").to_string();
        let month: Vec<&Value> = hist
            .iter()
            .filter(|h| {
                let t = num_of(h.get("t")) as i64;
                super::util::ts_to_str(t).get(..7).map(|s| s == now_ym).unwrap_or(false)
            })
            .collect();
        let base = month.first().copied().unwrap_or(&hist[0]);
        let last = &hist[hist.len() - 1];
        out.insert(
            "d_month".into(),
            super::util::fnum(round_n(num_of(last.get("b")) - num_of(base.get("b")), 2)),
        );
        out.insert("t_month".into(), base.get("t").cloned().unwrap_or(Value::Null));
        out.insert("n".into(), json!(hist.len()));
    }
    Value::Object(out)
}

fn est_key(pid: &str) -> Option<&'static str> {
    match pid {
        "bailian" => Some("dashscope"),
        "silicon" => Some("silicon"),
        _ => None,
    }
}

/// `view(est)`：页面用的整份 —— 账号清单 + 对账。
pub fn view(ctx: &Ctx, est: &Value) -> Value {
    let d = read_store(ctx);
    let accounts = d.get("accounts").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let mut rows: Vec<Value> = Vec::new();
    for a in &accounts {
        let mut p = match public_of(a) {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        p.insert("delta".into(), delta_of(a));
        let pid = str_or(a.get("provider"));
        let e = est_key(&pid).and_then(|k| est.get(k));
        let last = a.get("last").cloned().unwrap_or(json!({}));
        if let Some(e) = e {
            let by_ym_nonempty = e
                .get("by_ym")
                .and_then(|v| v.as_object())
                .map(|m| !m.is_empty())
                .unwrap_or(false);
            if by_ym_nonempty {
                let months = last.get("months").and_then(|v| v.as_array()).cloned().unwrap_or_default();
                let paid = last
                    .get("paid")
                    .cloned()
                    .filter(|v| !v.is_null())
                    .or_else(|| {
                        months
                            .iter()
                            .find(|m| num_of(m.get("net")) > 0.0)
                            .cloned()
                    });
                if let Some(paid) = paid {
                    let ym = str_or(paid.get("ym"));
                    let base = e
                        .get("by_ym")
                        .and_then(|v| v.as_object())
                        .and_then(|m| m.get(&ym))
                        .cloned();
                    let mut eo = Map::new();
                    eo.insert(
                        "cny".into(),
                        super::util::fnum(round_n(base.as_ref().map(|b| num_of(b.get("cny"))).unwrap_or(0.0), 2)),
                    );
                    eo.insert(
                        "calls".into(),
                        base.as_ref()
                            .and_then(|b| b.get("calls").cloned())
                            .filter(super::util::truthy)
                            .unwrap_or(json!(0)),
                    );
                    eo.insert("ym".into(), Value::String(ym.clone()));
                    eo.insert("known".into(), json!(base.is_some()));
                    let est_cny = round_n(base.as_ref().map(|b| num_of(b.get("cny"))).unwrap_or(0.0), 2);
                    p.insert("est".into(), Value::Object(eo));
                    if base.is_some() {
                        p.insert(
                            "gap".into(),
                            super::util::fnum(round_n(num_of(paid.get("net")) - est_cny, 2)),
                        );
                    }
                }
            }
        }
        rows.push(Value::Object(p));
    }

    let cny: Vec<&Value> = rows
        .iter()
        .filter(|r| !super::util::truthy(r.get("usd").unwrap_or(&Value::Null)))
        .collect();
    let mut total_balance = 0.0f64;
    let mut total_month = 0.0f64;
    for r in &cny {
        let last = r.get("last").unwrap_or(&Value::Null);
        if super::util::truthy(last.get("ok").unwrap_or(&Value::Null)) {
            total_balance += num_of(last.get("balance"));
            total_month += num_of(last.get("month"));
        }
    }

    let mut o = Map::new();
    o.insert("ok".into(), json!(true));
    o.insert("accounts".into(), Value::Array(rows.clone()));
    o.insert("store".into(), Value::String(store_path(ctx).to_string_lossy().to_string()));
    o.insert("n".into(), json!(rows.len()));
    // Python 是 `round(sum(...), 2)`：没有任何账号时 `sum([])` 是 int 0，
    // 序列化成 `0`；有数时才是浮点。差一个 `.0` 也算字节差异。
    let any_ok = cny.iter().any(|r| {
        let last = r.get("last").unwrap_or(&Value::Null);
        super::util::truthy(last.get("ok").unwrap_or(&Value::Null))
    });
    o.insert(
        "total_balance".into(),
        if any_ok { super::util::fnum(round_n(total_balance, 2)) } else { json!(0) },
    );
    o.insert(
        "total_month".into(),
        if any_ok { super::util::fnum(round_n(total_month, 2)) } else { json!(0) },
    );
    o.insert("at".into(), Value::String(super::util::now_hms()));
    Value::Object(o)
}
