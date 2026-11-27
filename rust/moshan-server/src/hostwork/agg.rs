// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：会话流水扫描 + 按天聚合（hostwork._scan_file / _apply_line / _add_usage /
//       _note_model / _new_agg / _scan_sessions / sessions_snapshot）。
// 不做：不解析「每轮请求干了什么」—— 那是 sessions.rs（dash 口径）；这里只产
//       务账：每天多少 token、多少轮、哪些工具、活跃分钟、额度中断。
//
// 缓存文件 `<数据根>/_本机工作/缓存/会话.json` 与 Python **双向兼容**：
//   {"v":17,"files":{<绝对路径>:{"v":17,"off":…,"size":…,"mtime":…,"ino":…,"agg":{…}}}}
// Python 的 `_json_write` 是 `json.dumps(obj, ensure_ascii=False)` —— 单行、
// separators 为 ", " 与 ": "。这里用 util::py_dumps_compact 逐字节复刻，
// 版本号/字段/缩进任何一样不对，Python 侧 `agg["last"]` 就会 KeyError。
#![allow(dead_code)]

use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use super::util::{py_dumps_compact, py_int, round_n, truthy};
use crate::paths::Ctx;

pub const PARSE_VERSION: i64 = 17;

fn re(p: &str) -> Regex {
    Regex::new(p).expect("内置正则必须能编译")
}

fn rx_sr() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r"(?is)<system-reminder[\s\S]*?</system-reminder>"))
}
fn rx_uq() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r"(?is)<user_query>([\s\S]*?)</user_query>"))
}
fn rx_tag() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r"</?[a-zA-Z][^>]{0,160}>"))
}
fn rx_utt_skip() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        re(r"^(This session is being continued|Summary of the conversation|\[Request interrupted|Please continue with the conversation|Summary: \s*\d+\.)")
    })
}
fn rx_utt_block() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        re(r"<task-notification|<cb_summary|<cb_compact|compact-summary|<conversation_history_summary|Summary of the conversation between")
    })
}
fn rx_mode() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r"You are now in ([A-Za-z][A-Za-z ]{2,14}?) mode"))
}
fn rx_expert() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r"(?s)<current-expert>[\s\S]{0,200}?(ex_[A-Za-z0-9]{6,})"))
}
fn rx_img() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r"@image#\d+:"))
}
fn rx_img_ph() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r"@image#\d+:\S*"))
}
fn rx_ws() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r"\s+"))
}
fn rx_free() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r"免费"))
}
fn rx_reset() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r"(?:将在|可在)\s*(\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2})"))
}
fn rx_strip_at() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r"@.*$"))
}

pub fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

fn wb_root() -> PathBuf {
    home().join(".workbuddy").join("projects")
}

fn own_dir(ctx: &Ctx) -> PathBuf {
    ctx.library_dir.join("_本机工作")
}

pub fn cache_path(ctx: &Ctx) -> PathBuf {
    own_dir(ctx).join("缓存").join("会话.json")
}

pub fn proc_path(ctx: &Ctx) -> PathBuf {
    own_dir(ctx).join("进程.jsonl")
}

// ------------------------------------------------------------------ 统计态

#[derive(Clone, Default)]
pub struct Scan {
    pub done: i64,
    pub total: i64,
    pub running: bool,
    pub started: String,
    pub err: String,
    pub bytes: i64,
    pub cached_bytes: i64,
}

static SCAN: Mutex<Scan> = Mutex::new(Scan {
    done: 0,
    total: 0,
    running: false,
    started: String::new(),
    err: String::new(),
    bytes: 0,
    cached_bytes: 0,
});

static SCAN_AT: Mutex<f64> = Mutex::new(0.0);
static RUNNING: AtomicBool = AtomicBool::new(false);
/// 已从磁盘装过缓存（Python `_loaded`）。
static LOADED: AtomicBool = AtomicBool::new(false);
static SESS: Mutex<Option<Map<String, Value>>> = Mutex::new(None);
static SCAN_LOCK: Mutex<()> = Mutex::new(());

