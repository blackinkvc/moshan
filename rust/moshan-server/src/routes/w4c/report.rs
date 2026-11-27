// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：`moshan/report.py` + `report_runner.py` + `metrics.py`（数据部分）的移植，
//       以及 4 条路由（取报告 / 清单 / 立刻补一轮 / 用默认程序打开）。
// 不做：不采样（那是 metrics 采集线程的活），不启动定时器。
//
// 报告是**会落到磁盘给人看**的 markdown，所以渲染出来的每个数字、每个空格
// 都按 Python 的 f-string 口径复刻：`round` 走 half-even、`{x}` 对整数值的浮点
// 要打成 `25.0`、`{x:.1f}` 用正确舍入。差一格就是一个满屏 diff。

#![allow(dead_code)]

use super::py;
use crate::jsonio;
use crate::paths::Ctx;
use crate::routes::common::{body_obj, http_err, peek, q, CtxState};
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{Datelike, NaiveDate, Weekday};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const KINDS: &[&str] = &["day", "week", "month", "quarter", "half", "year"];
const WEEKDAY: [&str; 7] = ["一", "二", "三", "四", "五", "六", "日"];
const BAR: &str = "█";
const BLOCKS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

fn prefix(kind: &str) -> Option<&'static str> {
    match kind {
        "day" => Some("日报"),
        "week" => Some("周报"),
        "month" => Some("月报"),
        "quarter" => Some("季报"),
        "half" => Some("半年报"),
        "year" => Some("年报"),
        _ => None,
    }
}

fn keep_of(kind: &str) -> Option<usize> {
    match kind {
        "day" => Some(30),
        "week" => Some(12),
        "month" => Some(36),
        _ => None,
    }
}

// ---------------------------------------------------------------- 路径

/// `metrics._root()`：优先取 inbox_dir 的父目录（存在时），否则资料库。
pub fn metrics_root(ctx: &Ctx) -> PathBuf {
    let inbox = ctx.inbox_dir();
    if let Some(parent) = inbox.parent() {
        if parent.exists() {
            return parent.to_path_buf();
        }
    }
    ctx.library_dir.clone()
}

fn sys_dir(ctx: &Ctx) -> PathBuf {
    metrics_root(ctx).join("10-系统记录")
}

fn snap_path(ctx: &Ctx) -> PathBuf {
    sys_dir(ctx).join("机器").join("快照.jsonl")
}

fn daily_path(ctx: &Ctx) -> PathBuf {
    sys_dir(ctx).join("使用报告").join("_数据").join("日聚合.jsonl")
}

pub fn hub(ctx: &Ctx) -> PathBuf {
    sys_dir(ctx).join("使用报告")
}

fn index_path(ctx: &Ctx) -> PathBuf {
    hub(ctx).join("_索引.json")
}

/// `paths.base_dir()`：源码态就是程序目录，打包后落状态目录。
fn base_dir(ctx: &Ctx) -> PathBuf {
    if ctx.frozen { ctx.state_dir.clone() } else { ctx.app_dir.clone() }
}

// ---------------------------------------------------------------- 周期

fn parse_date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
}

/// Python `calendar.monthrange(y,m)[1]`
fn month_len(y: i32, m: u32) -> u32 {
    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    let first_next = NaiveDate::from_ymd_opt(ny, nm, 1).unwrap();
    (first_next - chrono::Duration::days(1)).day()
}

pub fn key_of(kind: &str, d: NaiveDate) -> String {
    match kind {
        "day" => d.format("%Y-%m-%d").to_string(),
        "week" => {
            let iso = d.iso_week();
            format!("{}-W{:02}", iso.year(), iso.week())
        }
        "month" => format!("{}-{:02}", d.year(), d.month()),
        "quarter" => format!("{}-Q{}", d.year(), (d.month() - 1) / 3 + 1),
        "half" => format!("{}-H{}", d.year(), if d.month() <= 6 { 1 } else { 2 }),
        _ => d.year().to_string(),
    }
}

pub fn span_of(kind: &str, key: &str) -> Option<(NaiveDate, NaiveDate)> {
    match kind {
        "day" => {
            let d = parse_date(key)?;
            Some((d, d))
        }
        "week" => {
            let (y, w) = key.split_once("-W")?;
            let mon = NaiveDate::from_isoywd_opt(y.parse().ok()?, w.parse().ok()?, Weekday::Mon)?;
            Some((mon, mon + chrono::Duration::days(6)))
        }
        "month" => {
            let (y, m) = key.split_once('-')?;
            let (y, m): (i32, u32) = (y.parse().ok()?, m.parse().ok()?);
            Some((NaiveDate::from_ymd_opt(y, m, 1)?, NaiveDate::from_ymd_opt(y, m, month_len(y, m))?))
        }
        "quarter" => {
            let y: i32 = key.get(..4)?.parse().ok()?;
            let q: u32 = key.chars().last()?.to_digit(10)?;
            let m0 = (q - 1) * 3 + 1;
            let m1 = m0 + 2;
            Some((NaiveDate::from_ymd_opt(y, m0, 1)?, NaiveDate::from_ymd_opt(y, m1, month_len(y, m1))?))
        }
        "half" => {
            let y: i32 = key.get(..4)?.parse().ok()?;
            let h: u32 = key.chars().last()?.to_digit(10)?;
            let (m0, m1) = if h == 1 { (1, 6) } else { (7, 12) };
            Some((NaiveDate::from_ymd_opt(y, m0, 1)?, NaiveDate::from_ymd_opt(y, m1, month_len(y, m1))?))
        }
        _ => {
            let y: i32 = key.parse().ok()?;
            Some((NaiveDate::from_ymd_opt(y, 1, 1)?, NaiveDate::from_ymd_opt(y, 12, 31)?))
        }
    }
}

pub fn title_of(kind: &str, key: &str) -> String {
    let Some((a, b)) = span_of(kind, key) else { return String::new() };
    match kind {
        "day" => format!(
            "{} 月 {} 日 · 周{} · 使用日报",
            a.month(),
            a.day(),
            WEEKDAY[a.weekday().num_days_from_monday() as usize]
        ),
        "week" => {
            let w: i64 = key.split_once("-W").and_then(|(_, w)| w.parse().ok()).unwrap_or(0);
            format!("第 {w} 周 · {} 月 {}–{} 日 · 使用周报", a.month(), a.day(), b.day())
        }
        "month" => format!("{} 年 {} 月 · 使用月报", a.year(), a.month()),
        "quarter" => format!(
            "{} 年 Q{}（{}–{} 月）· 使用季报",
            a.year(),
            key.chars().last().unwrap_or('0'),
            a.month(),
            b.month()
        ),
        "half" => format!(
            "{} 年 {}半年 · 使用半年报",
            a.year(),
            if key.ends_with('1') { "上" } else { "下" }
        ),
        _ => format!("{} 年 · 使用年报", a.year()),
    }
}

/// `file_of(kind, key)` 需要 HUB；HUB 由 ctx 决定，所以这里都走显式路径。
fn file_of_in(hub: &Path, kind: &str, key: &str) -> Option<PathBuf> {
    let p = prefix(kind)?;
    Some(hub.join(format!("{p}-{key}.md")))
}

