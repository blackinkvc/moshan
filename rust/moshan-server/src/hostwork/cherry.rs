// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：Cherry Studio 的用量账本（ai_usage_record）→ 人民币。对齐 cherry_usage.py。
// 不做：不做账号余额（那在 billing），也不碰 WorkBuddy 的积分账。
//
// 为什么不用 Cherry 自己记的 cost：那是**美元**单价，比百炼的人民币价差一到两倍；
// 只有价表里没有的模型才退回它自算的美元值（按 USD_CNY 折），并标注清楚。
#![allow(dead_code)]

use super::platform;
use super::util::{fnum, py_double, py_int, round_n};
use crate::paths::Ctx;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

pub const USD_CNY: f64 = 7.10;
const BUSY_FROM: i64 = 8;
const BUSY_TO: i64 = 22;

/// 价表：元/百万 token，`(闲时, 忙时)`。
fn prices() -> &'static [(&'static str, [(f64, f64); 3])] {
    &[
        ("deepseek-v4-flash-0731", [(1.5, 3.0), (0.15, 0.3), (4.5, 9.0)]),
        ("deepseek-v4.1-flash", [(1.0, 2.0), (0.02, 0.04), (4.0, 8.0)]),
        ("deepseek-v4.1-flash-1017", [(1.0, 2.0), (0.02, 0.04), (4.0, 8.0)]),
        ("deepseek-v4-pro-0813", [(4.5, 9.0), (0.45, 0.9), (13.5, 27.0)]),
        ("deepseek-v4-pro", [(12.0, 12.0), (1.2, 1.2), (24.0, 24.0)]),
        ("qwen3.5-flash", [(0.2, 0.2), (0.02, 0.02), (2.0, 2.0)]),
        ("qwen3.5-plus", [(0.8, 0.8), (0.08, 0.08), (8.0, 8.0)]),
        ("qwen3.7-flash", [(0.2, 0.2), (0.04, 0.04), (0.8, 0.8)]),
        ("qwen3.8-flash", [(0.8, 0.8), (0.1, 0.1), (2.7, 2.7)]),
        ("deepseek-ai/deepseek-v3.2", [(4.0, 4.0), (0.4, 0.4), (6.0, 6.0)]),
        ("deepseek-ai/deepseek-v4-flash", [(1.5, 3.0), (0.15, 0.3), (4.5, 9.0)]),
        ("deepseek-ai/deepseek-v4.1-flash", [(1.0, 2.0), (0.02, 0.04), (4.0, 8.0)]),
    ]
}

fn vendor_of(pid: &str) -> &'static str {
    match pid {
        "dashscope" => "阿里百炼",
        "silicon" => "硅基流动",
        _ => "",
    }
}

fn is_local(pid: &str) -> bool {
    matches!(
        pid,
        "lmstudio" | "ollama" | "cherryai" | "72a4aecc-4dc3-43f2-82b2-82c7b4b73915"
    )
}

/// `_price_for(model_id, model_name)`：先精确、再小写、最后子串包含。
fn price_for(model_id: &str, model_name: &str) -> Option<[(f64, f64); 3]> {
    let list = prices();
    for key in [model_id, model_name] {
        if let Some((_, p)) = list.iter().find(|(k, _)| *k == key) {
            return Some(*p);
        }
    }
    let low_id = model_id.to_lowercase();
    let low_name = model_name.to_lowercase();
    for key in [low_id.as_str(), low_name.as_str()] {
        if let Some((_, p)) = list.iter().find(|(k, _)| *k == key) {
            return Some(*p);
        }
    }
    let low = if model_id.is_empty() { model_name } else { model_id }.to_lowercase();
    for (k, p) in list {
        if !k.is_empty() && low.contains(k) {
            return Some(*p);
        }
    }
    None
}

pub fn db_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    Path::new(&home)
        .join("Library")
        .join("Application Support")
        .join("CherryStudio")
        .join("Data")
        .join("cherrystudio.sqlite")
}

/// `_sig(path) = (st_mtime_ns, st_size)`。
fn sig_of(p: &Path) -> (i64, i64) {
    match std::fs::metadata(p) {
        Ok(m) => {
            let ns = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0);
            (ns, m.len() as i64)
        }
        Err(_) => (0, 0),
    }
}

