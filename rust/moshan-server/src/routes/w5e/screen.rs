// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：系统屏幕使用时间（对齐 moshan/workstat.py 的 screen / _app_names /
//       _screen_from_samples / 前台采样）。
// 不做：不画图、不判断「人在不在」。只把 knowledgeC.db 的 /app/usage 区间折成
//       「每天每个 App 多少秒」「每小时有人在用多少秒」。
//
// 为什么 SQL 走 sqlite3 命令行并把时间列 printf('%.17g')：Cargo.toml 里没有
// sqlite 依赖（不许改），而默认的 JSON 数字只印 15 位有效数字；17 位才保证
// 双精度原样回来，跟 Python 的 sqlite3 模块取到的是同一个 double。
#![allow(dead_code)]

use crate::hostwork::platform;
use crate::routes::w5e::util::{dumps_spaced, loads, round_int};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const CF_EPOCH: f64 = 978_307_200.0;
pub const SAMPLE_EVERY: f64 = 60.0;
const IDLE_MAX_S: f64 = 180.0;

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

pub fn kc_db() -> PathBuf {
    home()
        .join("Library")
        .join("Application Support")
        .join("Knowledge")
        .join("knowledgeC.db")
}

pub fn cache_dir(root: &Path) -> PathBuf {
    root.join("_本机工作").join("缓存")
}

fn name_cache_path(root: &Path) -> PathBuf {
    cache_dir(root).join("应用名.json")
}

fn sample_path(root: &Path) -> PathBuf {
    cache_dir(root).join("屏幕采样.jsonl")
}

/// 兜底译名：系统盘扫不到的那几个（与 Python `_NAME_FIX` 同序）。
pub const NAME_FIX: [(&str, &str); 11] = [
    ("com.tencent.workbuddy.mac", "WorkBuddy"),
    ("cn.trae.app", "TRAE"),
    ("md.obsidian", "Obsidian"),
    ("com.apple.Safari", "Safari"),
    ("com.apple.Terminal", "Terminal"),
    ("com.apple.finder", "访达"),
    ("com.apple.dt.Xcode", "Xcode"),
    ("com.tencent.xinWeChat", "微信"),
    ("com.tencent.qq", "QQ"),
    ("com.netease.uuremote", "UU 远程"),
    ("com.kingsoft.wpsoffice.mac", "WPS Office"),
];

// ------------------------------------------------------------------ Info.plist

fn unescape_xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(j) = rest.find(';') else {
            out.push_str(rest);
            return out;
        };
        let ent = &rest[1..j];
        let rep = match ent {
            "amp" => Some("&".to_string()),
            "lt" => Some("<".to_string()),
            "gt" => Some(">".to_string()),
            "quot" => Some("\"".to_string()),
            "apos" => Some("'".to_string()),
            _ => {
                if let Some(hex) = ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X")) {
                    u32::from_str_radix(hex, 16).ok().and_then(char::from_u32).map(|c| c.to_string())
                } else if let Some(dec) = ent.strip_prefix('#') {
                    dec.parse::<u32>().ok().and_then(char::from_u32).map(|c| c.to_string())
                } else {
                    None
                }
            }
        };
        match rep {
            Some(r) => out.push_str(&r),
            None => {
                out.push('&');
                out.push_str(ent);
                out.push(';');
            }
        }
        rest = &rest[j + 1..];
    }
    out.push_str(rest);
    out
}

/// 从 XML plist 里取一个 `<key>K</key><string>V</string>` 的 V。
fn plist_get(text: &str, key: &str) -> Option<String> {
    let pat = format!("<key>{}</key>", key);
    let mut from = 0usize;
    while let Some(i) = text[from..].find(&pat) {
        let start = from + i + pat.len();
        let tail = &text[start..];
        let t = tail.trim_start();
        if let Some(body) = t.strip_prefix("<string>") {
            if let Some(end) = body.find("</string>") {
                return Some(unescape_xml(&body[..end]));
            }
        }
        from = start;
    }
    None
}