fn shift(kind: &str, d: NaiveDate, n: i64) -> NaiveDate {
    match kind {
        "day" => d - chrono::Duration::days(n),
        "week" => d - chrono::Duration::days(n * 7),
        "month" => {
            let mut y = d.year();
            let mut m = d.month() as i64 - n;
            while m <= 0 {
                m += 12;
                y -= 1;
            }
            NaiveDate::from_ymd_opt(y, m as u32, 1).unwrap_or(d)
        }
        "quarter" => {
            let mut q = ((d.month() - 1) / 3 + 1) as i64 - n;
            let mut y = d.year();
            while q <= 0 {
                q += 4;
                y -= 1;
            }
            NaiveDate::from_ymd_opt(y, ((q - 1) * 3 + 1) as u32, 1).unwrap_or(d)
        }
        "half" => {
            let mut h = (if d.month() <= 6 { 0 } else { 1 }) - n;
            let mut y = d.year();
            while h < 0 {
                h += 2;
                y -= 1;
            }
            NaiveDate::from_ymd_opt(y, if h == 0 { 1 } else { 7 }, 1).unwrap_or(d)
        }
        _ => NaiveDate::from_ymd_opt(d.year() - n as i32, 1, 1).unwrap_or(d),
    }
}

/// 最近 back 个「已结束」周期的 key，从近到远。
pub fn past(kind: &str, today: NaiveDate, back: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for i in 1..=(back as i64 * 3 + 3) {
        let d = shift(kind, today, i);
        let k = key_of(kind, d);
        if seen.contains(&k) {
            continue;
        }
        seen.push(k.clone());
        if let Some((_, b)) = span_of(kind, &k) {
            if b < today {
                out.push(k);
            }
        }
        if out.len() >= back {
            break;
        }
    }
    out
}

// ---------------------------------------------------------------- 数字格式

fn hhmm(minutes: f64) -> String {
    let m = py::py_rint(minutes);
    if m < 60 {
        return format!("{m} 分钟");
    }
    let h = m / 60;
    let mm = m % 60;
    if mm != 0 {
        format!("{h} 小时 {mm} 分")
    } else {
        format!("{h} 小时")
    }
}

fn hours(minutes: f64) -> String {
    format!("{:.1}", minutes / 60.0)
}

fn bar(minutes: f64, top: f64, width: i32) -> String {
    if top == 0.0 || minutes <= 0.0 {
        return String::new();
    }
    let n = py::py_rint(f64::min(1.0, minutes / top) * width as f64);
    if n != 0 { BAR.repeat(n as usize) } else { "·".to_string() }
}

fn spark(vals: &[f64], hi: Option<f64>) -> String {
    if vals.is_empty() {
        return String::new();
    }
    let m = vals.iter().cloned().fold(f64::MIN, f64::max);
    let top = hi.filter(|h| *h != 0.0).unwrap_or(if m != 0.0 { m } else { 1.0 });
    let mut out = String::new();
    for v in vals {
        let idx = py::py_rint(v / top * (BLOCKS.len() - 1) as f64);
        let idx = idx.clamp(0, BLOCKS.len() as i64 - 1) as usize;
        out.push_str(BLOCKS[idx]);
    }
    out
}

fn delta(a: f64, b: f64, unit: &str, nd: usize) -> String {
    let d = b - a;
    let eps = if nd == 1 { 0.05 } else { 0.005 };
    if d.abs() < eps {
        return "持平".to_string();
    }
    let arrow = if d > 0.0 { "↑" } else { "↓" };
    format!("{arrow} {:.*}{}", nd, d.abs(), unit)
}

// ---------------------------------------------------------------- JSON 取值

fn sub<'a>(v: &'a Value, k: &str) -> &'a Value {
    v.get(k).unwrap_or(&Value::Null)
}

fn sf(v: &Value, k: &str) -> f64 {
    py::f64_of(v.get(k))
}

fn ss(v: &Value, k: &str) -> String {
    py::s_of(v.get(k))
}

fn ssd(v: &Value, k: &str, k2: &str) -> String {
    py::s_of(sub(v, k).get(k2))
}

fn sfd(v: &Value, k: &str, k2: &str) -> f64 {
    py::f64_of(sub(v, k).get(k2))
}

fn numv(v: &Value) -> String {
    py::num_str(v)
}

// ---------------------------------------------------------------- 聚合

fn days_of(kind: &str, key: &str, daily: &Map<String, Value>) -> Vec<Value> {
    let Some((a, b)) = span_of(kind, key) else { return Vec::new() };
    let (a, b) = (a.format("%Y-%m-%d").to_string(), b.format("%Y-%m-%d").to_string());
    let mut keys: Vec<&String> = daily.keys().collect();
    keys.sort();
    keys.into_iter()
        .filter(|d| **d >= a && **d <= b)
        .filter_map(|d| daily.get(d).cloned())
        .collect()
}

/// `combine`：把若干天的聚合压成一个区间汇总，按天等权。
pub fn combine(recs: &[Value]) -> Value {
    if recs.is_empty() {
        return json!({});
    }
    let n = recs.len() as f64;
    let mem_avg = recs.iter().map(|r| sfd(r, "mem", "avg")).sum::<f64>() / n;
    let load_avg = recs.iter().map(|r| sfd(r, "load", "avg")).sum::<f64>() / n;
    let mut hourly = [0i64; 24];
    for r in recs {
        if let Some(arr) = r.get("hourly").and_then(|v| v.as_array()) {
            for (i, v) in arr.iter().enumerate() {
                if i < 24 {
                    hourly[i] += py::i64_of(Some(v));
                }
            }
        }
    }
    // top 进程：按出现次数加权
    let mut order: Vec<String> = Vec::new();
    let mut agg: HashMap<String, (f64, f64, i64)> = HashMap::new();
    for r in recs {
        if let Some(arr) = r.get("top").and_then(|v| v.as_array()) {
            for p in arr {
                let name = ss(p, "n");
                if name.is_empty() {
                    continue;
                }
                if !agg.contains_key(&name) {
                    order.push(name.clone());
                    agg.insert(name.clone(), (0.0, 0.0, 0));
                }
                let e = agg.get_mut(&name).unwrap();
                e.0 += sf(p, "avg") * (py::i64_of(p.get("hits")).max(1) as f64);
                e.1 = f64::max(e.1, sf(p, "max"));
                e.2 += py::i64_of(p.get("hits"));
            }
        }
    }
    let mut ranked: Vec<(String, f64, f64, i64)> = order
        .iter()
        .map(|k| {
            let (sum, mx, hits) = agg[k];
            (k.clone(), sum, mx, hits)
        })
        .collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let top: Vec<Value> = ranked
        .iter()
        .take(8)
        .map(|(k, sum, mx, hits)| {
            json!({"n": k, "avg": py::py_round(sum / (*hits).max(1) as f64, 1),
                   "max": py::py_round(*mx, 1), "hits": hits})
        })
        .collect();

    // max(recs, key=...) 等值取第一个
    let mut busiest = &recs[0];
    for r in recs {
        if sf(r, "awake_min") > sf(busiest, "awake_min") {
            busiest = r;
        }
    }
    let mut mem_peak = &recs[0];
    for r in recs {
        if sfd(r, "mem", "max") > sfd(mem_peak, "mem", "max") {
            mem_peak = r;
        }
    }
    let mut load_peak = &recs[0];
    for r in recs {
        if sfd(r, "load", "max") > sfd(load_peak, "load", "max") {
            load_peak = r;
        }
    }
    let mem_max = recs.iter().map(|r| sfd(r, "mem", "max")).fold(0.0f64, f64::max);
    let load_max = recs.iter().map(|r| sfd(r, "load", "max")).fold(0.0f64, f64::max);
    let min_free = recs
        .iter()
        .map(|r| {
            let v = sfd(r, "disk", "min_free");
            if v == 0.0 { 999.0 } else { v }
        })
        .fold(f64::MAX, f64::min);
    let disk_start = {
        let v = sfd(&recs[0], "disk", "start");
        if v == 0.0 { json!(0) } else { json!(v) }
    };
    let disk_end = {
        let v = sfd(&recs[recs.len() - 1], "disk", "end");
        if v == 0.0 { json!(0) } else { json!(v) }
    };
    let cpus = recs
        .iter()
        .find(|r| py::i64_of(r.get("cpus")) != 0)
        .map(|r| py::i64_of(r.get("cpus")))
        .unwrap_or(10);
    json!({
        "days": recs.len(),
        "awake": recs.iter().map(|r| py::i64_of(r.get("awake_min"))).sum::<i64>(),
        "mem_avg": py::py_round(mem_avg, 1),
        "mem_max": py::py_round(mem_max, 1),
        "mem_max_day": ss(mem_peak, "date"),
        "load_avg": py::py_round(load_avg, 2),
        "load_max": py::py_round(load_max, 2),
        "load_max_day": ss(load_peak, "date"),
        "disk_start": disk_start,
        "disk_end": disk_end,
        "min_free": min_free,
        "boots": recs.iter().map(|r| py::i64_of(r.get("boots"))).sum::<i64>(),
        "hourly": hourly.to_vec(),
        "top": top,
        "busiest": {"date": busiest.get("date").cloned().unwrap_or(Value::Null),
                    "awake": py::i64_of(busiest.get("awake_min"))},
        "cpus": cpus,
    })
}