fn hour_of_ms(ms: i64) -> i64 {
    use chrono::{Local, TimeZone};
    match Local.timestamp_millis_opt(ms) {
        chrono::LocalResult::Single(t) => {
            use chrono::Timelike;
            t.hour() as i64
        }
        _ => 0,
    }
}

fn ym_of_ms(ms: i64) -> String {
    match ms_str(ms) {
        s if s.len() >= 7 => s[..7].to_string(),
        _ => String::new(),
    }
}

fn ymd_of_ms(ms: i64) -> String {
    ms_str(ms)
}

fn ms_str(ms: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_millis_opt(ms) {
        chrono::LocalResult::Single(t) => t.format("%Y-%m-%d").to_string(),
        _ => String::new(),
    }
}

fn now_ym_list() -> Vec<String> {
    use chrono::{Datelike, Duration as CD, Local};
    let mut d = Local::now().date_naive();
    let mut out = Vec::new();
    for _ in 0..4 {
        out.push(format!("{:04}-{:02}", d.year(), d.month()));
        let first = d.with_day(1).unwrap_or(d);
        d = first - CD::days(1);
    }
    out
}

/// 按月、按供应商的账（billing 对账要用，跟 range 无关）。
#[derive(Default, Clone)]
struct MV {
    cny: f64,
    calls: i64,
    ti: i64,
    to: i64,
    tc: i64,
}

fn mv_obj(v: &MV) -> Value {
    let mut m = Map::new();
    m.insert("cny".into(), fnum(round_n(v.cny, 2)));
    m.insert("calls".into(), json!(v.calls));
    m.insert("in".into(), json!(v.ti));
    m.insert("out".into(), json!(v.to));
    m.insert("cache".into(), json!(v.tc));
    Value::Object(m)
}

fn month_obj(ym: &str, by: &[(String, MV)]) -> Value {
    let sum: f64 = by.iter().map(|(_, v)| v.cny).sum();
    let mut bv = Map::new();
    for (k, v) in by {
        bv.insert(k.clone(), mv_obj(v));
    }
    let mut m = Map::new();
    m.insert("ym".into(), Value::String(ym.to_string()));
    // Python 是 `round(sum(...), 2)`，空月份时 `sum([])` 是 **int 0**，序列化成 `0`
    // 而不是 `-0.0`。差一个字节也算差异，所以空月份单独走整数。
    m.insert(
        "cny".into(),
        if by.is_empty() { json!(0) } else { fnum(round_n(sum, 2)) },
    );
    m.insert("by_vendor".into(), Value::Object(bv));
    Value::Object(m)
}

fn not_found(db: &Path) -> Value {
    let mut m = Map::new();
    m.insert("found".into(), json!(false));
    m.insert("db".into(), Value::String(db.to_string_lossy().to_string()));
    m.insert("reason".into(), Value::String("这台机器上没装 Cherry Studio".into()));
    m.insert("cny".into(), fnum(0.0));
    m.insert("calls".into(), json!(0));
    m.insert("models".into(), json!([]));
    m.insert("by_day".into(), json!([]));
    m.insert("unpriced".into(), json!([]));
    Value::Object(m)
}

struct ModelRow {
    key: (String, String),
    provider: String,
    vendor_id: String,
    model: String,
    model_id: String,
    n: i64,
    ti: i64,
    to: i64,
    tc: i64,
    cny: f64,
    cny_cherry: f64,
    priced: bool,
    busy_n: i64,
    hit: Option<f64>,
}

