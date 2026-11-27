// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：`moshan/brief.py` 的 Rust 版 —— 每日简报的生成 / 读 / 推送入口（3 条路由）。
// 不做：定时器本身（`scheduler_loop` / `_tick` 的抢牌与 sent 账）在
//       `routes/w5d/brief_push.rs`，那里复用本文件的 `build` 与 `push_only`。
//
// 数据文件：`<数据根>/10-系统记录/简报/<日期>.json`（indent=1）与 `<日期>.md`。

use super::dashrs;
use super::hostwork;
use super::util::{self, get, int0, s_or, truthy};
use chrono::Datelike;
use crate::paths::Ctx;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

const TEXT_MAX: usize = 900;
const MEM_TTL_SECS: f64 = 120.0;

fn brief_root(ctx: &Ctx) -> PathBuf {
    if let Some(a) = ctx.cfg.get("allowed_roots").and_then(|x| x.as_array()) {
        if let Some(first) = a.first() {
            let s = s_or(first);
            if !s.is_empty() {
                return PathBuf::from(s);
            }
        }
    }
    util::home().join("Documents").join("AI项目")
}

fn brief_dir(root: &Path) -> PathBuf {
    let d = root.join("10-系统记录").join("简报");
    util::ensure_dir(&d);
    d
}

// ---------------------------------------------------------------- 小工具

fn wan(n: i64) -> String {
    if n >= 10000 {
        format!("{} 万", util::f1(n as f64 / 10000.0))
    } else {
        n.to_string()
    }
}

fn cut(s: &str, n: usize) -> String {
    let joined = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let len = joined.chars().count();
    if len <= n {
        joined
    } else {
        format!("{}…", util::cut(&joined, n - 1))
    }
}

fn weekday_cn(day: &str) -> String {
    let Ok(d) = chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d") else {
        return String::new();
    };
    let names = ["一", "二", "三", "四", "五", "六", "日"];
    format!("周{}", names[d.weekday().num_days_from_monday() as usize])
}

fn yest_of(day: &str) -> String {
    let d = util::parse_date(day).unwrap_or_else(|| util::now_local().date_naive());
    (d - chrono::Duration::days(1)).format("%Y-%m-%d").to_string()
}

// ---------------------------------------------------------------- 各段取数

struct Middle {
    svcs: Vec<Value>,
    disk: Value,
    gal: Value,
    hb: Value,
    g_day: i64,
    img_day: i64,
}

fn middle(ctx: &Ctx, root: &Path) -> Middle {
    let svcs = dashrs::services();
    let disk = dashrs::disk(root);
    let gal = dashrs::gallery(root);
    let hb_file = crate::paths::rt_join(ctx, "10-系统记录").join("心跳").join("最新.json");
    let hb = match util::load_json(&hb_file) {
        Some(v) if truthy(&v) => v,
        _ => dashrs::heartbeat(&ctx.runtime_dir),
    };
    let hb = {
        let checks = hb.get("checks").and_then(|x| x.as_array()).cloned().unwrap_or_default();
        let bad: Vec<Value> = checks
            .iter()
            .filter(|c| !truthy(get(c, "ok")))
            .cloned()
            .collect();
        json!({
            "ok": hb.get("ok").cloned().unwrap_or(Value::Null),
            "at": s_or(get(&hb, "at")),
            "bad": bad,
        })
    };
    let _ = ctx;
    Middle { svcs, disk, gal, hb, g_day: 0, img_day: 0 }
}

fn gallery_day(root: &Path, day: &str) -> (i64, i64) {
    let idx = util::load_json(&root.join("04-存图").join("_索引.json")).unwrap_or(json!({}));
    let arts = idx.get("articles").and_then(|a| a.as_array()).cloned().unwrap_or_default();
    let mut g = 0;
    let mut n = 0;
    for a in arts {
        if util::cut(&s_or(get(&a, "archived_at")), 10) != day {
            continue;
        }
        g += 1;
        n += int0(get(&a, "count"));
    }
    (g, n)
}

fn empty_tokens() -> Value {
    json!({
        "in": 0, "out": 0, "cached": 0, "calls": 0, "total": 0,
        "models": [], "sessions": [], "cost": Value::Null, "points": Value::Null,
        "currency": "¥", "unpriced": [], "cache_hit": Value::Null, "ok": false,
    })
}