// ---------------------------------------------------------------- 产出统计

fn pocket_events(ctx: &Ctx, a: &str, b: &str) -> Map<String, Value> {
    let mut out = Map::new();
    for k in ["upload", "images", "galleries", "articles", "text", "chat"] {
        out.insert(k.to_string(), json!(0));
    }
    let p = base_dir(ctx).join("pocket.jsonl");
    if !p.exists() {
        return out;
    }
    let Ok(text) = std::fs::read_to_string(&p) else { return out };
    for line in text.split('\n') {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(e) = serde_json::from_str::<Value>(line) else { continue };
        let ts: String = py::s_of(e.get("ts")).chars().take(10).collect();
        if ts.is_empty() || ts.as_str() < a || ts.as_str() > b {
            continue;
        }
        match py::s_of(e.get("event")).as_str() {
            "upload" => bump(&mut out, "upload", py::i64_of(e.get("saved"))),
            "link_done" if py::s_of(e.get("state")) == "done" => {
                bump(&mut out, "galleries", 1);
                bump(&mut out, "images", py::i64_of(e.get("images")));
            }
            "article_done" if py::s_of(e.get("state")) == "done" => bump(&mut out, "articles", 1),
            "chat" => bump(&mut out, "chat", 1),
            _ => {}
        }
    }
    out
}

fn bump(m: &mut Map<String, Value>, k: &str, d: i64) {
    let cur = py::i64_of(m.get(k));
    m.insert(k.to_string(), json!(cur + d));
}

fn flow_days(ctx: &Ctx, a: &str, b: &str) -> (i64, i64, i64) {
    let p = metrics_root(ctx)
        .join("09-个人工作台")
        .join("流水")
        .join("流水.json");
    if !p.exists() {
        return (0, 0, 0);
    }
    let Some(data) = jsonio::read_json(&p) else { return (0, 0, 0) };
    let mut days = 0;
    let mut items = 0;
    let mut done = 0;
    if let Some(arr) = data.get("days").and_then(|v| v.as_array()) {
        for day in arr {
            let d = py::s_of(day.get("date"));
            if d.is_empty() || d.as_str() < a || d.as_str() > b {
                continue;
            }
            let its = day.get("items").and_then(|v| v.as_array()).cloned().unwrap_or_default();
            days += 1;
            items += its.len() as i64;
            done += its
                .iter()
                .filter(|it| it.get("done").map(|v| v.as_bool() == Some(true)).unwrap_or(false))
                .count() as i64;
        }
    }
    (items, done, days)
}

fn output_block(ctx: &Ctx, a: &str, b: &str) -> String {
    let ev = pocket_events(ctx, a, b);
    let (items, done, days) = flow_days(ctx, a, b);
    let mut bits: Vec<String> = Vec::new();
    let g = py::i64_of(ev.get("galleries"));
    if g != 0 {
        bits.push(format!("存图 {g} 组（{} 张）", py::i64_of(ev.get("images"))));
    }
    let ar = py::i64_of(ev.get("articles"));
    if ar != 0 {
        bits.push(format!("存文章 {ar} 篇"));
    }
    let up = py::i64_of(ev.get("upload"));
    if up != 0 {
        bits.push(format!("手机传文件 {up} 个"));
    }
    let ch = py::i64_of(ev.get("chat"));
    if ch != 0 {
        bits.push(format!("对话 {ch} 次"));
    }
    let line1 = if bits.is_empty() { "这段时间没有入库动作".to_string() } else { bits.join("、") };
    let line2 = if items != 0 {
        format!("工作流水 {items} 条，勾掉 {done} 条（记了 {days} 天）")
    } else {
        "工作流水没有记录".to_string()
    };
    format!("- {line1}\n- {line2}")
}

// ---------------------------------------------------------------- 渲染片段

fn hour_table(hourly: &[i64]) -> String {
    let mut buckets: Vec<i64> = Vec::new();
    for i in (0..24).step_by(3) {
        let get = |j: usize| hourly.get(j).cloned().unwrap_or(0);
        buckets.push(get(i) + get(i + 1) + get(i + 2));
    }
    let top = buckets.iter().cloned().max().unwrap_or(0).max(1) as f64;
    let total: i64 = buckets.iter().sum();
    let unit = if total > 600 { "小时" } else { "分钟" };
    let mut rows = vec!["| 时段 | 活跃 | |".to_string(), "|---|---|---|".to_string()];
    for (i, v) in buckets.iter().enumerate() {
        let span = format!("{:02}–{:02}", i * 3, i * 3 + 3);
        if *v == 0 {
            rows.push(format!("| {span} | — | |"));
            continue;
        }
        let shown = if unit == "小时" {
            format!("{:.1}", *v as f64 / 60.0)
        } else {
            v.to_string()
        };
        rows.push(format!("| {span} | {shown} {unit} | {} |", bar(*v as f64, top, 14)));
    }
    rows.join("\n")
}

fn top_table(top: &[Value], limit: usize) -> String {
    if top.is_empty() {
        return "_这一段时间没有采到进程数据。_".to_string();
    }
    let mut rows = vec![
        "| 进程 | 均 CPU | 峰 CPU | 出现 |".to_string(),
        "|---|---|---|---|".to_string(),
    ];
    for p in top.iter().take(limit) {
        rows.push(format!(
            "| `{}` | {}% | {}% | {} 次 |",
            ss(p, "n"),
            numv(sub(p, "avg")),
            numv(sub(p, "max")),
            numv(sub(p, "hits"))
        ));
    }
    rows.join("\n")
}

fn seg_text(segs: &[Value]) -> String {
    if segs.is_empty() {
        return "—".to_string();
    }
    let parts: Vec<String> = segs
        .iter()
        .filter_map(|s| {
            let a = s.as_array()?;
            Some(format!("{}–{}", py::s_of(a.first()), py::s_of(a.get(1))))
        })
        .collect();
    parts.join(" · ")
}

