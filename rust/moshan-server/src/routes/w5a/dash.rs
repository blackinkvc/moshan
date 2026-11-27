// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：/api/dash 的组装（Python 是 dash.payload + server.dash_data 两段拼的，
//       25 个键的顺序就是字节顺序），加上 /api/trends。
// 不做：/api/trends/run（要拉起 Python 脚本 + pyexe 找解释器，留给上游）、
//       不跑自检/去重/OCR —— 那三块的 running/last 是**进程内**状态，本进程没有。
//
// 为什么字段顺序不能重排：前端虽然按名字取值，但逐字节对拍比的是整段 JSON。
// Python 那边是一个 dict 字面量从上往下插进去的，这里也必须按同一顺序 insert。
#![allow(unused_imports, dead_code)]

use axum::extract::State;
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{Datelike, Local, NaiveDateTime, TimeZone};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

use super::cards;
use super::disk as diskmod;
use super::heartbeat as hbmod;
use super::images;
use super::platform;
use super::util::{
    fnum, human, iso_of, load_json, load_obj, mtime, now_ymd, now_ymd_hms, obj, py_str, sval,
    tail_jsonl, truthy,
};
use super::worklog as wlmod;
use crate::ledger;
use crate::paths::Ctx;
use crate::routes::common::{peek, CtxState};

// ------------------------------------------------------------------ 根目录

/// `dash._root(cfg)`：allowed_roots[0]，没有就 inbox_dir 的父目录。
pub fn root_of(ctx: &Ctx) -> PathBuf {
    super::cards::root(ctx)
}

fn now_weekday() -> String {
    let wd = Local::now().weekday().num_days_from_monday();
    format!("周{}", ["一", "二", "三", "四", "五", "六", "日"][wd as usize])
}

// ------------------------------------------------------------------ 健康

fn health(root: &Path) -> Value {
    let base = root.join("05-采集").join("体检");
    let latest = load_obj(&base.join("latest.json"));
    let checks = match latest.get("checks") {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    };
    let lr = match checks.get("last_run") {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    };
    let tp = match checks.get("throughput") {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    };
    let src = match checks.get("sources") {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    };
    let mut hist: Vec<Value> = Vec::new();
    let hp = base.join("history.jsonl");
    if hp.exists() {
        if let Ok(text) = std::fs::read_to_string(&hp) {
            let lines: Vec<&str> = text.trim().lines().collect();
            let start = lines.len().saturating_sub(12);
            for ln in &lines[start..] {
                if let Ok(v) = serde_json::from_str::<Value>(ln) {
                    hist.push(v);
                }
            }
        }
    }
    let anomaly = match latest.get("anomalies") {
        Some(Value::Array(a)) => a.iter().take(5).cloned().collect(),
        _ => Vec::new(),
    };
    let hist_tail: Vec<Value> = {
        let start = hist.len().saturating_sub(8);
        hist[start..]
            .iter()
            .map(|h| {
                json!({
                    "at": super::util::str_or(h.get("checked_at")),
                    "status": super::util::str_or(h.get("status")),
                    "anomalies": h.get("anomaly_count").cloned().unwrap_or(json!(0)),
                })
            })
            .collect()
    };
    let mut m = Map::new();
    m.insert(
        "status".into(),
        {
            let s = super::util::str_or(latest.get("status"));
            if s.is_empty() { sval("unknown") } else { sval(&s) }
        },
    );
    m.insert("at".into(), super::util::or_empty(latest.get("checked_at")));
    m.insert("anomaly_count".into(), json!(super::util::int_of(latest.get("anomaly_count"))));
    m.insert("anomalies".into(), Value::Array(anomaly));
    m.insert("last_run_at".into(), super::util::or_empty(lr.get("last_run_at")));
    m.insert("last_status".into(), super::util::or_empty(lr.get("last_status")));
    m.insert("age_minutes".into(), lr.get("age_minutes").cloned().unwrap_or(Value::Null));
    m.insert("overdue".into(), Value::Bool(lr.get("overdue").map(truthy).unwrap_or(false)));
    m.insert(
        "new_items".into(),
        tp.get("this_round")
            .and_then(|v| v.get("new_items"))
            .cloned()
            .unwrap_or(Value::Null),
    );
    m.insert(
        "prev_new_items".into(),
        tp.get("previous_round")
            .and_then(|v| v.get("new_items"))
            .cloned()
            .unwrap_or(Value::Null),
    );
    m.insert("qdrant_ok".into(), tp.get("qdrant").and_then(|v| v.get("ok")).cloned().unwrap_or(Value::Null));
    m.insert(
        "qdrant_points".into(),
        tp.get("qdrant").and_then(|v| v.get("points_count")).cloned().unwrap_or(Value::Null),
    );
    m.insert(
        "sources".into(),
        json!({
            "failed": match src.get("failed") { Some(Value::Array(a)) => a.len(), _ => 0 },
            "kept": src.get("counts").and_then(|c| c.get("kept")).cloned().unwrap_or(Value::Null),
            "duplicate": src.get("counts").and_then(|c| c.get("duplicate")).cloned().unwrap_or(Value::Null),
        }),
    );
    m.insert("history".into(), Value::Array(hist_tail));
    Value::Object(m)
}

