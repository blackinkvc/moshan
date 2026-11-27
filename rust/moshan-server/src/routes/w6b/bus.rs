// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：`moshan/agentbus.py` 的 Rust 版 —— Agent 总线（别的 AI 读写中台的唯一入口）。
//       两条 append-only 的 jsonl，一份小时桶聚合，一份按天汇总。
// 不做：不写 README（`/agent` 那条路由不在本泳道）、不做后台调度。
//
// 落盘口径全在这里定死：`json.dumps(rec, ensure_ascii=False)`（默认分隔符
// `", "` / `": "`）+ 一次 append + fsync。缩进/分隔符差一个空格，磁盘就分叉。

#![allow(dead_code)]

use super::util::{self, collapse, cut, dumps_line, int_float, now_ms, pystr, s_or, truthy};
use serde_json::{json, Map, Value};
use sha1::{Digest, Sha1};
use std::path::{Path, PathBuf};

pub const V: i64 = 1;
pub const MAX_TEXT: usize = 4000;
pub const MAX_BATCH: usize = 200;
pub const KEEP_DAYS: i64 = 400;

// ---------------------------------------------------------------- 路径

pub fn agents_dir(root: &Path) -> PathBuf {
    root.join("_工作台").join("agents")
}

pub fn usage_path(root: &Path) -> PathBuf {
    agents_dir(root).join("usage.jsonl")
}

pub fn events_path(root: &Path) -> PathBuf {
    agents_dir(root).join("events.jsonl")
}

pub fn links_path(root: &Path) -> PathBuf {
    agents_dir(root).join("links.json")
}

// ---------------------------------------------------------------- 小工具

/// `_iso(ms)`：本地 `%Y-%m-%dT%H:%M:%S`。
pub fn iso(ms: i64) -> String {
    util::iso_of_ms(ms)
}

/// `_to_ms`：epoch 秒/毫秒、数字串、五种 ISO 写法；认不出用现在。
pub fn to_ms(v: &Value) -> i64 {
    if !truthy(v) {
        return now_ms();
    }
    match v {
        Value::Number(n) => {
            let x = n.as_f64().unwrap_or(0.0);
            return if x < 1e11 { (x * 1000.0) as i64 } else { x as i64 };
        }
        Value::Bool(b) => {
            let x = if *b { 1.0 } else { 0.0 };
            return if x < 1e11 { (x * 1000.0) as i64 } else { x as i64 };
        }
        Value::String(s) => {
            let t = s.trim();
            if !t.is_empty() && t.chars().all(|c| c.is_ascii_digit() || c == '.') {
                if let Ok(x) = t.parse::<f64>() {
                    return if x < 1e11 { (x * 1000.0) as i64 } else { x as i64 };
                }
            }
            for fmt in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M", "%Y-%m-%d"] {
                // Python: s[:len(fmt)+2].strip()[:19]
                let take = fmt.chars().count() + 2;
                let head: String = t.chars().take(take).collect();
                let head: String = head.trim().chars().take(19).collect();
                if let Ok(d) = chrono::NaiveDateTime::parse_from_str(&head, fmt_chrono(fmt)) {
                    return d.and_local_timezone(chrono::Local).single().map(|x| x.timestamp_millis()).unwrap_or_else(now_ms);
                }
                if let Ok(d) = chrono::NaiveDate::parse_from_str(&head, fmt_chrono(fmt)) {
                    if let Some(dt) = d.and_hms_opt(0, 0, 0) {
                        return dt.and_local_timezone(chrono::Local).single().map(|x| x.timestamp_millis()).unwrap_or_else(now_ms);
                    }
                }
            }
            now_ms()
        }
        _ => now_ms(),
    }
}

/// chrono 的格式串与 strftime 基本一致（这些写法都通用）。
fn fmt_chrono(fmt: &str) -> &str {
    fmt
}

/// `_int`：`max(0, int(float(v)))`。
pub fn as_int(v: &Value) -> i64 {
    let n = int_float(v);
    if n > 0 {
        n
    } else {
        0
    }
}

/// `_num`：`round(float(v), 4)`，空/认不出回 None。
pub fn as_num(v: &Value) -> Option<f64> {
    if !truthy(v) {
        return None;
    }
    util::fnum(v).map(|f| util::py_round(f, 4))
}

/// `_str(v, n)`：`str(v if v is not None else "").strip()[:n]`
pub fn as_str(v: &Value, n: usize) -> String {
    let s = match v {
        Value::Null => String::new(),
        other => pystr(other),
    };
    cut(s.trim(), n)
}