fn anomalies(recs: &[Value]) -> Vec<String> {
    let mut out = Vec::new();
    let hot: Vec<&Value> = recs
        .iter()
        .filter(|r| sfd(r, "mem", "max") >= 90.0)
        .collect();
    if !hot.is_empty() {
        let dates: Vec<String> = hot
            .iter()
            .take(3)
            .map(|r| {
                let d = ss(r, "date");
                d.chars().skip(5).collect::<String>()
            })
            .collect();
        out.push(format!(
            "内存冲到 90% 以上：{}{}",
            dates.join("、"),
            if hot.len() > 3 { "（等）" } else { "" }
        ));
    }
    let tight: Vec<&Value> = recs
        .iter()
        .filter(|r| {
            let v = sfd(r, "disk", "min_free");
            let v = if v == 0.0 { 999.0 } else { v };
            v < 20.0
        })
        .collect();
    if !tight.is_empty() {
        let minf = tight
            .iter()
            .map(|r| sfd(r, "disk", "min_free"))
            .fold(f64::MAX, f64::min);
        out.push(format!("可用磁盘低于 20 GB：{} 天，最低 {:.1} GB", tight.len(), minf));
    }
    let mut with_load: Vec<&Value> = recs.iter().collect();
    with_load.sort_by(|a, b| {
        sfd(b, "load", "max")
            .partial_cmp(&sfd(a, "load", "max"))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    if let Some(r) = with_load.first() {
        if sfd(r, "load", "max") >= 8.0 {
            let d = ss(r, "date");
            out.push(format!(
                "负载到过 {}（{} {}，{} 核）",
                numv(sub(r, "load").get("max").unwrap_or(&Value::Null)),
                d.chars().skip(5).collect::<String>(),
                ssd(r, "load", "max_at"),
                {
                    let c = py::i64_of(r.get("cpus"));
                    if c == 0 { 10 } else { c }
                }
            ));
        }
    }
    out
}

// ---------------------------------------------------------------- 各周期

pub fn render_day(ctx: &Ctx, key: &str, daily: &Map<String, Value>) -> String {
    let Some(rec) = daily.get(key) else { return String::new() };
    let awake = sf(rec, "awake_min");
    let span = sf(rec, "span_min");
    let ratio = if span >= awake && awake > 0.0 {
        (awake * 100.0 / span) as i64
    } else {
        100
    };
    let cover = if ratio < 90 { format!("，占在机 {ratio}%") } else { String::new() };

    let today = chrono::Local::now().date_naive().format("%Y-%m-%d").to_string();
    let live = key == today;
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!(
        "# {}{}",
        title_of("day", key),
        if live { "（进行中）" } else { "" }
    ));
    lines.push(String::new());
    lines.push(format!(
        "> 活跃 **{}**（{}–{}{}）；内存均价 {}%；磁盘 {:.1}% → {:.1}%。",
        hhmm(awake),
        ss(rec, "first"),
        ss(rec, "last"),
        cover,
        numv(sub(rec, "mem").get("avg").unwrap_or(&Value::Null)),
        sfd(rec, "disk", "start"),
        sfd(rec, "disk", "end")
    ));
    lines.push(String::new());
    let n = py::i64_of(rec.get("n"));
    if n < 300 {
        lines.push(format!(
            "⚠️ 本日只有 {n} 条采样（{} 起才有记录），此前的时段没采到，上面的活跃时长只代表这一段时间。",
            ss(rec, "first")
        ));
        lines.push(String::new());
    }
    lines.push("## 什么时候在用".to_string());
    lines.push(String::new());
    lines.push(format!(
        "**{}**",
        seg_text(rec.get("segs").and_then(|v| v.as_array()).map(|a| a.as_slice()).unwrap_or(&[]))
    ));
    lines.push(String::new());
    let hourly: Vec<i64> = rec
        .get("hourly")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().map(|x| py::i64_of(Some(x))).collect())
        .unwrap_or_else(|| vec![0; 24]);
    lines.push(hour_table(&hourly));
    lines.push(String::new());
    lines.push("## 资源".to_string());
    lines.push(String::new());
    lines.push("| 指标 | 读数 |".to_string());
    lines.push("|---|---|".to_string());
    let mem_avg = sfd(rec, "mem", "avg");
    if mem_avg != 0.0 {
        lines.push(format!(
            "| 内存 | 均值 {}% · 峰 {}%（{}）· 低 {}% |",
            numv(sub(rec, "mem").get("avg").unwrap_or(&Value::Null)),
            numv(sub(rec, "mem").get("max").unwrap_or(&Value::Null)),
            ssd(rec, "mem", "max_at"),
            numv(sub(rec, "mem").get("min").unwrap_or(&Value::Null))
        ));
    } else {
        lines.push("| 内存 | 本日没有内存采样 |".to_string());
    }
    let load_avg = sfd(rec, "load", "avg");
    if load_avg != 0.0 {
        let cpus = {
            let c = py::i64_of(rec.get("cpus"));
            if c == 0 { 10 } else { c }
        };
        lines.push(format!(
            "| 负载 | 均值 {} · 峰 {}（{}，{} 核） |",
            numv(sub(rec, "load").get("avg").unwrap_or(&Value::Null)),
            numv(sub(rec, "load").get("max").unwrap_or(&Value::Null)),
            ssd(rec, "load", "max_at"),
            cpus
        ));
    } else {
        lines.push("| 负载 | 本日没有负载采样 |".to_string());
    }
    lines.push(format!(
        "| 磁盘 | {:.1}% → {:.1}%（{}） · 最少剩 {:.1} GB |",
        sfd(rec, "disk", "start"),
        sfd(rec, "disk", "end"),
        delta(sfd(rec, "disk", "start"), sfd(rec, "disk", "end"), " pt", 1),
        sfd(rec, "disk", "min_free")
    ));
    lines.push(format!(
        "| 重启 | {} 次 · 样本 {n} 条 |",
        py::i64_of(rec.get("boots"))
    ));
    lines.push(String::new());
    lines.push("## 占用前五".to_string());
    lines.push(String::new());
    lines.push(top_table(
        rec.get("top").and_then(|v| v.as_array()).map(|a| a.as_slice()).unwrap_or(&[]),
        5,
    ));
    lines.push(String::new());
    lines.push("## 今天的活".to_string());
    lines.push(String::new());
    lines.push(output_block(ctx, key, key));
    let an = anomalies(&[rec.clone()]);
    if !an.is_empty() {
        lines.push(String::new());
        lines.push("## 需要留意".to_string());
        lines.push(String::new());
        for x in an {
            lines.push(format!("- {x}"));
        }
    }
    let mut s = lines.join("\n");
    s.push('\n');
    s
}

