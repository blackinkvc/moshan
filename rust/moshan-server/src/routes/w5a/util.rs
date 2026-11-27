// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：w5a 泳道内部的 Python 口径工具（真值、str、round、时间、jsonl 尾部、截断）。
// 不做：不做业务判断，也不往外暴露 —— 这些语义差一点，十几个采集器会同时错。
//
// 为什么自己写：Python 的 round 走二进制真值四舍六入五成双、str(True) 是 "True"、
// 按字符截断而不是字节、`datetime.fromtimestamp` 认本地时区。这些差异一出现
// 就在字节层面，靠肉眼永远查不出来。
#![allow(dead_code)]

use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
use serde_json::{json, Map, Value};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Python 真值：null / false / 0 / "" / [] / {} 为假。
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|x| x != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// `str(v)`（Python 口径，None→"None"、True→"True"、1299.0→"1299.0"）。
pub fn py_str(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        other => crate::jsonio::dumps(other, 0),
    }
}

/// `str(v or "")`：假值一律空串。
pub fn str_or(v: Option<&Value>) -> String {
    match v {
        Some(x) if truthy(x) => py_str(x),
        _ => String::new(),
    }
}

/// `v or ""`（保留原类型）。
pub fn or_empty(v: Option<&Value>) -> Value {
    match v {
        Some(x) if truthy(x) => x.clone(),
        _ => Value::String(String::new()),
    }
}

/// Python `float(v)`：数字 / 数字串 / "inf" / "nan"。布尔按 1/0。
pub fn py_float(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                return None;
            }
            t.parse::<f64>().ok()
        }
        _ => None,
    }
}