/// `_fingerprint(kind, r)`：sha1 前 16 个十六进制字符。
pub fn fingerprint(kind: &str, r: &Map<String, Value>) -> String {
    let g = |k: &str| -> String {
        // Python 的 str(x) 里 None 是 "None"，缺键同样是 None
        match r.get(k) {
            Some(v) => pystr(v),
            None => "None".to_string(),
        }
    };
    let ts = match r.get("ts") {
        Some(v) => util::int0(v),
        None => 0,
    };
    let key = [
        kind.to_string(),
        g("client"),
        g("model"),
        (ts / 60000).to_string(),
        g("in"),
        g("out"),
        g("calls"),
        g("task"),
        g("title"),
        g("text"),
    ]
    .join("|");
    let mut h = Sha1::new();
    h.update(key.as_bytes());
    let d = h.finalize();
    let hex: String = d.iter().map(|b| format!("{:02x}", b)).collect();
    hex.chars().take(16).collect()
}

// ---------------------------------------------------------------- 写：用量

/// `norm_usage`：一条上报 → 落盘记录（字段不全回 None）。
pub fn norm_usage(body: &Value, src: &str) -> Option<Map<String, Value>> {
    let b = body.as_object();
    let get = |k: &str| -> Value {
        b.and_then(|m| m.get(k)).cloned().unwrap_or(Value::Null)
    };
    let or_get = |a: &str, c: &str| -> Value {
        let x = get(a);
        if truthy(&x) {
            x
        } else {
            get(c)
        }
    };
    let client = as_str(&or_get("client", "app"), 40);
    if client.is_empty() {
        return None;
    }
    let tin = as_int(&get("in"));
    let tout = as_int(&get("out"));
    let cached = as_int(&get("cached"));
    let calls = as_int(&get("calls"));
    if tin == 0 && tout == 0 && calls == 0 {
        return None;
    }
    let ts = to_ms(&{
        let a = get("ts");
        if truthy(&a) {
            a
        } else {
            let c = get("at");
            if truthy(&c) {
                c
            } else {
                get("time")
            }
        }
    });

    let mut m = Map::new();
    m.insert("v".into(), json!(V));
    m.insert("kind".into(), json!("usage"));
    m.insert("id".into(), json!(as_str(&get("id"), 64)));
    m.insert("ts".into(), json!(ts));
    m.insert("at".into(), json!(iso(ts)));
    m.insert("client".into(), json!(client));
    let model = {
        let v = get("model");
        if truthy(&v) {
            v
        } else {
            json!("未知模型")
        }
    };
    m.insert("model".into(), json!(as_str(&model, 60)));
    m.insert("in".into(), json!(tin));
    m.insert("out".into(), json!(tout));
    m.insert("cached".into(), json!(cached));
    m.insert("calls".into(), json!(if calls > 0 { calls } else { 1 }));
    m.insert(
        "cost".into(),
        match as_num(&get("cost")) {
            Some(f) => json!(f),
            None => Value::Null,
        },
    );
    let currency = {
        let v = get("currency");
        if truthy(&v) {
            v
        } else {
            json!("CNY")
        }
    };
    m.insert("currency".into(), json!(as_str(&currency, 8)));
    let task = {
        let v = or_get("task", "note");
        if truthy(&v) {
            v
        } else {
            Value::Null
        }
    };
    m.insert("task".into(), json!(as_str(&task, 120)));
    let rf = {
        let a = get("ref");
        if truthy(&a) {
            a
        } else {
            let b2 = get("session");
            if truthy(&b2) {
                b2
            } else {
                get("url")
            }
        }
    };
    m.insert("ref".into(), json!(as_str(&rf, 200)));
    let s = {
        let v = get("src");
        if truthy(&v) {
            v
        } else {
            json!(src)
        }
    };
    m.insert("src".into(), json!(as_str(&s, 40)));
    if s_or(m.get("id").unwrap_or(&Value::Null)).is_empty() {
        let fp = fingerprint("usage", &m);
        m.insert("id".into(), json!(fp));
    }
    Some(m)
}

