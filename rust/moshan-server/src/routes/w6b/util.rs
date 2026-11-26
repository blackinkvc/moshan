// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：Python 语义小工具 —— 本泳道这批路由大量依赖 `x or y` / `bool(x)` /
//       `str(x)` / `int(float(x))` / `" ".join(s.split())` / `s[:n]`（按字符）/
//       `json.dumps(..., ensure_ascii=False)`（**默认分隔符是 `", "` 与 `": "`**）
//       这些口径。差一个空格就是响应/落盘字节差一处，所以集中一份。
// 不做：不做业务判断，不碰业务文件（读 jsonl 的例外，与 `_read_jsonl` 同口径）。

#![allow(dead_code)]

use chrono::{DateTime, Local, TimeZone};
use serde_json::{Map, Value};
use std::io::Write;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------- 真值与取值

/// Python 的真值：None/0/""/[]/{} 为假。
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// `str(v or "")`：假值一律空串。
pub fn s_or(v: &Value) -> String {
    if truthy(v) {
        pystr(v)
    } else {
        String::new()
    }
}

/// `str(v)`（None → "None"，True → "True"）。
pub fn pystr(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
        Value::Number(n) => num_str(n),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

pub fn num_str(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    float_str(n.as_f64().unwrap_or(0.0))
}

/// Python `str(float)`：整数值补 `.0`。
pub fn float_str(f: f64) -> String {
    if !f.is_finite() {
        return if f.is_nan() {
            "nan".into()
        } else if f > 0.0 {
            "inf".into()
        } else {
            "-inf".into()
        };
    }
    let s = format!("{}", f);
    if s.contains('.') || s.contains('e') || s.contains("inf") || s.contains("nan") {
        s
    } else {
        format!("{}.0", s)
    }
}

/// `int(float(v))`：截断取整，认不出回 0（`_int` 的上半截）。
pub fn int_float(v: &Value) -> i64 {
    match v {
        Value::Number(n) => n.as_f64().map(|f| f.trunc() as i64).unwrap_or(0),
        Value::Bool(b) => *b as i64,
        Value::String(s) => s.trim().parse::<f64>().map(|f| f.trunc() as i64).unwrap_or(0),
        _ => 0,
    }
}

/// `int(v or 0)`（`int0`）：字符串认不出回 0。
pub fn int0(v: &Value) -> i64 {
    if !truthy(v) {
        return 0;
    }
    match v {
        Value::Number(n) => n.as_f64().map(|f| f.trunc() as i64).unwrap_or(0),
        Value::String(s) => {
            let t = s.trim();
            if let Ok(i) = t.parse::<i64>() {
                i
            } else if let Ok(f) = t.parse::<f64>() {
                f.trunc() as i64
            } else {
                0
            }
        }
        Value::Bool(b) => *b as i64,
        _ => 0,
    }
}

/// `float(v)`；认不出回 None。
pub fn fnum(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// `round(x, n)`：CPython 用「精确二进制值 + 银行家舍入」，Rust 的
/// `(x*10^n).round()/10^n` 走的是四舍五入，所以这里改成按十进制字面量往返。
pub fn py_round(x: f64, n: u32) -> f64 {
    if !x.is_finite() {
        return x;
    }
    let s = format!("{:.*}", n as usize, x);
    s.parse::<f64>().unwrap_or(x)
}

/// `f"{x:.0f}"` / `f"{x:.1f}"` / `f"{x:.2f}"`
pub fn f0(x: f64) -> String {
    format!("{:.0}", x)
}
pub fn f1(x: f64) -> String {
    format!("{:.1}", x)
}
pub fn f2(x: f64) -> String {
    format!("{:.2}", x)
}

// ---------------------------------------------------------------- 字符串

/// `" ".join(s.split())` —— 折掉所有空白（含全角空格之外的 Unicode 空白）。
pub fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `s[:n]`：按**字符**截断（不是字节）。
pub fn cut(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// `s.strip()`（Python 的 strip 认全角空白，Rust 的 trim 也认 Unicode 空白）。
pub fn strip(s: &str) -> String {
    s.trim().to_string()
}

// ---------------------------------------------------------------- 时间

pub fn now_local() -> DateTime<Local> {
    Local::now()
}

pub fn now_ms() -> i64 {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    d.as_millis() as i64
}

pub fn now_secs() -> i64 {
    now_ms() / 1000
}

/// 毫秒时间戳 → 本地 `DateTime`（越界回当前时间，与 `_iso` 的兜底一致）。
pub fn dt_of_ms(ms: i64) -> DateTime<Local> {
    Local
        .timestamp_millis_opt(ms)
        .single()
        .unwrap_or_else(|| Local::now())
}

/// `datetime.fromtimestamp(ms/1000).strftime(fmt)`。
pub fn strftime_ms(ms: i64, fmt: &str) -> String {
    strftime_secs(ms.div_euclid(1000), fmt)
}

pub fn strftime_secs(secs: i64, fmt: &str) -> String {
    match Local.timestamp_opt(secs, 0).single() {
        Some(d) => d.format(fmt).to_string(),
        None => Local::now().format(fmt).to_string(),
    }
}

pub fn strftime_now(fmt: &str) -> String {
    Local::now().format(fmt).to_string()
}

/// Python `datetime.fromtimestamp()` 认的年份范围有限；越界时 `_iso` 回当前时间。
pub fn iso_of_ms(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    if secs > 253_402_300_799 || secs < -62_135_596_800 {
        return strftime_now("%Y-%m-%dT%H:%M:%S");
    }
    strftime_secs(secs, "%Y-%m-%dT%H:%M:%S")
}

// ---------------------------------------------------------------- JSON 落盘

/// `json.dumps(v, ensure_ascii=False)`：**默认分隔符** `", "` 与 `": "`。
/// `jsonio::dumps(v, 0)` 走的是 `","` + `": "`（compact 风格），
/// 而 jsonl 每一行是 Python 默认风格，差一个空格就是磁盘分叉。
pub fn dumps_line(v: &Value) -> String {
    let mut out = String::new();
    write_compact(v, 0, &mut out);
    out
}

fn write_compact(v: &Value, depth: usize, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&num_str(n)),
        Value::String(s) => escape_into(s, out),
        Value::Array(a) => {
            if a.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_compact(x, depth + 1, out);
            }
            out.push(']');
        }
        Value::Object(m) => {
            if m.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (k, x)) in m.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                escape_into(k, out);
                out.push_str(": ");
                write_compact(x, depth + 1, out);
            }
            out.push('}');
        }
    }
}