pub fn render_period(ctx: &Ctx, kind: &str, key: &str, daily: &Map<String, Value>) -> String {
    let recs = days_of(kind, key, daily);
    if recs.is_empty() {
        return String::new();
    }
    let c = combine(&recs);
    let (a, b) = match span_of(kind, key) {
        Some(x) => x,
        None => return String::new(),
    };
    let per_day = sf(&c, "awake") / (py::i64_of(c.get("days")).max(1) as f64);

    let mut lines: Vec<String> = vec![format!("# {}", title_of(kind, key)), String::new()];
    let busiest_date = ssd(&c, "busiest", "date");
    let tail: String = busiest_date.chars().skip(5).collect();
    match kind {
        "week" => lines.push(format!(
            "> 有记录 {} 天，活跃合计 **{} 小时**（日均 {}）。内存均价 {}%，最忙是 {}（{}）。",
            py::i64_of(c.get("days")),
            hours(sf(&c, "awake")),
            hours(per_day),
            numv(sub(&c, "mem_avg")),
            tail,
            hhmm(sfd(&c, "busiest", "awake"))
        )),
        "month" => lines.push(format!(
            "> {} 天有记录，活跃 **{} 小时**，日均 {} 小时。磁盘 {:.1}% → {:.1}%。",
            py::i64_of(c.get("days")),
            hours(sf(&c, "awake")),
            hours(per_day),
            sf(&c, "disk_start"),
            sf(&c, "disk_end")
        )),
        _ => lines.push(format!(
            "> {} 天有记录，活跃 **{} 小时**；日均 {} 小时。磁盘 {:.1}% → {:.1}%（{}）。",
            py::i64_of(c.get("days")),
            hours(sf(&c, "awake")),
            hours(per_day),
            sf(&c, "disk_start"),
            sf(&c, "disk_end"),
            delta(sf(&c, "disk_start"), sf(&c, "disk_end"), " pt", 1)
        )),
    }
    let sec = match kind {
        "week" => "每天",
        "month" | "quarter" => "日历",
        _ => "每月",
    };
    lines.push(String::new());
    lines.push(format!("## {sec}"));
    lines.push(String::new());

    if kind == "week" {
        lines.push("| 日期 | 活跃 | 时段 | 内存峰 | 负载峰 | 磁盘 |".to_string());
        lines.push("|---|---|---|---|---|---|".to_string());
        for r in &recs {
            let d = parse_date(&ss(r, "date")).unwrap_or(a);
            lines.push(format!(
                "| {}-{:02} 周{} | {}h | {}–{} | {}% | {} | {:.1}→{:.1}% |",
                d.month(),
                d.day(),
                WEEKDAY[d.weekday().num_days_from_monday() as usize],
                hours(sf(r, "awake_min")),
                ss(r, "first"),
                ss(r, "last"),
                numv(sub(r, "mem").get("max").unwrap_or(&Value::Null)),
                numv(sub(r, "load").get("max").unwrap_or(&Value::Null)),
                sfd(r, "disk", "start"),
                sfd(r, "disk", "end")
            ));
        }
    } else if kind == "month" || kind == "quarter" {
        let top = recs
            .iter()
            .map(|r| sf(r, "awake_min"))
            .fold(0.0f64, f64::max)
            .max(1.0);
        let mut by_day: HashMap<String, &Value> = HashMap::new();
        for r in &recs {
            by_day.insert(ss(r, "date"), r);
        }
        lines.push("```".to_string());
        lines.push(format!("         {}", ["一", "二", "三", "四", "五", "六", "日"].join(" ")));
        let mut row_start = a - chrono::Duration::days(a.weekday().num_days_from_monday() as i64);
        while row_start <= b {
            let days7: Vec<NaiveDate> =
                (0..7).map(|i| row_start + chrono::Duration::days(i)).collect();
            if !days7.iter().any(|d| by_day.contains_key(&d.format("%Y-%m-%d").to_string())) {
                row_start += chrono::Duration::days(7);
                continue;
            }
            let mut cells: Vec<String> = Vec::new();
            for d2 in &days7 {
                if *d2 < a || *d2 > b {
                    cells.push(" ".to_string());
                } else {
                    let k = d2.format("%Y-%m-%d").to_string();
                    match by_day.get(&k) {
                        Some(r) => cells.push(spark(&[sf(r, "awake_min")], Some(top))),
                        None => cells.push("·".to_string()),
                    }
                }
            }
            let label = if row_start >= a { row_start } else { a };
            let left = format!("{:02}/{:02}", label.month(), label.day());
            lines.push(format!("{:<9}{}", left, cells.join("  ")));
            row_start += chrono::Duration::days(7);
        }
        lines.push("```".to_string());
        lines.push(String::new());
        lines.push("每格一天：块越高，当天在电脑前越久；`·`＝那天没开机；空白＝不在本期。".to_string());
        lines.push(String::new());
        lines.push("## 每周".to_string());
        lines.push(String::new());
        lines.push("| 周 | 活跃 | 日均 | 内存峰 |".to_string());
        lines.push("|---|---|---|---|".to_string());
        let mut order: Vec<String> = Vec::new();
        let mut weeks: HashMap<String, Vec<&Value>> = HashMap::new();
        for r in &recs {
            let d = parse_date(&ss(r, "date")).unwrap_or(a);
            let wk = key_of("week", d);
            if !weeks.contains_key(&wk) {
                order.push(wk.clone());
            }
            weeks.entry(wk).or_default().push(r);
        }
        order.sort();
        for wk in order {
            let wr = &weeks[&wk];
            let aw: f64 = wr.iter().map(|x| sf(x, "awake_min")).sum();
            let mxl = wr
                .iter()
                .map(|x| sfd(x, "mem", "max"))
                .fold(f64::MIN, f64::max);
            lines.push(format!(
                "| {wk} | {}h | {}h | {}% |",
                hours(aw),
                hours(aw / wr.len() as f64),
                numv(&json!(mxl))
            ));
        }
    } else {
        lines.push("| 月份 | 有记录 | 活跃 | 日均 | 内存峰 | 磁盘 |".to_string());
        lines.push("|---|---|---|---|---|---|".to_string());
        let mut order: Vec<String> = Vec::new();
        let mut months: HashMap<String, Vec<&Value>> = HashMap::new();
        for r in &recs {
            let k: String = ss(r, "date").chars().take(7).collect();
            if !months.contains_key(&k) {
                order.push(k.clone());
            }
            months.entry(k).or_default().push(r);
        }
        order.sort();
        for mk in order {
            let mr = &months[&mk];
            let aw: f64 = mr.iter().map(|x| sf(x, "awake_min")).sum();
            let mxl = mr.iter().map(|x| sfd(x, "mem", "max")).fold(f64::MIN, f64::max);
            lines.push(format!(
                "| {mk} | {} 天 | {}h | {}h | {}% | {:.1}→{:.1}% |",
                mr.len(),
                hours(aw),
                hours(aw / mr.len() as f64),
                numv(&json!(mxl)),
                sfd(mr[0], "disk", "start"),
                sfd(mr[mr.len() - 1], "disk", "end")
            ));
        }
    }

    lines.push(String::new());
    lines.push("## 一天里的分布".to_string());
    lines.push(String::new());
    let hourly: Vec<i64> = c
        .get("hourly")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().map(|x| py::i64_of(Some(x))).collect())
        .unwrap_or_else(|| vec![0; 24]);
    lines.push(hour_table(&hourly));
    lines.push(String::new());
    lines.push("## 占用榜".to_string());
    lines.push(String::new());
    lines.push(top_table(
        c.get("top").and_then(|v| v.as_array()).map(|a| a.as_slice()).unwrap_or(&[]),
        8,
    ));
    lines.push(String::new());
    lines.push("## 极值".to_string());
    lines.push(String::new());
    lines.push(format!(
        "- 最忙：{}（{}）",
        ssd(&c, "busiest", "date"),
        hhmm(sfd(&c, "busiest", "awake"))
    ));
    lines.push(format!(
        "- 内存最高：{}%（{}）",
        numv(sub(&c, "mem_max")),
        ss(&c, "mem_max_day")
    ));
    lines.push(format!(
        "- 负载最高：{}（{}，{} 核）",
        numv(sub(&c, "load_max")),
        ss(&c, "load_max_day"),
        {
            let n = py::i64_of(c.get("cpus"));
            if n == 0 { 10 } else { n }
        }
    ));
    lines.push(format!(
        "- 磁盘：{:.1}% → {:.1}%，期间最少剩 {:.1} GB",
        sf(&c, "disk_start"),
        sf(&c, "disk_end"),
        sf(&c, "min_free")
    ));
    lines.push(format!("- 重启：{} 次", py::i64_of(c.get("boots"))));
    lines.push(String::new());
    lines.push("## 产出".to_string());
    lines.push(String::new());
    lines.push(output_block(ctx, &a.format("%Y-%m-%d").to_string(), &b.format("%Y-%m-%d").to_string()));
    lines.push(String::new());

    let prev_list = past(kind, a, 1);
    if let Some(prev_key) = prev_list.last() {
        let pr = days_of(kind, prev_key, daily);
        if !pr.is_empty() {
            let pc = combine(&pr);
            lines.push("## 对比上一期".to_string());
            lines.push(String::new());
            lines.push(format!(
                "- 活跃：{}h vs {}h（{}）",
                hours(sf(&c, "awake")),
                hours(sf(&pc, "awake")),
                delta(sf(&pc, "awake"), sf(&c, "awake"), "h", 1)
            ));
            lines.push(format!(
                "- 日均：{}h vs {}h",
                hours(per_day),
                hours(sf(&pc, "awake") / (py::i64_of(pc.get("days")).max(1) as f64))
            ));
            lines.push(format!(
                "- 内存均价：{}% vs {}%（{}）",
                numv(sub(&c, "mem_avg")),
                numv(sub(&pc, "mem_avg")),
                delta(sf(&pc, "mem_avg"), sf(&c, "mem_avg"), " pt", 1)
            ));
            lines.push(String::new());
        }
    }
    let an = anomalies(&recs);
    if !an.is_empty() {
        lines.push("## 需要留意".to_string());
        lines.push(String::new());
        for x in an {
            lines.push(format!("- {x}"));
        }
        lines.push(String::new());
    }
    lines.join("\n")
}