/// `add_usage`：`{"ok","written","dup","bad","path","ids"}`
pub fn add_usage(root: &Path, body: &Value, src: &str) -> Value {
    let items: Vec<Value> = match body {
        Value::Array(a) => a.iter().take(MAX_BATCH).cloned().collect(),
        other => vec![other.clone()],
    };
    let mut recs: Vec<Map<String, Value>> = Vec::new();
    let mut bad = 0i64;
    for it in &items {
        match norm_usage(it, src) {
            Some(r) => recs.push(r),
            None => bad += 1,
        }
    }
    let p = usage_path(root);
    let mut have: Vec<String> = Vec::new();
    for x in util::read_jsonl(&p) {
        if let Some(id) = x.get("id") {
            if truthy(id) {
                have.push(pystr(id));
            }
        }
    }
    let mut fresh: Vec<Value> = Vec::new();
    let mut ids: Vec<String> = Vec::new();
    for r in &recs {
        let id = s_or(r.get("id").unwrap_or(&Value::Null));
        if have.iter().any(|h| *h == id) {
            continue;
        }
        have.push(id.clone());
        ids.push(id);
        fresh.push(Value::Object(r.clone()));
    }
    let n = util::append_jsonl(&p, &fresh);
    let mut out = Map::new();
    out.insert("ok".into(), json!(!recs.is_empty()));
    out.insert("written".into(), json!(n));
    out.insert("dup".into(), json!(recs.len().saturating_sub(n)));
    out.insert("bad".into(), json!(bad));
    out.insert("path".into(), json!(p.display().to_string()));
    out.insert("ids".into(), json!(ids));
    Value::Object(out)
}

// ---------------------------------------------------------------- 写：事件

pub const EVENT_KINDS: [&str; 6] = ["note", "metric", "task", "link", "file", "span"];

/// `norm_event`
pub fn norm_event(body: &Value, src: &str) -> Option<Map<String, Value>> {
    let b = body.as_object();
    let get = |k: &str| -> Value { b.and_then(|m| m.get(k)).cloned().unwrap_or(Value::Null) };
    let pick = |a: &str, c: &str| -> Value {
        let x = get(a);
        if truthy(&x) {
            x
        } else {
            get(c)
        }
    };
    let title = as_str(&pick("title", "task"), 120);
    let text = {
        let v = {
            let x = get("text");
            if truthy(&x) {
                x
            } else {
                let y = get("detail");
                if truthy(&y) {
                    y
                } else {
                    get("body")
                }
            }
        };
        as_str(&v, MAX_TEXT)
    };
    let data = get("data");
    if title.is_empty() && text.is_empty() && !truthy(&data) {
        return None;
    }
    let mut kind = as_str(&get("kind"), 16);
    if kind.is_empty() {
        kind = "note".to_string();
    }
    if !EVENT_KINDS.contains(&kind.as_str()) {
        kind = "note".to_string();
    }
    let ts = to_ms(&{
        let a = get("ts");
        if truthy(&a) {
            a
        } else {
            let c = get("at");
            if truthy(&c) {
                c
            } else {
                get("time")
            }
        }
    });

    let mut m = Map::new();
    m.insert("v".into(), json!(V));
    m.insert("kind".into(), json!("event"));
    m.insert("id".into(), json!(as_str(&get("id"), 64)));
    m.insert("ts".into(), json!(ts));
    m.insert("at".into(), json!(iso(ts)));
    let ns = {
        let v = pick("ns", "space");
        if truthy(&v) {
            v
        } else {
            json!("杂项")
        }
    };
    m.insert("ns".into(), json!(as_str(&ns, 40)));
    m.insert("type".into(), json!(kind));
    m.insert("title".into(), json!(title));
    m.insert("text".into(), json!(text));
    m.insert(
        "data".into(),
        match &data {
            Value::Object(_) | Value::Array(_) => data.clone(),
            _ => Value::Null,
        },
    );
    let s = {
        let v = get("src");
        if truthy(&v) {
            v
        } else {
            json!(src)
        }
    };
    m.insert("src".into(), json!(as_str(&s, 40)));
    let tags: Vec<Value> = match b.and_then(|x| x.get("tags")) {
        Some(Value::Array(a)) => a
            .iter()
            .filter(|t| !pystr(t).trim().is_empty())
            .map(|t| json!(as_str(t, 24)))
            .take(8)
            .collect(),
        _ => Vec::new(),
    };
    m.insert("tags".into(), json!(tags));
    let rf = pick("ref", "url");
    m.insert("ref".into(), json!(as_str(&rf, 200)));
    if s_or(m.get("id").unwrap_or(&Value::Null)).is_empty() {
        let fp = fingerprint("event", &m);
        m.insert("id".into(), json!(fp));
    }
    Some(m)
}

