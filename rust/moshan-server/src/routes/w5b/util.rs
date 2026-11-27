// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：w5b 泳道内部用到的一小组 Python 口径工具（真值、str()、float()、round()、
//       打码、时间格式、原子落盘）。
// 不做：不做业务判断，也不往外暴露 —— 这些语义一旦与 Python 不同，四个模块会同时错。
//
// 为什么自己写：`str(True)` 在 Python 是 "True"，`json.dumps` 是 `true`；
// `round(2.675, 2)` 走的是二进制真值的四舍六入五成双，而 `f64::round` 是远离零进位。
// 这些差异都出现在**落盘 JSON**里，差一个字符就是磁盘分叉。
#![allow(dead_code)]

use serde_json::{json, Map, Value};
use std::path::Path;

/// `json.dumps(v, ensure_ascii=False)`（不带 indent）：分隔符是 `", "` 与 `": "`。
/// 与 `jsonio::dumps(v, 0)` 的区别就在这两个空格 —— 巡检历史 jsonl 是这么写的。
pub fn dumps_py_default(v: &Value) -> String {
    let mut out = String::new();
    write_default(v, &mut out);
    out
}

fn write_default(v: &Value, out: &mut String) {
    match v {
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_default(x, out);
            }
            out.push(']');
        }
        Value::Object(m) => {
            out.push('{');
            for (i, (k, x)) in m.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&escape_str(k));
                out.push_str(": ");
                write_default(x, out);
            }
            out.push('}');
        }
        other => out.push_str(&jsonio_scalar(other)),
    }
}

fn jsonio_scalar(v: &Value) -> String {
    let s = crate::jsonio::dumps(v, 0);
    s
}

fn escape_str(s: &str) -> String {
    let mut out = String::new();
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
    out
}

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

/// `v or ""` —— Python 里最常见的取值写法。
pub fn or_empty(v: Option<&Value>) -> Value {
    match v {
        Some(x) if truthy(x) => x.clone(),
        _ => Value::String(String::new()),
    }
}

/// Python `str(v)`：None→"None"、True→"True"、1299.0→"1299.0"、字符串原样。
pub fn py_str(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        other => crate::jsonio::dumps(other, 0),
    }
}

/// `str(v or "")` 的合并写法：假值一律空串，其余按 `str()`。
pub fn str_or(v: Option<&Value>) -> String {
    match v {
        Some(x) if truthy(x) => py_str(x),
        _ => String::new(),
    }
}

/// Python `float(v)`：数字、数字串（含首尾空白）、"inf"/"nan" 都收。
/// 布尔按 1.0/0.0（Python 也这么干）。
pub fn py_float(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                return None;
            }
            let low = t.to_ascii_lowercase();
            let sign = if low.strip_prefix('-').is_some() {
                -1.0
            } else {
                1.0
            };
            let body = low.trim_start_matches(['+', '-']);
            match body {
                "inf" | "infinity" => return Some(sign * f64::INFINITY),
                "nan" => return Some(f64::NAN),
                _ => {}
            }
            // Python 不接受 "1_000" / "0x10"，但接受 "1e3" / ".5" / "5."
            if t.contains('_') || t.contains("0x") || t.contains("0X") {
                return None;
            }
            t.parse::<f64>().ok()
        }
        _ => None,
    }
}

/// `_num()`：None/"" 为 None，否则 round(float(v), 2)，非数字为 None。
pub fn num2(v: Option<&Value>) -> Option<f64> {
    let v = v?;
    if v.is_null() {
        return None;
    }
    if let Value::String(s) = v {
        if s.is_empty() {
            return None;
        }
    }
    py_float(v).map(|x| {
        if x.is_nan() {
            x
        } else {
            round2(x)
        }
    })
}

/// Python `round(x, 2)`：按二进制真值四舍六入五成双。
/// Rust 的 `{:.2}` 格式化本身就是「正确舍入 + 平局取偶」，与 Python 同口径；
/// 再解析回 f64 就得到与 `json.dumps` 相同的短表示。
pub fn round2(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{:.2}", x).parse().unwrap_or(x)
}

pub fn round1(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{:.1}", x).parse().unwrap_or(x)
}

/// 数字进 JSON：整数值的浮点仍按 `12.0` 输出（与 Python `round(float)` 一致）。
pub fn fnum(x: f64) -> Value {
    serde_json::Number::from_f64(x).map(Value::Number).unwrap_or(Value::Null)
}