pub fn scan_state() -> Scan {
    SCAN.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// `dict(SCAN)`：扫描进度那一格（字段顺序照 Python）。
pub fn scan_json() -> Value {
    let s = scan_state();
    let mut m = serde_json::Map::new();
    m.insert("done".into(), serde_json::json!(s.done));
    m.insert("total".into(), serde_json::json!(s.total));
    m.insert("running".into(), serde_json::json!(s.running));
    m.insert("started".into(), serde_json::Value::String(s.started));
    m.insert("err".into(), serde_json::Value::String(s.err));
    m.insert("bytes".into(), serde_json::json!(s.bytes));
    m.insert("cached_bytes".into(), serde_json::json!(s.cached_bytes));
    serde_json::Value::Object(m)
}

pub fn scan_at() -> f64 {
    *SCAN_AT.lock().unwrap_or_else(|e| e.into_inner())
}

/// `ensure_sessions_loaded`：服务刚起先把磁盘那份装上。
///
/// 缓存缺失时**同步**扫一遍：Python 那边是启动线程（scan_loop）先扫，页面来要
/// 数据时拿到的已是一份完整的账。这里懒启动，但同样给完整的一份 —— 半份账
/// 比慢几秒糟得多（两张卡的数字会对不上）。
pub fn ensure_sessions_loaded(ctx: &Ctx) {
    if LOADED.swap(true, Ordering::SeqCst) {
        return;
    }
    if let Some(disk) = crate::jsonio::read_json(&cache_path(ctx)) {
        if disk.get("v").and_then(|x| x.as_i64()) == Some(PARSE_VERSION) {
            if let Some(Value::Object(files)) = disk.get("files") {
                let mut g = SESS.lock().unwrap_or_else(|e| e.into_inner());
                *g = Some(files.clone());
            }
        }
    }
    let empty = {
        let g = SESS.lock().unwrap_or_else(|e| e.into_inner());
        g.as_ref().map(|m| m.is_empty()).unwrap_or(true)
    };
    if empty {
        scan_sessions(ctx, false);
    } else if !RUNNING.load(Ordering::SeqCst) {
        // 和 Python 的 `scan_loop` 首轮等价：账已经从磁盘上装好了，但仍然在后台
        // 增量补扫一遍（只读新写进来的行）。顺带把 `scan` 那一格填上 —— 它是
        // 「本进程扫过什么」的运行时状态，不是缓存里的数据。
        let cp = cache_path(ctx);
        std::thread::spawn(move || scan_sessions_inner(&cp, false));
    }
}

/// `sessions_snapshot()`：有 last 的会话，按 last 倒序。
pub fn sessions_snapshot(ctx: &Ctx) -> Vec<Value> {
    ensure_sessions_loaded(ctx);
    let items: Vec<Value> = {
        let g = SESS.lock().unwrap_or_else(|e| e.into_inner());
        g.as_ref()
            .map(|m| m.values().cloned().collect::<Vec<_>>())
            .unwrap_or_default()
    };
    let mut out: Vec<Value> = Vec::new();
    for e in items {
        if let Some(agg) = e.get("agg") {
            if !truthy(agg.get("last").unwrap_or(&Value::Null)) {
                continue;
            }
            out.push(agg.clone());
        }
    }
    out.sort_by(|a, b| {
        let la = py_int(a.get("last").unwrap_or(&Value::Null));
        let lb = py_int(b.get("last").unwrap_or(&Value::Null));
        lb.cmp(&la)
    });
    out
}

/// 内部：全部 agg（含没有 last 的，给 pulse 的 opts 用）。
pub(crate) fn aggs(ctx: &Ctx) -> Vec<Value> {
    ensure_sessions_loaded(ctx);
    let g = SESS.lock().unwrap_or_else(|e| e.into_inner());
    g.as_ref()
        .map(|m| {
            m.values()
                .filter_map(|e| e.get("agg").cloned())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

// ------------------------------------------------------------------ 小节

fn new_agg() -> Map<String, Value> {
    let mut m = Map::new();
    for k in [
        "id", "proj", "cwd", "title",
    ] {
        m.insert(k.into(), Value::String(String::new()));
    }
    for k in ["first", "last", "asks", "calls"] {
        m.insert(k.into(), json!(0));
    }
    for k in ["models", "tools", "skills", "agents", "mcps", "modes", "experts"] {
        m.insert(k.into(), Value::Object(Map::new()));
    }
    m.insert("utt".into(), json!([]));
    m.insert("errs".into(), json!([]));
    m.insert("last_ok".into(), json!(0));
    m.insert("titles".into(), json!([]));
    m.insert("act".into(), Value::Object(Map::new()));
    m.insert("hw".into(), Value::Object(Map::new()));
    m.insert("lastts".into(), json!(0));
    m.insert("days".into(), Value::Object(Map::new()));
    m
}

fn day_of(ms: i64) -> String {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|t| t.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

fn hour_key(ms: i64) -> String {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|t| format!("{}T{}", t.format("%Y-%m-%d"), t.format("%H")))
        .unwrap_or_default()
}

fn today_iso() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

fn bump(d: &mut Map<String, Value>, k: &str, n: i64) {
    let cur = py_int(d.get(k).unwrap_or(&Value::Null));
    d.insert(k.to_string(), json!(cur + n));
}

fn obj_mut<'a>(m: &'a mut Map<String, Value>, k: &str) -> &'a mut Map<String, Value> {
    if !m.get(k).map(|v| v.is_object()).unwrap_or(false) {
        m.insert(k.to_string(), Value::Object(Map::new()));
    }
    m.get_mut(k).and_then(|v| v.as_object_mut()).unwrap()
}

fn arr_mut<'a>(m: &'a mut Map<String, Value>, k: &str) -> &'a mut Vec<Value> {
    if !m.get(k).map(|v| v.is_array()).unwrap_or(false) {
        m.insert(k.to_string(), json!([]));
    }
    m.get_mut(k).and_then(|v| v.as_array_mut()).unwrap()
}