/// `add_event`
pub fn add_event(root: &Path, body: &Value, src: &str) -> Value {
    let items: Vec<Value> = match body {
        Value::Array(a) => a.iter().take(MAX_BATCH).cloned().collect(),
        other => vec![other.clone()],
    };
    let mut recs: Vec<Map<String, Value>> = Vec::new();
    let mut bad = 0i64;
    for it in &items {
        match norm_event(it, src) {
            Some(r) => recs.push(r),
            None => bad += 1,
        }
    }
    let p = events_path(root);
    let mut have: Vec<String> = Vec::new();
    for x in util::read_jsonl(&p) {
        if let Some(id) = x.get("id") {
            if truthy(id) {
                have.push(pystr(id));
            }
        }
    }
    let mut fresh: Vec<Value> = Vec::new();
    let mut ids: Vec<String> = Vec::new();
    for r in &recs {
        let id = s_or(r.get("id").unwrap_or(&Value::Null));
        if have.iter().any(|h| *h == id) {
            continue;
        }
        have.push(id.clone());
        ids.push(id);
        fresh.push(Value::Object(r.clone()));
    }
    let n = util::append_jsonl(&p, &fresh);
    let mut out = Map::new();
    out.insert("ok".into(), json!(!recs.is_empty()));
    out.insert("written".into(), json!(n));
    out.insert("dup".into(), json!(recs.len().saturating_sub(n)));
    out.insert("bad".into(), json!(bad));
    out.insert("path".into(), json!(p.display().to_string()));
    out.insert("ids".into(), json!(ids));
    Value::Object(out)
}

// ---------------------------------------------------------------- 读

/// `_since_ms`：`7d` / `24h` / `today` / ISO / epoch → 毫秒起点；空或 all = 0。
pub fn since_ms(v: &str) -> i64 {
    let s = v.trim().to_lowercase();
    if s.is_empty() || s == "all" || s == "0" {
        return 0;
    }
    if let Some((n, unit)) = split_dur(&s) {
        let secs = match unit {
            'd' => 86400,
            'h' => 3600,
            _ => 60,
        };
        return now_ms() - n * secs * 1000;
    }
    if s == "today" {
        return today_start_ms();
    }
    if s == "yesterday" || s == "昨天" {
        return today_start_ms() - 86400 * 1000;
    }
    to_ms(&json!(v))
}

fn split_dur(s: &str) -> Option<(i64, char)> {
    let bytes = s.as_bytes();
    if bytes.len() < 2 {
        return None;
    }
    let unit = *bytes.last().unwrap() as char;
    if !"dhm".contains(unit) {
        return None;
    }
    let num = &s[..s.len() - 1];
    if num.is_empty() || !num.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    // 允许数字与单位之间有空白
    let num = num.trim();
    num.parse::<i64>().ok().map(|n| (n, unit))
}

fn today_start_ms() -> i64 {
    let now = chrono::Local::now();
    let d = now.date_naive();
    d.and_hms_opt(0, 0, 0)
        .and_then(|x| x.and_local_timezone(chrono::Local).single())
        .map(|x| x.timestamp_millis())
        .unwrap_or(0)
}

/// `read()`：读自报账。
pub fn read(root: &Path, ns: &str, since: &str, limit: i64, q: &str, client: &str) -> Value {
    let since_ms_v = since_ms(since);
    let want = if ns.trim().is_empty() {
        "all".to_string()
    } else {
        ns.trim().to_lowercase()
    };
    let lim = if limit < 1 {
        1
    } else if limit > 1000 {
        1000
    } else {
        limit
    };
    let mut out: Vec<Value> = Vec::new();
    if want == "all" || want == "usage" || want == "用量" {
        out.extend(util::read_jsonl(&usage_path(root)));
    }
    if want == "all" || want == "events" || want == "event" || want == "事件" {
        out.extend(util::read_jsonl(&events_path(root)));
    }
    if since_ms_v != 0 {
        out.retain(|x| util::int0(x.get("ts").unwrap_or(&Value::Null)) >= since_ms_v);
    }
    if !client.is_empty() {
        out.retain(|x| {
            let c = x.get("client").cloned().unwrap_or(Value::Null);
            let c = if truthy(&c) { c } else { x.get("src").cloned().unwrap_or(Value::Null) };
            s_or(&c) == client
        });
    }
    if !q.is_empty() {
        let ql = q.to_lowercase();
        out.retain(|x| dumps_line(x).to_lowercase().contains(&ql));
    }
    // Python 的 sort 稳定：同 ts 保持「先 usage 后 events」的读取顺序。
    out.sort_by(|a, b| {
        let ta = util::int0(a.get("ts").unwrap_or(&Value::Null));
        let tb = util::int0(b.get("ts").unwrap_or(&Value::Null));
        tb.cmp(&ta)
    });
    let n = out.len();
    let items: Vec<Value> = out.into_iter().take(lim as usize).collect();
    let mut files = Map::new();
    files.insert("usage".into(), json!(usage_path(root).display().to_string()));
    files.insert("events".into(), json!(events_path(root).display().to_string()));
    let mut o = Map::new();
    o.insert("ok".into(), json!(true));
    o.insert("n".into(), json!(n));
    o.insert("items".into(), json!(items));
    o.insert(
        "since".into(),
        json!(if since_ms_v != 0 { iso(since_ms_v) } else { "all".to_string() }),
    );
    o.insert("files".into(), Value::Object(files));
    Value::Object(o)
}

