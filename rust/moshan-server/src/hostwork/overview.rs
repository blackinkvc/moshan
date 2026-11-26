// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：`GET /api/overview` —— 本机工作的机器指标 + token 总账 + 时钟 / 额度 / 实付账，
//       一次给全（对齐 hostwork.overview）。含降级形状 `_ov_fallback`。
// 不做：不在这里拼 prompts / llm —— 那两段是路由层（server.py 的 _with_prompts /
//       _with_llm）加的装饰，放在 w5a/mod.rs 的 handler 里。
#![allow(dead_code)]

use super::agg;
use super::billing;
use super::cherry;
use super::clients;
use super::hourly;
use super::machine;
use super::pocket;
use super::price;
use super::procs_state;
use super::procnames;
use super::quota;
use super::spec;
use super::util::{self, fnum, py_int, round_n, round1, round2, str_or, truthy};
use crate::paths::Ctx;
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::sync::Mutex;

pub const SER_KEYS: [&str; 6] = ["cpu", "mem", "dw", "dr", "net", "pwr"];

pub fn range_label(key: &str) -> (&'static str, i64) {
    match key {
        "today" => ("今天", 1),
        "7d" => ("7 天", 7),
        "30d" => ("30 天", 30),
        "all" => ("全部", 0),
        _ => ("7 天", 7),
    }
}

fn range_days(key: &str) -> i64 {
    range_label(key).1
}