fn load(days: i64, db: &Path) -> Value {
    if !db.exists() {
        return not_found(db);
    }
    let sql = "SELECT provider_id, provider_name, model_id, model_name, input_tokens,\
 output_tokens, cache_read_tokens, cost, created_at FROM ai_usage_record";
    let rows = match platform::sqlite_rows(db, sql, Duration::from_secs(4)) {
        Some(r) => r,
        None => Vec::new(),
    };
    let now_ms = {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    };
    let lo: i64 = if days != 0 { now_ms - days * 86400 * 1000 } else { 0 };

    let ym_list = now_ym_list();
    let cur_ym = ym_list[0].clone();
    // 每月的 (pid → MV)，顺序按首次出现
    let mut months: Vec<(String, Vec<(String, MV)>)> =
        ym_list.iter().map(|y| (y.clone(), Vec::new())).collect();

    let mut models: Vec<ModelRow> = Vec::new();
    let mut by_day: Vec<(String, f64, i64)> = Vec::new();
    let mut unpriced: Vec<(String, i64)> = Vec::new();
    let mut tot_cny = 0.0f64;
    let mut tot_cherry = 0.0f64;
    let mut t_in = 0i64;
    let mut t_out = 0i64;
    let mut t_cache = 0i64;
    let mut n = 0i64;
    let mut all_cny = 0.0f64;
    let mut all_calls = 0i64;
    let mut first_ts = 0i64;
    let mut last_ts = 0i64;

    for r in &rows {
        let pid = r.get("provider_id").map(|v| super::util::str_or(Some(v))).unwrap_or_default();
        if is_local(&pid) {
            continue;
        }
        let pname = r.get("provider_name").map(|v| super::util::str_or(Some(v))).unwrap_or_default();
        let mid = r.get("model_id").map(|v| super::util::str_or(Some(v))).unwrap_or_default();
        let mname = r.get("model_name").map(|v| super::util::str_or(Some(v))).unwrap_or_default();
        let ti = py_int(r.get("input_tokens").unwrap_or(&Value::Null));
        let to = py_int(r.get("output_tokens").unwrap_or(&Value::Null));
        let tc = py_int(r.get("cache_read_tokens").unwrap_or(&Value::Null));
        if ti == 0 && to == 0 {
            continue;
        }
        let cost_v = r.get("cost").cloned().unwrap_or(Value::Null);
        let ts = py_int(r.get("created_at").unwrap_or(&Value::Null));

        let pr = price_for(&mid, &mname);
        let busy = (BUSY_FROM..BUSY_TO).contains(&hour_of_ms(ts));
        let k = if busy { 1 } else { 0 };
        let unc = (ti - tc).max(0);
        let mut cny: Option<f64> = None;
        if let Some(p) = pr {
            let pick = |idx: usize| if k == 1 { p[idx].1 } else { p[idx].0 };
            cny = Some((unc as f64 * pick(0) + tc as f64 * pick(1) + to as f64 * pick(2)) / 1e6);
        }
        let cherry: Option<f64> = match &cost_v {
            Value::Null => None,
            other => super::util::py_float(other).map(|x| x * USD_CNY),
        };
        let cny = match cny {
            Some(c) => c,
            None => match cherry {
                None => {
                    let name = if mname.is_empty() { mid.clone() } else { mname.clone() };
                    match unpriced.iter_mut().find(|(k, _)| *k == name) {
                        Some((_, v)) => *v += ti + to,
                        None => unpriced.push((name, ti + to)),
                    }
                    continue;
                }
                Some(c) => c,
            },
        };

        all_cny += cny;
        all_calls += 1;
        if ts != 0 && (first_ts == 0 || ts < first_ts) {
            first_ts = ts;
        }
        if ts != 0 && ts > last_ts {
            last_ts = ts;
        }
        if ts != 0 {
            let ym = ym_of_ms(ts);
            if let Some((_, bucket)) = months.iter_mut().find(|(y, _)| *y == ym) {
                let pos = match bucket.iter().position(|(k, _)| *k == pid) {
                    Some(i) => i,
                    None => {
                        bucket.push((pid.clone(), MV::default()));
                        bucket.len() - 1
                    }
                };
                let mv = &mut bucket[pos].1;
                mv.cny += cny;
                mv.calls += 1;
                mv.ti += ti;
                mv.to += to;
                mv.tc += tc;
            }
        }
        if lo != 0 && ts < lo {
            continue;
        }

        let mkey = (pid.clone(), if mid.is_empty() { if mname.is_empty() { "?".into() } else { mname.clone() } } else { mid.clone() });
        let idx = match models.iter().position(|m| m.key == mkey) {
            Some(i) => i,
            None => {
                let vendor = {
                    let v = vendor_of(&pid);
                    if v.is_empty() { if !pname.is_empty() { pname.clone() } else if !pid.is_empty() { pid.clone() } else { "?".into() } } else { v.to_string() }
                };
                models.push(ModelRow {
                    key: mkey.clone(),
                    provider: vendor,
                    vendor_id: pid.clone(),
                    model: if mname.is_empty() { if mid.is_empty() { "?".into() } else { mid.clone() } } else { mname.clone() },
                    model_id: mid.clone(),
                    n: 0,
                    ti: 0,
                    to: 0,
                    tc: 0,
                    cny: 0.0,
                    cny_cherry: 0.0,
                    priced: pr.is_some(),
                    busy_n: 0,
                    hit: None,
                });
                models.len() - 1
            }
        };
        let m = &mut models[idx];
        m.n += 1;
        m.ti += ti;
        m.to += to;
        m.tc += tc;
        m.cny += cny;
        if let Some(c) = cherry {
            m.cny_cherry += c;
        }
        if busy {
            m.busy_n += 1;
        }

        let d = ymd_of_ms(ts);
        match by_day.iter_mut().find(|(k, _, _)| *k == d) {
            Some((_, c, cnt)) => {
                *c += cny;
                *cnt += 1;
            }
            None => by_day.push((d, cny, 1)),
        }

        tot_cny += cny;
        tot_cherry += cherry.unwrap_or(0.0);
        t_in += ti;
        t_out += to;
        t_cache += tc;
        n += 1;
    }

    // 按金额降序（Python sorted 稳定）
    let mut order: Vec<usize> = (0..models.len()).collect();
    order.sort_by(|&a, &b| models[b].cny.partial_cmp(&models[a].cny).unwrap_or(std::cmp::Ordering::Equal));
    let out_models: Vec<Value> = order
        .iter()
        .map(|&i| {
            let m = &models[i];
            let hit = if m.ti != 0 {
                Some(round_n(m.tc as f64 / m.ti as f64, 3))
            } else {
                None
            };
            let mut o = Map::new();
            o.insert("provider".into(), Value::String(m.provider.clone()));
            o.insert("vendor_id".into(), Value::String(m.vendor_id.clone()));
            o.insert("model".into(), Value::String(m.model.clone()));
            o.insert("model_id".into(), Value::String(m.model_id.clone()));
            o.insert("n".into(), json!(m.n));
            o.insert("in".into(), json!(m.ti));
            o.insert("out".into(), json!(m.to));
            o.insert("cache".into(), json!(m.tc));
            o.insert("cny".into(), fnum(round_n(m.cny, 2)));
            o.insert("cny_cherry".into(), fnum(round_n(m.cny_cherry, 2)));
            o.insert("priced".into(), json!(m.priced));
            o.insert("busy_n".into(), json!(m.busy_n));
            o.insert("hit".into(), match hit { Some(h) => fnum(h), None => Value::Null });
            Value::Object(o)
        })
        .collect();

    // by_day 按日期升序
    by_day.sort_by(|a, b| a.0.cmp(&b.0));
    let by_day_out: Vec<Value> = by_day
        .iter()
        .map(|(d, c, n)| {
            let mut o = Map::new();
            o.insert("d".into(), Value::String(d.clone()));
            o.insert("cny".into(), fnum(*c));
            o.insert("n".into(), json!(*n));
            Value::Object(o)
        })
        .collect();

    // unpriced 按 token 降序（稳定）
    let mut uorder: Vec<usize> = (0..unpriced.len()).collect();
    uorder.sort_by(|&a, &b| unpriced[b].1.cmp(&unpriced[a].1));
    let unpriced_out: Vec<Value> = uorder
        .iter()
        .take(6)
        .map(|&i| json!([unpriced[i].0, unpriced[i].1]))
        .collect();

    let vendor_n = {
        let mut v: Vec<&str> = Vec::new();
        for m in &models {
            if !v.contains(&m.vendor_id.as_str()) {
                v.push(&m.vendor_id);
            }
        }
        v.len()
    };

    let mut out = Map::new();
    out.insert("found".into(), json!(true));
    out.insert("db".into(), Value::String(db.to_string_lossy().to_string()));
    out.insert("days".into(), json!(days));
    out.insert("calls".into(), json!(n));
    let mut tok = Map::new();
    tok.insert("in".into(), json!(t_in));
    tok.insert("out".into(), json!(t_out));
    tok.insert("cache".into(), json!(t_cache));
    tok.insert("uncached".into(), json!((t_in - t_cache).max(0)));
    out.insert("tokens".into(), Value::Object(tok));
    out.insert("cny".into(), fnum(round_n(tot_cny, 2)));
    out.insert("cny_cherry".into(), fnum(round_n(tot_cherry, 2)));
    out.insert("usd_cny".into(), fnum(USD_CNY));
    out.insert("models".into(), Value::Array(out_models));
    out.insert("by_day".into(), Value::Array(by_day_out));
    out.insert("unpriced".into(), Value::Array(unpriced_out));
    out.insert("busy_from".into(), json!(BUSY_FROM));
    out.insert("busy_to".into(), json!(BUSY_TO));
    out.insert("vendor_n".into(), json!(vendor_n));
    let mut all = Map::new();
    all.insert("cny".into(), fnum(round_n(all_cny, 2)));
    all.insert("calls".into(), json!(all_calls));
    all.insert("since".into(), Value::String(if first_ts != 0 { ymd_of_ms(first_ts) } else { String::new() }));
    all.insert("until".into(), Value::String(if last_ts != 0 { ymd_of_ms(last_ts) } else { String::new() }));
    out.insert("all".into(), Value::Object(all));
    out.insert(
        "months".into(),
        Value::Array(months.iter().map(|(y, by)| month_obj(y, by)).collect()),
    );
    if let Some((_, by)) = months.iter().find(|(y, _)| *y == cur_ym) {
        out.insert("month".into(), month_obj(&cur_ym, by));
    } else {
        out.insert("month".into(), month_obj(&cur_ym, &[]));
    }
    let prev_ym = ym_list[1].clone();
    if let Some((_, by)) = months.iter().find(|(y, _)| *y == prev_ym) {
        out.insert("prev".into(), month_obj(&prev_ym, by));
    } else {
        out.insert("prev".into(), month_obj(&prev_ym, &[]));
    }
    out.insert("at".into(), Value::String(crate::hostwork::util::now_hms()));
    Value::Object(out)
}