fn tokens(ctx: &Ctx, day: &str) -> Value {
    let empty = empty_tokens();
    let price_cfg = hostwork::price_config(ctx);
    let mut by_model: Vec<(String, Value)> = Vec::new();
    let mut calls: i64 = 0;
    let mut sess: Vec<Value> = Vec::new();
    for s in hostwork::sessions_snapshot(ctx) {
        let Some(dm) = s.get("days").and_then(|d| d.get(day)) else {
            continue;
        };
        calls += int0(get(dm, "calls"));
        if let Some(Value::Object(models)) = dm.get("models") {
            for (m, v) in models {
                let idx = match by_model.iter().position(|(k, _)| k == m) {
                    Some(i) => i,
                    None => {
                        by_model.push((m.clone(), json!({"in": 0, "out": 0, "cached": 0, "n": 0})));
                        by_model.len() - 1
                    }
                };
                let slot = &mut by_model[idx].1;
                for k in ["in", "out", "cached", "n"] {
                    let cur = int0(get(slot, k)) + int0(get(v, k));
                    if let Some(o) = slot.as_object_mut() {
                        o.insert(k.to_string(), json!(cur));
                    }
                }
            }
        }
        let mut title = s_or(get(&s, "title")).trim().to_string();
        let utt = s.get("utt").and_then(|u| u.as_array()).cloned().unwrap_or_default();
        if title.is_empty() {
            if let Some(last) = utt.last() {
                title = match last {
                    Value::Array(a) => a.get(1).map(util::pystr).unwrap_or_default(),
                    _ => String::new(),
                };
            }
        }
        if !title.is_empty() {
            sess.push(json!({
                "title": title,
                "calls": int0(get(dm, "calls")),
                "tokens": int0(get(dm, "in")) + int0(get(dm, "out")),
                "proj": s_or(get(&s, "proj")),
            }));
        }
    }
    sess.sort_by(|a, b| int0(get(b, "tokens")).cmp(&int0(get(a, "tokens"))));
    let sessions: Vec<Value> = sess.iter().take(10).cloned().collect();
    let mut base = empty.clone();
    if let Some(o) = base.as_object_mut() {
        o.insert("sessions".into(), json!(sessions));
        o.insert("ok".into(), json!(true));
    }
    if by_model.is_empty() {
        return base;
    }
    let tin: i64 = by_model.iter().map(|(_, v)| int0(get(v, "in"))).sum();
    let tout: i64 = by_model.iter().map(|(_, v)| int0(get(v, "out"))).sum();
    let cached: i64 = by_model.iter().map(|(_, v)| int0(get(v, "cached"))).sum();
    let mut cost = 0.0;
    let mut points = 0.0;
    let mut any_cost = false;
    let mut any_pts = false;
    let mut unpriced: Vec<String> = Vec::new();
    let mut rows: Vec<Value> = Vec::new();
    let mut order: Vec<usize> = (0..by_model.len()).collect();
    order.sort_by(|a, b| {
        let sa = int0(get(&by_model[*a].1, "in")) + int0(get(&by_model[*a].1, "out"));
        let sb = int0(get(&by_model[*b].1, "in")) + int0(get(&by_model[*b].1, "out"));
        sb.cmp(&sa)
    });
    for i in order {
        let (m, v) = &by_model[i];
        let price = hostwork::price_for(m, &price_cfg);
        let (pt, ct) = hostwork::cost_of(v, &price);
        if let Some(c) = ct {
            cost += c;
            any_cost = true;
        }
        if let Some(p) = pt {
            points += p;
            any_pts = true;
        }
        if ct.is_none() && pt.is_none() {
            unpriced.push(m.clone());
        }
        rows.push(json!({
            "model": m,
            "in": int0(get(v, "in")),
            "out": int0(get(v, "out")),
            "cached": int0(get(v, "cached")),
            "n": int0(get(v, "n")),
            "cost": ct.map(|x| json!(util::py_round(x, 2))).unwrap_or(Value::Null),
            "points": pt.map(|x| json!(util::py_round(x, 1))).unwrap_or(Value::Null),
        }));
    }
    let cache_hit = if tin != 0 {
        json!(util::py_round(cached as f64 / tin as f64, 3))
    } else {
        Value::Null
    };
    json!({
        "in": tin, "out": tout, "cached": cached, "calls": calls,
        "total": tin + tout,
        "models": rows.iter().take(6).cloned().collect::<Vec<_>>(),
        "sessions": sess.iter().take(10).cloned().collect::<Vec<_>>(),
        "cost": if any_cost { json!(util::py_round(cost, 2)) } else { Value::Null },
        "points": if any_pts { json!(util::py_round(points, 1)) } else { Value::Null },
        "currency": s_or(get(&price_cfg, "currency")),
        "unpriced": unpriced.iter().take(4).cloned().collect::<Vec<_>>(),
        "cache_hit": cache_hit,
        "ok": true,
    })
}