/// 与 `ensure_ascii=False` 一致：中文原样，只转义 JSON 必须转义的。
fn escape_into(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// `_read_jsonl`：坏行跳过（写到一半断电的半行不能拖垮整份）。
pub fn read_jsonl(path: &Path) -> Vec<Value> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        // Python 用 errors="replace" 读；这里退回按字节有损转换
        Err(_) => match std::fs::read(path) {
            Ok(b) => String::from_utf8_lossy(&b).to_string(),
            Err(_) => return Vec::new(),
        },
    };
    text.lines()
        .filter_map(|ln| {
            let s = ln.trim();
            if s.is_empty() {
                return None;
            }
            match serde_json::from_str::<Value>(s) {
                Ok(v) if v.is_object() => Some(v),
                _ => None,
            }
        })
        .collect()
}

/// 读所有行（含非对象），`_read_jsonl` 只要对象；有些地方要保留原样行。
pub fn read_lines(path: &Path) -> Vec<String> {
    match std::fs::read(path) {
        Ok(b) => String::from_utf8_lossy(&b).to_string().lines().map(|s| s.to_string()).collect(),
        Err(_) => Vec::new(),
    }
}

/// `_append`：一次性写多行 + fsync；返回写入条数。
pub fn append_jsonl(path: &Path, recs: &[Value]) -> usize {
    if recs.is_empty() {
        return 0;
    }
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    let mut blob = String::new();
    for r in recs {
        blob.push_str(&dumps_line(r));
        blob.push('\n');
    }
    match std::fs::OpenOptions::new().create(true).append(true).open(path) {
        Ok(mut f) => {
            if f.write_all(blob.as_bytes()).is_err() {
                return 0;
            }
            let _ = f.flush();
            let _ = f.sync_all();
            recs.len()
        }
        Err(_) => 0,
    }
}

// ---------------------------------------------------------------- 路径

/// `Path.home()`
pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn ensure_dir(p: &Path) {
    let _ = std::fs::create_dir_all(p);
}

/// 建好父目录（写文件前用）。
pub fn ensure_parent(p: &Path) -> std::io::Result<()> {
    match p.parent() {
        Some(d) => std::fs::create_dir_all(d),
        None => Ok(()),
    }
}

/// `_load(path, default)`：读不到/坏 JSON 一律给 default。
pub fn load_json(p: &Path) -> Option<Value> {
    crate::jsonio::read_json(p)
}

/// `v.get(k).unwrap_or(&Value::Null)` 的短名字 —— 调用点多，省点噪音。
pub fn get<'a>(v: &'a Value, k: &str) -> &'a Value {
    v.get(k).unwrap_or(&Value::Null)
}

/// `Path.expanduser()`
pub fn expanduser(raw: &str) -> PathBuf {
    let s = raw.trim();
    if s == "~" {
        return home();
    }
    if let Some(rest) = s.strip_prefix("~/") {
        return home().join(rest);
    }
    PathBuf::from(s)
}

/// `datetime.fromtimestamp(ts)` 的日期部分（本地）。
pub fn day_of_ts(ts: f64) -> String {
    if ts <= 0.0 {
        return String::new();
    }
    let secs = ts.trunc() as i64;
    strftime_secs(secs, "%Y-%m-%d")
}

/// `time.strftime("%Y-%m-%d", time.localtime(ts))`
pub fn day_of_ts_f(ts: f64) -> String {
    day_of_ts(ts)
}

// ---------------------------------------------------------------- 错误文本

/// Python `str(OSError)`：`[Errno 2] No such file or directory: '/x'`。
/// 对拍脚本会把 `(os error N)` 与 `[Errno N] ` 归一，但**保留 strerror 与 errno**，
/// 所以这里必须给真 errno，不能只写一句「失败了」。
pub fn py_os_err(e: &std::io::Error) -> String {
    match e.raw_os_error() {
        Some(code) => format!("[Errno {}] {}", code, strerror(code)),
        None => e.to_string(),
    }
}

pub fn py_os_err_path(e: &std::io::Error, path: &Path) -> String {
    match e.raw_os_error() {
        Some(code) => format!("[Errno {}] {}: '{}'", code, strerror(code), path.display()),
        None => e.to_string(),
    }
}

fn strerror(code: i32) -> String {
    unsafe {
        let p = libc::strerror(code);
        if p.is_null() {
            return format!("OS error {}", code);
        }
        std::ffi::CStr::from_ptr(p).to_string_lossy().to_string()
    }
}

/// 空 Map（方便构造有序对象）。
pub fn obj() -> Map<String, Value> {
    Map::new()
}