/// `_utt_text`：把一条用户消息压成一句可读的话；纯 meta 的返回空串。
pub fn utt_text(txt: &str, limit: usize) -> String {
    let s0 = txt;
    if rx_utt_block().is_match(s0) {
        return String::new();
    }
    let mut s = match rx_uq().captures(s0) {
        Some(c) => c.get(1).map(|m| m.as_str().to_string()).unwrap_or_default(),
        None => rx_sr().replace_all(s0, " ").to_string(),
    };
    s = rx_tag().replace_all(&s, " ").to_string();
    let had_img = rx_img().is_match(&s);
    if had_img {
        s = rx_img_ph().replace_all(&s, " ").to_string();
    }
    s = s
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&");
    s = rx_ws().replace_all(&s, " ").to_string();
    s = s.trim().to_string();
    if had_img {
        s = format!("［图］{}", s);
    }
    if s.chars().count() < 2 || rx_utt_skip().is_match(&s) {
        return String::new();
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() > limit {
        format!("{}…", chars[..limit].iter().collect::<String>())
    } else {
        s
    }
}

/// `_expert_label`：ex_xxx → 专家名（recents → bundle-map → id 尾 6 位）。
fn expert_label(eid: &str) -> String {
    if eid.is_empty() {
        return String::new();
    }
    static CACHE: Mutex<Option<(f64, HashMap<String, String>)>> = Mutex::new(None);
    // 取最新那份 storage/*/global/expert.json
    let base = home().join(".workbuddy").join("storage");
    let mut files: Vec<(f64, PathBuf)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&base) {
        for e in rd.flatten() {
            let p = e.path().join("global").join("expert.json");
            if let Ok(md) = std::fs::metadata(&p) {
                let mt = md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(-1.0);
                files.push((mt, p));
            }
        }
    }
    files.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    if let Some((mt, p)) = files.first() {
        let mut g = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        let need = match g.as_ref() {
            Some((m, _)) => *m != *mt,
            None => true,
        };
        if need {
            let mut amap: HashMap<String, String> = HashMap::new();
            if let Some(raw) = crate::jsonio::read_json(p) {
                if let Some(recs) = raw.get("recents").and_then(|x| x.as_array()) {
                    for row in recs {
                        if row.is_object() {
                            let rid = super::util::str_or(row.get("id"));
                            let nm = super::util::str_or(row.get("name"));
                            if !rid.is_empty() && !nm.is_empty() {
                                amap.insert(rid, nm);
                            }
                        }
                    }
                }
            }
            let bmp = home()
                .join(".workbuddy")
                .join("app")
                .join("cache")
                .join("experts")
                .join("expert-bundle-map.json");
            if let Some(Value::Object(bmap)) = crate::jsonio::read_json(&bmp) {
                for (rid, slug) in bmap {
                    if rid.starts_with("ex_") {
                        let s = super::util::str_or(Some(&slug));
                        if !s.is_empty() {
                            amap.entry(rid).or_insert(s);
                        }
                    }
                }
            }
            *g = Some((*mt, amap));
        }
        if let Some((_, map)) = g.as_ref() {
            if let Some(v) = map.get(eid) {
                return v.clone();
            }
        }
    }
    let chars: Vec<char> = eid.chars().collect();
    chars[chars.len().saturating_sub(6)..].iter().collect()
}

fn reset_ms(msg: &str) -> i64 {
    let Some(c) = rx_reset().captures(msg) else {
        return 0;
    };
    let s = c.get(1).map(|m| m.as_str()).unwrap_or("");
    match chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        Ok(d) => d.and_local_timezone(chrono::Local).single().map(|t| t.timestamp_millis()).unwrap_or(0),
        Err(_) => 0,
    }
}

