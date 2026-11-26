// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：节奏账（hostwork.pulse）—— 日行 / 7×24 热力 / 每周分层 / 范畴 opts。
// 不做：不扫 jsonl（读 agg.days / act / hw 与 sessions 表的 space）；20 秒缓存。

use chrono::Timelike;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use super::agg::aggs;
use super::sessions::session_index;
use super::util::{fnum, py_int, round_n, str_or, truthy};
use crate::paths::Ctx;

const SPACE_FALLBACK: &str = "未归属空间";

struct Cache {
    at: f64,
    days: i64,
    scope: String,
    data: Value,
}

static PULSE: Mutex<Option<Cache>> = Mutex::new(None);

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn drow<'a>(daymap: &'a mut Map<String, Value>, d: &str) -> &'a mut Map<String, Value> {
    if !daymap.get(d).map(|v| v.is_object()).unwrap_or(false) {
        let mut m = Map::new();
        m.insert("d".into(), json!(d));
        m.insert("act".into(), json!(0.0));
        m.insert("tok".into(), json!(0));
        m.insert("in".into(), json!(0));
        m.insert("out".into(), json!(0));
        m.insert("ask".into(), json!(0));
        m.insert("call".into(), json!(0));
        m.insert("tools".into(), json!({}));
        m.insert("space".into(), json!(""));
        m.insert("layers".into(), json!({}));
        daymap.insert(d.to_string(), Value::Object(m));
    }
    daymap.get_mut(d).and_then(|v| v.as_object_mut()).unwrap()
}

