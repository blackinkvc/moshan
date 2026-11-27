// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：按 Python `json.dumps(..., ensure_ascii=False, indent=N)` 的格式序列化，并原子落盘。
// 不做：不提供通用 JSON 库能力。
//
// 为什么要自己写：`serde_json::to_string_pretty` 固定 2 空格，而账本/计划用的是
// `indent=1`。这些文件是**用户会直接打开看的**（`12-记账/账本.json`），
// 迁移期两边轮流写，格式一变就成了满屏 diff，看着像坏了。

use serde_json::Value;
use std::io::Write;
use std::path::Path;

pub fn dumps(v: &Value, indent: usize) -> String {
    let mut out = String::new();
    write_value(v, indent, 0, &mut out);
    out
}

fn pad(out: &mut String, indent: usize, depth: usize) {
    for _ in 0..indent * depth {
        out.push(' ');
    }
}

fn write_value(v: &Value, indent: usize, depth: usize, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => escape_into(s, out),
        Value::Array(a) => {
            if a.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push('\n');
                pad(out, indent, depth + 1);
                write_value(x, indent, depth + 1, out);
            }
            out.push('\n');
            pad(out, indent, depth);
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
                    out.push(',');
                }
                out.push('\n');
                pad(out, indent, depth + 1);
                escape_into(k, out);
                out.push_str(": ");
                write_value(x, indent, depth + 1, out);
            }
            out.push('\n');
            pad(out, indent, depth);
            out.push('}');
        }
    }
}

/// 与 Python `ensure_ascii=False` 一致：中文原样输出，只转义 JSON 必须转义的字符。
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

/// `_atomic_write`：先写临时文件再 replace，别让半截 JSON 留在盘上。
/// 账本是**唯一真源**，写坏了等于所有记录全丢。
pub fn atomic_write(path: &Path, text: &str) -> std::io::Result<()> {
    // 读不到就别写：空内容落盘会把真数据覆盖掉。
    if crate::paths::deny_library_read(path) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let name = path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let tmp = path.with_file_name(format!("{}.{}.tmp", name, std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.flush()?;
        let _ = f.sync_all();
    }
    std::fs::rename(&tmp, path)
}

/// 读 JSON；缺失/损坏一律回默认值（Python 的 `_load_json` 就是这么做的）。
pub fn read_json(path: &Path) -> Option<Value> {
    // 媒体根被系统授权卡住时 `open()` 不返回；这道闸让「读」直接变「没有」。
    // 闸设在这里而不是每个调用点：走 jsonio 的模块太多，逐个数一定会漏。
    if crate::paths::deny_library_read(path) {
        return None;
    }
    std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok())
}
