// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：「谁在干活」—— hostwork.clients_view：WorkBuddy（模型/工具/MCP/技能/专家）
//       + TRAE（命令/进程/记忆）+ 口袋（按脚本）。
// 不做：不做进程→客户端的色块映射（proc 字段给空表：进程采样归 overview 那边，
//       这里只出账）。

use serde_json::{json, Map, Value};

use super::agg::sessions_snapshot;
use super::price::{cost_of, price_for};
use super::util::{fnum, py_float, py_int, str_or};
use super::{pocket, sessions};
use crate::paths::Ctx;

const GB: f64 = 1073741824.0;

fn range_days(key: &str) -> i64 {
    match key {
        "today" => 1,
        "7d" => 7,
        "30d" => 30,
        "all" => 0,
        _ => 7,
    }
}

fn day_keys(days: i64) -> Vec<String> {
    if days == 0 {
        return Vec::new();
    }
    let today = chrono::Local::now().date_naive();
    (0..days)
        .map(|i| (today - chrono::Duration::days(days - 1 - i)).format("%Y-%m-%d").to_string())
        .rev()
        .collect()
}

/// Python `Counter.most_common()`：次数降序，同次数按首次出现顺序。
fn most_common(c: &[(String, i64)], limit: Option<usize>) -> Vec<(String, i64)> {
    let mut v: Vec<(String, i64)> = c.to_vec();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    if let Some(n) = limit {
        v.truncate(n);
    }
    v
}

fn bump(c: &mut Vec<(String, i64)>, k: &str, n: i64) {
    if let Some(slot) = c.iter_mut().find(|(name, _)| name == k) {
        slot.1 += n;
    } else {
        c.push((k.to_string(), n));
    }
}

fn merge_obj(c: &mut Vec<(String, i64)>, v: Option<&Value>) {
    if let Some(Value::Object(m)) = v {
        for (k, x) in m {
            bump(c, k, py_int(x));
        }
    }
}

/// `trae_work.client_view(days)`
pub fn trae_client_view(ctx: &Ctx, days: i64) -> Option<Value> {
    let since = if days != 0 {
        (super::agg::now_secs() - days as f64 * 86400.0) * 1000.0
    } else {
        0.0
    };
    let raw = sessions::trae_items(&ctx.runtime_dir, since);
    if raw.is_empty() {
        return None;
    }
    let src = |x: &Value, name: &str| {
        x.get("sources")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter().any(|s| {
                    str_or(s.get("kind")) == "code" && str_or(s.get("name")) == name
                })
            })
            .unwrap_or(false)
    };
    // `trae_work.items` 的合并规则：已经有记忆说清这轮干了什么的会话，不再单列「在场」
    // （记忆 end 前后 90 分钟内算同一轮）。少了这一步，卡片上的会话数会多出几条。
    let mem_ends: Vec<f64> = raw
        .iter()
        .filter(|x| src(x, "TRAE 记忆") || src(x, "TRAE 自述"))
        .filter_map(|x| super::util::local_ts(&str_or(x.get("end"))))
        .map(|t| t as f64 * 1000.0)
        .collect();
    let its: Vec<&Value> = raw
        .iter()
        .filter(|x| {
            if !src(x, "会话活跃") {
                return true;
            }
            match super::util::local_ts(&str_or(x.get("end"))) {
                Some(t) => {
                    let se = t as f64 * 1000.0;
                    !mem_ends.iter().any(|me| (se - me).abs() <= 90.0 * 60.0 * 1000.0)
                }
                None => true,
            }
        })
        .collect();
    let mut n_cmd = 0;
    let mut n_proc = 0;
    let mut n_sess = 0;
    let mut n_mem = 0;
    let mut ms = 0i64;
    let mut last = String::new();
    for x in &its {
        if src(x, "命令记录") {
            n_cmd += 1;
        }
        if src(x, "进程证据") {
            n_proc += 1;
        }
        if src(x, "会话活跃") {
            n_sess += 1;
        }
        if src(x, "TRAE 记忆") || src(x, "TRAE 自述") {
            n_mem += 1;
        }
        ms += py_int(x.get("secs").unwrap_or(&Value::Null)) * 1000;
        let e = str_or(x.get("end"));
        let chars: Vec<char> = e.chars().collect();
        if chars.len() >= 16 {
            let v: String = chars[5..16].iter().collect();
            if v > last {
                last = v;
            }
        }
    }
    let n_task = super::trae::task_rows(ctx, since).len();
    let mut kids: Vec<Value> = Vec::new();
    for (kind, n) in [
        ("自述", n_mem),
        ("命令记录", n_cmd),
        ("会话在场", n_sess),
        ("进程时段", n_proc),
        ("任务清单", n_task as i64),
    ] {
        if n != 0 {
            kids.push(json!({"kind": "tool", "name": kind, "n": n}));
        }
    }
    Some(json!({
        "id": "trae", "name": "TRAE", "glyph": "◈", "kind": "client",
        "calls": its.len(), "asks": 0,
        "ms": ms,
        "sessions": its.len(),
        "last": last,
        "tokens": {"in": 0, "out": 0, "cached": 0},
        "kids": kids,
    }))
}