// ------------------------------------------------------------------ 报告 / 巡检

fn report(root: &Path) -> Value {
    let base = root.join("10-系统记录").join("使用报告");
    let idx = base.join("_索引.json");
    let mut out = Map::new();
    out.insert("count".into(), json!(0));
    out.insert("latest".into(), Value::Null);
    out.insert("updated_at".into(), sval(""));
    out.insert("recent".into(), json!([]));
    if !idx.exists() {
        return Value::Object(out);
    }
    let data = match load_json(&idx) {
        Some(v) => v,
        None => return Value::Object(out),
    };
    let items: Vec<Value> = match data.get("items") {
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    };
    out.insert("count".into(), json!(items.len()));
    out.insert("updated_at".into(), super::util::or_empty(data.get("updated_at")));
    let days: Vec<Value> = items
        .iter()
        .filter(|i| {
            i.get("kind").and_then(|k| k.as_str()) == Some("day")
                && truthy(i.get("key").unwrap_or(&Value::Null))
        })
        .cloned()
        .collect();
    if let Some(top) = days
        .iter()
        .max_by(|a, b| super::util::str_or(a.get("key")).cmp(&super::util::str_or(b.get("key"))))
    {
        out.insert(
            "latest".into(),
            json!({
                "key": super::util::str_or(top.get("key")),
                "file": super::util::str_or(top.get("file")),
                "head": super::util::str_or(top.get("head")),
            }),
        );
    }
    let recent: Vec<Value> = items
        .iter()
        .take(6)
        .map(|i| {
            json!({
                "kind": i.get("kind").cloned().unwrap_or(Value::Null),
                "key": i.get("key").cloned().unwrap_or(Value::Null),
                "file": i.get("file").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    out.insert("recent".into(), Value::Array(recent));
    Value::Object(out)
}

/// `_human_span`：秒 → 「2 天 3 小时」。
fn human_span(sec: f64) -> String {
    let sec = sec.max(0.0) as i64;
    let d = sec / 86400;
    let mut r = sec % 86400;
    let h = r / 3600;
    r %= 3600;
    let m = r / 60;
    if d != 0 {
        format!("{} 天 {} 小时", d, h)
    } else if h != 0 {
        format!("{} 小时 {} 分", h, m)
    } else if m != 0 {
        format!("{} 分", m)
    } else {
        "刚刚".to_string()
    }
}

/// `_short_title`：从原因里截一句短标题。
fn short_title(reason: &str, limit: usize) -> String {
    let s = reason.trim().replace('\n', " ");
    for sep in ["。", "；", "：", "（", "(", ";", "，", ","] {
        if let Some(i) = s.find(sep) {
            let n = s[..i].chars().count();
            if n > 0 && n <= limit {
                return s[..i].to_string();
            }
        }
    }
    let s0 = s.split('。').next().unwrap_or("").to_string();
    if s0.chars().count() > limit {
        format!("{}…", s0.chars().take(limit).collect::<String>())
    } else if s0.is_empty() {
        "巡查中断".to_string()
    } else {
        s0
    }
}

fn parse_dt(s: &str) -> Option<chrono::DateTime<Local>> {
    NaiveDateTime::parse_from_str(s.get(..19)?, "%Y-%m-%d %H:%M:%S")
        .ok()
        .and_then(|n| Local.from_local_datetime(&n).single())
}

fn inspect(root: &Path) -> Value {
    let base = root.join("10-系统记录").join("巡检");
    let mut rows_at: Vec<(String, Map<String, Value>)> = Vec::new();

    for r in tail_jsonl(&base.join("历史.jsonl"), 400) {
        let at = super::util::str_or(r.get("at"));
        if at.is_empty() {
            continue;
        }
        let i = match rows_at.iter().position(|(k, _)| *k == at) {
            Some(i) => i,
            None => {
                rows_at.push((at.clone(), {
                    let mut m = Map::new();
                    m.insert("at".into(), sval(&at));
                    m.insert("kind".into(), sval("check"));
                    m
                }));
                rows_at.len() - 1
            }
        };
        let d = &mut rows_at[i].1;
        let get = |k: &str| r.get(k).cloned().unwrap_or(Value::Null);
        d.insert("passed".into(), get("passed"));
        d.insert("total".into(), get("total"));
        d.insert("disk".into(), r.get("disk_pct").cloned().unwrap_or(Value::Null));
        d.insert("mem".into(), r.get("mem_pct").cloned().unwrap_or(Value::Null));
        d.insert("load".into(), r.get("load1").cloned().unwrap_or(Value::Null));
        d.insert("up".into(), r.get("services_up").cloned().unwrap_or(Value::Null));
        d.insert("svc".into(), r.get("services_total").cloned().unwrap_or(Value::Null));
        d.insert(
            "failed".into(),
            match r.get("failed") {
                Some(Value::Array(a)) => Value::Array(a.iter().take(3).cloned().collect()),
                _ => json!([]),
            },
        );
        d.insert(
            "down_svc".into(),
            match r.get("services_down") {
                Some(Value::Array(a)) => Value::Array(a.iter().take(4).cloned().collect()),
                _ => json!([]),
            },
        );
        d.insert("ok".into(), Value::Bool(r.get("ok").map(truthy).unwrap_or(false)));
        d.insert("src".into(), super::util::or_empty(r.get("source")));
    }

    for r in tail_jsonl(&base.join("修复记录.jsonl"), 400) {
        let kind = {
            let k = super::util::str_or(r.get("kind"));
            if k.is_empty() { "round".to_string() } else { k }
        };
        if kind != "round" {
            continue;
        }
        let at = super::util::str_or(r.get("at"));
        if at.is_empty() {
            continue;
        }
        let i = match rows_at.iter().position(|(k, _)| *k == at) {
            Some(i) => i,
            None => {
                rows_at.push((at.clone(), {
                    let mut m = Map::new();
                    m.insert("at".into(), sval(&at));
                    m.insert("kind".into(), sval("round"));
                    m
                }));
                rows_at.len() - 1
            }
        };
        let unresolved: Vec<Value> = match r.get("unresolved") {
            Some(Value::Array(a)) => a
                .iter()
                .take(3)
                .map(|u| {
                    if u.is_object() {
                        sval(&super::util::str_or(u.get("name")))
                    } else {
                        sval(&py_str(u))
                    }
                })
                .collect(),
            _ => Vec::new(),
        };
        let d = &mut rows_at[i].1;
        d.insert("passed".into(), r.get("checks_passed").cloned().unwrap_or(Value::Null));
        d.insert("total".into(), r.get("checks_total").cloned().unwrap_or(Value::Null));
        d.insert("up".into(), r.get("services_up").cloned().unwrap_or(Value::Null));
        d.insert("svc".into(), r.get("services_total").cloned().unwrap_or(Value::Null));
        d.insert("fixed".into(), json!(super::util::int_of(r.get("fixed"))));
        d.insert(
            "bad".into(),
            json!(super::util::int_of(r.get("failed")) + super::util::int_of(r.get("manual"))),
        );
        d.insert("unresolved".into(), Value::Array(unresolved));
        d.insert(
            "src".into(),
            sval(if r.get("heal").map(truthy).unwrap_or(false) { "巡检 + 自愈" } else { "只检查" }),
        );
    }

    let mut reps: Vec<(String, String)> = Vec::new();
    let rdir = base.join("报告");
    if let Ok(rd) = std::fs::read_dir(&rdir) {
        let mut files: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                let n = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                n.starts_with('2') && n.ends_with(".md")
            })
            .collect();
        files.sort();
        let start = files.len().saturating_sub(80);
        for p in &files[start..] {
            let k = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            let ch: Vec<char> = k.chars().collect();
            if ch.len() == 15 && ch[8] == '-' {
                let key = format!(
                    "{}-{}-{} {}:{}:{}",
                    ch[0..4].iter().collect::<String>(),
                    ch[4..6].iter().collect::<String>(),
                    ch[6..8].iter().collect::<String>(),
                    ch[9..11].iter().collect::<String>(),
                    ch[11..13].iter().collect::<String>(),
                    ch[13..15].iter().collect::<String>()
                );
                reps.push((key, p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()));
            }
        }
    }

    for (at, d) in rows_at.iter_mut() {
        if let Some((_, f)) = reps.iter().find(|(k, _)| k == at) {
            d.insert("report".into(), sval(f));
        }
        if super::util::str_or(d.get("kind")) == "round" {
            let bad = super::util::int_of(d.get("bad"));
            let fixed = super::util::int_of(d.get("fixed"));
            if bad != 0 {
                d.insert("level".into(), sval("bad"));
                d.insert("verdict".into(), sval(&format!("{} 项没修好", bad)));
            } else if fixed != 0 {
                d.insert("level".into(), sval("warn"));
                d.insert("verdict".into(), sval(&format!("修了 {} 项", fixed)));
            } else {
                d.insert("level".into(), sval("ok"));
                d.insert("verdict".into(), sval("通过"));
            }
        } else {
            let ok = d.get("ok").map(truthy).unwrap_or(false);
            d.insert("level".into(), sval(if ok { "ok" } else { "bad" }));
            d.insert("verdict".into(), sval(if ok { "通过" } else { "有异常" }));
        }
        let mut parts: Vec<String> = Vec::new();
        let p = d.get("passed").cloned().unwrap_or(Value::Null);
        let t = d.get("total").cloned().unwrap_or(Value::Null);
        if !p.is_null() && truthy(&t) {
            parts.push(format!("{}/{} 检查", py_str(&p), py_str(&t)));
        }
        let up = d.get("up").cloned().unwrap_or(Value::Null);
        let svc = d.get("svc").cloned().unwrap_or(Value::Null);
        if !up.is_null() && truthy(&svc) {
            parts.push(format!("常驻 {}/{}", py_str(&up), py_str(&svc)));
        }
        d.insert("brief".into(), sval(&parts.join(" · ")));
    }

    let mut rows: Vec<Value> = rows_at.into_iter().map(|(_, m)| Value::Object(m)).collect();
    rows.sort_by(|a, b| super::util::str_or(b.get("at")).cmp(&super::util::str_or(a.get("at"))));
    rows.truncate(40);

    let downs_src = tail_jsonl(&base.join("宕机.jsonl"), 400);
    let start = downs_src.len().saturating_sub(6);
    let downs: Vec<Value> = downs_src[start..]
        .iter()
        .map(|r| {
            let reason = super::util::str_or(r.get("reason")).trim().to_string();
            let title = {
                let t = super::util::str_or(r.get("title")).trim().to_string();
                if t.is_empty() { short_title(&reason, 12) } else { t }
            };
            let duration = {
                let d = super::util::str_or(r.get("duration"));
                if d.is_empty() {
                    human_span(r.get("seconds").and_then(|v| v.as_f64()).unwrap_or(0.0))
                } else {
                    d
                }
            };
            json!({
                "from": super::util::str_or(r.get("from")),
                "to": super::util::str_or(r.get("to")),
                "duration": duration,
                "seconds": super::util::int_of(r.get("seconds")),
                "title": title,
                "reason": reason,
                "fix": super::util::str_or(r.get("fix")).trim(),
                "source": super::util::str_or(r.get("source")),
            })
        })
        .collect();

    let alive = load_obj(&base.join("在线.json"));
    let alive_at = super::util::str_or(alive.get("at"));
    let mut up_span = String::new();
    let mut fresh = false;
    if !alive_at.is_empty() {
        if let Some(t) = parse_dt(&alive_at) {
            fresh = (Local::now() - t).num_seconds() < 180;
        }
    }
    if truthy(alive.get("started").unwrap_or(&Value::Null)) {
        if let Some(t) = parse_dt(&super::util::str_or(alive.get("started"))) {
            up_span = human_span((Local::now() - t).num_seconds() as f64);
        }
    }

    let mut m = Map::new();
    m.insert("rows".into(), Value::Array(rows));
    m.insert("downs".into(), Value::Array(downs));
    m.insert("reports".into(), json!(reps.len()));
    m.insert(
        "alive".into(),
        json!({
            "at": alive_at,
            "pid": alive.get("pid").cloned().unwrap_or(Value::Null),
            "started": super::util::or_empty(alive.get("started")),
            "always": alive.get("always").map(truthy).unwrap_or(false),
            "up": up_span,
            "fresh": fresh,
        }),
    );
    Value::Object(m)
}

// ------------------------------------------------------------------ 新闻

fn trends_cfg_root(ctx: &Ctx) -> PathBuf {
    if let Some(Value::String(s)) = ctx.cfg.get("ai_trends").and_then(|v| v.get("root")) {
        return PathBuf::from(s);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join("Documents").join("AI项目")
}

fn runs_dir(ctx: &Ctx) -> PathBuf {
    trends_cfg_root(ctx).join("10-系统记录").join("新闻").join("runs")
}

fn read_run(ctx: &Ctx, day: &str) -> Value {
    let p = runs_dir(ctx).join(format!("{}.json", day));
    if !p.exists() {
        return json!({});
    }
    let mut d = match load_json(&p) {
        Some(Value::Object(m)) => m,
        _ => return json!({}),
    };
    d.insert("_path".into(), sval(&p.to_string_lossy()));
    d.insert(
        "_dir".into(),
        sval(&format!(
            "{}/{}",
            trends_cfg_root(ctx).join("01-研究").join("普通").join("技术动向").to_string_lossy(),
            day
        )),
    );
    Value::Object(d)
}

fn trends_latest(ctx: &Ctx) -> Value {
    let rd = runs_dir(ctx);
    if !rd.is_dir() {
        return json!({});
    }
    let mut files: Vec<PathBuf> = match std::fs::read_dir(&rd) {
        Ok(r) => r.flatten().map(|e| e.path()).filter(|p| p.extension().map(|x| x == "json").unwrap_or(false)).collect(),
        Err(_) => return json!({}),
    };
    files.sort_by(|a, b| {
        b.file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .cmp(&a.file_stem().map(|s| s.to_string_lossy().to_string()))
    });
    for p in files.iter().take(7) {
        let stem = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let d = read_run(ctx, &stem);
        if d.as_object().map(|m| !m.is_empty()).unwrap_or(false) {
            return d;
        }
    }
    json!({})
}

fn trends_history(ctx: &Ctx, limit: usize) -> Vec<Value> {
    let rd = runs_dir(ctx);
    if !rd.is_dir() {
        return Vec::new();
    }
    let mut files: Vec<PathBuf> = match std::fs::read_dir(&rd) {
        Ok(r) => r.flatten().map(|e| e.path()).filter(|p| p.extension().map(|x| x == "json").unwrap_or(false)).collect(),
        Err(_) => return Vec::new(),
    };
    files.sort_by(|a, b| {
        b.file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .cmp(&a.file_stem().map(|s| s.to_string_lossy().to_string()))
    });
    let mut out = Vec::new();
    for p in files.iter().take(limit) {
        let stem = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let d = read_run(ctx, &stem);
        if d.as_object().map(|m| m.is_empty()).unwrap_or(true) {
            continue;
        }
        let s = d.get("stats").cloned().unwrap_or_else(|| json!({}));
        let items = match s.get("day_total") {
            Some(v) if truthy(v) => v.clone(),
            _ => s.get("new").cloned().unwrap_or(json!(0)),
        };
        let date = {
            let x = super::util::str_or(d.get("date"));
            if x.is_empty() { stem.clone() } else { x }
        };
        let status = {
            let x = super::util::str_or(d.get("status"));
            if x.is_empty() { "?".to_string() } else { x }
        };
        out.push(json!({
            "date": date,
            "status": status,
            "items": items,
            "seen": s.get("seen").cloned().unwrap_or(json!(0)),
            "finished_at": super::util::or_empty(d.get("finished_at")),
            "unavailable": d.get("unavailable").cloned().unwrap_or(json!([])),
        }));
    }
    out
}

fn already_done(ctx: &Ctx, day: &str) -> bool {
    let d = read_run(ctx, day);
    let st = super::util::str_or(d.get("status"));
    st == "ok" || st == "partial"
}

fn next_run_at(ctx: &Ctx) -> String {
    let hour = ctx
        .cfg
        .get("ai_trends")
        .and_then(|v| v.get("hour"))
        .and_then(|v| v.as_i64())
        .unwrap_or(9);
    let now = Local::now();
    let mut nxt = now
        .date_naive()
        .and_hms_opt(hour as u32, 0, 0)
        .and_then(|n| Local.from_local_datetime(&n).single())
        .unwrap_or(now);
    if nxt <= now {
        nxt += chrono::Duration::days(1);
    }
    nxt.format("%Y-%m-%d %H:%M").to_string()
}

/// `_trends`：工作台里那张新闻卡。
fn trends_block(ctx: &Ctx) -> Value {
    let latest = trends_latest(ctx);
    let mut srcs: Vec<Value> = Vec::new();
    if let Some(Value::Array(list)) = latest.get("sources") {
        for s in list {
            srcs.push(json!({
                "source": super::util::str_or(s.get("source")),
                "ok": s.get("ok").map(truthy).unwrap_or(false),
                "items": s.get("items").cloned().unwrap_or(json!(0)),
                "detail": super::util::str_or(s.get("detail")),
            }));
        }
    }
    let stats = latest.get("stats").cloned().unwrap_or_else(|| json!({}));
    let mut stat_m = Map::new();
    for k in ["new", "day_total", "news", "github", "hf", "crowd"] {
        stat_m.insert(k.to_string(), stats.get(k).cloned().unwrap_or(Value::Null));
    }
    let failed: Vec<Value> = srcs
        .iter()
        .filter(|s| !s.get("ok").map(truthy).unwrap_or(false))
        .map(|s| sval(&super::util::str_or(s.get("source"))))
        .collect();
    let is_empty = latest.as_object().map(|m| m.is_empty()).unwrap_or(true);
    let mut m = Map::new();
    m.insert("date".into(), super::util::or_empty(latest.get("date")));
    m.insert(
        "status".into(),
        {
            let s = super::util::str_or(latest.get("status"));
            if !s.is_empty() {
                sval(&s)
            } else if is_empty {
                sval("未跑")
            } else {
                sval("")
            }
        },
    );
    m.insert("stats".into(), Value::Object(stat_m));
    m.insert("sources".into(), Value::Array(srcs));
    m.insert("failed_sources".into(), Value::Array(failed));
    m.insert("next_at".into(), sval(&next_run_at(ctx)));
    m.insert("today_done".into(), Value::Bool(already_done(ctx, &now_ymd())));
    m.insert("running".into(), Value::Bool(false));
    m.insert("output".into(), super::util::or_empty(latest.get("output")));
    m.insert(
        "llm_ok".into(),
        latest.get("llm").and_then(|v| v.get("ok")).cloned().unwrap_or(Value::Null),
    );
    m.insert("degraded".into(), latest.get("degraded").cloned().unwrap_or(json!([])));
    m.insert(
        "history".into(),
        Value::Array(trends_history(ctx, 7)),
    );
    Value::Object(m)
}

// ------------------------------------------------------------------ 去重 / OCR

fn dedupe_block() -> Value {
    // 账在 dedupe_runner.STATE（进程内）。本进程没跑过去重，就按「没跑过」出账。
    json!({
        "at": "",
        "groups": 0,
        "freed_h": human(0.0),
        "dirs": Value::Null,
        "images": Value::Null,
        "running": false,
        "ms": Value::Null,
    })
}

/// `dash._heartbeat()`：Python 读进程内 heartbeat.STATE。Rust 的调度器把同一份
/// 状态写在 crate::live；只有「本进程还没跑过心跳」（例如 MOSHAN_NO_SCHED=1）时，
/// 才退回读 Python 落盘的 最新.json，免得卡片整块空掉。
fn heartbeat_block(root: &Path) -> Value {
    let live = crate::live::heartbeat_dash();
    let empty = live.get("ok").map(|v| v.is_null()).unwrap_or(true)
        && super::util::str_or(live.get("at")).is_empty();
    if empty {
        return hbmod::heartbeat_at(&root.join("10-系统记录"));
    }
    live
}

fn ocr_block() -> Value {
    // ocr_runner.STATE 是进程内状态，现在由 w5d 的调度线程写进 crate::live。
    // 没开调度（MOSHAN_NO_SCHED=1）时保持 Python 的初值形状（at 空、计数 0）。
    crate::live::ocr_dash()
}

// ------------------------------------------------------------------ 流水

fn flow_dir(root: &Path) -> PathBuf {
    root.join("09-个人工作台").join("流水")
}

fn flow_src_path(root: &Path) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    let live = PathBuf::from(home)
        .join("Documents")
        .join("生活区")
        .join("工作流水")
        .join("工作流水202604-.md");
    if live.exists() {
        return live;
    }
    flow_dir(root).join("原始流水.md")
}

/// `_flow_key`：去掉空白与首尾标点。
fn flow_key(t: &str) -> String {
    let s: String = t.chars().filter(|c| !c.is_whitespace()).collect();
    s.trim_matches(|c: char| "，,、：:;；。. ".contains(c)).to_string()
}

/// `_flow_kinds`：group / trip / task。
fn flow_kinds(items: &mut [Value]) {
    for i in 0..items.len() {
        let lv = super::util::int_of(items[i].get("level"));
        let has_child = items
            .get(i + 1)
            .map(|n| super::util::int_of(n.get("level")) > lv)
            .unwrap_or(false);
        let text = super::util::str_or(items[i].get("text")).trim().to_string();
        let cat = super::util::str_or(items[i].get("cat")).trim().to_string();
        let kind = if has_child || (!cat.is_empty() && text == cat) {
            "group"
        } else if cat == "行程" || text.starts_with("行程") {
            "trip"
        } else {
            "task"
        };
        if let Value::Object(m) = &mut items[i] {
            m.insert("kind".into(), sval(kind));
        }
    }
}

fn flow_counts(days: &[Value]) -> Map<String, Value> {
    let mut items: Vec<&Value> = Vec::new();
    for d in days {
        if let Some(Value::Array(a)) = d.get("items") {
            items.extend(a.iter());
        }
    }
    let tasks: Vec<&&Value> = items
        .iter()
        .filter(|it| {
            let k = super::util::str_or(it.get("kind"));
            k == "task" || k.is_empty()
        })
        .collect();
    let mut m = Map::new();
    m.insert("days".into(), json!(days.len()));
    m.insert("items".into(), json!(tasks.len()));
    m.insert(
        "done".into(),
        json!(tasks.iter().filter(|it| it.get("done").map(|v| v == &Value::Bool(true)).unwrap_or(false)).count()),
    );
    m.insert(
        "open".into(),
        json!(tasks.iter().filter(|it| it.get("done").map(|v| v == &Value::Bool(false)).unwrap_or(false)).count()),
    );
    m.insert(
        "trips".into(),
        json!(items.iter().filter(|it| super::util::str_or(it.get("kind")) == "trip").count()),
    );
    m.insert(
        "groups".into(),
        json!(items.iter().filter(|it| super::util::str_or(it.get("kind")) == "group").count()),
    );
    m
}

fn flow_summary(ctx: &Ctx) -> Value {
    let root = root_of(ctx);
    let src = flow_src_path(&root);
    // 手写 md 的解析在 09-个人工作台/流水/_build.py 里（一个 Python 脚本）。
    // 这里只做 json 兜底：底稿解析不出来时 Python 走的也是这条路。
    let d = load_obj(&flow_dir(&root).join("流水.json"));
    let mut days: Vec<Value> = match d.get("days") {
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    };
    for day in days.iter_mut() {
        if let Value::Object(m) = day {
            if let Some(Value::Array(items)) = m.get_mut("items") {
                flow_kinds(items);
            }
        }
    }
    let missing = days.is_empty();
    let dated: Vec<Value> = days
        .iter()
        .filter(|d| truthy(d.get("date").unwrap_or(&Value::Null)))
        .cloned()
        .collect();
    let undated: Vec<Value> = days
        .iter()
        .filter(|d| !truthy(d.get("date").unwrap_or(&Value::Null)))
        .map(|d| {
            let items = d.get("items").cloned().unwrap_or(json!([]));
            let count = match &items {
                Value::Array(a) => a
                    .iter()
                    .filter(|it| {
                        let k = super::util::str_or(it.get("kind"));
                        k == "task" || k.is_empty()
                    })
                    .count(),
                _ => 0,
            };
            json!({"title": super::util::str_or(d.get("title")), "items": items, "count": count})
        })
        .collect();
    let mut months: Vec<(String, Vec<Value>)> = Vec::new();
    for d in dated.iter() {
        let m = super::util::str_or(d.get("month"));
        match months.iter().position(|(k, _)| *k == m) {
            Some(i) => months[i].1.push(d.clone()),
            None => months.push((m, vec![d.clone()])),
        }
    }
    months.sort_by(|a, b| b.0.cmp(&a.0));
    let out_months: Vec<Value> = months
        .iter()
        .map(|(m, ds)| {
            let mut c = flow_counts(ds);
            let mut row = Map::new();
            row.insert("month".into(), sval(m));
            for (k, v) in c.iter() {
                row.insert(k.clone(), v.clone());
            }
            let _ = &mut c;
            Value::Object(row)
        })
        .collect();
    let with_items: Vec<Value> = dated
        .iter()
        .filter(|d| d.get("items").map(truthy).unwrap_or(false))
        .cloned()
        .collect();
    let start = with_items.len().saturating_sub(2);
    let mut recent: Vec<Value> = with_items[start..]
        .iter()
        .map(|d| {
            let items: Vec<Value> = match d.get("items") {
                Some(Value::Array(a)) => a
                    .iter()
                    .filter(|it| super::util::str_or(it.get("kind")) != "group")
                    .take(12)
                    .cloned()
                    .collect(),
                _ => Vec::new(),
            };
            let count = match d.get("items") {
                Some(Value::Array(a)) => a
                    .iter()
                    .filter(|it| {
                        let k = super::util::str_or(it.get("kind"));
                        k == "task" || k.is_empty()
                    })
                    .count(),
                _ => 0,
            };
            json!({"date": super::util::str_or(d.get("date")), "items": items, "count": count})
        })
        .collect();
    recent.reverse();

    let mut cats: Vec<(String, i64)> = Vec::new();
    for d in dated.iter() {
        if let Some(Value::Array(items)) = d.get("items") {
            for it in items {
                if super::util::str_or(it.get("kind")) == "group" {
                    continue;
                }
                let c = {
                    let c = super::util::str_or(it.get("cat"));
                    if c.is_empty() { "其他".to_string() } else { c }
                };
                match cats.iter().position(|(k, _)| *k == c) {
                    Some(i) => cats[i].1 += 1,
                    None => cats.push((c, 1)),
                }
            }
        }
    }
    cats.sort_by(|a, b| b.1.cmp(&a.1));
    let mut by_date: Vec<(String, i64)> = Vec::new();
    for d in dated.iter() {
        let date = super::util::str_or(d.get("date"));
        let n = match d.get("items") {
            Some(Value::Array(a)) => a
                .iter()
                .filter(|it| {
                    let k = super::util::str_or(it.get("kind"));
                    k == "task" || k.is_empty()
                })
                .count() as i64,
            _ => 0,
        };
        by_date.push((date, n));
    }
    let today = now_ymd();
    let mut week: Vec<Value> = Vec::new();
    for i in (0..7).rev() {
        let day = (Local::now().date_naive() - chrono::Duration::days(i))
            .format("%Y-%m-%d")
            .to_string();
        let n = by_date.iter().find(|(d, _)| *d == day).map(|(_, n)| *n).unwrap_or(0);
        week.push(json!({"date": day, "count": n}));
    }
    let src_mtime = mtime(&src);
    let total = flow_counts(&dated);
    let today_count = by_date.iter().find(|(d, _)| *d == today).map(|(_, n)| *n).unwrap_or(0);

    let mut m = Map::new();
    m.insert("missing".into(), Value::Bool(missing));
    m.insert("src_mtime".into(), if src_mtime > 0.0 { fnum(src_mtime) } else { json!(0) });
    m.insert("months".into(), Value::Array(out_months));
    m.insert("recent".into(), Value::Array(recent));
    m.insert(
        "cats".into(),
        Value::Array(cats.iter().map(|(k, v)| json!([k, v])).collect()),
    );
    m.insert("dir".into(), sval(&flow_dir(&root).to_string_lossy()));
    m.insert("src".into(), sval("json"));
    m.insert("src_path".into(), sval(&src.to_string_lossy()));
    m.insert("undated".into(), Value::Array(undated));
    m.insert("today".into(), json!({"date": today, "count": today_count}));
    m.insert("week".into(), Value::Array(week));
    for (k, v) in total.iter() {
        m.insert(k.clone(), v.clone());
    }
    Value::Object(m)
}

// ------------------------------------------------------------------ 机器章 / 终端风暴

fn hostname_full() -> String {
    platform::hostname_short()
}

fn machine_block(ctx: &Ctx) -> Value {
    let p = cards::workbench(ctx).join("machine.json");
    let d = load_obj(&p);
    let host = {
        let h = super::util::str_or(d.get("host"));
        if h.is_empty() { hostname_full() } else { h }
    };
    let cur = hostname_full();
    let foreign = {
        let short = |s: &str| s.split('.').next().unwrap_or("").trim().to_lowercase();
        if host.is_empty() || cur.is_empty() || short(&host) == short(&cur) {
            String::new()
        } else {
            host.clone()
        }
    };
    json!({"host": host, "foreign": foreign})
}

fn storm_block(ctx: &Ctx) -> Value {
    let own = cards::root(ctx).join("_本机工作");
    let doc = load_obj(&own.join("异常.json"));
    let mut items: Vec<Value> = match doc.get("items") {
        Some(Value::Array(a)) => a.iter().filter(|x| x.is_object()).cloned().collect(),
        _ => Vec::new(),
    };
    items.sort_by(|a, b| super::util::str_or(b.get("end")).cmp(&super::util::str_or(a.get("end"))));
    let total = items.len();
    let mut now_mb = 0.0f64;
    let mut base_mb = 0.0f64;
    let mut at = String::new();
    let since = Local::now().timestamp() as f64 - 3.0 * 3600.0;
    let mut rows: Vec<Value> = Vec::new();
    if let Ok(text) = std::fs::read_to_string(own.join("进程.jsonl")) {
        for ln in text.lines().rev() {
            let ln = ln.trim();
            if ln.is_empty() {
                continue;
            }
            let d: Value = match serde_json::from_str(ln) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let s = super::util::str_or(d.get("at"));
            let t = match parse_dt(&s) {
                Some(t) => t.timestamp() as f64,
                None => continue,
            };
            if t < since {
                break;
            }
            rows.push(d);
        }
        rows.reverse();
    }
    if let Some(last) = rows.last() {
        at = super::util::str_or(last.get("at"));
        now_mb = round1(term_of(last).0);
        let tv: Vec<f64> = rows
            .iter()
            .filter(|r| r.get("terms").map(|t| t.is_object()).unwrap_or(false))
            .map(|r| {
                r.get("terms")
                    .and_then(|t| t.get("mb"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0)
            })
            .collect();
        if !tv.is_empty() {
            base_mb = round1(tv.iter().cloned().fold(f64::INFINITY, f64::min));
        }
    }
    json!({
        "items": Value::Array(items.iter().take(8).cloned().collect()),
        "total": total,
        "now_mb": fnum(now_mb),
        "base_mb": fnum(base_mb),
        "at": at,
        "sampled": !at.is_empty(),
        "host": platform::hostname_short(),
        "threshold_mb": fnum(1200.0),
        "updated_at": doc.get("updated_at").cloned().unwrap_or(json!(0)),
    })
}

fn round1(x: f64) -> f64 {
    super::util::round1(x)
}

/// `stormwatch._term_of`：一条采样里终端占多少 MB。
fn term_of(row: &Value) -> (f64, Vec<Value>) {
    if let Some(Value::Object(tm)) = row.get("terms") {
        if tm.contains_key("n") {
            let mb = tm.get("mb").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let top: Vec<Value> = match tm.get("top") {
                Some(Value::Array(a)) => a.clone(),
                _ => Vec::new(),
            };
            return (mb, top);
        }
    }
    let mut total = 0.0;
    let mut detail = Vec::new();
    if let Some(Value::Array(top)) = row.get("top") {
        for it in top {
            let name = it.get(0).map(|v| py_str(v)).unwrap_or_default();
            let mem = it.get(2).and_then(|v| v.as_f64()).unwrap_or(0.0);
            let n = name.to_lowercase();
            let n = n.strip_suffix(".exe").unwrap_or(&n).to_string();
            if ["windowsterminal", "wt", "conhost", "openconsole", "terminal", "iterm2", "iterm", "alacritty", "wezterm"].contains(&n.as_str()) {
                total += mem;
                detail.push(json!([name, round1(mem)]));
            }
        }
    }
    (total, detail)
}

// ------------------------------------------------------------------ 组装

pub fn payload(ctx: &Ctx) -> Value {
    let root = root_of(ctx);
    let _now = Local::now();
    let gallery = images::gallery(&root);
    let reader = images::reader(&root);
    let services = Value::Array(cards::services(ctx));
    let trends = trends_block(ctx);

    let mut m = Map::new();
    m.insert("now".into(), sval(&now_ymd_hms()));
    m.insert("today".into(), sval(&now_ymd()));
    m.insert("weekday".into(), sval(&now_weekday()));
    m.insert("root".into(), sval(&root.to_string_lossy()));
    m.insert("services".into(), services);
    m.insert("disk".into(), diskmod::disk(&root));
    m.insert("gallery".into(), gallery);
    m.insert("reader".into(), reader);
    m.insert("media".into(), images::media(&root));
    m.insert("llmcost".into(), images::llmcost(&root));
    m.insert("files".into(), images::files(&root));
    m.insert("health".into(), health(&root));
    m.insert("heartbeat".into(), heartbeat_block(&root));
    m.insert("trends".into(), trends);
    m.insert("dedupe".into(), dedupe_block());
    m.insert("ocr".into(), ocr_block());
    m.insert("worklog".into(), wlmod::worklog(ctx));
    m.insert("flow".into(), flow_summary(ctx));
    m.insert("report".into(), report(&root));
    m.insert("inspect".into(), inspect(&root));
    m.insert("apps".into(), cards::apps_payload(ctx));
    m.insert("notes".into(), cards::read_notes(ctx));
    Value::Object(m)
}

/// `GET /api/dash`
pub async fn dash(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let c = ctx.clone();
    match tokio::task::spawn_blocking(move || {
        let mut v = payload(&c);
        if let Value::Object(m) = &mut v {
            m.insert("ledger".into(), ledger::payload(&c));
            m.insert("machine".into(), machine_block(&c));
            m.insert("storm".into(), storm_block(&c));
        }
        v
    })
    .await
    {
        Ok(v) => Json(v).into_response(),
        Err(e) => crate::routes::common::http_err(500, &format!("internal error: {e}")),
    }
}

/// `GET /api/trends`
pub async fn trends_state(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let c = ctx.clone();
    match tokio::task::spawn_blocking(move || {
        let hour = c
            .cfg
            .get("ai_trends")
            .and_then(|v| v.get("hour"))
            .and_then(|v| v.as_i64())
            .unwrap_or(9);
        let enabled = c
            .cfg
            .get("ai_trends")
            .and_then(|v| v.get("enabled"))
            .map(|v| truthy(v))
            .unwrap_or(true);
        let day = now_ymd();
        json!({
            "hour": hour,
            "enabled": enabled,
            "model": "qwen3.5-4b-gguf:q4km",
            "next_at": next_run_at(&c),
            "today_done": already_done(&c, &day),
            "state": crate::live::trends_value(),
            "latest": trends_latest(&c),
            "history": trends_history(&c, 14),
        })
    })
    .await
    {
        Ok(v) => Json(v).into_response(),
        Err(e) => crate::routes::common::http_err(500, &format!("internal error: {e}")),
    }
}
