// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：w5e 泳道内部的 Python 口径小工具（有序表、round、title、json.dumps 的两种形态）。
// 不做：不做业务判断。这些语义差一点，响应就在字节层面分叉。
//
// 为什么自己写：`crate::jsonio::dumps` 只会带缩进；workstat 的缓存是
// `separators=(",",":")`、存档是 `sort_keys=True`，三者都是磁盘契约。
#![allow(dead_code)]

use serde_json::{Map, Value};
use std::collections::HashMap;
use std::hash::Hash;

/// Python dict 的替身：保留插入顺序 + O(1) 查找（serde_json::Map 的键是 String，
/// 这里要拿元组当键，所以自己写一个最小的）。
pub struct OrdMap<K: Eq + Hash + Clone, V> {
    items: Vec<(K, V)>,
    idx: HashMap<K, usize>,
}

impl<K: Eq + Hash + Clone, V: Clone> Clone for OrdMap<K, V> {
    fn clone(&self) -> Self {
        Self { items: self.items.clone(), idx: self.idx.clone() }
    }
}

impl<K: Eq + Hash + Clone, V> Default for OrdMap<K, V> {
    fn default() -> Self {
        Self { items: Vec::new(), idx: HashMap::new() }
    }
}

impl<K: Eq + Hash + Clone, V> OrdMap<K, V> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, k: &K) -> Option<&V> {
        self.idx.get(k).map(|i| &self.items[*i].1)
    }

    pub fn get_mut(&mut self, k: &K) -> Option<&mut V> {
        match self.idx.get(k) {
            Some(i) => Some(&mut self.items[*i].1),
            None => None,
        }
    }

    pub fn insert(&mut self, k: K, v: V) {
        match self.idx.get(&k) {
            Some(i) => self.items[*i].1 = v,
            None => {
                self.idx.insert(k.clone(), self.items.len());
                self.items.push((k, v));
            }
        }
    }

    /// `dict.setdefault(k, default())` —— 键已在就取旧值（借用给调用方）。
    pub fn or_insert_with<F: FnOnce() -> V>(&mut self, k: K, f: F) -> &mut V {
        if !self.idx.contains_key(&k) {
            self.idx.insert(k.clone(), self.items.len());
            self.items.push((k.clone(), f()));
        }
        let i = self.idx[&k];
        &mut self.items[i].1
    }

    pub fn contains(&self, k: &K) -> bool {
        self.idx.contains_key(k)
    }

    pub fn remove(&mut self, k: &K) {
        if let Some(i) = self.idx.remove(k) {
            self.items.remove(i);
            for (j, (_, _)) in self.items.iter().enumerate().skip(i) {
                self.idx.insert(self.items[j].0.clone(), j);
            }
        }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, (K, V)> {
        self.items.iter()
    }

    pub fn keys(&self) -> Vec<K> {
        self.items.iter().map(|(k, _)| k.clone()).collect()
    }
}

/// Python `round(x)`（无 ndigits）：按二进制真值、四舍六入五成双，返回整数。
pub fn round_int(x: f64) -> i64 {
    if !x.is_finite() {
        return 0;
    }
    x.round_ties_even() as i64
}

/// Python `str.title()`：词首字母大写、其后小写；非字母（含数字）另起一词。
pub fn py_title(s: &str) -> String {
    let mut out = String::new();
    let mut prev_alpha = false;
    for c in s.chars() {
        if c.is_alphabetic() {
            if prev_alpha {
                out.extend(c.to_lowercase());
            } else {
                out.extend(c.to_uppercase());
            }
            prev_alpha = true;
        } else {
            out.push(c);
            prev_alpha = false;
        }
    }
    out
}

fn escape_json(s: &str, out: &mut String) {
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

fn write_compact(v: &Value, out: &mut String, sep: &str, colon: &str) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => escape_json(s, out),
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(sep);
                }
                write_compact(x, out, sep, colon);
            }
            out.push(']');
        }
        Value::Object(m) => {
            out.push('{');
            for (i, (k, x)) in m.iter().enumerate() {
                if i > 0 {
                    out.push_str(sep);
                }
                escape_json(k, out);
                out.push_str(colon);
                write_compact(x, out, sep, colon);
            }
            out.push('}');
        }
    }
}

/// `json.dumps(v, ensure_ascii=False, separators=(",", ":"))` —— 缓存文件的形态。
pub fn dumps_nospace(v: &Value) -> String {
    let mut out = String::new();
    write_compact(v, &mut out, ",", ":");
    out
}

/// `json.dumps(v, ensure_ascii=False, sort_keys=True)` —— 存档文件的形态。
/// 键序是码点序（Rust 的 &str 序与 UTF-8 字节序一致，等价）。
pub fn dumps_sorted(v: &Value) -> String {
    let sv = sort_value(v);
    let mut out = String::new();
    write_compact(&sv, &mut out, ", ", ": ");
    out
}

fn sort_value(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let mut out = Map::new();
            for k in keys {
                out.insert(k.clone(), sort_value(&m[k]));
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(sort_value).collect()),
        other => other.clone(),
    }
}

/// `json.dumps(v, ensure_ascii=False)` —— 单行、分隔符 ", " / ": "（应用名缓存）。
pub fn dumps_spaced(v: &Value) -> String {
    let mut out = String::new();
    write_compact(v, &mut out, ", ", ": ");
    out
}

