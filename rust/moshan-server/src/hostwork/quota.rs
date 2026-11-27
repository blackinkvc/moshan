// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：额度中断 / 模型切换（hostwork.quota_events / _title_at）。
// 不做：不重新扫会话（读 agg.errs / agg.switches）；不改 KIND_LABEL 的文案。

use serde_json::{json, Map, Value};
use std::collections::HashSet;

use super::agg::sessions_snapshot;
use super::sessions::session_index;
use super::util::{py_int, str_or};
use crate::paths::Ctx;

const QUOTA_MERGE_MS: i64 = 10 * 60 * 1000;
const SWITCH_AFTER_ERR_MS: i64 = 5 * 60 * 1000;

pub fn kind_label() -> Value {
    json!({"credit": "积分花完", "quota": "额度超限", "free": "免费额度",
           "queue": "队列满", "unavailable": "模型不可用",
           "net": "网络中断", "other": "其他中断"})
}

fn kind_label_of(kind: &str) -> String {
    match kind {
        "credit" => "积分花完",
        "quota" => "额度超限",
        "free" => "免费额度",
        "queue" => "队列满",
        "unavailable" => "模型不可用",
        "net" => "网络中断",
        "other" => "其他中断",
        _ => kind,
    }
    .to_string()
}

/// `_proj_of`：剥掉 /Users/xxx/ 只留项目名。
pub fn proj_of(cwd: &str) -> String {
    let mut s = cwd.trim_end_matches('/').to_string();
    for pre in ["/Users/", "/home/"] {
        if let Some(rest) = s.strip_prefix(pre) {
            match rest.find('/') {
                Some(i) => {
                    s = rest[i + 1..].to_string();
                    return s;
                }
                None => {
                    s = rest.to_string();
                    return s;
                }
            }
        }
    }
    s
}

fn title_at(agg: &Value, ts: i64) -> String {
    let mut hit = String::new();
    if let Some(Value::Array(hist)) = agg.get("titles") {
        for t in hist {
            let Some(pair) = t.as_array() else { continue };
            let v = pair.first().map(py_int).unwrap_or(0);
            let s = pair.get(1).map(|v| str_or(Some(v))).unwrap_or_default();
            if v != 0 && v <= ts {
                hit = s;
            } else if v > ts {
                break;
            }
        }
    }
    hit
}