// ---------------------------------------------------------------- 统计

/// `by_day(root, days)`：按天汇总。
pub fn by_day(root: &Path, days: i64) -> Vec<(String, Value)> {
    let since = if days > 0 { now_ms() - days * 86400 * 1000 } else { 0 };
    let mut order: Vec<String> = Vec::new();
    let mut map: std::collections::HashMap<String, Map<String, Value>> = Default::default();
    for d in util::read_jsonl(&usage_path(root)) {
        let ts = util::int0(d.get("ts").unwrap_or(&Value::Null));
        if since != 0 && ts < since {
            continue;
        }
        let day: String = iso(ts).chars().take(10).collect();
        if !map.contains_key(&day) {
            order.push(day.clone());
            let mut b = Map::new();
            b.insert("in".into(), json!(0));
            b.insert("out".into(), json!(0));
            b.insert("cached".into(), json!(0));
            b.insert("calls".into(), json!(0));
            b.insert("models".into(), Value::Object(Map::new()));
            map.insert(day.clone(), b);
        }
        let b = map.get_mut(&day).unwrap();
        let tin = as_int(d.get("in").unwrap_or(&Value::Null));
        let tout = as_int(d.get("out").unwrap_or(&Value::Null));
        let cached = as_int(d.get("cached").unwrap_or(&Value::Null));
        let calls = {
            let c = as_int(d.get("calls").unwrap_or(&Value::Null));
            if c == 0 {
                1
            } else {
                c
            }
        };
        bump(b, "in", tin);
        bump(b, "out", tout);
        bump(b, "cached", cached);
        bump(b, "calls", calls);
        let model = {
            let m = d.get("model").cloned().unwrap_or(Value::Null);
            if truthy(&m) {
                pystr(&m)
            } else {
                "未知模型".to_string()
            }
        };
        let models = b.get_mut("models").unwrap().as_object_mut().unwrap();
        if !models.contains_key(&model) {
            let mut m = Map::new();
            m.insert("in".into(), json!(0));
            m.insert("out".into(), json!(0));
            m.insert("cached".into(), json!(0));
            m.insert("n".into(), json!(0));
            m.insert("cost".into(), Value::Null);
            models.insert(model.clone(), Value::Object(m));
        }
        let m = models.get_mut(&model).unwrap().as_object_mut().unwrap();
        bump(m, "in", tin);
        bump(m, "out", tout);
        bump(m, "cached", cached);
        bump(m, "n", calls);
        if let Some(c) = as_num(d.get("cost").unwrap_or(&Value::Null)) {
            let prev = m.get("cost").and_then(|x| x.as_f64()).unwrap_or(0.0);
            m.insert("cost".into(), json!(util::py_round(prev + c, 4)));
        }
    }
    order
        .into_iter()
        .map(|k| {
            let v = map.remove(&k).unwrap();
            (k, Value::Object(v))
        })
        .collect()
}

fn bump(m: &mut Map<String, Value>, k: &str, n: i64) {
    let cur = m.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
    m.insert(k.to_string(), json!(cur + n));
}