pub fn build(ctx: &Ctx, kind: &str, key: &str, daily: &Map<String, Value>) -> String {
    if kind == "day" {
        return render_day(ctx, key, daily);
    }
    render_period(ctx, kind, key, daily)
}

// ---------------------------------------------------------------- 落盘

fn is_live(p: &Path) -> bool {
    match std::fs::read_to_string(p) {
        Ok(text) => text.lines().next().unwrap_or("").contains("（进行中）"),
        Err(_) => false,
    }
}

fn write_report(
    ctx: &Ctx,
    kind: &str,
    key: &str,
    force: bool,
    daily: &Map<String, Value>,
) -> Option<PathBuf> {
    let p = file_of_in(&hub(ctx), kind, key)?;
    if p.exists() && !force && !is_live(&p) {
        return None;
    }
    let md = build(ctx, kind, key, daily);
    if md.trim().is_empty() {
        return None;
    }
    if p.exists() {
        if let Ok(old) = std::fs::read_to_string(&p) {
            if old == md {
                return None;
            }
        }
    }
    let _ = std::fs::create_dir_all(hub(ctx));
    if std::fs::write(&p, md).is_err() {
        return None;
    }
    Some(p)
}

/// `metrics.load_daily`
pub fn load_daily(ctx: &Ctx) -> Map<String, Value> {
    let mut out = Map::new();
    let p = daily_path(ctx);
    if !p.exists() {
        return out;
    }
    let Ok(text) = std::fs::read_to_string(&p) else { return out };
    for line in text.split('\n') {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(rec @ Value::Object(_)) = serde_json::from_str::<Value>(line) {
            let d = py::s_of(rec.get("date"));
            if !d.is_empty() {
                out.insert(d, rec);
            }
        }
    }
    out
}

pub fn save_daily(ctx: &Ctx, recs: &Map<String, Value>) {
    let dir = sys_dir(ctx).join("使用报告").join("_数据");
    let _ = std::fs::create_dir_all(&dir);
    let p = dir.join("日聚合.jsonl");
    let mut keys: Vec<&String> = recs.keys().collect();
    keys.sort();
    let mut body = String::new();
    for k in keys {
        body.push_str(&compact_dumps(&recs[k]));
        body.push('\n');
    }
    let _ = jsonio::atomic_write(&p, &body);
}

/// `json.dumps(x)`（默认分隔符 `", "` / `": "`）—— 与 jsonio::dumps 的 indent
/// 版不是一回事，落 jsonl 必须用它。
pub fn compact_dumps(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => (if *b { "true" } else { "false" }).to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => serde_json::to_string(s).unwrap_or_default(),
        Value::Array(a) => {
            let items: Vec<String> = a.iter().map(compact_dumps).collect();
            format!("[{}]", items.join(", "))
        }
        Value::Object(o) => {
            let items: Vec<String> = o
                .iter()
                .map(|(k, x)| format!("{}: {}", serde_json::to_string(k).unwrap_or_default(), compact_dumps(x)))
                .collect();
            format!("{{{}}}", items.join(", "))
        }
    }
}

/// `metrics.rollup_day`
fn rollup_day(ctx: &Ctx, day: &str, snaps: Option<Vec<Value>>) -> Option<Value> {
    let mut rows: Vec<Value> = snaps.unwrap_or_else(|| load_snapshot(ctx, Some(day)))
        .into_iter()
        .filter(|r| !py::s_of(r.get("at")).is_empty())
        .collect();
    if rows.len() < 2 {
        return None;
    }
    rows.sort_by_key(|r| py::s_of(r.get("at")));

    let mut awake = 0.0f64;
    let mut hourly = [0.0f64; 24];
    let mut hourly_mem: Vec<Vec<f64>> = vec![Vec::new(); 24];
    let mut segs_idx: Vec<(usize, usize)> = Vec::new();
    let gap = 20.0;

    for (i, r) in rows.iter().enumerate() {
        let at = py::s_of(r.get("at"));
        let h: i64 = at.get(11..13).and_then(|s| s.parse().ok()).unwrap_or(-1);
        if (0..24).contains(&h) && r.get("mem_pct").map(|v| !v.is_null()).unwrap_or(false) {
            hourly_mem[h as usize].push(py::f64_of(r.get("mem_pct")));
        }
        if i == 0 {
            segs_idx.push((0, 0));
            continue;
        }
        let dt = minutes_between(&py::s_of(rows[i - 1].get("at")), &at);
        if dt > gap {
            segs_idx.push((i, i));
            continue;
        }
        if dt >= 1.0 {
            awake += dt;
            if (0..24).contains(&h) {
                hourly[h as usize] += dt;
            }
        }
        if let Some(last) = segs_idx.last_mut() {
            last.1 = i;
        }
    }
    let mut segs: Vec<Value> = Vec::new();
    for (a_i, b_i) in segs_idx {
        if minutes_between(&py::s_of(rows[a_i].get("at")), &py::s_of(rows[b_i].get("at"))) >= 8.0 {
            segs.push(json!([slice_time(&py::s_of(rows[a_i].get("at"))), slice_time(&py::s_of(rows[b_i].get("at")))]));
        }
    }
    segs.truncate(10);

    let mem: Vec<f64> = rows
        .iter()
        .filter(|r| r.get("mem_pct").map(|v| !v.is_null()).unwrap_or(false))
        .map(|r| py::f64_of(r.get("mem_pct")))
        .collect();
    let load: Vec<f64> = rows
        .iter()
        .filter(|r| r.get("load1").map(|v| !v.is_null()).unwrap_or(false))
        .map(|r| py::f64_of(r.get("load1")))
        .collect();
    let mut mem_max_row = &rows[0];
    for r in &rows {
        if py::f64_of(r.get("mem_pct")) > py::f64_of(mem_max_row.get("mem_pct")) {
            mem_max_row = r;
        }
    }
    let mut load_max_row = &rows[0];
    for r in &rows {
        if py::f64_of(r.get("load1")) > py::f64_of(load_max_row.get("load1")) {
            load_max_row = r;
        }
    }
    let mut boots = 0;
    for i in 1..rows.len() {
        let u0 = py::f64_of(rows[i - 1].get("uptime_h"));
        let u1 = py::f64_of(rows[i].get("uptime_h"));
        if u0 > 0.5 && u1 + 0.5 < u0 {
            boots += 1;
        }
    }
    let mut order: Vec<String> = Vec::new();
    let mut agg: HashMap<String, (f64, f64, i64)> = HashMap::new();
    for r in &rows {
        if let Some(arr) = r.get("procs").and_then(|v| v.as_array()) {
            for p in arr {
                let n = py::s_of(p.get("n"));
                if n.is_empty() {
                    continue;
                }
                if !agg.contains_key(&n) {
                    order.push(n.clone());
                    agg.insert(n.clone(), (0.0, 0.0, 0));
                }
                let e = agg.get_mut(&n).unwrap();
                e.0 += py::f64_of(p.get("c"));
                e.1 = f64::max(e.1, py::f64_of(p.get("c")));
                e.2 += 1;
            }
        }
    }
    let mut ranked: Vec<(String, f64, f64, i64)> = order
        .iter()
        .map(|k| {
            let (s, mx, hits) = agg[k];
            (k.clone(), s, mx, hits)
        })
        .collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let top: Vec<Value> = ranked
        .iter()
        .take(8)
        .map(|(k, s, mx, hits)| {
            json!({"n": k, "avg": py::py_round(s / (*hits).max(1) as f64, 1),
                   "max": py::py_round(*mx, 1), "hits": hits})
        })
        .collect();

    let first = slice_time(&py::s_of(rows[0].get("at")));
    let last = slice_time(&py::s_of(rows[rows.len() - 1].get("at")));
    let span = minutes_between(&py::s_of(rows[0].get("at")), &py::s_of(rows[rows.len() - 1].get("at")));
    let mem_max = mem.iter().cloned().fold(f64::MIN, f64::max);
    let mem_min = mem.iter().cloned().fold(f64::MAX, f64::min);
    let load_max = load.iter().cloned().fold(f64::MIN, f64::max);
    let full = rows.iter().filter(|r| {
        r.get("procs").map(|v| v.as_array().map(|a| !a.is_empty()).unwrap_or(false)).unwrap_or(false)
    }).count();
    let min_free = rows
        .iter()
        .map(|r| py::f64_of(r.get("free_gb")))
        .fold(f64::MAX, f64::min);
    let uptime_max = rows.iter().map(|r| py::f64_of(r.get("uptime_h"))).fold(f64::MIN, f64::max);

    Some(json!({
        "date": day,
        "n": rows.len(),
        "full": full,
        "awake_min": py::py_round(awake, 0),
        "span_min": py::py_round(span, 0),
        "first": first,
        "last": last,
        "boots": boots,
        "segs": segs,
        "mem": {"avg": if mem.is_empty() { json!(0.0) } else { json!(py::py_round(mem.iter().sum::<f64>() / mem.len() as f64, 1)) },
                "max": if mem.is_empty() { json!(0.0) } else { json!(py::py_round(mem_max, 1)) },
                "min": if mem.is_empty() { json!(0.0) } else { json!(py::py_round(mem_min, 1)) },
                "max_at": if mem.is_empty() { String::new() } else { slice_time(&py::s_of(mem_max_row.get("at"))) }},
        "load": {"avg": if load.is_empty() { json!(0.0) } else { json!(py::py_round(load.iter().sum::<f64>() / load.len() as f64, 2)) },
                 "max": if load.is_empty() { json!(0.0) } else { json!(py::py_round(load_max, 2)) },
                 "max_at": if load.is_empty() { String::new() } else { slice_time(&py::s_of(load_max_row.get("at"))) }},
        "disk": {"start": py::py_round(py::f64_of(rows[0].get("disk_pct")), 1),
                 "end": py::py_round(py::f64_of(rows[rows.len()-1].get("disk_pct")), 1),
                 "min_free": py::py_round(min_free, 1)},
        "uptime_max": uptime_max,
        "cpus": py::i64_of(rows[rows.len()-1].get("cpus")),
        "hourly": hourly.iter().map(|x| py::py_rint(*x)).collect::<Vec<i64>>(),
        "hourly_mem": hourly_mem.iter().map(|v| {
            if v.is_empty() { Value::Null } else { json!(py::py_round(v.iter().sum::<f64>() / v.len() as f64, 1)) }
        }).collect::<Vec<Value>>(),
        "top": top,
    }))
}

