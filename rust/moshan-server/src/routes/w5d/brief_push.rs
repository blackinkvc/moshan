// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：`moshan/brief.py` 的定时推送（`scheduler_loop / _tick / _claim /
//       _sent_state / _save_sent / _write_push_state`）。
// 不做：不重算简报内容（那是 `w4b::brief::build`），也不手动推送
//       （`/api/brief/push` 走 `brief::push`，与本文件无关）。
//
// 窗口而不是时刻：8–11 点之间只要进程活着、今天没推完就补。
// 去重靠 `<日期>.sent.json`（哪几段已成功、试过几跳）与 `_推送.json`（今天推完了）。
// 抢牌 `<日期>.push` 是「今天归我」的唯一凭据，推成功与否都不删 ——
// 失败重试只补没发出去的段，不能靠放牌（放了另一个实例会接着发，手机收两份）。
//
// 开关：`MOSHAN_NO_PUSH=1` **只关这条定时器**，手动推送照常。

#![cfg(feature = "lane_w4b")]

use crate::paths::Ctx;
use crate::routes::w4b::brief;
use crate::routes::w5d::util;
use chrono::Timelike;
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const PUSH_FROM: u32 = 8;
const PUSH_TO: u32 = 11;
const PUSH_TRIES: i64 = 5;
const TICK_SECS: u64 = 60;

fn env_flag(key: &str) -> bool {
    match std::env::var(key) {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            !v.is_empty() && v != "0" && v != "false" && v != "no"
        }
        Err(_) => false,
    }
}

fn brief_dir(root: &Path) -> PathBuf {
    let d = root.join("10-系统记录").join("简报");
    let _ = std::fs::create_dir_all(&d);
    d
}

fn push_state(root: &Path) -> PathBuf {
    brief_dir(root).join("_推送.json")
}

fn claim_path(day: &str, root: &Path) -> PathBuf {
    brief_dir(root).join(format!("{day}.push"))
}

fn sent_path(day: &str, root: &Path) -> PathBuf {
    brief_dir(root).join(format!("{day}.sent.json"))
}

fn now_stamp() -> String {
    util::local_fmt("%Y-%m-%d %H:%M:%S")
}

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Python 真值：None/0/""/[]/{} 为假。
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn int_of(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f.trunc() as i64))
            .unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse::<i64>().unwrap_or(0),
        Some(Value::Bool(b)) => {
            if *b {
                1
            } else {
                0
            }
        }
        _ => 0,
    }
}

/// `[str(x) for x in (v or []) if x]`：数组、字符串（按字符）、对象（按键）。
fn str_list(v: Option<&Value>) -> Vec<String> {
    let mut out = Vec::new();
    match v {
        Some(Value::Array(a)) => {
            for x in a {
                if truthy(x) {
                    out.push(util::py_str(x));
                }
            }
        }
        Some(Value::String(s)) => {
            for c in s.chars() {
                out.push(c.to_string());
            }
        }
        Some(Value::Object(o)) => {
            for k in o.keys() {
                out.push(k.clone());
            }
        }
        _ => {}
    }
    out
}

// ---------------------------------------------------------------- 账

/// `_sent_state`：今天哪几段已经发成功、试过几跳。
fn sent_state(day: &str, root: &Path) -> (Vec<String>, i64) {
    let st = util::read_json(&sent_path(day, root)).unwrap_or(Value::Null);
    let ids = str_list(st.get("ids"));
    let tries = int_of(st.get("tries"));
    (ids, tries)
}

/// `_save_sent`：`json.dumps` 默认分隔符、单行、ensure_ascii=False。
fn save_sent(day: &str, root: &Path, ids: &[String], tries: i64) {
    let uniq: BTreeSet<String> = ids.iter().filter(|x| !x.is_empty()).cloned().collect();
    let body = json!({
        "ids": uniq.into_iter().collect::<Vec<_>>(),
        "tries": tries,
        "at": now_stamp(),
    });
    let _ = crate::jsonio::atomic_write(&sent_path(day, root), &util::py_json_compact(&body));
}

/// `_claim`：`O_CREAT|O_EXCL`。建不出来（权限/磁盘）时返回 true ——
/// 宁可重一条也别挡着推。
fn claim(day: &str, root: &Path) -> bool {
    let p = claim_path(day, root);
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&p)
    {
        Ok(mut f) => {
            let _ = write!(f, "{}", epoch_secs());
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(_) => true,
    }
}

fn write_push_state(root: &Path, day: &str) {
    let body = json!({"last": day, "at": now_stamp()});
    let _ = crate::jsonio::atomic_write(&push_state(root), &util::py_json_compact(&body));
}

fn push_done(root: &Path, today: &str) -> bool {
    util::read_json(&push_state(root))
        .and_then(|v| v.get("last").cloned())
        .map(|v| util::py_str(&v) == today)
        .unwrap_or(false)
}

