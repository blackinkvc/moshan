// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：dash._heartbeat() —— 工作台「心跳」卡。
// 不做：不跑自检、不修东西（自愈在 Python 的 heartbeat.py 里，本泳道只读结果）。
//
// 与 Python 的差别只在「状态从哪来」：Python 读进程内的 heartbeat.STATE，
// 这份东西每轮结果都会落盘到 10-系统记录/心跳/最新.json（_save_state，indent=2），
// 内容就是 STATE["last"] 本身。Rust 是另一个进程，只能读这份文件；
// running 恒 false —— 本进程没有心跳线程，硬写成 true 才是撒谎。
#![allow(unused_imports, dead_code)]

use serde_json::{json, Map, Value};
use std::path::Path;

use super::util::{load_obj, obj, sval, truthy};

/// `_heartbeat()`：`sys_dir` 是 `10-系统记录`。
pub fn heartbeat_at(sys_dir: &Path) -> Value {
    let last = load_obj(&sys_dir.join("心跳").join("最新.json"));
    let last_opt = if last.is_empty() { None } else { Some(last) };

    let mut checks: Vec<Value> = Vec::new();
    if let Some(last) = &last_opt {
        if let Some(Value::Array(arr)) = last.get("checks") {
            for c in arr {
                let mut m = Map::new();
                m.insert("id".into(), sval(&super::util::str_or(c.get("id"))));
                m.insert("name".into(), sval(&super::util::str_or(c.get("name"))));
                m.insert(
                    "ok".into(),
                    Value::Bool(c.get("ok").map(truthy).unwrap_or(false)),
                );
                m.insert(
                    "level".into(),
                    match c.get("level") {
                        Some(v) if truthy(v) => v.clone(),
                        _ => sval("info"),
                    },
                );
                m.insert("detail".into(), sval(&super::util::str_or(c.get("detail"))));
                checks.push(Value::Object(m));
            }
        }
    }

    let bad: Vec<Value> = checks
        .iter()
        .filter(|c| !c.get("ok").map(truthy).unwrap_or(false))
        .cloned()
        .collect();

    let at = match &last_opt {
        Some(last) => match last.get("at") {
            Some(v) if truthy(v) => v.clone(),
            _ => sval(""),
        },
        None => sval(""),
    };

    obj(vec![
        (
            "ok",
            match &last_opt {
                Some(last) => Value::Bool(last.get("ok").map(truthy).unwrap_or(false)),
                None => Value::Null,
            },
        ),
        ("at", at),
        ("running", Value::Bool(false)),
        ("heal", Value::Bool(true)),
        ("checks", Value::Array(checks)),
        ("bad", Value::Array(bad)),
    ])
}
