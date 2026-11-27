// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：overview 里 `prompts` 那一段（提示词条数 / 字数 / 折算 token / 活跃天数）。
//       对齐 workstat.py 的 view() + server.py 的 _prompts_brief()。
// 不做：不做屏幕使用时间、热力图、按 agent 榜 —— 那几项只服务 /api/workstat。
//
// 数据源与 workstat 一致：`~/.workbuddy/projects/*/*.jsonl` 里带 `<user_query>` 的用户消息，
// 外加 TRAE / Cherry / Claude 的自报轮次。字数按**字符**算，token 按中文 0.6 / 其他 0.3 折。
#![allow(dead_code)]

use super::platform;
use super::sessions;
use super::util::{fnum, now_ymd, round_n, str_or, truthy};
use crate::paths::Ctx;
use regex::bytes::Regex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

const KEEP_DAYS: i64 = 200;
const LENS: [i64; 6] = [20, 50, 100, 200, 500, 1000];

#[derive(Default, Clone)]
struct DayAgg {
    prompts: i64,
    chars: i64,
    ptok: i64,
    reqs: i64,
}

#[derive(Default)]
struct FileBucket {
    ino: u64,
    size: u64,
    off: u64,
    days: HashMap<String, DayAgg>,
    /// rid → 这一轮落在哪天（同一天里同一 rid 只算一轮）
    rids: HashMap<String, String>,
}

struct Res {
    rid: Regex,
    ts: Regex,
    q: Regex,
    total: Regex,
}