fn plist_json(path: &Path) -> Option<Value> {
    let raw = std::fs::read(path).ok()?;
    if raw.starts_with(b"bplist") {
        let p = path.to_string_lossy().to_string();
        let (code, out, _) = platform::run_cmd_full(
            &["/usr/bin/plutil", "-convert", "json", "-o", "-", &p],
            Duration::from_secs(8),
        );
        if code != 0 {
            return None;
        }
        return serde_json::from_str(&out).ok();
    }
    let text = String::from_utf8_lossy(&raw);
    let mut m = Map::new();
    for k in ["CFBundleIdentifier", "CFBundleDisplayName", "CFBundleName"] {
        if let Some(v) = plist_get(&text, k) {
            m.insert(k.to_string(), Value::String(v));
        }
    }
    if m.is_empty() {
        return None;
    }
    Some(Value::Object(m))
}

static NAMES: Mutex<Option<(f64, Vec<(String, String)>)>> = Mutex::new(None);

/// `_app_names()`：bundle id → App 名。扫系统里的 .app，缓存一天。
pub fn app_names(root: &Path) -> Vec<(String, String)> {
    {
        let g = NAMES.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, m)) = g.as_ref() {
            if !m.is_empty() && now_secs() - *at < 86400.0 {
                return m.clone();
            }
        }
    }
    let mut ordered: Vec<(String, String)> =
        NAME_FIX.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
    let mut idx: HashMap<String, usize> = HashMap::new();
    for (i, (k, _)) in ordered.iter().enumerate() {
        idx.insert(k.clone(), i);
    }
    let put = |ordered: &mut Vec<(String, String)>,
               idx: &mut HashMap<String, usize>,
               k: String,
               v: String| {
        match idx.get(&k) {
            Some(i) => ordered[*i].1 = v,
            None => {
                idx.insert(k.clone(), ordered.len());
                ordered.push((k, v));
            }
        }
    };
    if let Some(Value::Object(raw)) = crate::jsonio::read_json(&name_cache_path(root)) {
        for (k, v) in raw {
            let vs = match v {
                Value::String(s) => s,
                other => crate::hostwork::util::py_str(&other),
            };
            put(&mut ordered, &mut idx, k, vs);
        }
    }
    let mut roots: Vec<PathBuf> = vec![
        PathBuf::from("/Applications"),
        PathBuf::from("/System/Applications"),
        PathBuf::from("/System/Applications/Utilities"),
        PathBuf::from("/Applications/Utilities"),
        home().join("Applications"),
    ];
    roots.dedup();
    for r in roots {
        let rd = match std::fs::read_dir(&r) {
            Ok(x) => x,
            Err(_) => continue,
        };
        let mut apps: Vec<PathBuf> = Vec::new();
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.ends_with(".app") || name.starts_with('.') {
                continue;
            }
            let p = e.path();
            if !p.is_dir() {
                continue;
            }
            apps.push(p);
        }
        for app in apps {
            let pl = app.join("Contents").join("Info.plist");
            if !pl.is_file() {
                continue;
            }
            let Some(d) = plist_json(&pl) else { continue };
            let bid = d
                .get("CFBundleIdentifier")
                .map(crate::hostwork::util::py_str)
                .unwrap_or_default()
                .trim()
                .to_string();
            let mut nm = String::new();
            if let Some(v) = d.get("CFBundleDisplayName") {
                if crate::hostwork::util::truthy(v) {
                    nm = crate::hostwork::util::py_str(v);
                }
            }
            if nm.trim().is_empty() {
                if let Some(v) = d.get("CFBundleName") {
                    if crate::hostwork::util::truthy(v) {
                        nm = crate::hostwork::util::py_str(v);
                    }
                }
            }
            let nm = nm.trim().to_string();
            if bid.is_empty() || nm.is_empty() {
                continue;
            }
            if NAME_FIX.iter().any(|(b, _)| *b == bid) {
                continue;
            }
            put(&mut ordered, &mut idx, bid, nm);
        }
    }
    {
        let mut g = NAMES.lock().unwrap_or_else(|e| e.into_inner());
        *g = Some((now_secs(), ordered.clone()));
    }
    let mut m = Map::new();
    for (k, v) in &ordered {
        m.insert(k.clone(), Value::String(v.clone()));
    }
    let _ = std::fs::create_dir_all(cache_dir(root));
    let _ = std::fs::write(name_cache_path(root), dumps_spaced(&Value::Object(m)));
    ordered
}