/// `mask()`：≤8 位用「•」占位，长的留前四后四。
pub fn mask(v: &str) -> String {
    if v.is_empty() {
        return String::new();
    }
    let chars: Vec<char> = v.chars().collect();
    if chars.len() <= 8 {
        let n = chars.len().saturating_sub(2).max(2);
        let tail: String = chars[chars.len().saturating_sub(2)..].iter().collect();
        return format!("{}{}", "•".repeat(n), tail);
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

/// `datetime.now().strftime("%H:%M:%S")`
pub fn now_hms() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

pub fn now_ymd_hms() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

pub fn now_ymd() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

pub fn now_ym() -> String {
    chrono::Local::now().format("%Y-%m").to_string()
}

/// 读 JSON 文件，任何失败回 None（Python 的 `_load` 口径）。
pub fn load_json(p: &Path) -> Option<Value> {
    std::fs::read_to_string(p).ok().and_then(|s| serde_json::from_str(&s).ok())
}

/// `_write`：indent=1 + 原子替换 + chmod 600。
pub fn write_private(p: &Path, v: &Value) -> std::io::Result<()> {
    crate::jsonio::atomic_write(p, &crate::jsonio::dumps(v, 1))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// `body.get(k)` 取对象里的键。
pub fn get<'a>(v: &'a Value, k: &str) -> Option<&'a Value> {
    v.get(k)
}

/// 取对象；不是对象就当空对象（Python `(v or {})` 之后按 dict 用）。
pub fn obj_or_empty(v: Option<&Value>) -> Map<String, Value> {
    match v {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    }
}

/// `str(x)[:n]` —— 按字符截断（Python 切的是码点，不是字节）。
pub fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// 4 位十六进制不进位（阿里云签名里 nonce 只要随机串）。
pub fn rand_hex(n: usize) -> String {
    use std::io::Read;
    let mut buf = vec![0u8; n.div_ceil(2)];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok();
    let seed = if ok {
        buf
    } else {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        (0..n.div_ceil(2)).map(|i| ((t >> (i * 8)) & 0xff) as u8).collect()
    };
    seed.iter().map(|b| format!("{b:02x}")).collect::<String>()[..n].to_string()
}

/// `str_or(Some(v))` —— 给 `Option<&Value>::map()` 用（`str_or` 收的是 Option）。
pub fn sv(v: &Value) -> String {
    str_or(Some(v))
}

/// FastAPI `body: dict | None = None` 的口径：空体 / 坏 JSON / 不是对象 → `{}`。
pub fn lenient_body(b: &[u8]) -> Value {
    if b.is_empty() {
        return Value::Object(Map::new());
    }
    match serde_json::from_slice::<Value>(b) {
        Ok(v) if v.is_object() => v,
        _ => Value::Object(Map::new()),
    }
}

/// FastAPI `body: dict`（必填）在空体/坏 JSON 时的 422 形状。
pub fn missing_body() -> Value {
    json!({"detail": [{
        "type": "missing",
        "loc": ["body"],
        "msg": "Field required",
        "input": null,
        "url": "https://errors.pydantic.dev/2.14/v/missing",
    }]})
}

/// FastAPI 对 `body: dict | None = None` 的口径：**没有体**才当 `{}`；
/// 坏 JSON、不是对象照样 422。返回 Err 时是已经成形的那一帧。
pub fn optional_body(b: &[u8]) -> Result<Value, (u16, Value)> {
    if b.is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    strict_body(b)
}

/// FastAPI 对 `body: dict`（必填）的口径：空体 422 missing，坏 JSON 422 json_invalid，
/// 非对象 422 dict_type。
pub fn strict_body(b: &[u8]) -> Result<Value, (u16, Value)> {
    if b.is_empty() {
        return Err((422, missing_body()));
    }
    match serde_json::from_slice::<Value>(b) {
        Ok(v) => {
            if v.is_object() {
                Ok(v)
            } else {
                Err((
                    422,
                    json!({"detail": [{
                        "type": "dict_type",
                        "loc": ["body"],
                        "msg": "Input should be a valid dictionary",
                        "input": v,
                        "url": "https://errors.pydantic.dev/2.14/v/dict_type",
                    }]}),
                ))
            }
        }
        Err(e) => {
            // pydantic 的 loc 位置是 **0 起的字节偏移**，serde 的 column 是 1 起的
            let col = e.column().saturating_sub(1);
            Err((
                422,
                json!({"detail": [{
                    "type": "json_invalid",
                    "loc": ["body", col],
                    "msg": "JSON decode error",
                    "input": {},
                    "ctx": {"error": json_err_text(&e.to_string())},
                }]}),
            ))
        }
    }
}

/// serde 的报错译回 `json.JSONDecodeError` 那几句（pydantic 原样透传）。
fn json_err_text(msg: &str) -> String {
    let m = msg.to_lowercase();
    if m.contains("key must be a string") {
        "Expecting property name enclosed in double quotes".to_string()
    } else if m.contains("eof while parsing a value") || m.contains("expected value") {
        "Expecting value".to_string()
    } else if m.contains("trailing characters") {
        "Extra data".to_string()
    } else if m.contains("expected `,` or `}`") || m.contains("expected `,` or `]`") {
        "Expecting ',' delimiter".to_string()
    } else if m.contains("expected `:`") {
        "Expecting ':' delimiter".to_string()
    } else {
        "Expecting value".to_string()
    }
}