fn fmt(ms: i64) -> String {
    if ms == 0 {
        return String::new();
    }
    use chrono::{Local, TimeZone};
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|t| t.format("%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

fn hm(ms: i64) -> String {
    if ms == 0 {
        return String::new();
    }
    use chrono::{Local, TimeZone};
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|t| t.format("%H:%M").to_string())
        .unwrap_or_default()
}

struct Raw {
    ts: i64,
    code: i64,
    reset: i64,
    kind: String,
    msg: String,
    proj: String,
    title: String,
    was: String,
    gone: bool,
    sid: String,
}

/// `quota_events(days)`
pub fn quota_events(ctx: &Ctx, days: i64) -> Value {
    let snap = sessions_snapshot(ctx);
    let idx = session_index(20.0);
    let now_s = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let lo = if days != 0 {
        ((now_s - days as f64 * 86400.0) * 1000.0) as i64
    } else {
        0
    };
    let mut raw: Vec<Raw> = Vec::new();
    let mut sws: Vec<Value> = Vec::new();
    let mut ok_ts: i64 = 0;
    for agg in &snap {
        ok_ts = ok_ts.max(py_int(agg.get("last_ok").unwrap_or(&Value::Null)));
        let rows = agg.get("errs").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let sw_rows = agg.get("switches").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        if rows.is_empty() && sw_rows.is_empty() {
            continue;
        }
        let agg_proj = str_or(agg.get("proj"));
        let cwd = str_or(agg.get("cwd"));
        let proj = {
            let p = proj_of(&cwd);
            if !p.is_empty() { p } else { agg_proj }
        };
        let sid = {
            let s = str_or(agg.get("id")).trim().to_string();
            if !s.is_empty() { s } else { String::new() }
        };
        let meta = idx.get(&sid).cloned().unwrap_or_else(|| json!({}));
        let cur = {
            let a = str_or(meta.get("title")).trim().chars().take(60).collect::<String>();
            if !a.is_empty() {
                a
            } else {
                str_or(agg.get("title")).chars().take(60).collect()
            }
        };
        let gone = meta.get("deleted").map(super::util::truthy).unwrap_or(false);
        let mut err_ts: Vec<i64> = Vec::new();
        for row in &rows {
            let Some(a) = row.as_array() else { continue };
            if a.len() < 3 {
                continue;
            }
            let ts = py_int(&a[0]);
            let code = py_int(&a[1]);
            let reset = py_int(&a[2]);
            let (kind, msg) = if a.len() >= 5 {
                (str_or(a.get(3)).trim().to_string(), str_or(a.get(4)))
            } else {
                (String::new(), String::new())
            };
            let mut kind = kind;
            if kind.is_empty() {
                kind = match code {
                    14018 => "credit",
                    6004 => "quota",
                    6021 => "queue",
                    11134 => "unavailable",
                    _ => "",
                }
                .to_string();
            }
            if ts == 0 {
                continue;
            }
            err_ts.push(ts);
            if ts < lo || !matches!(kind.as_str(), "credit" | "quota" | "free" | "queue" | "unavailable" | "net" | "other") {
                continue;
            }
            let was = title_at(agg, ts).chars().take(60).collect::<String>();
            raw.push(Raw {
                ts,
                code,
                reset,
                kind,
                msg,
                proj: proj.clone(),
                title: cur.clone(),
                was: if !was.is_empty() && was != cur { was } else { String::new() },
                gone,
                sid: sid.clone(),
            });
        }
        // 模型切换
        let turns: Vec<i64> = agg
            .get("turns")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().map(py_int).collect())
            .unwrap_or_default();
        let mut turned: HashSet<(String, i64)> = HashSet::new();
        for row in &sw_rows {
            let Some(a) = row.as_array() else { continue };
            if a.len() < 3 {
                continue;
            }
            let sts = py_int(&a[0]);
            let frm = str_or(a.get(1));
            let to = str_or(a.get(2));
            if sts == 0 || sts < lo {
                continue;
            }
            if !turns.is_empty() {
                let mut ti = 0usize;
                for (i, tv) in turns.iter().enumerate() {
                    if *tv <= sts {
                        ti = i;
                    } else {
                        break;
                    }
                }
                let key = (sid.clone(), turns[ti]);
                if turned.contains(&key) {
                    continue;
                }
                turned.insert(key);
            }
            let after = err_ts.iter().any(|t| {
                let d = sts - *t;
                d >= 0 && d <= SWITCH_AFTER_ERR_MS
            });
            let was = title_at(agg, sts).chars().take(60).collect::<String>();
            sws.push(json!({
                "at": sts, "frm": frm, "to": to, "auto": after,
                "proj": proj, "title": cur,
                "was": if !was.is_empty() && was != cur { was } else { String::new() },
                "gone": gone, "sid": sid,
            }));
        }
    }
    raw.sort_by(|a, b| a.ts.cmp(&b.ts));
    let mut out: Vec<Value> = Vec::new();
    for r in raw {
        let merge = out.last().map(|prev| {
            let pk = str_or(prev.get("kind"));
            let ps = str_or(prev.get("sid"));
            let pl = py_int(prev.get("last").unwrap_or(&Value::Null));
            pk == r.kind && ps == r.sid && r.ts - pl <= QUOTA_MERGE_MS
        }).unwrap_or(false);
        if merge {
            let prev = out.last_mut().unwrap();
            let o = prev.as_object_mut().unwrap();
            o.insert("last".into(), json!(r.ts));
            o.insert("n".into(), json!(py_int(o.get("n").unwrap_or(&Value::Null)) + 1));
            let reset = py_int(o.get("reset").unwrap_or(&Value::Null)).max(r.reset);
            o.insert("reset".into(), json!(reset));
            if str_or(o.get("title")).is_empty() && !r.title.is_empty() {
                o.insert("title".into(), json!(r.title));
            }
            if str_or(o.get("was")).is_empty() && !r.was.is_empty() {
                o.insert("was".into(), json!(r.was));
            }
            if str_or(o.get("msg")).is_empty() && !r.msg.is_empty() {
                o.insert("msg".into(), json!(r.msg));
            }
        } else {
            let mut m = Map::new();
            m.insert("code".into(), json!(r.code));
            m.insert("kind".into(), json!(r.kind));
            m.insert("at".into(), json!(r.ts));
            m.insert("last".into(), json!(r.ts));
            m.insert("n".into(), json!(1));
            m.insert("reset".into(), json!(r.reset));
            m.insert("proj".into(), json!(r.proj));
            m.insert("title".into(), json!(r.title));
            m.insert("was".into(), json!(r.was));
            m.insert("gone".into(), json!(r.gone));
            m.insert("sid".into(), json!(r.sid));
            m.insert("msg".into(), json!(r.msg));
            out.push(Value::Object(m));
        }
    }
    out.reverse(); // 最近的在最上面

    let now = now_s * 1000.0;
    for e in out.iter_mut() {
        let o = e.as_object_mut().unwrap();
        let at = py_int(o.get("at").unwrap_or(&Value::Null));
        let last = py_int(o.get("last").unwrap_or(&Value::Null));
        let reset = py_int(o.get("reset").unwrap_or(&Value::Null));
        o.insert("at_s".into(), json!(fmt(at)));
        o.insert("hm".into(), json!(hm(at)));
        o.insert(
            "reset_s".into(),
            json!(if reset as f64 > now && ok_ts <= last { fmt(reset) } else { String::new() }),
        );
        o.insert("kind_s".into(), json!(kind_label_of(&str_or(o.get("kind")))));
    }
    for s in sws.iter_mut() {
        let at = py_int(s.get("at").unwrap_or(&Value::Null));
        s["at_s"] = json!(fmt(at));
        s["hm"] = json!(hm(at));
    }
    sws.sort_by(|a, b| {
        py_int(b.get("at").unwrap_or(&Value::Null)).cmp(&py_int(a.get("at").unwrap_or(&Value::Null)))
    });
    let credit_n = out.iter().filter(|e| str_or(e.get("kind")) == "credit").count();
    let quota_n = out
        .iter()
        .filter(|e| matches!(str_or(e.get("kind")).as_str(), "quota" | "free"))
        .count();
    let free_n = out.iter().filter(|e| str_or(e.get("kind")) == "free").count();
    let other_n = out
        .iter()
        .filter(|e| !matches!(str_or(e.get("kind")).as_str(), "credit" | "quota" | "free"))
        .count();
    let retries: i64 = out.iter().map(|e| py_int(e.get("n").unwrap_or(&Value::Null))).sum();
    let last = out.first().map(|e| py_int(e.get("last").unwrap_or(&Value::Null))).unwrap_or(0);
    let last_kind = out.first().map(|e| str_or(e.get("kind"))).unwrap_or_default();
    let mut blocked = 0i64;
    if let Some(e0) = out.first() {
        let k = str_or(e0.get("kind"));
        let reset = py_int(e0.get("reset").unwrap_or(&Value::Null));
        let last0 = py_int(e0.get("last").unwrap_or(&Value::Null));
        if matches!(k.as_str(), "quota" | "free") && reset as f64 > now && ok_ts <= last0 {
            blocked = reset;
        }
    }
    let events: Vec<Value> = out.iter().take(40).cloned().collect();
    let sw_take: Vec<Value> = sws.iter().take(20).cloned().collect();
    json!({
        "days": days,
        "events": events,
        "credit_n": credit_n,
        "quota_n": quota_n,
        "free_n": free_n,
        "other_n": other_n,
        "retries": retries,
        "last": last,
        "last_s": if last != 0 { fmt(last) } else { String::new() },
        "last_kind": last_kind,
        "last_ok": ok_ts,
        "last_ok_s": if ok_ts != 0 { fmt(ok_ts) } else { String::new() },
        "blocked_until": blocked,
        "blocked_until_s": if blocked != 0 { fmt(blocked) } else { String::new() },
        "switches": sw_take,
        "switch_n": sws.len(),
        "kinds": kind_label(),
    })
}

/// overview 的 quota 失败兜底（server.py 里的形状）。
pub fn quota_fallback(days: i64) -> Value {
    json!({"days": days, "events": [], "credit_n": 0, "quota_n": 0, "retries": 0,
           "last": 0, "last_s": "", "last_kind": "", "blocked_until": 0,
           "blocked_until_s": "", "last_ok": 0, "last_ok_s": "",
           "switches": [], "switch_n": 0, "free_n": 0, "other_n": 0,
           "kinds": kind_label()})
}