// ------------------------------------------------------------------ 区间并集

/// `_union`：区间并集（秒）。同一个 App 的区间会首尾相叠，直接加会虚高。
pub fn union(ivals: &mut Vec<(f64, f64)>) -> f64 {
    if ivals.is_empty() {
        return 0.0;
    }
    ivals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mut tot = 0.0;
    let (mut cs, mut ce) = ivals[0];
    for (s, e) in ivals.iter().skip(1) {
        if *s > ce {
            tot += ce - cs;
            cs = *s;
            ce = *e;
        } else if *e > ce {
            ce = *e;
        }
    }
    tot += ce - cs;
    if tot < 0.0 {
        0.0
    } else {
        tot
    }
}

fn day_of(secs: f64) -> String {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_opt(secs as i64, 0)
        .single()
        .map(|t| t.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

fn hour_start_ms(secs: f64) -> (i64, u32) {
    use chrono::{Local, TimeZone, Timelike};
    match Local.timestamp_opt(secs as i64, 0).single() {
        Some(t) => {
            let h = t.hour();
            let start = t
                .with_minute(0)
                .and_then(|x| x.with_second(0))
                .and_then(|x| x.with_nanosecond(0))
                .unwrap_or(t);
            (start.timestamp(), h)
        }
        None => (secs as i64, 0),
    }
}

fn next_midnight(secs: f64) -> f64 {
    use chrono::{Local, TimeZone, Timelike};
    match Local.timestamp_opt(secs as i64, 0).single() {
        Some(t) => {
            let d = t.date_naive() + chrono::Duration::days(1);
            let n = d.and_hms_opt(0, 0, 0).unwrap();
            Local
                .from_local_datetime(&n)
                .single()
                .map(|x| x.timestamp() as f64)
                .unwrap_or_else(|| {
                    let h = t.hour();
                    let m = t.minute();
                    let s = t.second();
                    secs + ((24 - h as i64) * 3600 - m as i64 * 60 - s as i64) as f64
                })
        }
        None => secs,
    }
}

// ------------------------------------------------------------------ 采样兜底

/// `_screen_from_samples`：把自采的前台样本折成和系统库一样的形状。
pub fn from_samples(root: &Path, days: i64) -> Value {
    let mut out = Map::new();
    out.insert("ok".into(), json!(false));
    out.insert(
        "why".into(),
        json!("还没采到前台数据（服务刚起，每分钟一条）"),
    );
    out.insert("days".into(), json!({}));
    out.insert("hours".into(), json!({}));
    out.insert("names".into(), json!({}));
    out.insert("span".into(), json!(["", ""]));
    out.insert("n_days".into(), json!(0));
    out.insert("src".into(), json!("sampler"));
    let sp = sample_path(root);
    if !sp.exists() {
        return Value::Object(out);
    }
    let since = now_secs() - days as f64 * 86400.0;
    let mut apps: Map<String, Value> = Map::new();
    let mut hours: Map<String, Value> = Map::new();
    let mut names: Map<String, Value> = Map::new();
    let mut first: f64 = 0.0;
    let mut last: f64 = 0.0;
    let text = match std::fs::read(&sp) {
        Ok(b) => String::from_utf8_lossy(&b).to_string(),
        Err(_) => return Value::Object(out),
    };
    for ln in text.split('\n') {
        if ln.trim().is_empty() {
            continue;
        }
        let r: Value = match loads(ln) {
            Some(v) => v,
            None => continue,
        };
        let obj = match r.as_object() {
            Some(o) => o,
            None => continue,
        };
        let ts = crate::hostwork::util::py_double(obj.get("ts").unwrap_or(&Value::Null));
        if ts < since {
            continue;
        }
        let nm = crate::hostwork::util::str_or(obj.get("app")).trim().to_string();
        if nm.is_empty() {
            continue;
        }
        if first == 0.0 {
            first = ts;
        }
        if ts > last {
            last = ts;
        }
        let d = day_of(ts);
        let (_, h) = hour_start_ms(ts);
        names.insert(nm.clone(), Value::String(nm.clone()));
        let row = match apps.get_mut(&d) {
            Some(Value::Object(o)) => o,
            _ => {
                apps.insert(d.clone(), json!({}));
                apps.get_mut(&d).unwrap().as_object_mut().unwrap()
            }
        };
        let cur = crate::hostwork::util::py_double(row.get(&nm).unwrap_or(&Value::Null));
        row.insert(nm.clone(), json!(round1(cur + SAMPLE_EVERY)));
        let hs = match hours.get_mut(&d) {
            Some(Value::Array(a)) => a,
            _ => {
                hours.insert(d.clone(), json!(vec![0.0f64; 24]));
                hours.get_mut(&d).unwrap().as_array_mut().unwrap()
            }
        };
        let idx = (h as usize).min(23);
        let cur = serde_json::from_value::<f64>(hs[idx].clone()).unwrap_or(0.0);
        hs[idx] = json!(round1(cur + SAMPLE_EVERY));
    }
    if apps.is_empty() {
        return Value::Object(out);
    }
    let mut days_out = Map::new();
    for (d, v) in apps.iter() {
        let obj = v.as_object().cloned().unwrap_or_default();
        let mut total = 0.0;
        for (_, x) in obj.iter() {
            total += serde_json::from_value::<f64>(x.clone()).unwrap_or(0.0);
        }
        let mut items: Vec<(String, f64)> = obj
            .iter()
            .map(|(k, x)| (k.clone(), serde_json::from_value::<f64>(x.clone()).unwrap_or(0.0)))
            .collect();
        items.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let mut ap = Map::new();
        for (k, x) in items.into_iter().take(14) {
            ap.insert(k, json!(round1(x)));
        }
        let mut row = Map::new();
        row.insert("total".into(), json!(round1(total)));
        row.insert("apps".into(), Value::Object(ap));
        days_out.insert(d.clone(), Value::Object(row));
    }
    let mut hout = Map::new();
    for (d, hs) in hours.iter() {
        let arr: Vec<Value> = hs
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|x| json!(round1(serde_json::from_value::<f64>(x.clone()).unwrap_or(0.0))))
                    .collect()
            })
            .unwrap_or_default();
        hout.insert(d.clone(), Value::Array(arr));
    }
    let span = if first != 0.0 {
        json!([day_of(first), day_of(last)])
    } else {
        json!(["", ""])
    };
    out.insert("ok".into(), json!(true));
    out.insert("why".into(), json!(""));
    out.insert(
        "days".into(),
        Value::Object(days_out),
    );
    out.insert("hours".into(), Value::Object(hout));
    out.insert("names".into(), Value::Object(names));
    out.insert("span".into(), span);
    out.insert("n_days".into(), json!(apps.len()));
    out.insert("src".into(), json!("sampler"));
    Value::Object(out)
}