// 20 秒缓存，键 = (库文件签名, days)
static CACHE: Mutex<Option<(f64, (i64, i64, i64), Value)>> = Mutex::new(None);

/// `view(days=7)`：days=0 表示全部。
pub fn view(days: i64) -> Value {
    let db = db_path();
    let sig = sig_of(&db);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    {
        let g = CACHE.lock().unwrap();
        if let Some((at, s, data)) = g.as_ref() {
            if *s == (sig.0, sig.1, days) && now - *at < 20.0 {
                return data.clone();
            }
        }
    }
    let data = match std::panic::catch_unwind(|| load(days, &db)) {
        Ok(v) => v,
        Err(_) => {
            let mut m = Map::new();
            m.insert("found".into(), json!(db.exists()));
            m.insert("db".into(), Value::String(db.to_string_lossy().to_string()));
            m.insert("days".into(), json!(days));
            m.insert("calls".into(), json!(0));
            m.insert("tokens".into(), json!({"in": 0, "out": 0, "cache": 0, "uncached": 0}));
            m.insert("cny".into(), fnum(0.0));
            m.insert("cny_cherry".into(), fnum(0.0));
            m.insert("models".into(), json!([]));
            m.insert("by_day".into(), json!([]));
            m.insert("unpriced".into(), json!([]));
            Value::Object(m)
        }
    };
    let mut g = CACHE.lock().unwrap();
    *g = Some((now, (sig.0, sig.1, days), data.clone()));
    data
}

/// `_billing_est(cherry)`：把「按月、按供应商」的估算折成 billing 要的形状。
pub fn billing_est(cherry: &Value) -> Value {
    let mut est = Map::new();
    for m in cherry.get("months").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
        let ym = super::util::str_or(m.get("ym"));
        if let Some(bv) = m.get("by_vendor").and_then(|v| v.as_object()) {
            for (k, v) in bv {
                let e = est
                    .entry(k.clone())
                    .or_insert_with(|| json!({"by_ym": {}}));
                if let Some(by_ym) = e.get_mut("by_ym").and_then(|x| x.as_object_mut()) {
                    let mut row = Map::new();
                    row.insert("cny".into(), v.get("cny").cloned().unwrap_or(json!(0)));
                    row.insert("calls".into(), v.get("calls").cloned().unwrap_or(json!(0)));
                    by_ym.insert(ym.clone(), Value::Object(row));
                }
            }
        }
    }
    Value::Object(est)
}

pub fn _unused_ctx(_ctx: &Ctx) {}

#[allow(dead_code)]
fn _num(v: &Value) -> Option<f64> {
    if v.is_null() {
        return None;
    }
    Some(round_n(py_double(v), 2))
}