fn slice_time(at: &str) -> String {
    at.get(11..16).unwrap_or("").to_string()
}

fn minutes_between(a: &str, b: &str) -> f64 {
    let pa = chrono::NaiveDateTime::parse_from_str(a, "%Y-%m-%d %H:%M:%S");
    let pb = chrono::NaiveDateTime::parse_from_str(b, "%Y-%m-%d %H:%M:%S");
    match (pa, pb) {
        (Ok(x), Ok(y)) => (y - x).num_milliseconds() as f64 / 60000.0,
        _ => 0.0,
    }
}

fn load_snapshot(ctx: &Ctx, day: Option<&str>) -> Vec<Value> {
    let p = snap_path(ctx);
    if !p.exists() {
        return Vec::new();
    }
    let Ok(text) = std::fs::read_to_string(&p) else { return Vec::new() };
    let mut out = Vec::new();
    for line in text.split('\n') {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(d) = day {
            if line_day(line) != d {
                continue;
            }
        }
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            out.push(v);
        }
    }
    out
}

fn line_day(ln: &str) -> String {
    let Some(i) = ln.find("\"at\"") else { return String::new() };
    let rest = &ln[i + 4..];
    let Some(j) = rest.find('"') else { return String::new() };
    let after = &rest[j + 1..];
    after.chars().take(10).collect()
}

/// `metrics.rollup_all`
pub fn rollup_all(ctx: &Ctx) -> Map<String, Value> {
    let mut recs = load_daily(ctx);
    let mut changed = 0;
    let mut days: Vec<String> = load_snapshot(ctx, None)
        .iter()
        .map(|r| py::s_of(r.get("at")).chars().take(10).collect::<String>())
        .filter(|s| !s.is_empty())
        .collect();
    days.sort();
    days.dedup();
    for d in days {
        if let Some(rec) = rollup_day(ctx, &d, None) {
            recs.insert(d, rec);
            changed += 1;
        }
    }
    if changed > 0 {
        save_daily(ctx, &recs);
    }
    recs
}

// ---------------------------------------------------------------- 索引与归档

fn parse_key(kind: &str, name: &str) -> Option<String> {
    let pat = match kind {
        "day" => r"^日报-(\d{4}-\d{2}-\d{2})\.md$",
        "week" => r"^周报-(\d{4}-W\d{2})\.md$",
        "month" => r"^月报-(\d{4}-\d{2})\.md$",
        "quarter" => r"^季报-(\d{4}-Q\d)\.md$",
        "half" => r"^半年报-(\d{4}-H\d)\.md$",
        _ => r"^年报-(\d{4})\.md$",
    };
    let re = regex::Regex::new(pat).ok()?;
    re.captures(name).and_then(|m| m.get(1).map(|x| x.as_str().to_string()))
}

/// `report.write_index`：现扫目录重建索引（手动删过文件也能纠回来）。
pub fn write_index(ctx: &Ctx) -> PathBuf {
    let h = hub(ctx);
    let _ = std::fs::create_dir_all(&h);
    let mut items: Vec<Value> = Vec::new();
    for kind in KINDS {
        let Some(pf) = prefix(kind) else { continue };
        let Ok(rd) = std::fs::read_dir(&h) else { continue };
        let mut paths: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .map(|n| {
                        let n = n.to_string_lossy();
                        n.starts_with(&format!("{pf}-")) && n.ends_with(".md")
                    })
                    .unwrap_or(false)
            })
            .collect();
        paths.sort();
        for p in paths {
            let name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            let Some(k) = parse_key(kind, &name) else { continue };
            let mut head = String::new();
            if let Ok(text) = std::fs::read_to_string(&p) {
                for ln in text.split('\n') {
                    let s = ln.trim();
                    if let Some(rest) = s.strip_prefix("> ") {
                        head = rest.to_string();
                        break;
                    }
                }
            }
            let meta = std::fs::metadata(&p).ok();
            let bytes = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let at = meta
                .and_then(|m| m.modified().ok())
                .map(|t| {
                    let dt: chrono::DateTime<chrono::Local> = t.into();
                    dt.format("%Y-%m-%d %H:%M").to_string()
                })
                .unwrap_or_default();
            items.push(json!({
                "kind": kind, "key": k, "file": name,
                "title": title_of(kind, &k), "head": head,
                "bytes": bytes, "at": at,
            }));
        }
    }
    let order = |k: &str| KINDS.iter().position(|x| *x == k).unwrap_or(99);
    items.sort_by(|a, b| {
        let ka = (order(&ss(a, "kind")), ss(a, "key"));
        let kb = (order(&ss(b, "kind")), ss(b, "key"));
        ka.cmp(&kb)
    });
    let out = json!({
        "updated_at": chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        "count": items.len(),
        "items": items,
    });
    let p = index_path(ctx);
    let _ = jsonio::atomic_write(&p, &jsonio::dumps(&out, 1));
    p
}