fn err_kind(code: i64) -> &'static str {
    match code {
        14018 => "credit",
        6004 => "quota",
        6021 => "queue",
        11134 => "unavailable",
        _ => "",
    }
}

const ERR_NET_CODES: [&str; 6] = [
    "ECONNRESET", "ETIMEDOUT", "ENOTFOUND", "EPIPE", "ECONNREFUSED", "UND_ERR_SOCKET",
];

/// `_note_model`：一轮只在第一次请求时定格模型，变了才记一笔切换。
fn note_model(agg: &mut Map<String, Value>, rid: &str, name: &str, model: &str, ts: i64) {
    let key = {
        let a = rid.trim();
        if !a.is_empty() { a.to_string() } else { model.trim().to_string() }
    };
    if key.is_empty() {
        return;
    }
    let label = {
        let a = name.trim();
        if !a.is_empty() {
            a.to_string()
        } else {
            let b = model.trim();
            if !b.is_empty() { b.to_string() } else { key.clone() }
        }
    };
    let turns: Vec<i64> = agg
        .get("turns")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().map(py_int).collect())
        .unwrap_or_default();
    let mut cur = 0i64;
    for tv in turns.iter().rev() {
        if *tv <= ts {
            cur = *tv;
            break;
        }
    }
    let prev: Option<(i64, String, String)> = agg.get("turn_model").and_then(|v| v.as_array()).map(|a| {
        (
            py_int(a.first().unwrap_or(&Value::Null)),
            a.get(1).map(|v| super::util::str_or(Some(v))).unwrap_or_default(),
            a.get(2).map(|v| super::util::str_or(Some(v))).unwrap_or_default(),
        )
    });
    if let Some((p0, _, _)) = &prev {
        if *p0 == cur {
            return;
        }
    }
    if let Some((_, pkey, plabel)) = &prev {
        if !pkey.is_empty() && *pkey != key {
            let lst = arr_mut(agg, "switches");
            lst.push(json!([ts, plabel, label, pkey, key]));
            if lst.len() > 60 {
                let cut = lst.len() - 60;
                lst.drain(..cut);
            }
        }
    }
    agg.insert("turn_model".into(), json!([cur, key, label]));
}

/// `_add_usage`：把一次请求的用量记进「会话 / 当天 / 模型」三处。
fn add_usage(agg: &mut Map<String, Value>, pd: &Map<String, Value>, day: &str, ts: i64) {
    let u = match pd.get("usage") {
        Some(Value::Object(m)) if truthy(&Value::Bool(true)) => m.clone(),
        _ => return,
    };
    if !truthy(u.get("totalTokens").unwrap_or(&Value::Null)) {
        return;
    }
    let model = {
        let m = super::util::str_or(pd.get("model"));
        if !m.is_empty() { m } else {
            let n = super::util::str_or(pd.get("requestModelName"));
            if !n.is_empty() { n } else { "未知模型".to_string() }
        }
    };
    if ts != 0 && ts > py_int(agg.get("last_ok").unwrap_or(&Value::Null)) {
        agg.insert("last_ok".into(), json!(ts));
    }
    note_model(
        agg,
        &super::util::str_or(pd.get("requestModelId")),
        &super::util::str_or(pd.get("requestModelName")),
        &model,
        ts,
    );
    let tin = {
        let v = py_int(u.get("inputTokens").unwrap_or(&Value::Null));
        if truthy(u.get("inputTokens").unwrap_or(&Value::Null)) { v } else { py_int(u.get("input_tokens").unwrap_or(&Value::Null)) }
    };
    let tout = {
        let v = py_int(u.get("outputTokens").unwrap_or(&Value::Null));
        if truthy(u.get("outputTokens").unwrap_or(&Value::Null)) { v } else { py_int(u.get("output_tokens").unwrap_or(&Value::Null)) }
    };
    let mut cached = 0i64;
    if let Some(Value::Array(det)) = u.get("inputTokensDetails") {
        if let Some(Value::Object(d0)) = det.first() {
            cached = py_int(d0.get("cached_tokens").unwrap_or(&Value::Null));
        }
    }
    bump(agg, "calls", 1);
    let models = obj_mut(agg, "models");
    add_model(models, &model, tin, tout, cached, 1);
    // day_row
    let days = obj_mut(agg, "days");
    let dm = day_row(days, day);
    let c = py_int(dm.get("calls").unwrap_or(&Value::Null)) + 1;
    dm.insert("calls".into(), json!(c));
    for (k, v) in [("in", tin), ("out", tout), ("cached", cached)] {
        let cur = py_int(dm.get(k).unwrap_or(&Value::Null)) + v;
        dm.insert(k.into(), json!(cur));
    }
    let dmodels = obj_mut(dm, "models");
    add_model(dmodels, &model, tin, tout, cached, 1);
}