fn day_keys(days: i64) -> Vec<String> {
    if days == 0 {
        return Vec::new();
    }
    let today = chrono::Local::now().date_naive();
    (0..days)
        .rev()
        .map(|i| (today - chrono::Duration::days(i)).format("%Y-%m-%d").to_string())
        .collect()
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

/// `_apps_with_icons`
fn apps_with_icons(icon_dir: &std::path::Path, apps: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    for (i, a) in apps.iter().enumerate() {
        if i < 128 {
            if let Value::Object(m) = a {
                let mut m = m.clone();
                let path = str_or(m.get("path"));
                let name = str_or(m.get("name"));
                m.insert("icon".into(), Value::String(procnames::icon_name(icon_dir, &name, &path)));
                let mut kind = procnames::proc_kind(&name, &path).to_string();
                if m.get("icon").and_then(|v| v.as_str()).unwrap_or("").is_empty()
                    && kind == "app"
                    && (path.starts_with("/System/") || path.starts_with("/Library/Apple/"))
                {
                    kind = "sys".into();
                }
                m.insert("kind".into(), Value::String(kind));
                out.push(Value::Object(m));
                continue;
            }
        }
        out.push(a.clone());
    }
    out
}

/// `_hist_val(v, i)`
fn hist_val(v: &Value, i: usize) -> f64 {
    procs_state::hist_val(v, i)
}

/// `_app_series(hist, step=3, per=3, cap=12)`
fn app_series(hist: &[Value]) -> Value {
    let step = 3usize;
    let per = 3usize;
    let cap = 12usize;
    if hist.is_empty() {
        return json!({
            "at": [],
            "step": procs_state::TICK_SECS * step as i64,
            "keys": SER_KEYS,
            "cpu": [],
            "apps": [],
        });
    }
    let mut picked: Vec<Value> = hist.iter().step_by(step).cloned().collect();
    if let (Some(last), Some(real)) = (picked.last(), hist.last()) {
        if last != real {
            picked.push(real.clone());
        }
    }
    let mut names: Vec<String> = Vec::new();
    for i in 0..SER_KEYS.len() {
        let mut peak: Vec<(String, f64)> = Vec::new();
        for h in &picked {
            if let Some(apps) = h.get("apps").and_then(|v| v.as_object()) {
                for (n, v) in apps {
                    let x = hist_val(v, i);
                    match peak.iter_mut().find(|(nn, _)| nn == n) {
                        Some((_, pv)) => {
                            if x > *pv {
                                *pv = x;
                            }
                        }
                        None => peak.push((n.clone(), x)),
                    }
                }
            }
        }
        // sorted(peak.items(), key=-v) —— 稳定，同名同值时保持首次出现序
        peak.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        for (n, _) in peak.iter().take(per) {
            if !names.contains(n) {
                names.push(n.clone());
            }
        }
    }
    let mut out = Vec::new();
    for n in names.iter().take(cap) {
        let mut cols: Vec<Value> = Vec::new();
        for i in 0..SER_KEYS.len() {
            let mut col = Vec::new();
            for h in &picked {
                let v = h
                    .get("apps")
                    .and_then(|x| x.as_object())
                    .and_then(|m| m.get(n))
                    .cloned()
                    .unwrap_or(Value::Null);
                col.push(fnum(round_n(hist_val(&v, i), 1)));
            }
            cols.push(Value::Array(col));
        }
        let mut m = Map::new();
        m.insert("name".into(), Value::String(n.clone()));
        m.insert("v".into(), Value::Array(cols));
        out.push(Value::Object(m));
    }
    let at: Vec<Value> = picked
        .iter()
        .map(|h| {
            let ts = h.get("at").and_then(|v| v.as_f64()).unwrap_or(0.0) as i64;
            Value::String(super::util::ts_to_str(ts).get(11..16).unwrap_or("").to_string())
        })
        .collect();
    let cpu: Vec<Value> = picked
        .iter()
        .map(|h| fnum(round_n(h.get("cpu").and_then(|v| v.as_f64()).unwrap_or(0.0), 1)))
        .collect();
    json!({
        "at": at,
        "step": procs_state::TICK_SECS * step as i64,
        "keys": SER_KEYS,
        "cpu": cpu,
        "apps": out,
    })
}

// ------------------------------------------------------------------ 降级账

fn metrics_disk(ctx: &Ctx) -> Value {
    let sys = ctx.library_dir.join("10-系统记录");
    let target = if sys.exists() { sys } else { home() };
    super::disk::metrics_disk(&target)
}

/// `server._ov_fallback(key, reason)`
pub fn fallback(ctx: &Ctx, key: &str, reason: &str) -> Value {
    let machine_disk = metrics_disk(ctx);
    let s = super::procs::sample();
    let mem = s.get("mem").cloned().unwrap_or(json!({}));
    let cpu = s.get("cpu").cloned().unwrap_or(json!({}));
    let cpus = cpu
        .get("cores")
        .and_then(|v| v.as_i64())
        .filter(|n| *n > 0)
        .unwrap_or_else(|| super::procs::sample_cpus());

    let mut machine = Map::new();
    machine.insert("disk".into(), machine_disk);
    machine.insert("extra_disks".into(), json!([]));
    machine.insert(
        "mem".into(),
        json!({
            "mem_pct": mem.get("pct").cloned().unwrap_or(Value::Null),
            "mem_gb": mem.get("used_gb").cloned().unwrap_or(Value::Null),
            "mem_total_gb": mem.get("total_gb").cloned().unwrap_or(Value::Null),
        }),
    );
    machine.insert("load".into(), json!([]));
    machine.insert("cpus".into(), json!(cpus));
    machine.insert("uptime_h".into(), s.get("uptime_h").cloned().unwrap_or(Value::Null));
    machine.insert("cpu_pct".into(), cpu.get("pct").cloned().unwrap_or(Value::Null));
    machine.insert("cpu_ready".into(), json!(true));
    machine.insert("cpu_proc_pct".into(), Value::Null);
    machine.insert("rss_total_gb".into(), s.get("rss_gb").cloned().unwrap_or(Value::Null));
    machine.insert("net".into(), json!({"in_bps": 0.0, "out_bps": 0.0, "in_total": 0.0, "out_total": 0.0}));
    machine.insert("gpu".into(), json!({}));

    let snaps_path = ctx.library_dir.join("10-系统记录").join("机器").join("快照.jsonl");
    let snaps = super::disk::load_snapshot(&snaps_path, None);
    let tail: Vec<Value> = if snaps.len() > 60 {
        snaps[snaps.len() - 60..].to_vec()
    } else {
        snaps
    };
    let (dser, mser, lser) = super::disk::series_tail(&tail);

    let (label, days) = range_label(key);
    let mut o = Map::new();
    o.insert("ok".into(), json!(true));
    o.insert("degraded".into(), Value::String(reason.to_string()));
    o.insert("now".into(), Value::String(super::util::now_ymd_hms()));
    o.insert(
        "range".into(),
        json!({"key": key, "label": label, "days": days}),
    );
    o.insert("machine".into(), Value::Object(machine));
    o.insert("series".into(), json!({"disk": dser, "mem": mser, "load": lser, "cpus": cpus}));
    o.insert(
        "app_series".into(),
        json!({"at": [], "step": 30, "keys": [], "cpu": [], "apps": []}),
    );
    o.insert("spec".into(), json!({}));
    o.insert(
        "installed".into(),
        json!({"apps": [], "formula": [], "leaves": [], "cask": [], "brew": ""}),
    );
    o.insert(
        "units".into(),
        json!({"user": [], "global": [], "daemon": [], "brew_services": []}),
    );
    o.insert("apps".into(), json!([]));
    o.insert(
        "netproc".into(),
        json!({"ok": false, "every": 60, "err": "降级模式不采网络明细"}),
    );
    o.insert("clients".into(), json!([]));
    o.insert(
        "tokens".into(),
        json!({"by_day": [], "by_model": [],
               "totals": {"in": 0, "out": 0, "cached": 0, "calls": 0, "total": 0},
               "hourly": [], "hour_days": [], "hour_day": "", "cache_hit": 0}),
    );
    o.insert(
        "quota".into(),
        json!({"days": 90, "events": [], "credit_n": 0, "quota_n": 0, "free_n": 0,
               "other_n": 0, "retries": 0, "last": 0, "last_s": "", "last_kind": "",
               "last_ok": 0, "last_ok_s": "", "blocked_until": 0, "blocked_until_s": "",
               "switches": [], "switch_n": 0, "kinds": {}}),
    );
    o.insert(
        "cherry".into(),
        json!({"found": false, "calls": 0, "days": days}),
    );
    o.insert(
        "billing".into(),
        json!({"ok": false, "accounts": [], "n": 0, "total_balance": 0,
               "total_month": 0, "at": ""}),
    );
    o.insert(
        "money".into(),
        json!({"currency": "¥", "rows": [], "points": 0, "cost": 0,
               "any_points": false, "any_cost": false, "priced_ratio": 0,
               "unpriced": [], "formula": "", "points_formula": "", "config": ""}),
    );
    o.insert("pocket".into(), json!({"scripts": [], "recent": [], "tokens": 0}));
    o.insert("daily".into(), json!([]));
    o.insert(
        "scan".into(),
        json!({"done": 0, "total": 0, "running": false,
               "started": "", "err": "", "bytes": 0, "cached_bytes": 0}),
    );
    o.insert("procs_path".into(), Value::String(String::new()));
    Value::Object(o)
}

// ------------------------------------------------------------------ 单飞

// 冷启动这一趟要十几秒（提示词要读几万行 jsonl、cherry 要翻 sqlite、billing 要拉
// 余额）。并发轮询时不能每个请求都各算一遍：这里用一把锁把计算串起来，算完的结果
// 留 3 秒给排队的请求复用。Python 侧没有缓存（每次都现算），所以这里只做「排队 +
// 极短复用」，不改任何字段口径。
static OV_LOCK: Mutex<()> = Mutex::new(());
static OV_CACHE: Mutex<Option<(String, bool, f64, Value)>> = Mutex::new(None);

/// 路由入口：串行化 + 3 秒内复用（`fresh=1` 时只串行、不复用，等价 Python 的绕过缓存）
pub fn gated(ctx: &Ctx, range_key: &str, fresh: bool) -> Value {
    let _g = OV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let now = util::now_local().timestamp_millis() as f64 / 1000.0;
    if !fresh {
        if let Ok(c) = OV_CACHE.lock() {
            if let Some((k, f, at, v)) = c.as_ref() {
                if k == range_key && !*f && now - *at < 3.0 {
                    return v.clone();
                }
            }
        }
    }
    let v = overview(ctx, range_key, fresh);
    if let Ok(mut c) = OV_CACHE.lock() {
        // 时间戳取「算完那一刻」：冷启动这一趟十秒级，用开始时刻会让排队的请求
        // 拿到已过期的账，等于白排队。
        let at = util::now_local().timestamp_millis() as f64 / 1000.0;
        *c = Some((range_key.to_string(), fresh, at, v.clone()));
    }
    v
}

// ------------------------------------------------------------------ 主体

/// `overview(range_key, fresh)`
pub fn overview(ctx: &Ctx, range_key: &str, fresh: bool) -> Value {
    let days = range_days(range_key);
    let cfg = price::price_config(ctx);
    procs_state::start_at(ctx.library_dir.join("_本机工作"));
    let (proc, hist) = procs_state::snapshot();
    let mem = machine::mem();
    let disk = machine::disk(&ctx.library_dir);
    let load: Vec<Value> = machine::load();

    let clients_v = clients::clients_view(ctx, days);

    let (hourly_v, hour_days) = match std::panic::catch_unwind(|| {
        let h = hourly::hourly_tokens(ctx, "", false);
        let d = hourly::hourly_days(ctx);
        (h, d)
    }) {
        Ok((h, d)) => (h, d),
        Err(_) => (Vec::new(), Vec::new()),
    };

    let quota_v = match std::panic::catch_unwind(|| quota::quota_events(ctx, 90)) {
        Ok(v) => v,
        Err(_) => json!({"days": 90, "events": [], "credit_n": 0, "quota_n": 0,
                         "retries": 0, "last": 0, "last_s": "", "last_kind": "",
                         "blocked_until": 0, "blocked_until_s": "", "last_ok": 0,
                         "last_ok_s": "", "switches": [], "switch_n": 0,
                         "free_n": 0, "other_n": 0, "kinds": {}}),
    };

    let cherry_v = match std::panic::catch_unwind(|| cherry::view(days)) {
        Ok(v) => v,
        Err(_) => json!({"found": false, "cny": 0.0, "cny_cherry": 0.0,
                         "calls": 0, "models": [], "by_day": [], "unpriced": []}),
    };

    let est = cherry::billing_est(&cherry_v);
    let bill = match std::panic::catch_unwind(|| billing::view(ctx, &est)) {
        Ok(v) => v,
        Err(_) => json!({"ok": false, "accounts": [], "n": 0,
                         "total_balance": 0, "total_month": 0, "store": ""}),
    };

    // token 明细（按天 / 按模型）
    let keep: Option<Vec<String>> = if days != 0 { Some(day_keys(days)) } else { None };
    let mut by_day: Vec<(String, Map<String, Value>)> = Vec::new();
    let mut by_model: Vec<(String, Map<String, Value>)> = Vec::new();

    let mut add_day = |by_day: &mut Vec<(String, Map<String, Value>)>,
                       day: &str,
                       src: &Map<String, Value>| {
        let pos = match by_day.iter().position(|(d, _)| d == day) {
            Some(i) => i,
            None => {
                let mut m = Map::new();
                m.insert("d".into(), Value::String(day.to_string()));
                for k in ["in", "out", "cached", "calls"] {
                    m.insert(k.into(), json!(0));
                }
                by_day.push((day.to_string(), m));
                by_day.len() - 1
            }
        };
        let b = &mut by_day[pos].1;
        for k in ["in", "out", "cached", "calls"] {
            let cur = py_int(b.get(k).unwrap_or(&Value::Null));
            let add = py_int(src.get(k).unwrap_or(&Value::Null));
            b.insert(k.into(), json!(cur + add));
        }
    };

    for s in agg::sessions_snapshot(ctx) {
        let dmap = match s.get("days").and_then(|v| v.as_object()) {
            Some(m) => m,
            None => continue,
        };
        for (day, dm) in dmap {
            if let Some(k) = &keep {
                if !k.contains(day) {
                    continue;
                }
            }
            let dm = match dm.as_object() {
                Some(m) => m,
                None => continue,
            };
            add_day(&mut by_day, day, dm);
            if let Some(models) = dm.get("models").and_then(|v| v.as_object()) {
                for (mname, mv) in models {
                    let mv = match mv.as_object() {
                        Some(m) => m,
                        None => continue,
                    };
                    accumulate_model(&mut by_model, mname, mv, false);
                }
            }
        }
    }

    // 自报账并进同一份按天 / 按模型
    let usage = hourly::agents_usage_path(ctx);
    for (day, dm) in hourly::agent_by_day(&usage, days) {
        if let Some(k) = &keep {
            if !k.contains(&day) {
                continue;
            }
        }
        add_day(&mut by_day, &day, &dm);
        if let Some(models) = dm.get("models").and_then(|v| v.as_object()) {
            for (mname, mv) in models {
                let mv = match mv.as_object() {
                    Some(m) => m,
                    None => continue,
                };
                accumulate_model(&mut by_model, mname, mv, true);
            }
        }
    }

    // 定价一轮
    let mut rows: Vec<Value> = Vec::new();
    let mut tot_pts = 0.0f64;
    let mut tot_cost = 0.0f64;
    let mut any_pts = false;
    let mut any_cost = false;
    let mut priced_tokens = 0i64;
    let mut all_tokens = 0i64;
    let mut unpriced: Vec<String> = Vec::new();

    by_model.sort_by(|a, b| {
        let ta = py_int(a.1.get("in").unwrap_or(&Value::Null)) + py_int(a.1.get("out").unwrap_or(&Value::Null));
        let tb = py_int(b.1.get("in").unwrap_or(&Value::Null)) + py_int(b.1.get("out").unwrap_or(&Value::Null));
        tb.cmp(&ta)
    });
    for (m, v) in &by_model {
        let is_agent = v.get("src").and_then(|x| x.as_str()) == Some("agent");
        let price = if is_agent {
            price::explicit_price(m, &cfg)
        } else {
            Some(price::price_for(m, &cfg))
        }
        .unwrap_or(json!({}));
        let (pts, mut cost) = price::cost_of(&json!(v), &price);
        if is_agent {
            if let Some(cp) = v.get("cost_paid").and_then(|x| x.as_f64()) {
                cost = Some(round_n(cost.unwrap_or(0.0) + cp, 4));
            }
        }
        let tin = py_int(v.get("in").unwrap_or(&Value::Null));
        let tout = py_int(v.get("out").unwrap_or(&Value::Null));
        let tok = tin + tout;
        all_tokens += tok;
        if cost.is_some() || pts.is_some() {
            priced_tokens += tok;
        } else if !is_agent {
            unpriced.push(m.clone());
        }
        if let Some(p) = pts {
            tot_pts += p;
            any_pts = true;
        }
        if let Some(c) = cost {
            tot_cost += c;
            any_cost = true;
        }
        let mut row = Map::new();
        row.insert("model".into(), Value::String(m.clone()));
        row.insert("in".into(), json!(tin));
        row.insert("out".into(), json!(tout));
        row.insert("cached".into(), json!(py_int(v.get("cached").unwrap_or(&Value::Null))));
        row.insert("total".into(), json!(tok));
        row.insert("n".into(), json!(py_int(v.get("n").unwrap_or(&Value::Null))));
        row.insert("points".into(), match pts { Some(p) => fnum(p), None => Value::Null });
        row.insert("cost".into(), match cost { Some(c) => fnum(c), None => Value::Null });
        if is_agent {
            row.insert("src".into(), Value::String("agent".into()));
        }
        rows.push(Value::Object(row));
    }

    let mut totals = Map::new();
    let mut t_in = 0i64;
    let mut t_out = 0i64;
    let mut t_cached = 0i64;
    let mut t_calls = 0i64;
    for r in &rows {
        t_in += py_int(r.get("in").unwrap_or(&Value::Null));
        t_out += py_int(r.get("out").unwrap_or(&Value::Null));
        t_cached += py_int(r.get("cached").unwrap_or(&Value::Null));
        t_calls += py_int(r.get("n").unwrap_or(&Value::Null));
    }
    totals.insert("in".into(), json!(t_in));
    totals.insert("out".into(), json!(t_out));
    totals.insert("cached".into(), json!(t_cached));
    totals.insert("calls".into(), json!(t_calls));
    totals.insert("total".into(), json!(t_in + t_out));

    // machine
    let cpu_pct = {
        let a = proc.get("cpu_true").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let b = proc.get("cpu_pct").and_then(|v| v.as_f64()).unwrap_or(0.0);
        if a != 0.0 { a } else { b }
    };
    let cpu_proc_pct = proc.get("cpu_pct").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let cpus = super::procs::sample_cpus();
    let mut mmachine = Map::new();
    mmachine.insert("disk".into(), disk);
    mmachine.insert("extra_disks".into(), Value::Array(machine::extra_disks(fresh)));
    mmachine.insert("mem".into(), mem.clone());
    mmachine.insert("load".into(), Value::Array(load));
    mmachine.insert("net".into(), machine::net());
    mmachine.insert("power".into(), machine::power());
    mmachine.insert("gpu".into(), machine::gpu());
    mmachine.insert("cpus".into(), json!(cpus));
    mmachine.insert("uptime_h".into(), fnum(machine::uptime_h()));
    mmachine.insert("cpu_pct".into(), fnum(cpu_pct));
    mmachine.insert(
        "cpu_ready".into(),
        json!(proc.get("cpu_ready").and_then(|v| v.as_bool()).unwrap_or(false)),
    );
    mmachine.insert("cpu_proc_pct".into(), fnum(cpu_proc_pct));
    mmachine.insert(
        "rss_total_gb".into(),
        fnum(round_n(proc.get("total_rss").and_then(|v| v.as_f64()).unwrap_or(0.0) / 1024.0, 2)),
    );

    let series_days = if days != 0 { days.max(1) } else { 7 };
    let series = machine::snapshots(&ctx.library_dir, series_days);

    let apps_v = apps_with_icons(
        &ctx.library_dir.join("_本机工作").join("图标"),
        &proc.get("apps").and_then(|v| v.as_array()).cloned().unwrap_or_default(),
    );

    // 只留近一小时的 hist
    let now = super::util::now_local().timestamp() as f64;
    let hist_hour: Vec<Value> = hist
        .into_iter()
        .filter(|h| h.get("at").and_then(|v| v.as_f64()).unwrap_or(0.0) >= now - 3600.0)
        .collect();

    let mut tokens = Map::new();
    let by_day_sorted: Vec<Value> = {
        let mut v = by_day.clone();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v.into_iter().map(|(_, m)| Value::Object(m)).collect()
    };
    tokens.insert("by_day".into(), Value::Array(by_day_sorted));
    tokens.insert("by_model".into(), Value::Array(rows.clone()));
    tokens.insert("totals".into(), Value::Object(totals));
    tokens.insert("hourly".into(), Value::Array(hourly_v));
    tokens.insert("hour_days".into(), json!(hour_days));
    tokens.insert("hour_day".into(), Value::String(super::util::now_ymd()));
    tokens.insert(
        "cache_hit".into(),
        if t_in != 0 {
            fnum(round_n(t_cached as f64 / t_in as f64, 3))
        } else {
            Value::Null
        },
    );

    let mut money = Map::new();
    money.insert(
        "currency".into(),
        Value::String(
            cfg.get("currency")
                .and_then(|v| v.as_str())
                .unwrap_or("¥")
                .to_string(),
        ),
    );
    money.insert("rows".into(), Value::Array(rows));
    money.insert("points".into(), if any_pts { fnum(round_n(tot_pts, 1)) } else { Value::Null });
    money.insert("cost".into(), if any_cost { fnum(round_n(tot_cost, 2)) } else { Value::Null });
    money.insert("any_points".into(), json!(any_pts));
    money.insert("any_cost".into(), json!(any_cost));
    money.insert(
        "priced_ratio".into(),
        if all_tokens != 0 {
            fnum(round_n(priced_tokens as f64 / all_tokens as f64, 3))
        } else {
            Value::Null
        },
    );
    money.insert(
        "unpriced".into(),
        json!(unpriced.iter().take(6).cloned().collect::<Vec<_>>()),
    );
    money.insert(
        "formula".into(),
        Value::String(
            if any_pts && !any_cost {
                "积分 = 总 token ÷ 100 万 × 该模型实测积分单价（不是官方 API 价）"
            } else {
                "输入(未命中)×单价 + 输入(缓存命中)×缓存价 + 输出×输出价；单位 元/百万 token"
            }
            .into(),
        ),
    );
    money.insert(
        "points_formula".into(),
        Value::String("点数 = 总 token ÷ 100 万 × points_per_mtok".into()),
    );
    money.insert(
        "config".into(),
        Value::String(price::price_path(ctx).to_string_lossy().to_string()),
    );

    let (label, rdays) = range_label(range_key);
    let mut o = Map::new();
    o.insert("now".into(), Value::String(super::util::now_ymd_hms()));
    o.insert(
        "range".into(),
        json!({"key": range_key, "label": label, "days": rdays}),
    );
    o.insert("machine".into(), Value::Object(mmachine));
    o.insert("series".into(), series);
    o.insert("spec".into(), spec::specs(fresh, ctx));
    o.insert("installed".into(), spec::software(fresh));
    o.insert("units".into(), spec::units(fresh));
    o.insert("apps".into(), Value::Array(apps_v));
    o.insert("app_series".into(), app_series(&hist_hour));
    o.insert("netproc".into(), procs_state::netproc_info());
    o.insert("clients".into(), Value::Array(clients_v));
    o.insert("tokens".into(), Value::Object(tokens));
    o.insert("quota".into(), quota_v);
    o.insert("cherry".into(), cherry_v);
    o.insert("billing".into(), bill);
    o.insert("money".into(), Value::Object(money));
    o.insert("pocket".into(), pocket::pocket_events(days));
    o.insert("daily".into(), Value::Array(machine::daily_history(&ctx.library_dir, days.min(30) as usize)));
    o.insert("scan".into(), agg::scan_json());
    o.insert(
        "procs_path".into(),
        Value::String(agg::proc_path(ctx).to_string_lossy().to_string()),
    );
    Value::Object(o)
}

fn accumulate_model(
    by_model: &mut Vec<(String, Map<String, Value>)>,
    name: &str,
    src: &Map<String, Value>,
    agent: bool,
) {
    let pos = match by_model.iter().position(|(m, _)| m == name) {
        Some(i) => i,
        None => {
            let mut m = Map::new();
            m.insert("model".into(), Value::String(name.to_string()));
            for k in ["in", "out", "cached", "n"] {
                m.insert(k.into(), json!(0));
            }
            by_model.push((name.to_string(), m));
            by_model.len() - 1
        }
    };
    let cur = &mut by_model[pos].1;
    for k in ["in", "out", "cached", "n"] {
        let a = py_int(cur.get(k).unwrap_or(&Value::Null));
        let b = py_int(src.get(k).unwrap_or(&Value::Null));
        cur.insert(k.into(), json!(a + b));
    }
    if agent {
        cur.insert("src".into(), Value::String("agent".into()));
        if let Some(c) = src.get("cost").and_then(|v| v.as_f64()) {
            let paid = cur.get("cost_paid").and_then(|v| v.as_f64()).unwrap_or(0.0);
            cur.insert("cost_paid".into(), fnum(round_n(paid + c, 4)));
        }
    }
}