fn round1(x: f64) -> f64 {
    crate::hostwork::util::round_n(x, 1)
}

fn kc_probe() -> String {
    match std::fs::File::open(kc_db()) {
        Ok(_) => "文件能打开，是 SQLite 那一层失败（库被占用或已损坏）".to_string(),
        Err(e) => {
            let eno = e.raw_os_error().unwrap_or(0);
            if eno == libc::EPERM || eno == libc::EACCES {
                "连打开都被拒（EPERM），是 TCC 没授权，不是路径或 SQL 的问题".to_string()
            } else if eno == libc::ENOENT {
                "文件不存在（这台机器没开过屏幕使用时间？）".to_string()
            } else {
                format!("打不开库文件（errno={}）", eno)
            }
        }
    }
}

fn self_exe() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::canonicalize(p).ok())
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default()
}

// ------------------------------------------------------------------ screen

/// `screen(days)`：系统屏幕使用时间。
pub fn screen(root: &Path, days_in: i64) -> Value {
    let db = kc_db();
    if !db.exists() {
        return from_samples(root, days_in);
    }
    let days = days_in.clamp(1, 90);
    let since = now_secs() - days as f64 * 86400.0 - CF_EPOCH;
    let sql = format!(
        "SELECT ZVALUESTRING AS b, printf('%.17g',ZSTARTDATE) AS s, printf('%.17g',ZENDDATE) AS e \
         FROM ZOBJECT WHERE ZSTREAMNAME='/app/usage' AND ZENDDATE>{}",
        since
    );
    let p = db.to_string_lossy().to_string();
    let (code, out, err) = platform::run_cmd_piped(
        &["sqlite3", "-readonly", "-json", &p, &sql],
        Duration::from_secs(8),
    );
    let rows: Vec<Value> = if code == 0 {
        let t = out.trim();
        if t.is_empty() {
            Vec::new()
        } else {
            match loads(t) {
                Some(Value::Array(a)) => a,
                _ => Vec::new(),
            }
        }
    } else {
        let mut fb = from_samples(root, days);
        let exe = self_exe();
        let msg = err
            .trim()
            .strip_prefix("Error: ")
            .unwrap_or(err.trim())
            .to_string();
        let why = format!(
            "系统屏幕使用库读不了（OperationalError: {}）——{}。{}在那之前只用本机自己采的前台样本（{} 起）",
            msg,
            kc_probe(),
            if !exe.is_empty() {
                format!("要补历史：把 {} 加进「系统设置 · 隐私与安全性 · 完全磁盘访问」并打开开关；", exe)
            } else {
                "要补历史：给本服务的解释器「完全磁盘访问」；".to_string()
            },
            fb.get("span")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .unwrap_or("")
        );
        if let Some(o) = fb.as_object_mut() {
            o.insert("why".into(), Value::String(why));
            o.insert("need_fda".into(), json!(true));
            o.insert("exe".into(), Value::String(exe));
        }
        return fb;
    };
    if rows.is_empty() {
        return from_samples(root, days);
    }

    let names = app_names(root);
    let mut per_app: Vec<((String, String), Vec<(f64, f64)>)> = Vec::new();
    let mut app_idx: HashMap<(String, String), usize> = HashMap::new();
    let mut per_hour: Vec<((String, i64), Vec<(f64, f64)>)> = Vec::new();
    let mut hour_idx: HashMap<(String, i64), usize> = HashMap::new();
    let mut per_day: Vec<(String, Vec<(f64, f64)>)> = Vec::new();
    let mut day_idx: HashMap<String, usize> = HashMap::new();
    let mut first: f64 = 0.0;
    let mut last: f64 = 0.0;
    for row in rows {
        let bundle = crate::hostwork::util::str_or(row.get("b"));
        let s0 = crate::hostwork::util::str_or(row.get("s"));
        let e0 = crate::hostwork::util::str_or(row.get("e"));
        let (mut s, mut e) = match (s0.trim().parse::<f64>(), e0.trim().parse::<f64>()) {
            (Ok(a), Ok(b)) => (a, b),
            _ => continue,
        };
        if bundle.is_empty() || e <= s {
            continue;
        }
        e = e.min(s + 6.0 * 3600.0);
        s += CF_EPOCH;
        e += CF_EPOCH;
        if first == 0.0 || s < first {
            first = s;
        }
        if e > last {
            last = e;
        }
        let mut cur = s;
        while cur < e {
            let day = day_of(cur);
            let nxt = next_midnight(cur);
            let seg = e.min(nxt);
            let k = (day.clone(), bundle.clone());
            match app_idx.get(&k) {
                Some(i) => per_app[*i].1.push((cur, seg)),
                None => {
                    app_idx.insert(k.clone(), per_app.len());
                    per_app.push((k, vec![(cur, seg)]));
                }
            }
            match day_idx.get(&day) {
                Some(i) => per_day[*i].1.push((cur, seg)),
                None => {
                    day_idx.insert(day.clone(), per_day.len());
                    per_day.push((day.clone(), vec![(cur, seg)]));
                }
            }
            let mut hcur = cur;
            while hcur < seg {
                let (h0, hh) = hour_start_ms(hcur);
                let hnxt = h0 as f64 + 3600.0;
                let hend = seg.min(hnxt);
                let hk = (day.clone(), hh as i64);
                match hour_idx.get(&hk) {
                    Some(i) => per_hour[*i].1.push((hcur, hend)),
                    None => {
                        hour_idx.insert(hk.clone(), per_hour.len());
                        per_hour.push((hk, vec![(hcur, hend)]));
                    }
                }
                hcur = hend;
            }
            cur = seg;
        }
    }

    // days_out / all_out
    let mut all_out: Vec<(String, f64)> = Vec::new();
    for (day, ivs) in per_day.iter_mut() {
        let u = union(ivs);
        all_out.push((day.clone(), u));
    }
    let mut days_out: Vec<(String, Vec<(String, f64)>)> = Vec::new();
    let mut days_idx: HashMap<String, usize> = HashMap::new();
    for ((day, bundle), ivs) in per_app.iter_mut() {
        let secs = union(ivs);
        if secs < 1.0 {
            continue;
        }
        let i = match days_idx.get(day) {
            Some(i) => *i,
            None => {
                days_idx.insert(day.clone(), days_out.len());
                days_out.push((day.clone(), Vec::new()));
                days_out.len() - 1
            }
        };
        let row = &mut days_out[i].1;
        match row.iter_mut().find(|(b, _)| b == bundle) {
            Some(x) => x.1 += secs,
            None => row.push((bundle.clone(), secs)),
        }
    }
    let mut hours_out: Vec<(String, [f64; 24])> = Vec::new();
    let mut hours_idx: HashMap<String, usize> = HashMap::new();
    for ((day, h), ivs) in per_hour.iter_mut() {
        let i = match hours_idx.get(day) {
            Some(i) => *i,
            None => {
                hours_idx.insert(day.clone(), hours_out.len());
                hours_out.push((day.clone(), [0.0; 24]));
                hours_out.len() - 1
            }
        };
        hours_out[i].1[(*h as usize).min(23)] = union(ivs);
    }

    let mut days_map = Map::new();
    for (day, apps) in days_out.iter() {
        let mut items = apps.clone();
        items.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let mut ap = Map::new();
        for (b, v) in items.into_iter().take(14) {
            ap.insert(b, json!(round1(v)));
        }
        let mut row = Map::new();
        let tot = all_out
            .iter()
            .find(|(d, _)| d == day)
            .map(|(_, v)| *v)
            .unwrap_or(0.0);
        row.insert("total".into(), json!(round1(tot)));
        row.insert("apps".into(), Value::Object(ap));
        days_map.insert(day.clone(), Value::Object(row));
    }
    let mut hout = Map::new();
    for (d, hs) in hours_out.iter() {
        let arr: Vec<Value> = hs.iter().map(|x| json!(round1(*x))).collect();
        hout.insert(d.clone(), Value::Array(arr));
    }
    let mut nm = Map::new();
    for (k, v) in names.iter() {
        nm.insert(k.clone(), Value::String(v.clone()));
    }

    let mut out = Map::new();
    out.insert("ok".into(), json!(true));
    out.insert("why".into(), json!(""));
    out.insert("days".into(), Value::Object(days_map));
    out.insert("hours".into(), Value::Object(hout));
    out.insert("names".into(), Value::Object(nm));
    out.insert(
        "span".into(),
        if first != 0.0 {
            json!([day_of(first), day_of(last)])
        } else {
            json!(["", ""])
        },
    );
    out.insert("n_days".into(), json!(days_out.len()));
    out.insert("src".into(), json!("system"));
    Value::Object(out)
}