fn add_model(m: &mut Map<String, Value>, name: &str, tin: i64, tout: i64, cached: i64, n: i64) {
    if !m.get(name).map(|v| v.is_object()).unwrap_or(false) {
        m.insert(
            name.to_string(),
            json!({"in": 0, "out": 0, "cached": 0, "n": 0}),
        );
    }
    if let Some(Value::Object(e)) = m.get_mut(name) {
        for (k, v) in [("in", tin), ("out", tout), ("cached", cached), ("n", n)] {
            let cur = py_int(e.get(k).unwrap_or(&Value::Null)) + v;
            e.insert(k.into(), json!(cur));
        }
    }
}

fn day_row<'a>(days: &'a mut Map<String, Value>, day: &str) -> &'a mut Map<String, Value> {
    if !days.get(day).map(|v| v.is_object()).unwrap_or(false) {
        days.insert(
            day.to_string(),
            json!({"calls": 0, "in": 0, "out": 0, "cached": 0, "asks": 0,
                   "tools": {}, "models": {}}),
        );
    }
    days.get_mut(day).and_then(|v| v.as_object_mut()).unwrap()
}

/// `_apply_line`
fn apply_line(agg: &mut Map<String, Value>, d: &Map<String, Value>) {
    let t = d.get("type").map(|v| super::util::str_or(Some(v))).unwrap_or_default();
    let ts = py_int(d.get("timestamp").unwrap_or(&Value::Null));
    if ts != 0 {
        let last = py_int(agg.get("last").unwrap_or(&Value::Null)).max(ts);
        agg.insert("last".into(), json!(last));
        if py_int(agg.get("first").unwrap_or(&Value::Null)) == 0 {
            agg.insert("first".into(), json!(ts));
        }
    }
    if let Some(cwd) = d.get("cwd") {
        if truthy(cwd) && super::util::str_or(Some(&agg.get("cwd").cloned().unwrap_or(Value::Null))).is_empty() {
            agg.insert("cwd".into(), Value::String(super::util::str_or(Some(cwd))));
        }
    }
    if let Some(sid) = d.get("sessionId") {
        if truthy(sid) && super::util::str_or(agg.get("id")).is_empty() {
            agg.insert("id".into(), Value::String(super::util::str_or(Some(sid))));
        }
    }

    if t == "ai-title" {
        let new: String = super::util::str_or(d.get("aiTitle")).trim().chars().take(120).collect();
        let cur = super::util::str_or(agg.get("title"));
        if !new.is_empty() && new != cur {
            arr_mut(agg, "titles").push(json!([ts, new.clone()]));
            agg.insert("title".into(), Value::String(new));
        }
        return;
    }

    // 中断
    let perr = d
        .get("providerData")
        .and_then(|v| v.as_object())
        .and_then(|m| m.get("error"))
        .and_then(|v| v.as_object())
        .cloned();
    if let Some(err) = perr {
        if ts != 0 {
            let status = py_int(err.get("status").unwrap_or(&Value::Null));
            let raw_code = err.get("code").cloned().unwrap_or(Value::Null);
            let code = py_int(&raw_code);
            let msg: String = super::util::str_or(err.get("message")).chars().take(90).collect();
            let mut kind = err_kind(code).to_string();
            if kind == "quota" && rx_free().is_match(&msg) {
                kind = "free".into();
            } else if kind.is_empty() {
                let rc = match &raw_code {
                    Value::String(s) => s.clone(),
                    Value::Null => String::new(),
                    other => other.to_string(),
                };
                if ERR_NET_CODES.contains(&rc.as_str())
                    || truthy(err.get("isNetworkError").unwrap_or(&Value::Null))
                {
                    kind = "net".into();
                } else if code != 0 && code != 6020 {
                    kind = "other".into();
                } else if status >= 500 {
                    kind = "other".into();
                }
            }
            if !kind.is_empty() {
                arr_mut(agg, "errs").push(json!([ts, code, reset_ms(&msg), kind, msg]));
            }
        }
    }

    if t != "message" && t != "function_call" {
        return;
    }

    let day = if ts != 0 { day_of(ts) } else { today_iso() };

    // 活跃时长
    if ts != 0 {
        let prev = py_int(agg.get("lastts").unwrap_or(&Value::Null));
        let sec = if prev != 0 && ts > prev {
            let gap = (ts - prev) as f64 / 1000.0;
            if gap <= 300.0 { gap } else { 0.0 }
        } else if prev != 0 {
            0.0
        } else {
            30.0
        };
        if sec > 0.0 {
            let act = obj_mut(agg, "act");
            let cur = super::util::py_float(act.get(&day).unwrap_or(&Value::Null)).unwrap_or(0.0);
            act.insert(day.clone(), json!(round_n(cur + sec, 1)));
            let hk = hour_key(ts);
            let hw = obj_mut(agg, "hw");
            let cur = super::util::py_float(hw.get(&hk).unwrap_or(&Value::Null)).unwrap_or(0.0);
            hw.insert(hk, json!(round_n(cur + sec, 1)));
        }
        let n = prev.max(ts);
        agg.insert("lastts".into(), json!(n));
    }

    if t == "message" {
        let role = d.get("role").map(|v| super::util::str_or(Some(v))).unwrap_or_default();
        let pd = d
            .get("providerData")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        if role == "user" {
            bump(agg, "asks", 1);
            let tl = arr_mut(agg, "turns");
            if ts != 0 {
                let last = tl.last().map(py_int).unwrap_or(0);
                if tl.is_empty() || last != ts {
                    tl.push(json!(ts));
                    if tl.len() > 200 {
                        let cut = tl.len() - 200;
                        tl.drain(..cut);
                    }
                }
            }
            {
                let days = obj_mut(agg, "days");
                let dm = day_row(days, &day);
                bump(dm, "asks", 1);
            }
            let mut txt = String::new();
            if let Some(Value::Array(parts)) = d.get("content") {
                for x in parts {
                    if let Value::Object(o) = x {
                        let ty = super::util::str_or(o.get("type"));
                        if ty == "input_text" || ty == "text" || ty.is_empty() {
                            txt.push_str(&super::util::str_or(o.get("text")));
                        }
                    }
                }
            }
            if !txt.is_empty() {
                if let Some(c) = rx_mode().captures(&txt) {
                    let label = py_title(c.get(1).unwrap().as_str().trim());
                    bump(obj_mut(agg, "modes"), &label, 1);
                }
                if let Some(e) = rx_expert().captures(&txt) {
                    let label = expert_label(e.get(1).unwrap().as_str());
                    bump(obj_mut(agg, "experts"), &label, 1);
                }
                let one = utt_text(&txt, 130);
                if !one.is_empty() {
                    let dup = arr_mut(agg, "utt")
                        .iter()
                        .rev()
                        .take(2)
                        .any(|x| x.get(1).map(|v| super::util::str_or(Some(v))).unwrap_or_default() == one);
                    if !dup {
                        arr_mut(agg, "utt").push(json!([ts, one]));
                        bump(agg, "utts", 1);
                    }
                    let lst = arr_mut(agg, "utt");
                    if lst.len() > 30 {
                        let cut = lst.len() - 30;
                        lst.drain(..cut);
                    }
                }
            }
        }
        add_usage(agg, &pd, &day, ts);
        return;
    }

    // function_call
    let pd = d
        .get("providerData")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    add_usage(agg, &pd, &day, ts);
    let name = super::util::str_or(d.get("name")).trim().to_string();
    if name.is_empty() {
        return;
    }
    let args = parse_args(d.get("arguments"));
    bump(obj_mut(agg, "tools"), &name, 1);
    {
        let days = obj_mut(agg, "days");
        let dm = day_row(days, &day);
        bump(obj_mut(dm, "tools"), &name, 1);
    }
    if let Some(rest) = name.strip_prefix("mcp__") {
        let parts: Vec<&str> = rest.split("__").collect();
        let srv = if !parts.is_empty() { parts[0] } else { rest };
        bump(obj_mut(agg, "mcps"), srv, 1);
    } else if name == "Skill" {
        let sk = {
            let a = super::util::str_or(args.get("skill")).trim().to_string();
            if !a.is_empty() { a } else {
                let b = super::util::str_or(args.get("name")).trim().to_string();
                if !b.is_empty() { b } else { "未命名技能".to_string() }
            }
        };
        bump(obj_mut(agg, "skills"), &sk, 1);
    } else if name == "Agent" || name == "Task" {
        let mut who = {
            let a = super::util::str_or(args.get("subagent_type")).trim().to_string();
            if !a.is_empty() { a } else { super::util::str_or(args.get("description")).trim().to_string() }
        };
        who = rx_strip_at().replace_all(&who, "").to_string();
        if who.is_empty() {
            who = "子代理".into();
        }
        bump(obj_mut(agg, "agents"), &who, 1);
    } else if name == "DeferExecuteTool" {
        let inner = super::util::str_or(args.get("toolName")).trim().to_string();
        if let Some(rest) = inner.strip_prefix("mcp__") {
            let srv = rest.split("__").next().unwrap_or(rest);
            bump(obj_mut(agg, "mcps"), srv, 1);
        } else if !inner.is_empty() {
            bump(obj_mut(agg, "tools"), &inner, 1);
        }
    }
}