fn res() -> &'static Res {
    static R: OnceLock<Res> = OnceLock::new();
    R.get_or_init(|| Res {
        rid: Regex::new(r#""conversationRequestId":"([0-9a-fA-F-]{8,64})""#).unwrap(),
        ts: Regex::new(r#""timestamp":(\d{13})"#).unwrap(),
        q: Regex::new(r"(?s)<user_query>(.*?)</user_query>").unwrap(),
        total: Regex::new(r#""totalTokens":(\d+)"#).unwrap(),
    })
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

fn wb_root() -> PathBuf {
    home().join(".workbuddy").join("projects")
}

fn mtime_ms(p: &Path) -> f64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

fn ino_of(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| platform::file_inode(&m)).unwrap_or(0)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn local_day(ms: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_millis_opt(ms) {
        chrono::LocalResult::Single(t) => t.format("%Y-%m-%d").to_string(),
        _ => String::new(),
    }
}

fn hour_key(ms: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_millis_opt(ms) {
        chrono::LocalResult::Single(t) => t.format("%Y-%m-%dT%H").to_string(),
        _ => String::new(),
    }
}

/// `_unesc`：把 `<user_query>` 里抠出来的内胆按 JSON 字符串解码。
fn unesc(body: &[u8]) -> String {
    let mut buf = Vec::with_capacity(body.len() + 2);
    buf.push(b'"');
    buf.extend_from_slice(body);
    buf.push(b'"');
    match serde_json::from_slice::<String>(&buf) {
        Ok(s) => s,
        Err(_) => String::from_utf8_lossy(body).to_string(),
    }
}

/// `_ptok`：中文 0.6 / 其他 0.3，最后按 Python `round()` 取整。
fn ptok(s: &str) -> i64 {
    let mut cjk: i64 = 0;
    let mut total: i64 = 0;
    for ch in s.chars() {
        total += 1;
        let o = ch as u32;
        if (0x3400..=0x9FFF).contains(&o)
            || (0xF900..=0xFAFF).contains(&o)
            || (0x3000..=0x303F).contains(&o)
            || (0xFF00..=0xFFEF).contains(&o)
        {
            cjk += 1;
        }
    }
    py_round_int(cjk as f64 * 0.6 + (total - cjk) as f64 * 0.3)
}

/// Python `round(x)`（无 ndigits）：四舍六入五成双。
fn py_round_int(x: f64) -> i64 {
    let f = x.floor();
    let diff = x - f;
    if diff > 0.5 {
        f as i64 + 1
    } else if diff < 0.5 {
        f as i64
    } else if (f as i64) % 2 == 0 {
        f as i64
    } else {
        f as i64 + 1
    }
}

fn link_count(s: &str) -> i64 {
    let low = s.to_lowercase();
    let mut n = 0i64;
    let mut from = 0;
    while let Some(p) = low[from..].find("http://").or_else(|| low[from..].find("https://")) {
        n += 1;
        from += p + 7;
    }
    n
}

/// `needle in haystack`（按字节；`windows(n)` 的长度必须与 needle 一致，
/// 此前把 13 字节的 `"role":"user"` 写成 windows(12)，整条提示词分支就没进去过）。
fn has(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

fn day_agg<'a>(days: &'a mut HashMap<String, DayAgg>, d: &str) -> &'a mut DayAgg {
    days.entry(d.to_string()).or_default()
}

fn scan_file(p: &Path, prev: Option<&FileBucket>) -> FileBucket {
    let st = match std::fs::metadata(p) {
        Ok(s) => s,
        Err(_) => return FileBucket::default(),
    };
    let ino = platform::file_inode(&st);
    let size = st.len();
    let same = prev
        .map(|b| b.ino == ino && b.size <= size)
        .unwrap_or(false);
    let mut off = if same { prev.unwrap().off } else { 0 };
    let mut days = if same { prev.unwrap().days.clone() } else { HashMap::new() };
    let mut rids = if same { prev.unwrap().rids.clone() } else { HashMap::new() };
    if size == off {
        return FileBucket { ino, size, off, days, rids };
    }
    let mut f = match std::fs::File::open(p) {
        Ok(f) => f,
        Err(_) => return FileBucket { ino, size, off, days, rids },
    };
    if f.seek(SeekFrom::Start(off)).is_err() {
        return FileBucket { ino, size, off, days, rids };
    }
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    let cut = match buf.iter().rposition(|b| *b == b'\n') {
        Some(c) => c,
        None => return FileBucket { ino, size, off, days, rids },
    };
    let chunk = &buf[..cut + 1];
    let r = res();
    for raw in chunk.split(|b| *b == b'\n') {
        if raw.is_empty() {
            continue;
        }
        if has(raw, b"<user_query>") && has(raw, b"\"role\":\"user\"") {
            let rid = r.rid.captures(raw);
            let tsm = r.ts.captures(raw);
            let q = r.q.captures(raw);
            if let (Some(rid), Some(tsm), Some(q)) = (rid, tsm, q) {
                let ts: i64 = std::str::from_utf8(&tsm[1])
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                let text = unesc(&q[1]);
                let body = text.trim();
                let n = body.chars().count() as i64;
                let key = hour_key(ts);
                if !key.is_empty() {
                    let d = &key[..10];
                    let a = day_agg(&mut days, d);
                    a.prompts += 1;
                    a.chars += n;
                    a.ptok += ptok(body);
                    let _ = link_count(&text);
                }
                let rid_s = String::from_utf8_lossy(&rid[1]).to_string();
                let d = local_day(ts);
                if !d.is_empty() {
                    let e = rids.entry(rid_s).or_insert_with(|| d.clone());
                    *e = d;
                }
            }
            continue;
        }
        if has(raw, b"\"usage\"") && r.total.is_match(raw) {
            if let Some(rid) = r.rid.captures(raw) {
                let rid_s = String::from_utf8_lossy(&rid[1]).to_string();
                if !rids.contains_key(&rid_s) {
                    if let Some(tsm) = r.ts.captures(raw) {
                        if let Ok(s) = std::str::from_utf8(&tsm[1]) {
                            if let Ok(ts) = s.parse::<i64>() {
                                let d = local_day(ts);
                                if !d.is_empty() {
                                    rids.insert(rid_s, d);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    off += (cut + 1) as u64;
    FileBucket { ino, size, off, days, rids }
}

fn cache_files() -> &'static Mutex<HashMap<String, FileBucket>> {
    static C: OnceLock<Mutex<HashMap<String, FileBucket>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 扫一遍会话流水，返回「日期 → 明细」。
fn scan_all() -> HashMap<String, DayAgg> {
    let root = wb_root();
    let since_ms = now_ms() as f64 - KEEP_DAYS as f64 * 86400000.0;
    let mut out: HashMap<String, DayAgg> = HashMap::new();
    let mut cache = cache_files().lock().unwrap();
    let mut live: Vec<String> = Vec::new();

    if root.is_dir() {
        if let Ok(rd) = std::fs::read_dir(&root) {
            for e in rd.flatten() {
                let dir = e.path();
                let rd2 = match std::fs::read_dir(&dir) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                for f in rd2.flatten() {
                    let p = f.path();
                    let name = f.file_name().to_string_lossy().to_string();
                    if !name.ends_with(".jsonl") || name.ends_with(".file-rollback.ndjson") {
                        continue;
                    }
                    if mtime_ms(&p) < since_ms {
                        continue;
                    }
                    let key = p.to_string_lossy().to_string();
                    live.push(key.clone());
                    let prev = cache.get(&key);
                    let b = scan_file(&p, prev);
                    cache.insert(key, b);
                }
            }
        }
    }
    cache.retain(|k, _| live.contains(k));

    for b in cache.values() {
        for (d, a) in &b.days {
            let t = day_agg(&mut out, d);
            t.prompts += a.prompts;
            t.chars += a.chars;
            t.ptok += a.ptok;
        }
        for (_, d) in &b.rids {
            day_agg(&mut out, d).reqs += 1;
        }
    }
    out
}

fn other_agents(ctx: &Ctx) -> Vec<Value> {
    let since = now_ms() as f64 - KEEP_DAYS as f64 * 86400000.0;
    let mut out: Vec<Value> = dash_trae_t0s(ctx, since)
        .into_iter()
        .map(|t0| json!({"t0": t0}))
        .collect();
    out.extend(sessions::cherry_items(since));
    out.extend(sessions::claude_items(since));
    out
}

/// `dash._trae_items()` 的口径（**只有 overview 的提示词这一段用**）。
///
/// 和 `trae_work.items()` 有两处**故意不一样**，因为 Python 侧就是两份实现：
///   1. 不按 message_id 去重 —— 同一条记忆落进两个项目目录时，`dash` 会数两遍；
///   2. 自述账只取文件**最后 400 行**，且 intent/actions 全空的记忆行跳过。
/// 提示词条数是「AI 动过几次」的计数，跟着 dash 走才能和 Python 的 overview 对齐。
fn dash_trae_t0s(ctx: &Ctx, since_ms: f64) -> Vec<i64> {
    let mut out: Vec<i64> = Vec::new();
    for root in [
        home().join(".trae-cn").join("memory").join("projects"),
        home().join(".trae").join("memory").join("projects"),
    ] {
        let mut files: Vec<PathBuf> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&root) {
            for e in rd.flatten() {
                if let Ok(rd2) = std::fs::read_dir(e.path()) {
                    for e2 in rd2.flatten() {
                        if let Ok(rd3) = std::fs::read_dir(e2.path()) {
                            for e3 in rd3.flatten() {
                                let f = e3.path();
                                let nm = f
                                    .file_name()
                                    .map(|s| s.to_string_lossy().to_string())
                                    .unwrap_or_default();
                                if nm.starts_with("session_memory_") && nm.ends_with(".jsonl") {
                                    files.push(f);
                                }
                            }
                        }
                    }
                }
            }
        }
        for p in files {
            if mtime_ms(&p) < since_ms {
                continue;
            }
            let Ok(b) = std::fs::read(&p) else { continue };
            let text = String::from_utf8_lossy(&b);
            for ln in text.lines() {
                let ln = ln.trim();
                if ln.is_empty() {
                    continue;
                }
                let Ok(o) = serde_json::from_str::<Value>(ln) else { continue };
                if !o.is_object() {
                    continue;
                }
                let ts = match chrono::NaiveDateTime::parse_from_str(
                    str_or(o.get("message_summary_time")).get(..19).unwrap_or(""),
                    "%Y-%m-%d %H:%M:%S",
                ) {
                    Ok(d) => d
                        .and_local_timezone(chrono::Local)
                        .single()
                        .map(|t| t.timestamp_millis())
                        .unwrap_or(0),
                    Err(_) => 0,
                };
                if ts == 0 || (ts as f64) < since_ms {
                    continue;
                }
                let intent = str_or(o.get("intent")).split_whitespace().collect::<Vec<_>>().join(" ");
                let acts = match o.get("actions") {
                    Some(Value::Array(a)) => a
                        .iter()
                        .filter(|x| truthy(x))
                        .count(),
                    _ => 0,
                };
                if intent.is_empty() && acts == 0 {
                    continue;
                }
                out.push(ts);
            }
        }
    }
    // agent 自述账：最后 400 行、at 在窗口内、task 非空
    for p in [
        ctx.library_dir.join("_本机工作").join("Agent自述.jsonl"),
        ctx.library_dir.join("_本机工作").join("TRAE自述.jsonl"),
    ] {
        if mtime_ms(&p) < since_ms {
            continue;
        }
        let Ok(b) = std::fs::read(&p) else { continue };
        let text = String::from_utf8_lossy(&b);
        let all: Vec<&str> = text.lines().collect();
        let st = all.len().saturating_sub(400);
        for ln in &all[st..] {
            let ln = ln.trim();
            if ln.is_empty() {
                continue;
            }
            let Ok(o) = serde_json::from_str::<Value>(ln) else { continue };
            if !o.is_object() {
                continue;
            }
            let t = super::util::as_f64(o.get("at").unwrap_or(&Value::Null)).unwrap_or(0.0) * 1000.0;
            if t == 0.0 || t < since_ms {
                continue;
            }
            if str_or(o.get("task")).split_whitespace().count() == 0 {
                continue;
            }
            out.push(t as i64);
        }
    }
    out
}

// 45 秒缓存（键 = days）
static CACHE: Mutex<Option<(std::time::Instant, i64, Value)>> = Mutex::new(None);

/// `_prompts_brief(days)`。
pub fn brief(ctx: &Ctx, days_in: i64) -> Value {
    if let Ok(g) = CACHE.lock() {
        if let Some((at, d, v)) = g.as_ref() {
            if *d == days_in && at.elapsed() < Duration::from_secs(45) {
                return v.clone();
            }
        }
    }
    let win_n = days_in.max(1);
    let view_days = win_n.max(7);

    let mut days = scan_all();
    for it in other_agents(ctx) {
        let t0 = it.get("t0").and_then(|v| v.as_f64()).unwrap_or(0.0) as i64;
        if t0 == 0 {
            continue;
        }
        let d = local_day(t0);
        if d.is_empty() {
            continue;
        }
        let a = day_agg(&mut days, &d);
        a.prompts += 1;
        a.reqs += 1;
    }

    // 窗口内每天一行（含 0）
    let today = now_ymd();
    let today_d = match chrono::NaiveDate::parse_from_str(&today, "%Y-%m-%d") {
        Ok(d) => d,
        Err(_) => return json!({"ok": false, "why": "日期解析失败"}),
    };
    let mut rows: Vec<(String, DayAgg)> = Vec::new();
    for i in (0..view_days).rev() {
        let d = (today_d - chrono::Duration::days(i)).format("%Y-%m-%d").to_string();
        let a = days.get(&d).cloned().unwrap_or_default();
        rows.push((d, a));
    }
    let tail: Vec<(String, DayAgg)> = rows
        .iter()
        .rev()
        .take(win_n as usize)
        .rev()
        .cloned()
        .collect();

    let n: i64 = tail.iter().map(|(_, a)| a.prompts).sum();
    let chars: i64 = tail.iter().map(|(_, a)| a.chars).sum();
    let ptok_sum: i64 = tail.iter().map(|(_, a)| a.ptok).sum();
    let active = tail.iter().filter(|(_, a)| a.prompts != 0 || a.reqs != 0).count() as i64;

    let by_day: Vec<Value> = tail
        .iter()
        .map(|(d, a)| json!({"d": d, "n": a.prompts, "c": a.chars, "t": a.ptok}))
        .collect();

    let mut o = serde_json::Map::new();
    o.insert("ok".into(), json!(true));
    o.insert("days".into(), json!(win_n));
    o.insert("n".into(), json!(n));
    o.insert("chars".into(), json!(chars));
    o.insert("ptok".into(), json!(ptok_sum));
    o.insert("per".into(), fnum(round_n(chars as f64 / n.max(1) as f64, 1)));
    o.insert("per_tok".into(), fnum(round_n(ptok_sum as f64 / n.max(1) as f64, 1)));
    o.insert("per_active".into(), fnum(round_n(n as f64 / active.max(1) as f64, 1)));
    o.insert("active_days".into(), json!(active));
    o.insert("by_day".into(), Value::Array(by_day));
    let v = Value::Object(o);

    if let Ok(mut g) = CACHE.lock() {
        *g = Some((std::time::Instant::now(), days_in, v.clone()));
    }
    v
}