fn cleanup(ctx: &Ctx) -> Vec<String> {
    let mut gone = Vec::new();
    let h = hub(ctx);
    if !h.exists() {
        return gone;
    }
    for kind in ["day", "week", "month"] {
        let Some(keep) = keep_of(kind) else { continue };
        let Some(pf) = prefix(kind) else { continue };
        let Ok(rd) = std::fs::read_dir(&h) else { continue };
        let mut files: Vec<(String, PathBuf)> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .map(|n| {
                        let n = n.to_string_lossy();
                        n.starts_with(&format!("{pf}-")) && n.ends_with(".md")
                    })
                    .unwrap_or(false)
            })
            .filter_map(|p| {
                let name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                parse_key(kind, &name).map(|k| (k, p))
            })
            .collect();
        files.sort_by(|a, b| b.0.cmp(&a.0));
        for (_, p) in files.into_iter().skip(keep) {
            if std::fs::remove_file(&p).is_ok() {
                gone.push(p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default());
            }
        }
    }
    gone
}

fn latest(ctx: &Ctx, kind: &str) -> Option<PathBuf> {
    let pf = prefix(kind)?;
    let h = hub(ctx);
    let rd = std::fs::read_dir(&h).ok()?;
    let mut best: Option<(String, PathBuf)> = None;
    for e in rd.flatten() {
        let p = e.path();
        let name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        if !(name.starts_with(&format!("{pf}-")) && name.ends_with(".md")) {
            continue;
        }
        if let Some(k) = parse_key(kind, &name) {
            if best.as_ref().map(|(bk, _)| k > *bk).unwrap_or(true) {
                best = Some((k, p));
            }
        }
    }
    best.map(|(_, p)| p)
}

fn due(ctx: &Ctx, today: NaiveDate, daily: &Map<String, Value>) -> Vec<(String, String)> {
    let mut plan = Vec::new();
    for (kind, back) in [
        ("day", 7usize),
        ("week", 5),
        ("month", 4),
        ("quarter", 3),
        ("half", 2),
        ("year", 2),
    ] {
        for k in past(kind, today, back) {
            if days_of(kind, &k, daily).is_empty() {
                continue;
            }
            if let Some(p) = file_of_in(&hub(ctx), kind, &k) {
                if p.exists() && !is_live(&p) {
                    continue;
                }
            }
            plan.push((kind.to_string(), k));
        }
    }
    plan
}

/// `report.run`
pub fn run(ctx: &Ctx, force: bool) -> Value {
    let daily = rollup_all(ctx);
    let mut made: Vec<Value> = Vec::new();
    let today = chrono::Local::now().date_naive();
    for (kind, key) in due(ctx, today, &daily) {
        if let Some(p) = write_report(ctx, &kind, &key, force, &daily) {
            made.push(json!({
                "kind": kind, "key": key,
                "file": p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
            }));
        }
    }
    let today_s = today.format("%Y-%m-%d").to_string();
    if daily.contains_key(&today_s) {
        if let Some(p) = write_report(ctx, "day", &today_s, true, &daily) {
            made.push(json!({
                "kind": "day", "key": today_s,
                "file": p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
                "live": true,
            }));
        }
    }
    let removed = cleanup(ctx);
    write_index(ctx);
    json!({"made": made, "removed": removed})
}

/// `report_runner.state()`
///
/// 运行态由 w5d 的 report 线程写入 crate::live；没开调度时是 Python 的初值形状
/// （started=0、samples=0、last_made=[]、runs=0）。
fn runner_state() -> Value {
    crate::live::report_value()
}

// ---------------------------------------------------------------- 路由

/// `GET /api/report?kind=&key=`
pub async fn one(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let kind = {
        let k = q(&uri, "kind");
        if k.is_empty() { "day".to_string() } else { k }
    };
    let key = q(&uri, "key");
    if prefix(&kind).is_none() {
        return internal_error();
    }
    let p = if !key.is_empty() { file_of_in(&hub(&ctx), &kind, &key) } else { latest(&ctx, &kind) };
    let Some(p) = p else {
        return http_err(404, "还没有生成这个周期的报告");
    };
    if !p.exists() {
        return http_err(404, "还没有生成这个周期的报告");
    }
    let body = std::fs::read_to_string(&p).unwrap_or_default();
    (
        [(header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
        body,
    )
        .into_response()
}

/// `GET /api/report/list`
pub async fn list(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let idx = write_index(&ctx);
    let mut data = match jsonio::read_json(&idx) {
        Some(v @ Value::Object(_)) => v,
        _ => json!({"items": [], "count": 0}),
    };
    if !data.is_object() {
        data = json!({"items": [], "count": 0});
    }
    data["state"] = runner_state();
    Json(data).into_response()
}

/// `POST /api/report/run?force=`
pub async fn run_route(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let pairs = crate::routes::common::query_map(&uri);
    let force = match pairs.into_iter().find(|(k, _)| k == "force") {
        None => false,
        Some((_, raw)) => match raw.trim().to_lowercase().as_str() {
            "1" | "true" | "t" | "yes" | "y" | "on" => true,
            "0" | "false" | "f" | "no" | "n" | "off" => false,
            _ => return bool_422(&raw),
        },
    };
    let ctx2 = ctx.clone();
    let res = tokio::task::spawn_blocking(move || run(&ctx2, force)).await;
    let res = match res {
        Ok(v) => v,
        Err(_) => return internal_error(),
    };
    crate::storage::log(
        &ctx,
        json!({"event": "report_manual",
               "made": res.get("made").and_then(|v| v.as_array()).map(|a| a.iter()
                   .map(|m| format!("{}:{}", ss(m, "kind"), ss(m, "key")))
                   .collect::<Vec<_>>()).unwrap_or_default(),
               "removed": res.get("removed").cloned().unwrap_or(json!([]))}),
    );
    // `{"ok": True, **res}`：ok 在最前，其余键按 res 的顺序跟在后面
    let mut out = Map::new();
    out.insert("ok".into(), json!(true));
    if let Value::Object(o) = res {
        for (k, v) in o {
            out.insert(k, v);
        }
    }
    Json(Value::Object(out)).into_response()
}

/// `POST /api/report/open`
pub async fn open(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: Option<Json<Value>>,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let body = body_obj(body);
    let kind = {
        let k = py::s_of(body.get("kind"));
        if k.is_empty() { "day".to_string() } else { k }
    };
    let key = py::s_of(body.get("key"));
    if prefix(&kind).is_none() {
        return internal_error();
    }
    let p = if !key.is_empty() { file_of_in(&hub(&ctx), &kind, &key) } else { latest(&ctx, &kind) };
    let Some(p) = p else {
        return http_err(404, "没有这份报告");
    };
    if !p.exists() {
        return http_err(404, "没有这份报告");
    }
    match crate::sysopen::open_path(&p) {
        Ok(_) => Json(json!({"ok": true, "file": p.to_string_lossy()})).into_response(),
        Err(_) => internal_error(),
    }
}

fn bool_422(raw: &str) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({"detail": [{
            "type": "bool_parsing",
            "loc": ["query", "force"],
            "msg": "Input should be a valid boolean, unable to interpret input",
            "input": raw,
            "url": "https://errors.pydantic.dev/2.14/v/bool_parsing",
        }]})),
    )
        .into_response()
}

fn internal_error() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "Internal Server Error",
    )
        .into_response()
}