/// `pulse(days, scope)`
pub fn pulse(ctx: &Ctx, days_in: i64, scope_in: &str) -> Value {
    let mut days = if days_in <= 0 { 75 } else { days_in };
    days = days.max(7).min(200);
    let mut scope = scope_in.trim().to_string();
    let (mut skind, mut sval) = ("", "");
    if let Some(rest) = scope.strip_prefix("space:") {
        skind = "space";
        sval = rest.trim();
    } else if let Some(rest) = scope.strip_prefix("proj:") {
        skind = "proj";
        sval = rest.trim();
    }
    if sval.is_empty() {
        skind = "";
        sval = "";
        scope = String::new();
    }
    let now = now_secs();
    {
        let g = PULSE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = g.as_ref() {
            if c.days == days && c.scope == scope && now - c.at < 20.0 {
                return c.data.clone();
            }
        }
    }

    let today = chrono::Local::now().date_naive();
    let since = (today - chrono::Duration::days(days - 1))
        .format("%Y-%m-%d")
        .to_string();
    let files = aggs(ctx);
    let idx = session_index(20.0);

    let mut daymap: Map<String, Value> = Map::new();
    let spans = [30i64, 90, 180];
    let mut hw_since: HashMap<i64, String> = HashMap::new();
    for s in spans {
        hw_since.insert(
            s,
            (today - chrono::Duration::days(s - 1)).format("%Y-%m-%d").to_string(),
        );
    }
    let mut hours = vec![0.0f64; 168];
    let mut hours_by: HashMap<i64, Vec<f64>> = spans.iter().map(|s| (*s, vec![0.0; 168])).collect();
    let mut opt_sp: Vec<(String, i64)> = Vec::new();
    let mut opt_pj: Vec<(String, i64)> = Vec::new();
    let mut last_ts = 0i64;

    let bump = |list: &mut Vec<(String, i64)>, key: &str, tok: i64| {
        if let Some(slot) = list.iter_mut().find(|(k, _)| k == key) {
            slot.1 += tok;
        } else {
            list.push((key.to_string(), tok));
        }
    };

    for agg in &files {
        let sid = str_or(agg.get("id"));
        let srow = idx.get(&sid).cloned().unwrap_or_else(|| json!({}));
        let sp_name = {
            let s = str_or(srow.get("space"));
            if s.is_empty() { SPACE_FALLBACK.to_string() } else { s }
        };
        let proj = str_or(agg.get("proj"));
        let mut win_tok = 0i64;
        if let Some(Value::Object(days_map)) = agg.get("days") {
            for (d, dm) in days_map {
                if d.as_str() >= since.as_str() {
                    win_tok += py_int(dm.get("in").unwrap_or(&Value::Null))
                        + py_int(dm.get("out").unwrap_or(&Value::Null));
                }
            }
        }
        if win_tok != 0 {
            bump(&mut opt_sp, &sp_name, win_tok);
            if !proj.is_empty() {
                bump(&mut opt_pj, &proj, win_tok);
            }
        }
        if skind == "space" && sp_name != sval {
            continue;
        }
        if skind == "proj" && proj != sval {
            continue;
        }
        last_ts = last_ts.max(py_int(agg.get("last").unwrap_or(&Value::Null)));
        let layer = if !skind.is_empty() {
            let t = str_or(srow.get("title")).trim().chars().take(26).collect::<String>();
            if t.is_empty() { "未命名会话".to_string() } else { t }
        } else {
            sp_name.clone()
        };
        if let Some(Value::Object(days_map)) = agg.get("days") {
            for (d, dm) in days_map {
                if d.as_str() < since.as_str() {
                    continue;
                }
                let row = drow(&mut daymap, d);
                let tin = py_int(dm.get("in").unwrap_or(&Value::Null));
                let tout = py_int(dm.get("out").unwrap_or(&Value::Null));
                for (k, v) in [("in", tin), ("out", tout), ("tok", tin + tout)] {
                    let cur = py_int(row.get(k).unwrap_or(&Value::Null));
                    row.insert(k.into(), json!(cur + v));
                }
                if tin + tout != 0 {
                    let lay = row
                        .entry("layers".to_string())
                        .or_insert_with(|| json!({}));
                    if let Some(m) = lay.as_object_mut() {
                        let cur = py_int(m.get(&layer).unwrap_or(&Value::Null));
                        m.insert(layer.clone(), json!(cur + tin + tout));
                    }
                }
                row.insert(
                    "ask".into(),
                    json!(py_int(row.get("ask").unwrap_or(&Value::Null))
                        + py_int(dm.get("asks").unwrap_or(&Value::Null))),
                );
                row.insert(
                    "call".into(),
                    json!(py_int(row.get("call").unwrap_or(&Value::Null))
                        + py_int(dm.get("calls").unwrap_or(&Value::Null))),
                );
                if let Some(Value::Object(tools)) = dm.get("tools") {
                    let rt = row.entry("tools".to_string()).or_insert_with(|| json!({}));
                    if let Some(m) = rt.as_object_mut() {
                        for (k, v) in tools {
                            let cur = py_int(m.get(k).unwrap_or(&Value::Null));
                            m.insert(k.clone(), json!(cur + py_int(v)));
                        }
                    }
                }
                if !sp_name.is_empty() && str_or(row.get("space")).is_empty() {
                    row.insert("space".into(), json!(sp_name));
                }
            }
        }
        if let Some(Value::Object(act)) = agg.get("act") {
            for (d, sec) in act {
                if d.as_str() >= since.as_str() {
                    let r = drow(&mut daymap, d);
                    let cur = super::util::py_float(r.get("act").unwrap_or(&Value::Null)).unwrap_or(0.0);
                    r.insert("act".into(), json!(cur + super::util::py_float(sec).unwrap_or(0.0)));
                }
            }
        }
        if let Some(Value::Object(hw)) = agg.get("hw") {
            for (k, sec) in hw {
                let kchars: Vec<char> = k.chars().collect();
                if kchars.len() < 10 {
                    continue;
                }
                let daypart: String = kchars[..10].iter().collect();
                if daypart.as_str() < since.as_str() {
                    continue;
                }
                let Ok(date) = chrono::NaiveDate::parse_from_str(&daypart, "%Y-%m-%d") else {
                    continue;
                };
                let hh: usize = if kchars.len() >= 13 {
                    kchars[11..13].iter().collect::<String>().parse().unwrap_or(0)
                } else {
                    0
                };
                use chrono::Datelike;
                let idxh = (date.weekday().num_days_from_monday() as usize) * 24 + hh.min(23);
                let v = super::util::py_float(sec).unwrap_or(0.0);
                hours[idxh] += v;
                for s in spans {
                    if s <= days && daypart.as_str() >= hw_since[&s].as_str() {
                        let arr = hours_by.get_mut(&s).unwrap();
                        arr[idxh] += v;
                    }
                }
            }
        }
    }

    let mut out: Vec<Value> = Vec::new();
    for i in (0..days).rev() {
        let d = (today - chrono::Duration::days(i)).format("%Y-%m-%d").to_string();
        let r = daymap.get(&d).cloned().unwrap_or_else(|| {
            json!({"d": d, "act": 0.0, "tok": 0, "in": 0, "out": 0,
                   "ask": 0, "call": 0, "tools": {}, "space": ""})
        });
        let mut r = r;
        let act = super::util::py_float(r.get("act").unwrap_or(&Value::Null)).unwrap_or(0.0);
        r["act"] = fnum(round_n(act / 60.0, 1));
        out.push(r);
    }

    // 每周
    let mut weeks: Map<String, Value> = Map::new();
    for r in &out {
        let d = chrono::NaiveDate::parse_from_str(&str_or(r.get("d")), "%Y-%m-%d");
        let Ok(d) = d else { continue };
        use chrono::Datelike;
        let mon = d - chrono::Duration::days(d.weekday().num_days_from_monday() as i64);
        let key = mon.format("%Y-%m-%d").to_string();
        if !weeks.contains_key(&key) {
            let mut w = Map::new();
            w.insert("w".into(), json!(key));
            w.insert(
                "to".into(),
                json!((mon + chrono::Duration::days(6)).format("%Y-%m-%d").to_string()),
            );
            w.insert("tok".into(), json!(0));
            w.insert("act".into(), json!(0.0));
            w.insert("ask".into(), json!(0));
            w.insert("spaces".into(), json!({}));
            weeks.insert(key.clone(), Value::Object(w));
        }
        let w = weeks.get_mut(&key).and_then(|v| v.as_object_mut()).unwrap();
        let tok = py_int(r.get("tok").unwrap_or(&Value::Null));
        w.insert("tok".into(), json!(py_int(w.get("tok").unwrap_or(&Value::Null)) + tok));
        let act = super::util::py_float(r.get("act").unwrap_or(&Value::Null)).unwrap_or(0.0);
        w.insert(
            "act".into(),
            fnum(super::util::py_float(w.get("act").unwrap_or(&Value::Null)).unwrap_or(0.0) + act),
        );
        w.insert(
            "ask".into(),
            json!(py_int(w.get("ask").unwrap_or(&Value::Null)) + py_int(r.get("ask").unwrap_or(&Value::Null))),
        );
        if let Some(Value::Object(layers)) = r.get("layers") {
            let sp = w.get_mut("spaces").and_then(|v| v.as_object_mut()).unwrap();
            for (k, v) in layers {
                let cur = py_int(sp.get(k).unwrap_or(&Value::Null));
                sp.insert(k.clone(), json!(cur + py_int(v)));
            }
        }
    }

    let mut tot: Vec<(String, i64)> = Vec::new();
    for r in &out {
        if let Some(Value::Object(layers)) = r.get("layers") {
            for (k, v) in layers {
                bump(&mut tot, k, py_int(v));
            }
        }
    }
    tot.sort_by(|a, b| b.1.cmp(&a.1));
    let mut spaces: Vec<String> = tot.iter().map(|(k, _)| k.clone()).collect();

    let mut layer_of = "space";
    if !skind.is_empty() {
        layer_of = "sess";
        if spaces.len() > 8 {
            let keep: Vec<String> = spaces.iter().take(7).cloned().collect();
            let keep_set: HashSet<String> = keep.iter().cloned().collect();
            let rest_name = format!("其他 {} 个会话", spaces.len() - 7);
            let mut has_rest = false;
            for w in weeks.values_mut() {
                let sp = match w.get_mut("spaces").and_then(|v| v.as_object_mut()) {
                    Some(x) => x,
                    None => continue,
                };
                let keys: Vec<String> = sp.keys().cloned().collect();
                let mut rest = 0i64;
                for k in keys {
                    if !keep_set.contains(&k) {
                        rest += py_int(&sp.remove(&k).unwrap_or(Value::Null));
                    }
                }
                if rest != 0 {
                    let cur = py_int(sp.get(&rest_name).unwrap_or(&Value::Null));
                    sp.insert(rest_name.clone(), json!(cur + rest));
                    has_rest = true;
                }
            }
            spaces = keep;
            if has_rest {
                spaces.push(rest_name);
            }
        }
    }

    let ranked = |list: &mut Vec<(String, i64)>, prefix: &str| -> Vec<Value> {
        list.sort_by(|a, b| b.1.cmp(&a.1));
        list.iter()
            .map(|(k, v)| json!({"k": format!("{}{}", prefix, k), "n": k, "tok": v}))
            .collect()
    };

    let day_rows: Vec<Value> = out.clone();
    let hours_v: Vec<Value> = hours.iter().map(|x| fnum(round_n(x / 60.0, 1))).collect();
    let mut hb = Map::new();
    for s in spans {
        let arr: Vec<Value> = hours_by[&s].iter().map(|x| fnum(round_n(x / 60.0, 1))).collect();
        hb.insert(s.to_string(), Value::Array(arr));
    }
    let week_rows: Vec<Value> = {
        let mut keys: Vec<&String> = weeks.keys().collect();
        keys.sort();
        keys.iter().map(|k| weeks[*k].clone()).collect()
    };
    let opt_spaces = ranked(&mut opt_sp, "space:");
    let opt_projs = ranked(&mut opt_pj, "proj:");
    let last_day = if last_ts != 0 {
        use chrono::{Local, TimeZone};
        Local
            .timestamp_millis_opt(last_ts)
            .single()
            .map(|t| t.format("%Y-%m-%d").to_string())
            .unwrap_or_default()
    } else {
        String::new()
    };
    let scope_key = scope.clone();
    let data = json!({
        "ok": true,
        "at": chrono::Local::now().format("%H:%M:%S").to_string(),
        "days": day_rows,
        "hours": hours_v,
        "hours_by": Value::Object(hb),
        "weeks": week_rows,
        "spaces": spaces,
        "layer_of": layer_of,
        "scope": scope,
        "last_day": last_day,
        "opts": {"spaces": opt_spaces, "projs": opt_projs},
        "span": {
            "from": out.first().map(|r| r.get("d").cloned().unwrap_or(json!(""))).unwrap_or(json!("")),
            "to": out.last().map(|r| r.get("d").cloned().unwrap_or(json!(""))).unwrap_or(json!("")),
        },
    });
    let mut g = PULSE.lock().unwrap_or_else(|e| e.into_inner());
    *g = Some(Cache { at: now, days, scope: scope_key, data: data.clone() });
    let _ = truthy;
    data
}