fn works(root: &Path, yest: &str, today: &str, t: &Value) -> Value {
    let mut done: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let add = |txt: &str, done: &mut Vec<String>, seen: &mut std::collections::HashSet<String>| {
        let txt = cut(txt, 52);
        if !txt.is_empty() && !seen.contains(&txt) {
            seen.insert(txt.clone());
            done.push(txt);
        }
    };
    const FILLER: [&str; 9] = ["继续", "好的", "可以", "对", "嗯", "是的", "行", "OK", "ok"];
    for s in t.get("sessions").and_then(|x| x.as_array()).cloned().unwrap_or_default() {
        let title = s_or(get(&s, "title")).trim().to_string();
        if title.is_empty() || FILLER.contains(&title.as_str()) || title.chars().count() < 4 {
            continue;
        }
        add(&title, &mut done, &mut seen);
        if done.len() >= 5 {
            break;
        }
    }
    let flow = util::load_json(&root.join("09-个人工作台").join("流水").join("流水.json"))
        .unwrap_or(json!({}));
    for d in flow.get("days").and_then(|x| x.as_array()).cloned().unwrap_or_default() {
        if s_or(get(&d, "date")) != yest {
            continue;
        }
        for it in d.get("items").and_then(|x| x.as_array()).cloned().unwrap_or_default() {
            if int0(get(&it, "level")) > 1 {
                continue;
            }
            add(&s_or(get(&it, "text")), &mut done, &mut seen);
        }
    }
    let notes = util::load_json(&root.join("09-个人工作台").join("notes.json")).unwrap_or(json!({}));
    let mut todo_rows: Vec<(i64, String)> = Vec::new();
    let mut ev_rows: Vec<String> = Vec::new();
    for t2 in notes.get("todos").and_then(|x| x.as_array()).cloned().unwrap_or_default() {
        if truthy(get(&t2, "done")) {
            continue;
        }
        let due = s_or(get(&t2, "due"));
        if !due.is_empty() && due.as_str() > today {
            continue;
        }
        let tag = if !due.is_empty() && due.as_str() < today {
            "（逾期）"
        } else if due == today {
            "（今天）"
        } else {
            ""
        };
        let rank = if !due.is_empty() && due.as_str() < today {
            0
        } else if due == today {
            1
        } else {
            2
        };
        todo_rows.push((rank, format!("{}{}", cut(&s_or(get(&t2, "text")), 40), tag)));
    }
    for ev in notes.get("events").and_then(|x| x.as_array()).cloned().unwrap_or_default() {
        if s_or(get(&ev, "date")) != today {
            continue;
        }
        let extra = [get(&ev, "time"), get(&ev, "to")]
            .into_iter()
            .filter(|x| truthy(x))
            .map(util::pystr)
            .collect::<Vec<_>>()
            .join(" ");
        let title = {
            let ttl = s_or(get(&ev, "title"));
            if ttl.is_empty() { "行程".to_string() } else { ttl }
        };
        let body = if extra.is_empty() {
            title
        } else {
            format!("{} · {}", title, extra)
        };
        ev_rows.push(cut(&body, 40));
    }
    todo_rows.sort_by_key(|x| x.0);
    let mut plan: Vec<String> = todo_rows.iter().take(5).map(|x| x.1.clone()).collect();
    plan.extend(ev_rows.iter().take(2).cloned());
    json!({
        "done": done.iter().take(6).cloned().collect::<Vec<_>>(),
        "plan": plan.iter().take(7).cloned().collect::<Vec<_>>(),
    })
}