/// `totals(root, days)`：`{"in","out","cached","calls","total","days"}`
pub fn totals(root: &Path, days: i64) -> Value {
    let rows = by_day(root, days);
    let mut t_in = 0i64;
    let mut t_out = 0i64;
    let mut t_cached = 0i64;
    let mut t_calls = 0i64;
    for (_, b) in &rows {
        t_in += b.get("in").and_then(|x| x.as_i64()).unwrap_or(0);
        t_out += b.get("out").and_then(|x| x.as_i64()).unwrap_or(0);
        t_cached += b.get("cached").and_then(|x| x.as_i64()).unwrap_or(0);
        t_calls += b.get("calls").and_then(|x| x.as_i64()).unwrap_or(0);
    }
    let mut o = Map::new();
    o.insert("in".into(), json!(t_in));
    o.insert("out".into(), json!(t_out));
    o.insert("cached".into(), json!(t_cached));
    o.insert("calls".into(), json!(t_calls));
    o.insert("total".into(), json!(t_in + t_out));
    o.insert("days".into(), json!(rows.len()));
    Value::Object(o)
}

/// `hour_buckets(root, days)`：usage.jsonl → `"YYYY-MM-DDTH"` 桶。
/// Python 侧按文件增量读；进程内首次调用等价于整份读一遍，这里就整份读
/// （值相同，少一套缓存状态）。
pub fn hour_buckets(root: &Path, days: i64) -> Map<String, Value> {
    let p = usage_path(root);
    let text = match std::fs::read(&p) {
        Ok(b) => String::from_utf8_lossy(&b).to_string(),
        Err(_) => return Map::new(),
    };
    let mut hours: Map<String, Value> = Map::new();
    for raw in text.split('\n') {
        let s = raw.trim();
        if s.is_empty() {
            continue;
        }
        let Ok(d) = serde_json::from_str::<Value>(s) else { continue };
        if !d.is_object() {
            continue;
        }
        let ts = util::int0(d.get("ts").unwrap_or(&Value::Null));
        if ts == 0 {
            continue;
        }
        let k: String = util::strftime_ms(ts, "%Y-%m-%dT%H");
        let tin = as_int(d.get("in").unwrap_or(&Value::Null));
        let tout = as_int(d.get("out").unwrap_or(&Value::Null));
        let cached = as_int(d.get("cached").unwrap_or(&Value::Null));
        let calls = {
            let c = as_int(d.get("calls").unwrap_or(&Value::Null));
            if c == 0 {
                1
            } else {
                c
            }
        };
        let model = {
            let m = d.get("model").cloned().unwrap_or(Value::Null);
            let s = if truthy(&m) { pystr(&m) } else { "未知模型".into() };
            cut(&s, 60)
        };
        let app = {
            let a = d.get("client").cloned().unwrap_or(Value::Null);
            let s = if truthy(&a) { pystr(&a) } else { "agent".into() };
            cut(&s, 24)
        };
        let cur = hours.entry(k).or_insert_with(|| {
            let mut b = Map::new();
            b.insert("in".into(), json!(0));
            b.insert("out".into(), json!(0));
            b.insert("cached".into(), json!(0));
            b.insert("calls".into(), json!(0));
            b.insert("models".into(), Value::Object(Map::new()));
            b.insert("apps".into(), Value::Object(Map::new()));
            Value::Object(b)
        });
        let b = cur.as_object_mut().unwrap();
        bump(b, "in", tin);
        bump(b, "out", tout);
        bump(b, "cached", cached);
        bump(b, "calls", calls);
        for dim in ["models", "apps"] {
            let name = if dim == "models" { model.clone() } else { app.clone() };
            let m = b.get_mut(dim).unwrap().as_object_mut().unwrap();
            let e = m.entry(name).or_insert_with(|| {
                let mut x = Map::new();
                x.insert("in".into(), json!(0));
                x.insert("out".into(), json!(0));
                x.insert("cached".into(), json!(0));
                x.insert("calls".into(), json!(0));
                Value::Object(x)
            });
            let e = e.as_object_mut().unwrap();
            bump(e, "in", tin);
            bump(e, "out", tout);
            bump(e, "cached", cached);
            bump(e, "calls", calls);
        }
    }
    // 老桶扔掉：只留最近 days 天
    let keep: Vec<String> = (0..days)
        .map(|i| {
            let d = chrono::Local::now().date_naive() - chrono::Duration::days(i);
            d.format("%Y-%m-%d").to_string()
        })
        .collect();
    hours.retain(|k, _| {
        let day: String = k.chars().take(10).collect();
        keep.iter().any(|x| *x == day)
    });
    hours
}

/// 便于别处拼 `{"ok": true, ...}`：把 map 转成 Value。
pub fn vobj(m: Map<String, Value>) -> Value {
    Value::Object(m)
}

/// `" ".join(...)` 的别名，别处读起来顺一点。
pub fn join_ws(s: &str) -> String {
    collapse(s)
}