/// Python `str.title()`。
fn py_title(s: &str) -> String {
    let mut out = String::new();
    let mut prev_alnum = false;
    for c in s.chars() {
        if c.is_alphanumeric() {
            if prev_alnum {
                out.extend(c.to_lowercase());
            } else {
                out.extend(c.to_uppercase());
            }
            prev_alnum = true;
        } else {
            out.push(c);
            prev_alnum = false;
        }
    }
    out
}

fn parse_args(raw: Option<&Value>) -> Map<String, Value> {
    match raw {
        Some(Value::String(s)) => {
            let t = s.trim();
            if !t.starts_with('{') {
                return Map::new();
            }
            serde_json::from_str::<Value>(t)
                .ok()
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default()
        }
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    }
}

// ------------------------------------------------------------------ 扫描

/// `_scan_file`：增量解析，只读上次 offset 之后追加的完整行。
fn scan_file(path: &Path, prev: Option<&Value>) -> Value {
    let st = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return prev.cloned().unwrap_or_else(|| json!({})),
    };
    let ino = st.ino();
    let size = st.size();
    let mtime = st
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let prev_agg = prev.and_then(|p| p.get("agg")).cloned();
    let same = prev
        .map(|p| {
            p.get("v").and_then(|x| x.as_i64()) == Some(PARSE_VERSION)
                && p.get("ino").and_then(|x| x.as_i64()) == Some(ino as i64)
                && p.get("size").and_then(|x| x.as_i64()).unwrap_or(0) <= size as i64
                && prev_agg.as_ref().map(|a| a.is_object()).unwrap_or(false)
        })
        .unwrap_or(false);
    let (off, mut agg) = if same {
        let off = prev.and_then(|p| p.get("off")).map(py_int).unwrap_or(0);
        let a = prev_agg.unwrap();
        (off, a.as_object().cloned().unwrap_or_else(new_agg))
    } else {
        (0i64, new_agg())
    };
    if size as i64 == off {
        if let Some(p) = prev {
            let mut p = p.clone();
            if let Some(o) = p.as_object_mut() {
                o.insert("size".into(), json!(size));
                o.insert("mtime".into(), json!(mtime));
                o.insert("ino".into(), json!(ino));
            }
            return p;
        }
        return json!({});
    }
    let f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return prev.cloned().unwrap_or_else(|| json!({})),
    };
    let mut reader = BufReader::with_capacity(1 << 20, f);
    if off > 0 && reader.seek(SeekFrom::Start(off as u64)).is_err() {
        return prev.cloned().unwrap_or_else(|| json!({}));
    }
    let mut consumed = off as u64;
    let mut chunk_bytes: i64 = 0;
    let mut skipped_meta = false;
    let mut line: Vec<u8> = Vec::new();
    loop {
        line.clear();
        let n = match reader.read_until(b'\n', &mut line) {
            Ok(n) => n,
            Err(_) => break,
        };
        if n == 0 {
            break;
        }
        if *line.last().unwrap() != b'\n' {
            // 半行：留到下次
            break;
        }
        consumed += n as u64;
        chunk_bytes += n as i64;
        if line.windows(14).any(|w| w == b"\"session-meta\"")
            || line.windows(22).any(|w| w == b"\"file-history-snapshot\"")
        {
            skipped_meta = true;
            continue;
        }
        if line.first() != Some(&b'{') {
            continue;
        }
        let text = String::from_utf8_lossy(&line);
        let d: Value = match serde_json::from_str(text.trim_end_matches('\n')) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(obj) = d.as_object() {
            apply_line(&mut agg, obj);
        }
    }
    let _ = skipped_meta;
    if chunk_bytes == 0 {
        return prev.cloned().unwrap_or_else(|| json!({}));
    }
    let proj = path
        .parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    agg.insert("proj".into(), Value::String(proj));
    {
        let mut s = SCAN.lock().unwrap_or_else(|e| e.into_inner());
        s.bytes += chunk_bytes;
    }
    json!({
        "v": PARSE_VERSION,
        "off": consumed as i64,
        "size": size,
        "mtime": mtime,
        "ino": ino,
        "agg": Value::Object(agg),
    })
}