fn news(root: &Path, day: &str) -> Value {
    let base = root.join("10-系统记录").join("新闻").join("days");
    let mut items: Vec<Value> = Vec::new();
    let mut used = String::new();
    for cand in [day.to_string(), util::today()] {
        match util::load_json(&base.join(format!("{}.json", cand))) {
            Some(Value::Array(arr)) if !arr.is_empty() => {
                items = arr;
                used = cand;
                break;
            }
            _ => {}
        }
    }
    if items.is_empty() {
        return json!({"items": [], "n": 0, "ai": 0, "day": ""});
    }
    let total = items.len() as i64;
    let ai = items.iter().filter(|x| truthy(get(x, "ai_related"))).count() as i64;
    let mut arr = items.clone();
    arr.sort_by(|a, b| {
        let sa = util::as_f64(get(a, "final_score")).unwrap_or(0.0);
        let sb = util::as_f64(get(b, "final_score")).unwrap_or(0.0);
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut picked: Vec<Value> = Vec::new();
    let mut per_src: Vec<(String, i64)> = Vec::new();
    for x in arr {
        let src = {
            let v = s_or(get(&x, "source"));
            if v.is_empty() { "?".to_string() } else { v }
        };
        let n = per_src.iter().find(|(k, _)| *k == src).map(|(_, v)| *v).unwrap_or(0);
        if n >= 2 {
            continue;
        }
        match per_src.iter_mut().find(|(k, _)| *k == src) {
            Some(slot) => slot.1 += 1,
            None => per_src.push((src, 1)),
        }
        picked.push(x);
        if picked.len() >= 5 {
            break;
        }
    }
    let mut top: Vec<Value> = Vec::new();
    for x in &picked {
        let name = {
            let v = s_or(get(x, "name"));
            if v.is_empty() { s_or(get(x, "title")) } else { v }
        };
        let what = {
            let v = s_or(get(x, "what"));
            if v.is_empty() { s_or(get(x, "why")) } else { v }
        };
        let tier = s_or(get(get(x, "grade"), "tier"));
        top.push(json!({
            "name": cut(&name, 60),
            "what": cut(&what, 68),
            "source": s_or(get(x, "source")),
            "heat": cut(&s_or(get(x, "heat")), 24),
            "tier": tier,
            "url": s_or(get(x, "url")),
        }));
    }
    json!({"items": top, "n": total, "ai": ai, "day": used})
}

fn follow(root: &Path, day: &str) -> Value {
    let feed = util::load_json(&root.join("11-关注追踪").join("feed.json")).unwrap_or(json!({}));
    let items = feed.get("items").and_then(|x| x.as_array()).cloned().unwrap_or_default();
    let day_items: Vec<Value> = items
        .iter()
        .filter(|x| util::cut(&s_or(get(x, "fetched")), 10) == day)
        .cloned()
        .collect();
    let digest = util::load_json(&root.join("11-关注追踪").join("简报.json")).unwrap_or(json!({}));
    let d = digest.get(day).cloned().unwrap_or(json!({}));

    let mut by: Vec<(String, i64)> = Vec::new();
    for x in &day_items {
        let k = {
            let v = s_or(get(x, "platform"));
            if v.is_empty() { "?".to_string() } else { v }
        };
        match by.iter_mut().find(|(kk, _)| *kk == k) {
            Some(slot) => slot.1 += 1,
            None => by.push((k, 1)),
        }
    }

    let pool: Vec<Value> = if !day_items.is_empty() {
        day_items.clone()
    } else if truthy(&d) {
        items.iter().take(6).cloned().collect()
    } else {
        Vec::new()
    };
    let mut picks: Vec<Value> = Vec::new();
    let mut seen_pl: Vec<(String, i64)> = Vec::new();
    for x in &pool {
        let plat = {
            let v = s_or(get(x, "platform"));
            if v.is_empty() { "?".to_string() } else { v }
        };
        let n = seen_pl.iter().find(|(k, _)| *k == plat).map(|(_, v)| *v).unwrap_or(0);
        if n >= 2 {
            continue;
        }
        match seen_pl.iter_mut().find(|(k, _)| *k == plat) {
            Some(slot) => slot.1 += 1,
            None => seen_pl.push((plat.clone(), 1)),
        }
        picks.push(json!({
            "platform": plat,
            "author": cut(&s_or(get(x, "author")), 16),
            "text": cut(&s_or(get(x, "text")), 48),
            "summary": cut(&s_or(get(x, "summary")), 60),
            "url": s_or(get(x, "url")),
        }));
        if picks.len() >= 4 {
            break;
        }
    }
    let points: Vec<String> = d
        .get("points")
        .and_then(|x| x.as_array())
        .map(|a| a.iter().take(3).map(|p| cut(&util::pystr(p), 40)).collect())
        .unwrap_or_default();
    let stale = !picks.is_empty() && day_items.is_empty();
    json!({
        "n": day_items.len(),
        "by": by.iter().map(|(k, v)| (k.clone(), json!(v))).collect::<serde_json::Map<_, _>>(),
        "picks": picks,
        "stale": stale,
        "text": cut(&s_or(get(&d, "text")), 160),
        "points": points,
        "at": s_or(get(&d, "at")),
        "model": s_or(get(&d, "model")),
    })
}

// ---------------------------------------------------------------- 组装成段

fn sec_middle(m: &Middle, _yest: &str) -> Value {
    let svcs = &m.svcs;
    let up: Vec<&Value> = svcs.iter().filter(|s| truthy(get(s, "up"))).collect();
    let down: Vec<&Value> = svcs
        .iter()
        .filter(|s| !truthy(get(s, "up")) && !truthy(get(s, "lazy")))
        .collect();
    let lazy: Vec<&Value> = svcs
        .iter()
        .filter(|s| !truthy(get(s, "up")) && truthy(get(s, "lazy")))
        .collect();
    let disk = &m.disk;
    let gal = &m.gal;
    let hb = &m.hb;

    let pct_v = {
        let a = get(disk, "used_pct");
        if truthy(a) { Some(a.clone()) } else { None }
    };
    let free = s_or(get(disk, "free_h"));
    let mut lines: Vec<String> = Vec::new();
    let mut first = format!("服务 {}/{} 在线", up.len(), svcs.len());
    if !down.is_empty() {
        let names: Vec<String> = down.iter().map(|s| s_or(get(s, "name"))).collect();
        first.push_str(&format!("，离线：{}", names.join("、")));
    }
    lines.push(first);
    if let Some(pct) = &pct_v {
        let mut l = format!("磁盘 {}%", util::pystr(pct));
        if !free.is_empty() {
            l.push_str(&format!("，剩 {}", free));
        }
        lines.push(l);
    }
    let bytes_h = {
        let v = s_or(get(gal, "bytes_h"));
        if v.is_empty() { "—".to_string() } else { v }
    };
    lines.push(format!(
        "存图 {} 组 / {} 张（{}）· 昨日 +{} 组 {} 张",
        int0(get(gal, "articles")),
        int0(get(gal, "images")),
        bytes_h,
        m.g_day,
        m.img_day
    ));
    if truthy(hb) {
        let at = s_or(get(hb, "at"));
        let ok = get(hb, "ok");
        if ok.is_null() {
            lines.push("心跳 还没有跑过第一轮".to_string());
        } else {
            let bad = hb.get("bad").and_then(|x| x.as_array()).cloned().unwrap_or_default();
            let when = if at.chars().count() > 10 {
                at.chars().skip(11).take(5).collect::<String>()
            } else {
                at.clone()
            };
            let when = if when.is_empty() { "—".to_string() } else { when };
            let mut l = format!(
                "心跳 {} {}",
                when,
                if truthy(ok) { "正常" } else { "异常" }
            );
            if !bad.is_empty() {
                let names: Vec<String> = bad
                    .iter()
                    .take(3)
                    .map(|c| cut(&s_or(get(c, "name")), 10))
                    .collect();
                l.push_str(&format!("，待修 {} 项：{}", bad.len(), names.join("、")));
            }
            lines.push(l);
        }
    }
    if !lazy.is_empty() {
        let names: Vec<String> = lazy.iter().take(5).map(|s| s_or(get(s, "name"))).collect();
        lines.push(format!("按需服务没开 {} 个：{}", lazy.len(), names.join("、")));
    }
    let head = format!(
        "{}/{} 服务在线 · 存图 {} 张",
        up.len(),
        svcs.len(),
        int0(get(gal, "images"))
    );
    let pct_high = pct_v
        .as_ref()
        .and_then(util::as_f64)
        .map(|p| p >= 92.0)
        .unwrap_or(false);
    let badge = if !down.is_empty() || pct_high { "warn" } else { "ok" };
    json!({
        "id": "middle", "title": "中台运行", "glyph": "server",
        "head": head, "badge": badge, "lines": lines,
    })
}

fn sec_token(t: &Value, _yest: &str) -> Value {
    if !truthy(get(t, "ok")) {
        return json!({
            "id": "token", "title": "昨日消耗", "glyph": "coin",
            "head": "采集层不可用", "badge": "warn",
            "lines": ["Agent 监控的采集层没读到（hostwork 模块不在或报错）"],
        });
    }
    if int0(get(t, "total")) == 0 {
        return json!({
            "id": "token", "title": "昨日消耗", "glyph": "coin",
            "head": "昨天没有 AI 会话", "badge": "ok",
            "lines": ["昨天这台机器上没有 AI 客户端的用量记录"],
        });
    }
    let cost = get(t, "cost");
    let points = get(t, "points");
    let cur = s_or(get(t, "currency"));
    let money = if !cost.is_null() {
        format!("{}{}", cur, util::f2(util::as_f64(cost).unwrap_or(0.0)))
    } else if !points.is_null() {
        format!("{} 积分", util::f0(util::as_f64(points).unwrap_or(0.0)))
    } else {
        "—".to_string()
    };
    let cache_hit = get(t, "cache_hit");
    let hit = if cache_hit.is_null() {
        "—".to_string()
    } else {
        format!("{}%", util::f0(util::as_f64(cache_hit).unwrap_or(0.0) * 100.0))
    };
    let mut lines = vec![
        format!(
            "合计 {} token（入 {} / 出 {}）",
            wan(int0(get(t, "total"))),
            wan(int0(get(t, "in"))),
            wan(int0(get(t, "out")))
        ),
        format!("{} 次请求 · 缓存命中 {}", int0(get(t, "calls")), hit),
        format!("折算 {}", money),
    ];
    for r in t.get("models").and_then(|x| x.as_array()).cloned().unwrap_or_default().iter().take(3) {
        let mut l = format!(
            "· {} {}",
            cut(&s_or(get(r, "model")), 26),
            wan(int0(get(r, "in")) + int0(get(r, "out")))
        );
        if !get(r, "cost").is_null() {
            l.push_str(&format!(" ≈ {}{}", cur, util::f2(util::as_f64(get(r, "cost")).unwrap_or(0.0))));
        }
        lines.push(l);
    }
    let unpriced = t.get("unpriced").and_then(|x| x.as_array()).cloned().unwrap_or_default();
    if !unpriced.is_empty() {
        let names: Vec<String> = unpriced.iter().map(|x| cut(&util::pystr(x), 18)).collect();
        lines.push(format!("未配单价：{}", names.join("、")));
    }
    let head = format!("{} token · {}", wan(int0(get(t, "total"))), money);
    json!({
        "id": "token", "title": "昨日消耗", "glyph": "coin",
        "head": head, "badge": "ok", "lines": lines,
    })
}

fn sec_work(w: &Value, _yest: &str, _today: &str) -> Value {
    let mut lines: Vec<String> = Vec::new();
    let done = w.get("done").and_then(|x| x.as_array()).cloned().unwrap_or_default();
    let plan = w.get("plan").and_then(|x| x.as_array()).cloned().unwrap_or_default();
    if !done.is_empty() {
        lines.push(format!("昨天做了 {} 件：", done.len()));
        for x in &done {
            lines.push(format!("· {}", util::pystr(x)));
        }
    } else {
        lines.push("昨天没有记录到明确的活（工作流水、会话标题都空着）".to_string());
    }
    if !plan.is_empty() {
        lines.push(String::new());
        lines.push(format!("今天 {} 项：", plan.len()));
        for x in &plan {
            lines.push(format!("· {}", util::pystr(x)));
        }
    } else {
        lines.push(String::new());
        lines.push("今天待办和行程都是空的".to_string());
    }
    let head = format!("昨 {} 件 · 今 {} 项", done.len(), plan.len());
    json!({
        "id": "work", "title": "工作与计划", "glyph": "list",
        "head": head, "badge": "ok", "lines": lines,
    })
}

fn sec_news(n: &Value) -> Value {
    if int0(get(n, "n")) == 0 {
        return json!({
            "id": "news", "title": "新闻热点", "glyph": "news",
            "head": "没有抓取产物", "badge": "warn",
            "lines": ["10-系统记录/新闻/days 里没有可用的文件"],
        });
    }
    let mut lines: Vec<String> = Vec::new();
    for (i, x) in n.get("items").and_then(|v| v.as_array()).cloned().unwrap_or_default().iter().enumerate() {
        lines.push(format!("{}. {}", i + 1, s_or(get(x, "name"))));
        let mut bits: Vec<String> = Vec::new();
        for k in ["what", "heat"] {
            let v = s_or(get(x, k));
            if !v.is_empty() {
                bits.push(v);
            }
        }
        let tier = s_or(get(x, "tier"));
        if !tier.is_empty() {
            bits.push(format!("档 {}", tier));
        }
        if !bits.is_empty() {
            lines.push(format!("   {}", bits.join(" · ")));
        }
    }
    let head = format!("{} 条 · AI 相关 {}", int0(get(n, "n")), int0(get(n, "ai")));
    json!({
        "id": "news", "title": "新闻热点", "glyph": "news",
        "head": head, "badge": "ok", "lines": lines,
    })
}

fn sec_follow(f: &Value) -> Value {
    if int0(get(f, "n")) == 0 && s_or(get(f, "text")).is_empty() {
        return json!({
            "id": "follow", "title": "关注更新", "glyph": "follow",
            "head": "昨天没有新更新", "badge": "ok",
            "lines": ["关注的那些人昨天没发新东西（或抓取没跑）"],
        });
    }
    let mut by: Vec<(String, i64)> = f
        .get("by")
        .and_then(|x| x.as_object())
        .map(|m| m.iter().map(|(k, v)| (k.clone(), int0(v))).collect())
        .unwrap_or_default();
    by.sort_by(|a, b| b.1.cmp(&a.1));
    let parts: Vec<String> = by.iter().map(|(k, v)| format!("{} {}", k, v)).collect();
    let mut lines: Vec<String> = Vec::new();
    if !parts.is_empty() {
        lines.push(format!("按平台：{}", parts.join(" · ")));
    }
    let text = s_or(get(f, "text"));
    if !text.is_empty() {
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.push(text);
    }
    let points = f.get("points").and_then(|x| x.as_array()).cloned().unwrap_or_default();
    if !points.is_empty() {
        lines.push(String::new());
        for p in &points {
            lines.push(format!("· {}", util::pystr(p)));
        }
    }
    let picks = f.get("picks").and_then(|x| x.as_array()).cloned().unwrap_or_default();
    if !picks.is_empty() {
        lines.push(String::new());
        if truthy(get(f, "stale")) {
            lines.push("（昨天那批已被新一轮覆盖，下面是此刻最新的）".to_string());
        }
        for p in &picks {
            let author = s_or(get(p, "author"));
            let who = if author.is_empty() { s_or(get(p, "platform")) } else { author };
            let body = {
                let t = s_or(get(p, "text"));
                if t.is_empty() { s_or(get(p, "summary")) } else { t }
            };
            lines.push(format!("· [{}] {}：{}", s_or(get(p, "platform")), who, body));
        }
    }
    let head = if int0(get(f, "n")) > 0 {
        format!("{} 条新动态 · {} 个平台", int0(get(f, "n")), by.len())
    } else {
        "昨日摘要 · 原文已翻页".to_string()
    };
    json!({
        "id": "follow", "title": "关注更新", "glyph": "follow",
        "head": head, "badge": "ok", "lines": lines,
    })
}

fn render_text(sec: &Value, day: &str, total: usize) -> String {
    let n = match s_or(get(sec, "id")).as_str() {
        "middle" => 1,
        "token" => 2,
        "work" => 3,
        "news" => 4,
        "follow" => 5,
        _ => 0,
    };
    let day_tail: String = day.chars().skip(5).collect();
    let mut lines: Vec<String> = vec![format!(
        "{}（{}/{}）· {} {}",
        s_or(get(sec, "title")),
        n,
        total,
        day_tail,
        weekday_cn(day)
    )];
    for l in sec.get("lines").and_then(|x| x.as_array()).cloned().unwrap_or_default() {
        lines.push(util::pystr(&l));
    }
    util::cut(&lines.join("\n"), TEXT_MAX)
}

// ---------------------------------------------------------------- 对外

static MEM: Mutex<Option<(String, Instant, Value)>> = Mutex::new(None);

fn mem_get(day: &str) -> Option<Value> {
    let guard = MEM.lock().ok()?;
    let (d, at, data) = guard.as_ref()?;
    if d == day && at.elapsed().as_secs_f64() < MEM_TTL_SECS {
        return Some(data.clone());
    }
    None
}

fn mem_set(day: &str, data: &Value) {
    if let Ok(mut guard) = MEM.lock() {
        *guard = Some((day.to_string(), Instant::now(), data.clone()));
    }
}

pub fn build(ctx: &Ctx, day: &str, force: bool) -> Value {
    let day = if day.is_empty() { util::today() } else { day.to_string() };
    if !force {
        if let Some(hit) = mem_get(&day) {
            return hit;
        }
    }
    let root = brief_root(ctx);
    let out_path = brief_dir(&ctx.runtime_dir).join(format!("{}.json", day));
    if !force {
        if let Some(cached) = util::load_json(&out_path) {
            if cached.is_object() && truthy(get(&cached, "sections")) {
                mem_set(&day, &cached);
                return cached;
            }
        }
    }

    let yest = yest_of(&day);
    let mut m = middle(ctx, &root);
    let (g, n) = gallery_day(&root, &yest);
    m.g_day = g;
    m.img_day = n;
    let tok = tokens(ctx, &yest);

    let mut sections: Vec<Value> = Vec::new();
    let mut errs: Vec<String> = Vec::new();
    let builders: Vec<(&str, Box<dyn Fn() -> Result<Value, String> + '_>)> = vec![
        ("middle", Box::new(|| Ok(sec_middle(&m, &yest)))),
        ("token", Box::new(|| Ok(sec_token(&tok, &yest)))),
        (
            "work",
            Box::new(|| Ok(sec_work(&works(&root, &yest, &day, &tok), &yest, &day))),
        ),
        ("news", Box::new(|| Ok(sec_news(&news(&ctx.runtime_dir, &yest))))),
        ("follow", Box::new(|| Ok(sec_follow(&follow(&root, &yest))))),
    ];
    for (sid, f) in builders {
        match f() {
            Ok(v) => sections.push(v),
            Err(e) => {
                errs.push(format!("{}: {}", sid, e));
                sections.push(json!({
                    "id": sid, "title": sid, "glyph": "pocket",
                    "head": "这一段取数失败", "badge": "warn",
                    "lines": [util::cut(&e, 160)],
                }));
            }
        }
    }
    for s in sections.iter_mut() {
        let text = render_text(s, &day, 5);
        if let Some(o) = s.as_object_mut() {
            o.insert("text".to_string(), json!(text));
        }
    }

    let data = json!({
        "day": day,
        "yest": yest,
        "weekday": weekday_cn(&day),
        "generated_at": util::now_stamp(),
        "sections": sections,
        "errors": errs,
    });
    let _ = util::write_json(&out_path, &data, 1);
    let _ = write_md(&ctx.runtime_dir, &data);
    mem_set(&day, &data);
    data
}

fn write_md(root: &Path, data: &Value) -> std::io::Result<()> {
    let mut lines: Vec<String> = vec![
        format!(
            "# 简报 · {} {}",
            s_or(get(data, "day")),
            s_or(get(data, "weekday"))
        ),
        String::new(),
        format!(
            "> 生成于 {}；覆盖 {} 的账。",
            s_or(get(data, "generated_at")),
            s_or(get(data, "yest"))
        ),
        String::new(),
    ];
    for s in data.get("sections").and_then(|x| x.as_array()).cloned().unwrap_or_default() {
        lines.push(format!(
            "## {} — {}",
            s_or(get(&s, "title")),
            s_or(get(&s, "head"))
        ));
        lines.push(String::new());
        for l in s.get("lines").and_then(|x| x.as_array()).cloned().unwrap_or_default() {
            lines.push(util::pystr(&l));
        }
        lines.push(String::new());
    }
    let p = brief_dir(root).join(format!("{}.md", s_or(get(data, "day"))));
    std::fs::write(p, lines.join("\n"))
}

pub fn days(root: &Path, limit: usize) -> Vec<String> {
    let d = brief_dir(root);
    let mut out: Vec<String> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&d) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(stem) = name.strip_suffix(".json") {
                out.push(stem.to_string());
            }
        }
    }
    out.sort_by(|a, b| b.cmp(a));
    out.truncate(limit);
    out
}

