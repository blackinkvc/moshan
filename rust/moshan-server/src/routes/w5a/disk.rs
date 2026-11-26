// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：磁盘三处口径 —— dash._disk（工作台卡片）、metrics._disk（overview 机器块）、
//       metrics.load_snapshot（历史采样，供 series / 日报用）。
// 不做：不采样、不落盘，只读；采样是 Python 侧 metrics 的活儿，这轮不迁。
//
// 注意口径不同：dash._disk 取的是「数据根」的 statvfs、字段是 used_pct/free_h/…；
// metrics._disk 取的是「10-系统记录（不存在则 HOME）」的 statvfs、字段是 disk_pct/free_gb/…。
// 两边都用 f_bavail（可用给普通用户），不是 f_bfree —— 这条搞错会出现 1~2% 的偏差。
#![allow(unused_imports, dead_code)]

use serde_json::{json, Map, Value};
use std::path::Path;

use super::platform;
use super::util::{fnum, human, load_json, pct, round1};

/// `dash._disk(root)`：工作台「磁盘」卡。
pub fn disk(root: &Path) -> Value {
    match platform::disk_usage(root) {
        Some((total, free)) => {
            let used = total as f64 - free as f64;
            let mut m = Map::new();
            m.insert("used_pct".into(), fnum(pct(used, total as f64)));
            m.insert("free_h".into(), Value::String(human(free as f64)));
            m.insert("total_h".into(), Value::String(human(total as f64)));
            m.insert("used_h".into(), Value::String(human(used)));
            Value::Object(m)
        }
        None => {
            let mut m = Map::new();
            m.insert("used_pct".into(), fnum(0.0));
            m.insert("free_h".into(), Value::String(String::new()));
            m.insert("total_h".into(), Value::String(String::new()));
            m.insert("used_h".into(), Value::String(String::new()));
            Value::Object(m)
        }
    }
}

/// `metrics._disk()`：overview 机器块 / 降级账用的那份（GB、一位小数）。
pub fn metrics_disk(target: &Path) -> Value {
    match platform::disk_usage(target) {
        Some((total, free)) => {
            let used = total as f64 - free as f64;
            let gb = 1073741824.0f64;
            let mut m = Map::new();
            m.insert(
                "disk_pct".into(),
                fnum(if total > 0 {
                    round1(used * 100.0 / total as f64)
                } else {
                    0.0
                }),
            );
            m.insert("free_gb".into(), fnum(round1(free as f64 / gb)));
            m.insert("total_gb".into(), fnum(round1(total as f64 / gb)));
            Value::Object(m)
        }
        None => {
            let mut m = Map::new();
            m.insert("disk_pct".into(), fnum(0.0));
            m.insert("free_gb".into(), fnum(0.0));
            m.insert("total_gb".into(), fnum(0.0));
            Value::Object(m)
        }
    }
}

/// `metrics._line_day(ln)`：从一行快照里抠日期，不整行 parse。
fn line_day(ln: &str) -> String {
    let i = match ln.find("\"at\"") {
        Some(i) => i,
        None => return String::new(),
    };
    let after = i + 4;
    let j = match ln[after..].find('"') {
        Some(j) => after + j,
        None => return String::new(),
    };
    // `ln[j+1:j+11]`
    ln.chars()
        .skip(j + 1)
        .take(10)
        .collect::<String>()
}

/// `metrics.load_snapshot(day=None)`：读 `10-系统记录/机器/快照.jsonl`。
pub fn load_snapshot(path: &Path, day: Option<&str>) -> Vec<Value> {
    if !path.exists() {
        return Vec::new();
    }
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for ln in text.split('\n') {
        if ln.trim().is_empty() {
            continue;
        }
        if let Some(d) = day {
            if line_day(ln) != d {
                continue;
            }
        }
        if let Ok(v) = serde_json::from_str::<Value>(ln) {
            out.push(v);
        }
    }
    out
}

/// 兜底用：快照里最后 N 条的 (at, disk_pct/mem_pct/load1) 三条折线。
pub fn series_tail(snaps: &[Value]) -> (Vec<Value>, Vec<Value>, Vec<Value>) {
    let mut disk = Vec::new();
    let mut mem = Vec::new();
    let mut load = Vec::new();
    for x in snaps {
        disk.push(json!([
            x.get("at").cloned().unwrap_or(Value::Null),
            x.get("disk_pct").cloned().unwrap_or(Value::Null)
        ]));
        mem.push(json!([
            x.get("at").cloned().unwrap_or(Value::Null),
            x.get("mem_pct").cloned().unwrap_or(Value::Null)
        ]));
        load.push(json!([
            x.get("at").cloned().unwrap_or(Value::Null),
            x.get("load1").cloned().unwrap_or(Value::Null)
        ]));
    }
    (disk, mem, load)
}