/// 分钟格：`bin(int(h or "0", 16)).count("1") * 60.0`。
pub fn bits_secs(h: &str) -> f64 {
    if h.is_empty() {
        return 0.0;
    }
    match u128::from_str_radix(h, 16) {
        Ok(n) => n.count_ones() as f64 * 60.0,
        Err(_) => 0.0,
    }
}

// ---------------------------------------------------------------- json.loads
//
// 为什么不用 `serde_json::from_str`：serde_json 默认的快解析对 17 位有效数字的
// 浮点会差 1 ULP（`13094.387999999999` 读成 `13094.388`），而 Python 的
// `json.loads` 走 strtod，是正确舍入。存档里的 `secs` 正是这种 17 位数，
// 读进来再 `max()` 取大，就会把历史值悄悄抬高 —— 写回磁盘后和 Python 分叉。
// `f64::from_str`（std）是正确舍入的，所以这里只补一层解析骨架。

/// `json.loads(text)`：数字一律走 std 的正确舍入解析。
pub fn loads(text: &str) -> Option<Value> {
    let mut p = JP { b: text.as_bytes(), i: 0 };
    p.ws();
    let v = p.value()?;
    p.ws();
    if p.i != p.b.len() {
        return None;
    }
    Some(v)
}

struct JP<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> JP<'a> {
    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn eat(&mut self, lit: &[u8]) -> bool {
        if self.b.len() >= self.i + lit.len() && &self.b[self.i..self.i + lit.len()] == lit {
            self.i += lit.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self) -> Option<Value> {
        match *self.b.get(self.i)? {
            b'{' => self.object(),
            b'[' => self.array(),
            b'"' => self.string().map(Value::String),
            b't' => self.eat(b"true").then_some(Value::Bool(true)),
            b'f' => self.eat(b"false").then_some(Value::Bool(false)),
            b'n' => self.eat(b"null").then_some(Value::Null),
            _ => self.number(),
        }
    }

    fn object(&mut self) -> Option<Value> {
        self.i += 1; // {
        let mut m = Map::new();
        self.ws();
        if self.b.get(self.i) == Some(&b'}') {
            self.i += 1;
            return Some(Value::Object(m));
        }
        loop {
            self.ws();
            let k = self.string()?;
            self.ws();
            if self.b.get(self.i) != Some(&b':') {
                return None;
            }
            self.i += 1;
            self.ws();
            let v = self.value()?;
            m.insert(k, v);
            self.ws();
            match self.b.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Some(Value::Object(m));
                }
                _ => return None,
            }
        }
    }

    fn array(&mut self) -> Option<Value> {
        self.i += 1; // [
        let mut a: Vec<Value> = Vec::new();
        self.ws();
        if self.b.get(self.i) == Some(&b']') {
            self.i += 1;
            return Some(Value::Array(a));
        }
        loop {
            self.ws();
            a.push(self.value()?);
            self.ws();
            match self.b.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Some(Value::Array(a));
                }
                _ => return None,
            }
        }
    }

    fn string(&mut self) -> Option<String> {
        if self.b.get(self.i) != Some(&b'"') {
            return None;
        }
        self.i += 1;
        let mut out = String::new();
        loop {
            let start = self.i;
            while self.i < self.b.len() && self.b[self.i] != b'"' && self.b[self.i] != b'\\' {
                self.i += 1;
            }
            if self.i > start {
                out.push_str(std::str::from_utf8(&self.b[start..self.i]).ok()?);
            }
            match self.b.get(self.i) {
                Some(b'"') => {
                    self.i += 1;
                    return Some(out);
                }
                Some(b'\\') => {
                    self.i += 1;
                    let c = *self.b.get(self.i)?;
                    self.i += 1;
                    match c {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{08}'),
                        b'f' => out.push('\u{0c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let cp = if (0xD800..0xDC00).contains(&hi) {
                                // 代理对：高 4 位 + 低 4 位拼成一个码点
                                if !self.eat(b"\\u") {
                                    return None;
                                }
                                let lo = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&lo) {
                                    return None;
                                }
                                0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                            } else {
                                hi
                            };
                            out.push(char::from_u32(cp)?);
                        }
                        _ => return None,
                    }
                }
                _ => return None,
            }
        }
    }

    fn hex4(&mut self) -> Option<u32> {
        let s = std::str::from_utf8(self.b.get(self.i..self.i + 4)?).ok()?;
        self.i += 4;
        u32::from_str_radix(s, 16).ok()
    }

    fn number(&mut self) -> Option<Value> {
        let start = self.i;
        while self.i < self.b.len()
            && matches!(self.b[self.i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
        {
            self.i += 1;
        }
        if self.i == start {
            return None;
        }
        let tok = std::str::from_utf8(&self.b[start..self.i]).ok()?;
        if tok.contains(['.', 'e', 'E']) {
            let f: f64 = tok.parse().ok()?;
            return serde_json::Number::from_f64(f).map(Value::Number);
        }
        if let Ok(n) = tok.parse::<i64>() {
            return Some(Value::Number(n.into()));
        }
        let f: f64 = tok.parse().ok()?;
        serde_json::Number::from_f64(f).map(Value::Number)
    }
}