/// `clients_view(days, cfg)`
pub fn clients_view(ctx: &Ctx, days: i64) -> Vec<Value> {
    let cfg = super::price::price_config(ctx);
    let sess = sessions_snapshot(ctx);
    let keep: Option<std::collections::HashSet<String>> =
        if days != 0 { Some(day_keys(days).into_iter().collect()) } else { None };

    let mut wb_calls = 0i64;
    let mut wb_asks = 0i64;
    let mut wb_in = 0i64;
    let mut wb_out = 0i64;
    let mut wb_cached = 0i64;
    let mut wb_sessions = 0i64;
    let mut wb_last = String::new();
    let mut models: Vec<(String, i64, i64, i64, i64)> = Vec::new(); // name, in,out,cached,n
    let mut tools: Vec<(String, i64)> = Vec::new();
    let mut mcps: Vec<(String, i64)> = Vec::new();
    let mut skills: Vec<(String, i64)> = Vec::new();
    let mut agents: Vec<(String, i64)> = Vec::new();
    let mut modes: Vec<(String, i64)> = Vec::new();
    let mut experts: Vec<(String, i64)> = Vec::new();

    for s in &sess {
        let mut used = false;
        if let Some(Value::Object(days_map)) = s.get("days") {
            for (day, dm) in days_map {
                if let Some(k) = &keep {
                    if !k.contains(day) {
                        continue;
                    }
                }
                used = true;
                wb_calls += py_int(dm.get("calls").unwrap_or(&Value::Null));
                wb_asks += py_int(dm.get("asks").unwrap_or(&Value::Null));
                wb_in += py_int(dm.get("in").unwrap_or(&Value::Null));
                wb_out += py_int(dm.get("out").unwrap_or(&Value::Null));
                wb_cached += py_int(dm.get("cached").unwrap_or(&Value::Null));
                merge_obj(&mut tools, dm.get("tools"));
                if let Some(Value::Object(md)) = dm.get("models") {
                    for (m, v) in md {
                        let e = models.iter_mut().find(|x| &x.0 == m);
                        let (tin, tout, tc, n) = (
                            py_int(v.get("in").unwrap_or(&Value::Null)),
                            py_int(v.get("out").unwrap_or(&Value::Null)),
                            py_int(v.get("cached").unwrap_or(&Value::Null)),
                            py_int(v.get("n").unwrap_or(&Value::Null)),
                        );
                        match e {
                            Some(slot) => {
                                slot.1 += tin;
                                slot.2 += tout;
                                slot.3 += tc;
                                slot.4 += n;
                            }
                            None => models.push((m.clone(), tin, tout, tc, n)),
                        }
                    }
                }
            }
        }
        if !used {
            continue;
        }
        wb_sessions += 1;
        merge_obj(&mut mcps, s.get("mcps"));
        merge_obj(&mut skills, s.get("skills"));
        merge_obj(&mut agents, s.get("agents"));
        merge_obj(&mut modes, s.get("modes"));
        merge_obj(&mut experts, s.get("experts"));
        let ts = py_int(s.get("last").unwrap_or(&Value::Null));
        if ts != 0 {
            let at = ms_md_hm(ts);
            if at > wb_last {
                wb_last = at;
            }
        }
    }

    let mut kids: Vec<Value> = Vec::new();
    // models: 按 (in+out) 降序，稳定
    let mut morder: Vec<usize> = (0..models.len()).collect();
    morder.sort_by(|&a, &b| (models[b].1 + models[b].2).cmp(&(models[a].1 + models[a].2)));
    for i in morder {
        let m = &models[i];
        let stat = json!({"in": m.1, "out": m.2, "cached": m.3, "n": m.4});
        let price = price_for(&m.0, &cfg);
        let (pts, cost) = cost_of(&stat, &price);
        kids.push(json!({
            "kind": "model", "name": m.0, "n": m.4,
            "tokens": {"in": m.1, "out": m.2, "cached": m.3},
            "points": opt_num(pts), "cost": opt_num(cost),
        }));
    }
    for (k, n) in most_common(&modes, None) {
        kids.push(json!({"kind": "mode", "name": k, "n": n}));
    }
    for (k, n) in most_common(&experts, None) {
        kids.push(json!({"kind": "expert", "name": k, "n": n}));
    }
    for (k, n) in most_common(&mcps, None) {
        kids.push(json!({"kind": "mcp", "name": k, "n": n}));
    }
    for (k, n) in most_common(&skills, None) {
        kids.push(json!({"kind": "skill", "name": k, "n": n}));
    }
    for (k, n) in most_common(&agents, None) {
        kids.push(json!({"kind": "agent", "name": k, "n": n}));
    }
    for (k, n) in most_common(&tools, Some(24)) {
        kids.push(json!({"kind": "tool", "name": k, "n": n}));
    }

    let mut wb = Map::new();
    wb.insert("id".into(), json!("workbuddy"));
    wb.insert("name".into(), json!("WorkBuddy"));
    wb.insert("glyph".into(), json!("✦"));
    wb.insert("kind".into(), json!("client"));
    wb.insert("calls".into(), json!(wb_calls));
    wb.insert("asks".into(), json!(wb_asks));
    wb.insert("ms".into(), json!(0));
    wb.insert("tokens".into(), json!({"in": wb_in, "out": wb_out, "cached": wb_cached}));
    wb.insert("kids".into(), Value::Array(kids));
    wb.insert("sessions".into(), json!(wb_sessions));
    wb.insert("last".into(), json!(wb_last));
    let mut out: Vec<Value> = vec![Value::Object(wb)];

    if let Some(tr) = trae_client_view(ctx, days) {
        out.push(tr);
    }

    let pk = pocket::pocket_events(days);
    let scripts = pk.get("scripts").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    if !scripts.is_empty() {
        let n: i64 = scripts.iter().map(|s| py_int(s.get("n").unwrap_or(&Value::Null))).sum();
        let ms: i64 = scripts.iter().map(|s| py_int(s.get("ms").unwrap_or(&Value::Null))).sum();
        let kids: Vec<Value> = scripts
            .iter()
            .map(|s| json!({
                "kind": "script", "name": s.get("script").cloned().unwrap_or(Value::Null),
                "n": s.get("n").cloned().unwrap_or(json!(0)),
                "ms": s.get("ms").cloned().unwrap_or(json!(0)),
                "fail": s.get("fail").cloned().unwrap_or(json!(0)),
            }))
            .collect();
        out.push(json!({
            "id": "pocket", "name": "口袋", "glyph": "◈", "kind": "client",
            "calls": n, "asks": 0, "ms": ms, "sessions": 0, "last": "",
            "tokens": {"in": 0, "out": 0, "cached": 0},
            "kids": kids,
        }));
    }
    // 客户端 ↔ 进程：左边那棵树上的色块要和进程榜里同一个软件同色，靠 owners 对起来
    let (snap, _) = super::procs_state::snapshot();
    let apps: Vec<Value> = snap
        .get("apps")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let icon_dir = ctx.runtime_dir.join("_本机工作").join("图标");
    for c in out.iter_mut() {
        let cid = str_or(c.get("id"));
        let pv = client_proc(&icon_dir, &cid, &apps);
        if let Some(o) = c.as_object_mut() {
            o.insert("proc".into(), pv);
        }
    }
    out
}