/// Python `log()`：`{"ts": ..., **entry}` 追加到 pocket.jsonl。
/// w5d 没有通用的 log 函数，这里按同一形状落盘并打一行 stderr。
fn log_event(ctx: &Ctx, entry: Map<String, Value>) {
    let base = if ctx.frozen {
        ctx.state_dir.clone()
    } else {
        ctx.app_dir.clone()
    };
    let mut rec = Map::new();
    rec.insert(
        "ts".into(),
        json!(util::local_fmt("%Y-%m-%dT%H:%M:%S%z")),
    );
    for (k, v) in entry {
        rec.insert(k, v);
    }
    let v = Value::Object(rec.clone());
    util::append_jsonl(&base.join("pocket.jsonl"), &v);
    eprintln!("[moshan-rs] {}", util::py_json_compact(&v));
}

// ---------------------------------------------------------------- 一跳

/// 跑一跳。返回值只给测试/日志看：
/// done / wait / busy / retry / giveup。
pub fn tick(ctx: &Ctx) -> &'static str {
    let now = chrono::Local::now();
    if !(PUSH_FROM..PUSH_TO).contains(&now.hour()) {
        return "wait";
    }
    let today = now.format("%Y-%m-%d").to_string();
    // 简报属于运行数据：分盘后它跟账目一起走，不留在装满图集的媒体盘上。
    let root = ctx.runtime_dir.clone();
    if push_done(&root, &today) {
        return "done";
    }

    let (ids, tries) = sent_state(&today, &root);
    if tries >= PUSH_TRIES {
        // 到点了就收手，只记一次 —— 否则整段窗口每分钟刷一行同样的放弃。
        if tries == PUSH_TRIES {
            save_sent(&today, &root, &ids, PUSH_TRIES + 1);
            log_event(
                ctx,
                Map::from_iter([
                    ("event".to_string(), json!("brief_push_giveup")),
                    ("day".to_string(), json!(today)),
                    ("sent".to_string(), json!(ids.len())),
                    ("tries".to_string(), json!(PUSH_TRIES)),
                ]),
            );
        }
        return "giveup";
    }

    // 牌子可能上一跳就抢到了（发了一半失败），那就跳过抢占直接续推。
    if !claim_path(&today, &root).exists() && !claim(&today, &root) {
        return "busy";
    }

    let data = brief::build(ctx, &today, false);
    let sections: Vec<Value> = data
        .get("sections")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|s| truthy(s.get("id").unwrap_or(&Value::Null)))
        .collect();
    let todo: Vec<String> = sections
        .iter()
        .map(|s| util::py_str(s.get("id").unwrap_or(&Value::Null)))
        .filter(|id| !ids.iter().any(|x| x == id))
        .collect();

    if todo.is_empty() {
        // 全发过了，只是没来得及落账（进程被 kill）。
        write_push_state(&root, &today);
        log_event(
            ctx,
            Map::from_iter([
                ("event".to_string(), json!("brief_push")),
                ("day".to_string(), json!(today)),
                ("sent".to_string(), json!(sections.len())),
                ("resumed".to_string(), json!(true)),
            ]),
        );
        return "done";
    }

    let r = brief::push_only(&data, false, Some(&todo));
    let got: Vec<String> = r
        .get("sent")
        .and_then(|x| x.as_array())
        .map(|a| str_list(Some(&Value::Array(a.clone()))))
        .unwrap_or_default();
    let mut sent = ids.clone();
    sent.extend(got.iter().cloned());
    save_sent(&today, &root, &sent, tries + 1);
    let left: Vec<String> = todo
        .iter()
        .filter(|x| !got.iter().any(|g| g == *x))
        .cloned()
        .collect();
    let ok = r.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    if ok && left.is_empty() {
        write_push_state(&root, &today);
        let uniq: BTreeSet<&String> = sent.iter().collect();
        log_event(
            ctx,
            Map::from_iter([
                ("event".to_string(), json!("brief_push")),
                ("day".to_string(), json!(today)),
                ("sent".to_string(), json!(uniq.len())),
                ("tries".to_string(), json!(tries + 1)),
            ]),
        );
        return "done";
    }
    log_event(
        ctx,
        Map::from_iter([
            ("event".to_string(), json!("brief_push_fail")),
            ("day".to_string(), json!(today)),
            ("left".to_string(), json!(left)),
            (
                "fail".to_string(),
                r.get("fail").cloned().unwrap_or(Value::Null),
            ),
            ("tries".to_string(), json!(tries + 1)),
        ]),
    );
    "retry"
}

// ---------------------------------------------------------------- 调度

/// 60 秒一跳。放在监听之后由 w5d::spawn 调，慢启动不拖 `/api/ping`。
pub fn start(ctx: std::sync::Arc<Ctx>) {
    if env_flag("MOSHAN_NO_PUSH") {
        eprintln!("[moshan-rs] MOSHAN_NO_PUSH=1：已关闭简报定时推送（手动 /api/brief/push 仍可用）");
        return;
    }
    std::thread::Builder::new()
        .name("moshan-brief-push".into())
        .spawn(move || loop {
            // 结果只给测试看；业务日志在 push / giveup / fail 三处各记一条。
            let _ = tick(&ctx);
            std::thread::sleep(Duration::from_secs(TICK_SECS));
        })
        .ok();
}