/// `_scan_sessions`
fn scan_sessions_inner(cache: &Path, force: bool) {
    let root = wb_root();
    if !root.exists() {
        return;
    }
    LOADED.store(true, Ordering::SeqCst);
    if force {
        let mut g = SESS.lock().unwrap_or_else(|e| e.into_inner());
        *g = Some(Map::new());
    } else {
        if let Some(disk) = crate::jsonio::read_json(cache) {
            if disk.get("v").and_then(|x| x.as_i64()) == Some(PARSE_VERSION) {
                if let Some(Value::Object(files)) = disk.get("files") {
                    let mut g = SESS.lock().unwrap_or_else(|e| e.into_inner());
                    *g = Some(files.clone());
                }
            }
        }
    }
    // 按 mtime 新的先扫（glob 与 stat 之间的轮转不拖垮整体）
    let mut files: Vec<(f64, PathBuf)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&root) {
        for e1 in rd.flatten() {
            let d = e1.path();
            if !d.is_dir() {
                continue;
            }
            if let Ok(rd2) = std::fs::read_dir(&d) {
                for e2 in rd2.flatten() {
                    let p = e2.path();
                    if !p.to_string_lossy().ends_with(".jsonl") {
                        continue;
                    }
                    let mt = std::fs::metadata(&p)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|x| x.as_secs_f64())
                        .unwrap_or(0.0);
                    files.push((-mt, p));
                }
            }
        }
    }
    files.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    {
        let mut s = SCAN.lock().unwrap_or_else(|e| e.into_inner());
        *s = Scan {
            total: files.len() as i64,
            done: 0,
            running: true,
            started: chrono::Local::now().format("%H:%M:%S").to_string(),
            err: String::new(),
            bytes: 0,
            cached_bytes: 0,
        };
    }
    let mut cur: Map<String, Value> = {
        let g = SESS.lock().unwrap_or_else(|e| e.into_inner());
        g.as_ref().cloned().unwrap_or_default()
    };
    for (i, (_, path)) in files.iter().enumerate() {
        let key = path.to_string_lossy().to_string();
        let prev = cur.get(&key).cloned();
        let entry = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            scan_file(path, prev.as_ref())
        }))
        .unwrap_or_else(|_| prev.clone().unwrap_or_else(|| json!({})));
        if entry.as_object().map(|m| !m.is_empty()).unwrap_or(false) {
            cur.insert(key, entry);
        }
        {
            let mut s = SCAN.lock().unwrap_or_else(|e| e.into_inner());
            s.done = (i + 1) as i64;
        }
        if (i + 1) % 6 == 0 {
            let mut g = SESS.lock().unwrap_or_else(|e| e.into_inner());
            *g = Some(cur.clone());
        }
    }
    {
        let mut g = SESS.lock().unwrap_or_else(|e| e.into_inner());
        *g = Some(cur.clone());
    }
    let payload = json!({"v": PARSE_VERSION, "files": cur});
    let text = py_dumps_compact(&payload);
    let _ = crate::jsonio::atomic_write(cache, &text);
    {
        let mut s = SCAN.lock().unwrap_or_else(|e| e.into_inner());
        s.running = false;
    }
}