/// `_client_proc(cid)`：拿进程榜里属于这家客户端、内存最大的那个，给色块用。
fn client_proc(icon_dir: &std::path::Path, cid: &str, apps: &[Value]) -> Value {
    let matches = |a: &Value| -> bool {
        let path = str_or(a.get("path"));
        let name = str_or(a.get("name"));
        let own = a.get("owners").and_then(|v| v.as_array());
        if let Some(own) = own {
            if own.iter().any(|x| str_or(Some(x)) == cid) {
                return true;
            }
        }
        let hay = format!("{} {}", path, name).to_lowercase();
        match cid {
            "workbuddy" => hay.contains("workbuddy"),
            "trae" => {
                hay.contains("trae cn") || hay.contains("trae.app") || hay.contains("/trae") || hay.contains("\\trae")
            }
            "pocket" => {
                hay.contains("local-ai-workspace/pocket")
                    || hay.contains("local-ai-workspace\\pocket")
                    || hay.contains("pocket.log")
                    || hay.contains("pocket/journal")
                    || hay.contains("pocket\\journal")
                    || hay.contains("/pocket/server.py")
                    || hay.contains("/moshan/")
            }
            _ => false,
        }
    };
    if !matches!(cid, "workbuddy" | "trae" | "pocket") {
        return json!({});
    }
    let mut best: Option<&Value> = None;
    for a in apps {
        if !matches(a) {
            continue;
        }
        let mem = a.get("mem").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let bmem = best.and_then(|b| b.get("mem").and_then(|v| v.as_f64())).unwrap_or(-1.0);
        if best.is_none() || mem > bmem {
            best = Some(a);
        }
    }
    let Some(a) = best else { return json!({}) };
    let name = str_or(a.get("name"));
    let path = str_or(a.get("path"));
    json!({
        "name": name,
        "icon": super::procnames::icon_name(icon_dir, &name, &path),
        "kind": super::procnames::proc_kind(&name, &path),
    })
}


fn opt_num(v: Option<f64>) -> Value {
    match v {
        Some(x) => fnum(x),
        None => Value::Null,
    }
}

fn ms_md_hm(ms: i64) -> String {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|t| t.format("%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

// 让 unused 的 GB 常量不报警（TRAE 侧将来补内存时用）。
const _: f64 = GB;
// 保留给 overview 里的调用方使用。
pub fn _unused(_x: f64) -> f64 {
    py_float(&Value::Null).unwrap_or(0.0)
}