// ------------------------------------------------------------------ 前台采样

static SAMPLER: OnceLock<()> = OnceLock::new();

/// `start_sampler()`：起前台采样线程（幂等）。
pub fn start_sampler(root: &Path) {
    let root = root.to_path_buf();
    SAMPLER.get_or_init(|| {
        std::thread::spawn(move || {
            let mut n = 0u64;
            loop {
                let _ = sample_once(&root);
                n += 1;
                if n % 720 == 0 {
                    prune_samples(&root);
                }
                std::thread::sleep(Duration::from_secs(SAMPLE_EVERY as u64));
            }
        });
    });
}

fn front_app() -> String {
    let (code, out, _) = platform::run_cmd_full(
        &["lsappinfo", "front"],
        Duration::from_secs(4),
    );
    if code != 0 {
        return String::new();
    }
    let asn = out.trim().to_string();
    if asn.is_empty() {
        return String::new();
    }
    let (code, info, _) = platform::run_cmd_full(
        &["lsappinfo", "info", "-only", "name", asn.as_str()],
        Duration::from_secs(4),
    );
    if code != 0 {
        return String::new();
    }
    let re = regex::Regex::new(r#""([^"]{1,48})""#).unwrap();
    re.captures(&info)
        .map(|c| c.get(1).map(|m| m.as_str().to_string()).unwrap_or_default())
        .unwrap_or_default()
}

fn hid_idle() -> f64 {
    let (code, out, _) =
        platform::run_cmd_full(&["ioreg", "-c", "IOHIDSystem"], Duration::from_secs(4));
    if code != 0 {
        return 0.0;
    }
    let re = regex::Regex::new(r#""HIDIdleTime"\s*=\s*(\d+)"#).unwrap();
    re.captures(&out)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse::<f64>().ok())
        .map(|v| v / 1e9)
        .unwrap_or(0.0)
}

fn sample_once(root: &Path) {
    let idle = hid_idle();
    if idle > IDLE_MAX_S {
        return;
    }
    let app = front_app();
    if app.is_empty() {
        return;
    }
    let rec = json!({
        "ts": now_secs() as i64,
        "app": app,
        "idle": round1(idle),
    });
    let _ = std::fs::create_dir_all(cache_dir(root));
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(sample_path(root))
    {
        use std::io::Write;
        let _ = f.write_all(format!("{}\n", dumps_spaced(&rec)).as_bytes());
    }
}

fn prune_samples(root: &Path) {
    let sp = sample_path(root);
    if !sp.exists() {
        return;
    }
    let cut = now_secs() - 60.0 * 86400.0;
    let text = match std::fs::read(&sp) {
        Ok(b) => String::from_utf8_lossy(&b).to_string(),
        Err(_) => return,
    };
    let mut keep = String::new();
    for ln in text.split_inclusive('\n') {
        let t = ln.trim_end_matches('\n');
        if t.trim().is_empty() {
            continue;
        }
        let ok = loads(t)
            .map(|v| {
                let ts = v
                    .get("ts")
                    .map(|x| crate::hostwork::util::py_double(x))
                    .unwrap_or(0.0);
                ts >= cut
            })
            .unwrap_or(false);
        if ok {
            keep.push_str(ln);
            if !ln.ends_with('\n') {
                keep.push('\n');
            }
        }
    }
    let tmp = sp.with_extension("tmp");
    let _ = std::fs::write(&tmp, keep);
    let _ = std::fs::rename(&tmp, &sp);
}

/// 采样文件里的分钟格（给 `_row_out` 之外的调用方留的钩子，未使用）。
pub fn _unused_round_int(x: f64) -> i64 {
    round_int(x)
}