pub fn days_for(ctx: &Ctx, limit: usize) -> Vec<String> {
    days(&ctx.runtime_dir, limit)
}

/// `push()`：逐条发微信。**一条一条发**是这个功能的前提，不是优化项。
///
/// 与 Python 同形：`dry` 只回将发的内容；真发时每段之间停 1.2 秒
/// （连着灌五条网关会挤在一起）。失败不让调用方 500 —— 只记进 `fail`。
pub fn push(data: &Value, dry: bool) -> Value {
    push_only(data, dry, None)
}

/// 定时补推用：只发 `only` 里的段（其余已发成功的不重发）。
pub fn push_only(data: &Value, dry: bool, only: Option<&[String]>) -> Value {
    let sections = data
        .get("sections")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    if dry {
        let sent: Vec<Value> = sections
            .iter()
            .map(|s| json!({"id": s_or(get(s, "id")), "text": s_or(get(s, "text"))}))
            .collect();
        return json!({"ok": true, "dry": true, "sent": sent});
    }
    let mut sent: Vec<String> = Vec::new();
    let mut fail: Vec<Value> = Vec::new();
    let mut first = true;
    for s in &sections {
        let sid = s_or(get(s, "id"));
        if let Some(list) = only {
            if !list.iter().any(|x| x == &sid) {
                continue;
            }
        }
        if !first {
            // Python 在每段（含失败那段）之后都 sleep(1.2)。
            std::thread::sleep(std::time::Duration::from_millis(1200));
        }
        first = false;
        let ok = crate::wechat::send(&s_or(get(s, "text")));
        if ok {
            sent.push(sid);
        } else {
            fail.push(json!({"id": sid, "error": "网关返回失败"}));
        }
    }
    json!({
        "ok": fail.is_empty(),
        "sent": sent,
        "fail": fail,
        "day": data.get("day").cloned().unwrap_or(Value::Null),
    })
}