/// Python `int(v)` 的宽松口径（字符串先 strip 再解析）。
pub fn int_of(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n.as_i64().unwrap_or_else(|| n.as_f64().unwrap_or(0.0) as i64),
        Some(Value::Bool(b)) => *b as i64,
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

/// `round(x, n)`：按二进制真值舍入（Rust 的 `{:.n}` 与 Python 同口径），再解析回 f64。
pub fn round_n(x: f64, n: usize) -> f64 {
    if !x.is_finite() {
        return x;
    }
    let s = match n {
        0 => format!("{:.0}", x),
        1 => format!("{:.1}", x),
        2 => format!("{:.2}", x),
        3 => format!("{:.3}", x),
        _ => format!("{:.1}", x),
    };
    s.parse().unwrap_or(x)
}

pub fn round1(x: f64) -> f64 {
    round_n(x, 1)
}

pub fn round2(x: f64) -> f64 {
    round_n(x, 2)
}

/// 数字进 JSON：整数也按浮点写（`json.dumps(7.0)` → `7.0`）。
pub fn fnum(x: f64) -> Value {
    if !x.is_finite() {
        return Value::Null;
    }
    serde_json::Number::from_f64(x).map(Value::Number).unwrap_or(Value::Null)
}

/// 可选浮点：None → null。
pub fn fnum_opt(x: Option<f64>) -> Value {
    match x {
        Some(v) => fnum(v),
        None => Value::Null,
    }
}

/// `_human()`：B / KB 取整、MB 以上一位小数。
pub fn human(n: f64) -> String {
    let mut n = if n.is_finite() { n } else { 0.0 };
    if n == 0.0 {
        n = 0.0;
    }
    let units = ["B", "KB", "MB", "GB", "TB"];
    for (_i, unit) in units.iter().enumerate() {
        if n < 1024.0 || *unit == "TB" {
            return if *unit == "B" || *unit == "KB" {
                format!("{:.0} {}", n, unit)
            } else {
                format!("{:.1} {}", n, unit)
            };
        }
        n /= 1024.0;
    }
    format!("{:.1} TB", n)
}

/// `_pct(a, b)`：b 为 0 时给 0.0。
pub fn pct(a: f64, b: f64) -> f64 {
    if b == 0.0 {
        0.0
    } else {
        round1(a * 100.0 / b)
    }
}

/// `str(x)[:n]`：按字符（码点）截断。
pub fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// `_clip`：截断时留省号。
pub fn clip_ellipsis(s: &str, n: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= n {
        return s.to_string();
    }
    if n == 0 {
        return String::new();
    }
    let mut out: String = chars[..n - 1].iter().collect();
    out.push('…');
    out
}

/// `_brief`：只截长名字，不加省号在中间（尾部补…）。
pub fn brief(s: &str, n: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= n {
        return s.to_string();
    }
    let mut out: String = chars[..n].iter().collect();
    out.push('…');
    out
}

/// `" ".join(s.split())`：折叠任意空白。
pub fn squeeze(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ------------------------------------------------------------------ 时间

pub fn now_local() -> DateTime<Local> {
    Local::now()
}

pub fn fmt_ymd_hms(t: DateTime<Local>) -> String {
    t.format("%Y-%m-%d %H:%M:%S").to_string()
}

pub fn fmt_hms(t: DateTime<Local>) -> String {
    t.format("%H:%M:%S").to_string()
}

pub fn fmt_ymd(t: DateTime<Local>) -> String {
    t.format("%Y-%m-%d").to_string()
}

pub fn now_hms() -> String {
    fmt_hms(now_local())
}

pub fn now_ymd_hms() -> String {
    fmt_ymd_hms(now_local())
}

pub fn now_ymd() -> String {
    fmt_ymd(now_local())
}

/// `_ms(ts)`：毫秒 → "%Y-%m-%d %H:%M:%S"；坏值给空串。
pub fn ms_to_str(ts: f64) -> String {
    if !ts.is_finite() {
        return String::new();
    }
    let millis = ts as i64;
    match Local.timestamp_millis_opt(millis) {
        chrono::LocalResult::Single(t) => t.format("%Y-%m-%d %H:%M:%S").to_string(),
        _ => String::new(),
    }
}

/// `_iso(v)`：把 'T' 换成空格，取前 19 位。
pub fn iso_of(v: Option<&Value>) -> String {
    let s = str_or(v).trim().replace('T', " ");
    let n = s.chars().count();
    let taken = if n >= 19 { s.chars().take(19).collect() } else { s };
    taken
}

/// 解析 "%Y-%m-%d %H:%M:%S"（只看前 19 个字符）。
pub fn parse_ymd_hms(s: &str) -> Option<NaiveDateTime> {
    let head: String = s.chars().take(19).collect();
    NaiveDateTime::parse_from_str(&head, "%Y-%m-%d %H:%M:%S").ok()
}

/// `_secs(a, b)`：两个时间串之间的秒数，`max(0, int(...))`。
pub fn secs_between(a: &str, b: &str) -> Option<i64> {
    let ta = parse_ymd_hms(a)?;
    let tb = parse_ymd_hms(b)?;
    Some(((tb - ta).num_seconds()).max(0))
}

/// 本地时间串 → 秒级时间戳。
pub fn local_ts(s: &str) -> Option<i64> {
    let t = parse_ymd_hms(s)?;
    Local.from_local_datetime(&t).single().map(|x| x.timestamp())
}

/// `datetime.fromtimestamp(x)` 的 "%Y-%m-%d %H:%M:%S"。
pub fn ts_to_str(secs: i64) -> String {
    match Local.timestamp_opt(secs, 0) {
        chrono::LocalResult::Single(t) => t.format("%Y-%m-%d %H:%M:%S").to_string(),
        _ => String::new(),
    }
}

// ------------------------------------------------------------------ 文件

/// 读 JSON；缺失/损坏一律 None（Python `_load(path, None)`）。
pub fn load_json(p: &Path) -> Option<Value> {
    std::fs::read_to_string(p).ok().and_then(|s| serde_json::from_str(&s).ok())
}

/// `_load(p, {})`：非 dict 也按空对象处理的地方用它。
pub fn load_obj(p: &Path) -> Map<String, Value> {
    match load_json(p) {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    }
}

/// mtime（秒，浮点）。取不到给 0。
pub fn mtime(p: &Path) -> f64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub fn file_size(p: &Path) -> i64 {
    std::fs::metadata(p).map(|m| m.len() as i64).unwrap_or(0)
}

/// `_tail_jsonl`：只读尾部 512KB，逐行解析成 dict，保留最后 limit 条。
pub fn tail_jsonl(p: &Path, limit: usize) -> Vec<Value> {
    let mut f = match std::fs::File::open(p) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let size = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = size.saturating_sub(512 * 1024);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    // Python 用 utf-8 + replace；Rust 的 lossy 同口径
    let text = String::from_utf8_lossy(&buf);
    let mut out: Vec<Value> = Vec::new();
    for ln in text.split('\n') {
        let ln = ln.trim();
        if ln.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(ln) {
            if v.is_object() {
                out.push(v);
            }
        }
    }
    if out.len() > limit {
        out.split_off(out.len() - limit)
    } else {
        out
    }
}

/// `_tail_by_event`：按时间从尾往回读，遇到早于 cutoff 的行就停。
pub fn tail_by_event(p: &Path, events: &[&str], days: i64) -> Vec<Value> {
    let cutoff = fmt_ymd_hms(now_local() - chrono::Duration::days(days));
    let cutoff = cutoff.chars().take(16).collect::<String>();
    let mut f = match std::fs::File::open(p) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let mut pos = f.metadata().map(|m| m.len()).unwrap_or(0);
    let mut out: Vec<Value> = Vec::new();
    let mut tail: Vec<u8> = Vec::new();
    let mut stop = false;
    while pos > 0 && !stop {
        let step = std::cmp::min(128 * 1024, pos);
        pos -= step;
        if f.seek(SeekFrom::Start(pos)).is_err() {
            break;
        }
        let mut chunk = vec![0u8; step as usize];
        if f.read_exact(&mut chunk).is_err() {
            break;
        }
        chunk.extend_from_slice(&tail);
        let mut lines: Vec<Vec<u8>> = chunk.split(|b| *b == b'\n').map(|s| s.to_vec()).collect();
        tail = if pos > 0 && !lines.is_empty() { lines.remove(0) } else { Vec::new() };
        for raw in lines.iter().rev() {
            let raw = String::from_utf8_lossy(raw);
            let raw = raw.trim();
            if raw.is_empty() {
                continue;
            }
            let d: Value = match serde_json::from_str(raw) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if !d.is_object() {
                continue;
            }
            let ts = str_or(d.get("ts"));
            if !ts.is_empty() && ts.chars().take(16).collect::<String>() < cutoff {
                stop = true;
                break;
            }
            if events.contains(&str_or(d.get("event")).as_str()) {
                out.push(d);
            }
        }
    }
    let t = String::from_utf8_lossy(&tail).trim().to_string();
    if !t.is_empty() {
        if let Ok(d) = serde_json::from_str::<Value>(&t) {
            let ts = str_or(d.get("ts"));
            if d.is_object()
                && events.contains(&str_or(d.get("event")).as_str())
                && ts.chars().take(16).collect::<String>() >= cutoff
            {
                out.push(d);
            }
        }
    }
    out.reverse();
    out
}

/// 按 `os.scandir` 顺序读一层目录（不排序），返回 (name, is_dir, is_file)。
pub fn scandir(p: &Path) -> Vec<(String, bool, bool)> {
    let mut out = Vec::new();
    let rd = match std::fs::read_dir(p) {
        Ok(r) => r,
        Err(_) => return out,
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let md = e.metadata();
        // Python 的 is_dir()/is_file() 跟随符号链接（scandir 的 entry.is_dir 也是）
        let is_dir = md.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        let is_file = md.as_ref().map(|m| m.is_file()).unwrap_or(false);
        out.push((name, is_dir, is_file));
    }
    out
}

/// `Path.rglob("*")` 的等价物：不跟随目录符号链接（Python 的 rglob 也不跟）。
pub fn rglob_all(p: &Path, limit: usize) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>, limit: usize) {
        if out.len() >= limit {
            return;
        }
        let rd = match std::fs::read_dir(dir) {
            Ok(r) => r,
            Err(_) => return,
        };
        for e in rd.flatten() {
            if out.len() >= limit {
                return;
            }
            let path = e.path();
            let md = match std::fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(_) => continue,
            };
            out.push(path.clone());
            if md.is_dir() {
                walk(&path, out, limit);
            }
        }
    }
    walk(p, &mut out, limit);
    out
}

/// 相对路径（用 `/` 分隔），失败时给原样。
pub fn rel_to(p: &Path, base: &Path) -> String {
    match p.strip_prefix(base) {
        Ok(r) => r
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join("/"),
        Err(_) => p.to_string_lossy().to_string(),
    }
}

// ------------------------------------------------------------------ JSON 组装

/// `{"k": v}` 的便捷构造（保持插入顺序）。
pub fn obj(pairs: Vec<(&str, Value)>) -> Value {
    let mut m = Map::new();
    for (k, v) in pairs {
        m.insert(k.to_string(), v);
    }
    Value::Object(m)
}

pub fn sval(s: &str) -> Value {
    Value::String(s.to_string())
}

pub fn ival(n: i64) -> Value {
    json!(n)
}