/// `scan_sessions(force)`：串行执行（后台线程与按需补扫不会抢同一个 _SESS）。
pub fn scan_sessions(ctx: &Ctx, force: bool) {
    let _g = SCAN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    scan_sessions_inner(&cache_path(ctx), force);
    *SCAN_AT.lock().unwrap_or_else(|e| e.into_inner()) = now_secs();
}

/// `_kick_scan`：需要补扫就丢到后台，不挡这次请求。
pub fn kick_scan(cache: &Path, max_age: f64) -> bool {
    if now_secs() - scan_at() < max_age {
        return false;
    }
    if RUNNING.swap(true, Ordering::SeqCst) {
        return false;
    }
    let cache = cache.to_path_buf();
    std::thread::spawn(move || {
        struct G;
        impl Drop for G {
            fn drop(&mut self) {
                RUNNING.store(false, Ordering::SeqCst);
            }
        }
        let _g = G;
        let _l = SCAN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        scan_sessions_inner(&cache, false);
        *SCAN_AT.lock().unwrap_or_else(|e| e.into_inner()) = now_secs();
    });
    true
}

pub fn max_last(aggs: &[Value]) -> i64 {
    aggs.iter()
        .map(|a| py_int(a.get("last").unwrap_or(&Value::Null)))
        .max()
        .unwrap_or(0)
}
