// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：Python 语义小工具 —— 这一组路由的判定链大量依赖 `x or y`、`bool(x)`、
//       `str(x)`、`int(x or 0)`、`round()`、`f"{x:.1f}"` 这些口径。
//       差一处就是响应字节差一处，所以集中实现一份，别在各文件里现写。
// 不做：不做业务判断，不碰磁盘（读 JSON 的例外，与 Python 的 `_load` 同口径）。

#![allow(dead_code)]

use chrono::{DateTime, Local, NaiveDate};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Python 的真值判定：None/0/""/[]/{} 为假。
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

/// Python `str(v)` 的口径（None 在 Python 里是 "None"，但调用点几乎都是
/// `str(x or "")`，所以另给 [`s_or`]，别拿这个当默认）。
pub fn pystr(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
        Value::Number(n) => num_str(n),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn num_str(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    let f = n.as_f64().unwrap_or(0.0);
    float_str(f)
}

/// Python `str(float)`：整数值补 `.0`（`90.0` 而不是 `90`）。
pub fn float_str(f: f64) -> String {
    if !f.is_finite() {
        return if f.is_nan() { "nan".into() } else if f > 0.0 { "inf".into() } else { "-inf".into() };
    }
    let s = format!("{}", f);
    if s.contains('.') || s.contains('e') || s.contains("inf") || s.contains("nan") {
        s
    } else {
        format!("{}.0", s)
    }
}

/// `str(v or "")`。
pub fn s_or(v: &Value) -> String {
    if truthy(v) {
        pystr(v)
    } else {
        String::new()
    }
}

/// `d.get(k)` 的缺省空值。
pub fn get<'a>(v: &'a Value, k: &str) -> &'a Value {
    v.get(k).unwrap_or(&Value::Null)
}

/// `int(v or 0)`。解析失败给 None（调用方自己决定形状）。
pub fn int_or(v: &Value) -> Option<i64> {
    match v {
        Value::Null => Some(0),
        Value::Bool(b) => Some(if *b { 1 } else { 0 }),
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f.trunc() as i64)),
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                Some(0)
            } else {
                t.parse::<i64>().ok()
            }
        }
        _ => None,
    }
}

/// `int(v or 0)`，失败给 0（Python 那边会抛，这里静默 —— 只在无异常分支用它）。
pub fn int0(v: &Value) -> i64 {
    int_or(v).unwrap_or(0)
}

/// `str(x).lstrip("-").isdigit()` + `int(...)` 的组合，计划里的 tok 就是这写法。
pub fn digits_or_zero(v: &Value) -> i64 {
    let raw = if truthy(v) { pystr(v) } else { "0".to_string() };
    let t = raw.trim_start_matches('-');
    if !t.is_empty() && t.chars().all(|c| c.is_ascii_digit()) {
        raw.parse::<i64>().unwrap_or(0)
    } else {
        0
    }
}

/// Python 字符切片 `s[:n]`（按字符，不按字节）。
pub fn cut(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// `s.strip()`（Python 的 strip 去所有 Unicode 空白）。
pub fn strip(s: &str) -> String {
    s.trim().to_string()
}

pub fn now_local() -> DateTime<Local> {
    Local::now()
}

/// `datetime.now().isoformat(timespec="seconds")`
pub fn now_iso() -> String {
    now_local().format("%Y-%m-%dT%H:%M:%S").to_string()
}

/// `datetime.now().strftime("%Y-%m-%d %H:%M:%S")`
pub fn now_stamp() -> String {
    now_local().format("%Y-%m-%d %H:%M:%S").to_string()
}

pub fn now_hms() -> String {
    now_local().format("%H:%M:%S").to_string()
}

pub fn today() -> String {
    now_local().format("%Y-%m-%d").to_string()
}

/// `datetime.utcnow().strftime("%Y-%m-%dT%H:%M:%S.000Z")`
pub fn utc_iso_ms() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S.000Z").to_string()
}

/// Python `round(x, n)` 的近似：Rust 的定点格式化本身就是「二进制真值 + 半偶舍入」，
/// 与 CPython 的 `round` 同口径（`round(2.675, 2) == 2.67`）。
pub fn py_round(x: f64, n: usize) -> f64 {
    match n {
        0 => format!("{:.0}", x).parse().unwrap_or(x),
        1 => format!("{:.1}", x).parse().unwrap_or(x),
        2 => format!("{:.2}", x).parse().unwrap_or(x),
        3 => format!("{:.3}", x).parse().unwrap_or(x),
        _ => x,
    }
}

/// Python `round(x)`（无第二参）= 返回 int，半偶舍入。
pub fn py_round_int(x: f64) -> i64 {
    format!("{:.0}", x).parse().unwrap_or(0)
}

pub fn f0(x: f64) -> String {
    format!("{:.0}", x)
}

pub fn f1(x: f64) -> String {
    format!("{:.1}", x)
}

pub fn f2(x: f64) -> String {
    format!("{:.2}", x)
}

/// 数值型字段取 f64（字符串里的数字也认，与 `float(...)` 在常见输入上等价）。
pub fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

pub fn parse_date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(&s[..s.len().min(10)], "%Y-%m-%d").ok()
}

/// `_load(path, default)`：缺失/损坏回 default。
pub fn load_json(p: &Path) -> Option<Value> {
    std::fs::read_to_string(p).ok().and_then(|s| serde_json::from_str(&s).ok())
}

/// `json.dumps(..., ensure_ascii=False, indent=N)` 落盘。
pub fn write_json(p: &Path, v: &Value, indent: usize) -> std::io::Result<()> {
    crate::jsonio::atomic_write(p, &crate::jsonio::dumps(v, indent))
}

/// `json.dumps(obj, ensure_ascii=False)`（indent=None）：单行，`, ` / `: ` 分隔。
/// hostwork 的 `_json_write` 就是这个口径，价表缓存按它写。
pub fn dumps_flat(v: &Value) -> String {
    let mut out = String::new();
    flat_into(v, &mut out);
    out
}

fn flat_into(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => escape(s, out),
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                flat_into(x, out);
            }
            out.push(']');
        }
        Value::Object(m) => {
            out.push('{');
            for (i, (k, x)) in m.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                escape(k, out);
                out.push_str(": ");
                flat_into(x, out);
            }
            out.push('}');
        }
    }
}

fn escape(s: &str, out: &mut String) {
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

/// `path.parent.mkdir(parents=True, exist_ok=True)`，失败忽略（Python 侧同口径）。
pub fn ensure_dir(p: &Path) {
    let _ = std::fs::create_dir_all(p);
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 把 `&str` 路径拼到 PathBuf 上（Windows 风格分隔符也当分隔符处理，与 Path 一致）。
pub fn join<P: AsRef<Path>>(base: P, parts: &[&str]) -> PathBuf {
    let mut p = base.as_ref().to_path_buf();
    for x in parts {
        p.push(x);
    }
    p
}
