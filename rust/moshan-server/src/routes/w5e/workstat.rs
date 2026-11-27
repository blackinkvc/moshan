// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：每日工作总结的**数据层**（对齐 moshan/workstat.py：scan / _fold /
//       _absorb / _heat / view）。
// 不做：不做 HTTP 装配（mod.rs 里），不重写会话聚合层（hostwork 的 agg/sessions
//       负责 dash 口径；这里要的是 workstat 自己那份「每轮账」——字段不同、
//       缓存文件也不同，所以在本目录里按 Python 的口径实现，格式与 Python 双向兼容）。
//
// 口径（改口径先改这段注释）：
//   · AI 时长 = 一轮请求的首末时间并集（分钟格 am 的位串，一位一分钟，+120 秒）。
//   · 人时长   = 系统记录里「有 App 在前台」的区间并集（screen.rs）。
//   · 提示词   = 会话 jsonl 里 `<user_query>` 的用户消息，按小时打桶（ph）。
//   · 永久存档 = 一天一条落在数据根里，只增不减（`_max_merge`）。
#![allow(dead_code)]

use crate::hostwork::platform;
use crate::hostwork::sessions as hs;
use crate::hostwork::util::{py_int, round_n, str_or, truthy};
use crate::paths::Ctx;
use crate::routes::w5e::screen;
use crate::routes::w5e::util::{
    bits_secs, dumps_nospace, dumps_sorted, loads, py_title, round_int, OrdMap,
};
use chrono::{Datelike, Local, NaiveDate, TimeZone, Timelike};
use regex::bytes::Regex as BRegex;
use regex::Regex as SRegex;
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------- 常量
const KEEP_DAYS: i64 = 200;
const SCAN_TTL: f64 = 60.0;
const MAX_FILES: usize = 4000;
const MARK_PAD_S: i64 = 120;
const TURN_CAP_MS: i64 = 4 * 3600 * 1000;
const LENS: [i64; 6] = [20, 50, 100, 200, 500, 1000];
const B_HIST: usize = 4;
const B_PTOK: usize = 11;
const B_LEN: usize = 12;
const PTOK_CJK: f64 = 0.6;
const PTOK_OTHER: f64 = 0.3;
const ARCHIVE_SAVE_S: f64 = 120.0;
pub const HOUR_COLS: [&str; 9] =
    ["ai", "human", "tok", "tin", "tout", "prompts", "chars", "links", "imgs"];
const C_AI: usize = 0;
const C_HUMAN: usize = 1;
const C_TOK: usize = 2;
const C_TIN: usize = 3;
const C_TOUT: usize = 4;
const C_PROMPTS: usize = 5;
const C_CHARS: usize = 6;
const C_LINKS: usize = 7;
const C_IMGS: usize = 8;

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn now_ms() -> i64 {
    (now_secs() * 1000.0) as i64
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

fn wb_root() -> PathBuf {
    home().join(".workbuddy").join("projects")
}

fn own_dir(ctx: &Ctx) -> PathBuf {
    ctx.runtime_dir.join("_本机工作")
}

fn cache_dir(ctx: &Ctx) -> PathBuf {
    own_dir(ctx).join("缓存")
}

pub fn cache_path(ctx: &Ctx) -> PathBuf {
    cache_dir(ctx).join("工作统计.json")
}

fn archive_path(ctx: &Ctx) -> PathBuf {
    own_dir(ctx).join("工作日报.json")
}

fn today() -> NaiveDate {
    Local::now().date_naive()
}

fn ymd(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

fn day_of(ms: i64) -> String {
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|t| t.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

fn hour_of(ms: i64) -> String {
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|t| format!("{}T{}", t.format("%Y-%m-%d"), t.format("%H")))
        .unwrap_or_default()
}

fn hour_start_ms(ms: i64) -> Option<i64> {
    let t = Local.timestamp_millis_opt(ms).single()?;
    let s = t.with_minute(0)?.with_second(0)?.with_nanosecond(0)?;
    Some(s.timestamp_millis())
}

fn hour_index(ms: i64) -> i64 {
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|t| t.hour() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------- 行级正则
fn r_rid() -> &'static BRegex {
    static R: OnceLock<BRegex> = OnceLock::new();
    R.get_or_init(|| BRegex::new(r#""conversationRequestId":"([0-9a-fA-F-]{8,64})""#).unwrap())
}
fn r_ts() -> &'static BRegex {
    static R: OnceLock<BRegex> = OnceLock::new();
    R.get_or_init(|| BRegex::new(r#""timestamp":(\d{13})"#).unwrap())
}
fn r_total() -> &'static BRegex {
    static R: OnceLock<BRegex> = OnceLock::new();
    R.get_or_init(|| BRegex::new(r#""totalTokens":(\d+)"#).unwrap())
}
fn r_tin() -> &'static BRegex {
    static R: OnceLock<BRegex> = OnceLock::new();
    R.get_or_init(|| BRegex::new(r#""inputTokens":(\d+)"#).unwrap())
}
fn r_tout() -> &'static BRegex {
    static R: OnceLock<BRegex> = OnceLock::new();
    R.get_or_init(|| BRegex::new(r#""outputTokens":(\d+)"#).unwrap())
}
fn r_cached() -> &'static BRegex {
    static R: OnceLock<BRegex> = OnceLock::new();
    R.get_or_init(|| BRegex::new(r#""cached_tokens":(\d+)"#).unwrap())
}
fn r_model() -> &'static BRegex {
    static R: OnceLock<BRegex> = OnceLock::new();
    R.get_or_init(|| BRegex::new(r#""requestModelName":"([^"]{1,60})""#).unwrap())
}
fn r_q() -> &'static BRegex {
    static R: OnceLock<BRegex> = OnceLock::new();
    R.get_or_init(|| BRegex::new(r"(?s)<user_query>(.*?)</user_query>").unwrap())
}
fn r_expert() -> &'static BRegex {
    static R: OnceLock<BRegex> = OnceLock::new();
    R.get_or_init(|| BRegex::new(r"(?s)<current-expert>[\s\S]{0,200}?(ex_[A-Za-z0-9]{6,})").unwrap())
}
fn r_mode() -> &'static BRegex {
    static R: OnceLock<BRegex> = OnceLock::new();
    R.get_or_init(|| BRegex::new(r"You are now in ([A-Za-z][A-Za-z ]{2,14}?) mode").unwrap())
}
fn r_mcp() -> &'static BRegex {
    static R: OnceLock<BRegex> = OnceLock::new();
    R.get_or_init(|| BRegex::new(r"mcp__([A-Za-z0-9_\-]+?)__").unwrap())
}
fn r_link() -> &'static SRegex {
    static R: OnceLock<SRegex> = OnceLock::new();
    R.get_or_init(|| SRegex::new(r"https?://").unwrap())
}
fn r_img() -> &'static SRegex {
    static R: OnceLock<SRegex> = OnceLock::new();
    R.get_or_init(|| SRegex::new(r"@image#\d+:|<image_local_path>").unwrap())
}

// ---------------------------------------------------------------- 状态
#[derive(Clone, Default)]
pub struct Req {
    pub t0: i64,
    pub t1: i64,
    pub tin: i64,
    pub tout: i64,
    pub cached: i64,
    pub calls: i64,
    pub model: String,
    pub expert: String,
    pub mcp: String,
    pub mode: String,
}

impl Req {
    fn to_json(&self) -> Value {
        json!([
            self.t0, self.t1, self.tin, self.tout, self.cached, self.calls,
            self.model, self.expert, self.mcp, self.mode
        ])
    }

    fn from_json(v: &Value) -> Option<Req> {
        let a = v.as_array()?;
        let gi = |i: usize| py_int(a.get(i).unwrap_or(&Value::Null));
        let gs = |i: usize| str_or(a.get(i));
        Some(Req {
            t0: gi(0),
            t1: gi(1),
            tin: gi(2),
            tout: gi(3),
            cached: gi(4),
            calls: gi(5),
            model: gs(6),
            expert: gs(7),
            mcp: gs(8),
            mode: gs(9),
        })
    }
}

#[derive(Clone, Default)]
pub struct FileState {
    pub off: i64,
    pub ino: i64,
    pub size: i64,
    pub reqs: OrdMap<String, Req>,
    pub ph: OrdMap<String, Vec<i64>>,
}

impl FileState {
    fn to_json(&self) -> Value {
        let mut reqs = Map::new();
        for (k, r) in self.reqs.iter() {
            reqs.insert(k.clone(), r.to_json());
        }
        let mut ph = Map::new();
        for (k, b) in self.ph.iter() {
            ph.insert(k.clone(), json!(b));
        }
        json!({
            "off": self.off,
            "ino": self.ino,
            "size": self.size,
            "reqs": Value::Object(reqs),
            "ph": Value::Object(ph),
        })
    }

    fn from_json(v: &Value) -> FileState {
        let mut f = FileState::default();
        let o = match v.as_object() {
            Some(o) => o,
            None => return f,
        };
        f.off = py_int(o.get("off").unwrap_or(&Value::Null));
        f.ino = py_int(o.get("ino").unwrap_or(&Value::Null));
        f.size = py_int(o.get("size").unwrap_or(&Value::Null));
        if let Some(Value::Object(rq)) = o.get("reqs") {
            for (k, r) in rq {
                if let Some(x) = Req::from_json(r) {
                    f.reqs.insert(k.clone(), x);
                }
            }
        }
        if let Some(Value::Object(ph)) = o.get("ph") {
            for (k, b) in ph {
                let mut v: Vec<i64> = Vec::new();
                if let Some(a) = b.as_array() {
                    for x in a {
                        v.push(py_int(x));
                    }
                }
                f.ph.insert(k.clone(), v);
            }
        }
        f
    }
}

#[derive(Clone, Default)]
pub struct State {
    pub files: OrdMap<String, Option<FileState>>,
    pub am: OrdMap<String, String>,
    pub at: f64,
}

impl State {
    fn to_json(&self) -> Value {
        let mut files = Map::new();
        for (k, f) in self.files.iter() {
            files.insert(
                k.clone(),
                match f {
                    Some(x) => x.to_json(),
                    None => json!({}),
                },
            );
        }
        let mut am = Map::new();
        for (k, v) in self.am.iter() {
            am.insert(k.clone(), Value::String(v.clone()));
        }
        json!({
            "v": 2,
            "files": Value::Object(files),
            "am": Value::Object(am),
            "at": self.at,
        })
    }

    fn from_json(v: &Value) -> State {
        let mut s = State::default();
        if let Some(Value::Object(files)) = v.get("files") {
            for (k, f) in files {
                let e = if f.as_object().map(|o| o.is_empty()).unwrap_or(true) {
                    None
                } else {
                    Some(FileState::from_json(f))
                };
                s.files.insert(k.clone(), e);
            }
        }
        if let Some(Value::Object(am)) = v.get("am") {
            for (k, x) in am {
                s.am.insert(k.clone(), str_or(Some(x)));
            }
        }
        s.at = crate::hostwork::util::py_double(v.get("at").unwrap_or(&Value::Null));
        s
    }
}

struct ScanCache {
    data: Arc<State>,
    at: f64,
    took: f64,
}

static SCACHE: Mutex<Option<ScanCache>> = Mutex::new(None);
static SCAN_LOCK: Mutex<()> = Mutex::new(());

// ---------------------------------------------------------------- 小工具
fn py_round1(x: f64) -> f64 {
    round_n(x, 1)
}

fn get_i(m: &Map<String, Value>, k: &str) -> i64 {
    py_int(m.get(k).unwrap_or(&Value::Null))
}

fn get_f(m: &Map<String, Value>, k: &str) -> f64 {
    crate::hostwork::util::py_double(m.get(k).unwrap_or(&Value::Null))
}

fn bump(m: &mut Map<String, Value>, k: &str, n: i64) {
    let cur = get_i(m, k);
    m.insert(k.to_string(), json!(cur + n));
}

fn set_f(m: &mut Map<String, Value>, k: &str, v: f64) {
    m.insert(k.to_string(), json!(v));
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

enum Num {
    I(i64),
    F(f64),
}

fn add_cell(cell: &mut Value, n: &Num) {
    let new = match (&*cell, n) {
        (Value::Number(x), Num::I(y)) if x.is_i64() => {
            json!(x.as_i64().unwrap_or(0) + *y)
        }
        (Value::Number(x), _) => {
            let a = x.as_f64().unwrap_or(0.0);
            let b = match n {
                Num::I(y) => *y as f64,
                Num::F(y) => *y,
            };
            json!(a + b)
        }
        (_, Num::I(y)) => json!(*y),
        (_, Num::F(y)) => json!(*y),
    };
    *cell = new;
}

// ---------------------------------------------------------------- 提示词折算
fn len_bucket(n: i64) -> usize {
    for (i, hi) in LENS.iter().enumerate() {
        if n < *hi {
            return i;
        }
    }
    LENS.len()
}

fn ptok(s: &str) -> i64 {
    let mut cjk = 0i64;
    let mut total = 0i64;
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
    round_int(cjk as f64 * PTOK_CJK + (total - cjk) as f64 * PTOK_OTHER)
}

/// `_unesc`：从原始行里抠出来的 `<user_query>` 内容是 JSON 字符串的内胆。
fn unesc(body: &[u8]) -> String {
    let mut quoted: Vec<u8> = Vec::with_capacity(body.len() + 2);
    quoted.push(b'"');
    quoted.extend_from_slice(body);
    quoted.push(b'"');
    match serde_json::from_slice::<String>(&quoted) {
        Ok(s) => s,
        Err(_) => String::from_utf8_lossy(body).to_string(),
    }
}

// ---------------------------------------------------------------- 分钟格
fn mark(am: &mut OrdMap<String, String>, ts_ms: i64) {
    let mut t = ts_ms;
    let end = t + MARK_PAD_S * 1000;
    let mut guard = 0;
    while t <= end && guard < 8 {
        guard += 1;
        let Some(h0) = hour_start_ms(t) else { return };
        let h1 = h0 + 3_600_000;
        let key = hour_of(t);
        let last = if end < h1 - 1 { end } else { h1 - 1 };
        let a = ((t - h0) / 60000).clamp(0, 59);
        let b = ((last - h0) / 60000).clamp(0, 59);
        let bits: u64 = if b >= a {
            let w = (b - a + 1) as u32;
            if w >= 64 {
                u64::MAX
            } else {
                ((1u64 << w) - 1) << a
            }
        } else {
            0
        };
        let cur = u64::from_str_radix(am.get(&key).map(|s| s.as_str()).unwrap_or("0"), 16).unwrap_or(0);
        am.insert(key, format!("{:x}", cur | bits));
        t = h1;
    }
}

// ---------------------------------------------------------------- 缓存读写
/// `json.loads(path.read_text())`：走正确舍入的浮点解析（见 util::loads）。
fn read_json_py(p: &Path) -> Option<Value> {
    let t = std::fs::read_to_string(p).ok()?;
    loads(&t)
}

fn load_cache(ctx: &Ctx) -> State {
    if let Some(raw) = read_json_py(&cache_path(ctx)) {
        if raw.get("v").and_then(|x| x.as_i64()) == Some(2) {
            return State::from_json(&raw);
        }
    }
    State::default()
}

fn save_cache(ctx: &Ctx, st: &State) {
    let text = dumps_nospace(&st.to_json());
    if crate::jsonio::atomic_write(&cache_path(ctx), &text).is_err() {
        // 落盘失败不影响这一次的展示（与 Python 同口径）
    }
}

// ---------------------------------------------------------------- 会话扫描
fn sess_files(since_ms: i64) -> Vec<PathBuf> {
    let root = wb_root();
    if !root.is_dir() {
        return Vec::new();
    }
    let mut out: Vec<PathBuf> = Vec::new();
    let rd = match std::fs::read_dir(&root) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    'outer: for e in rd.flatten() {
        let d = e.path();
        if !d.is_dir() {
            continue;
        }
        let rd2 = match std::fs::read_dir(&d) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for e2 in rd2.flatten() {
            let p = e2.path();
            let name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            if name.starts_with('.') || !name.ends_with(".jsonl") {
                continue;
            }
            if name.ends_with(".file-rollback.ndjson") {
                continue;
            }
            let ok = std::fs::metadata(&p)
                .map(|m| {
                    m.modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|x| x.as_secs_f64() * 1000.0 >= since_ms as f64)
                        .unwrap_or(false)
                })
                .unwrap_or(false);
            if !ok {
                continue;
            }
            out.push(p);
            if out.len() >= MAX_FILES {
                break 'outer;
            }
        }
    }
    out
}

fn scan_file(path: &Path, prev: Option<&FileState>, am: &mut OrdMap<String, String>) -> Option<FileState> {
    let st = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return prev.cloned(),
    };
    let ino = platform::file_inode(&st) as i64;
    let size = platform::file_size(&st) as i64;
    let same = match prev {
        Some(p) => p.ino == ino && p.size <= size,
        None => false,
    };
    let mut off = if same { prev.map(|p| p.off).unwrap_or(0) } else { 0 };
    let mut reqs: OrdMap<String, Req> = if same {
        prev.map(|p| p.reqs.clone()).unwrap_or_default()
    } else {
        OrdMap::new()
    };
    let mut fph: OrdMap<String, Vec<i64>> = if same {
        prev.map(|p| p.ph.clone()).unwrap_or_default()
    } else {
        OrdMap::new()
    };
    let done = |off: i64, reqs: OrdMap<String, Req>, ph: OrdMap<String, Vec<i64>>| FileState {
        off,
        ino,
        size,
        reqs,
        ph,
    };
    if size == off {
        return Some(done(off, reqs, fph));
    }
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Some(done(off, reqs, fph)),
    };
    if off > 0 && f.seek(SeekFrom::Start(off as u64)).is_err() {
        return Some(done(off, reqs, fph));
    }
    let mut buf: Vec<u8> = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return Some(done(off, reqs, fph));
    }
    let cut = match buf.iter().rposition(|b| *b == b'\n') {
        Some(i) => i,
        None => return Some(done(off, reqs, fph)),
    };
    let chunk = &buf[..cut + 1];

    for raw in chunk.split(|b| *b == b'\n') {
        if raw.is_empty() {
            continue;
        }
        // —— 提示词
        if find(raw, b"<user_query>").is_some() && find(raw, br#""role":"user""#).is_some() {
            let rid = r_rid().captures(raw);
            let tsm = r_ts().captures(raw);
            let q = r_q().captures(raw);
            if let (Some(rid), Some(tsm), Some(q)) = (rid, tsm, q) {
                let text = unesc(q.get(1).map(|m| m.as_bytes()).unwrap_or(b""));
                let body = text.trim();
                let n = body.chars().count() as i64;
                let ts = tsm
                    .get(1)
                    .and_then(|m| std::str::from_utf8(m.as_bytes()).ok())
                    .and_then(|s| s.parse::<i64>().ok())
                    .unwrap_or(0);
                let key = hour_of(ts);
                if !key.is_empty() {
                    let b = fph.or_insert_with(key.clone(), || vec![0i64; B_LEN]);
                    if b.len() < B_LEN {
                        b.resize(B_LEN, 0);
                    }
                    b[0] += 1;
                    b[1] += n;
                    b[2] += r_link().find_iter(&text).count() as i64;
                    b[3] += r_img().find_iter(&text).count() as i64;
                    b[B_PTOK] += ptok(body);
                    let bi = B_HIST + len_bucket(n);
                    if bi < b.len() {
                        b[bi] += 1;
                    }
                }
                let k = String::from_utf8_lossy(rid.get(1).map(|m| m.as_bytes()).unwrap_or(b"")).to_string();
                let r = reqs.or_insert_with(k, || Req {
                    t0: ts,
                    ..Default::default()
                });
                r.t0 = r.t0.min(ts);
                if r.expert.is_empty() {
                    if let Some(e) = r_expert().captures(raw) {
                        r.expert =
                            String::from_utf8_lossy(e.get(1).map(|m| m.as_bytes()).unwrap_or(b"")).to_string();
                    }
                }
                if r.mode.is_empty() {
                    if let Some(m) = r_mode().captures(raw) {
                        let s = String::from_utf8_lossy(m.get(1).map(|x| x.as_bytes()).unwrap_or(b""));
                        r.mode = py_title(s.trim());
                    }
                }
            }
            continue;
        }

        if find(raw, b"mcp__").is_some() {
            if let Some(rid) = r_rid().captures(raw) {
                let k = String::from_utf8_lossy(rid.get(1).map(|m| m.as_bytes()).unwrap_or(b"")).to_string();
                if let Some(r) = reqs.get_mut(&k) {
                    let mut seen: Vec<String> = Vec::new();
                    for m in r_mcp().captures_iter(raw) {
                        let srv = String::from_utf8_lossy(m.get(1).map(|x| x.as_bytes()).unwrap_or(b"")).to_string();
                        if srv.is_empty() || seen.contains(&srv) {
                            continue;
                        }
                        seen.push(srv.clone());
                        if r.mcp.contains(&srv) {
                            continue;
                        }
                        // Python: `(r[8] + "," + srv)[:80] if r[8] else srv`
                        // —— 第一次赋值不截断，只有追加时才截到 80 字。
                        r.mcp = if r.mcp.is_empty() {
                            srv
                        } else {
                            format!("{},{}", r.mcp, srv).chars().take(80).collect()
                        };
                    }
                }
            }
        }

        if find(raw, br#""usage""#).is_some() && r_total().is_match(raw) {
            let rid = r_rid().captures(raw);
            let tsm = r_ts().captures(raw);
            let (Some(rid), Some(tsm)) = (rid, tsm) else { continue };
            let k = String::from_utf8_lossy(rid.get(1).map(|m| m.as_bytes()).unwrap_or(b"")).to_string();
            let ts = match tsm
                .get(1)
                .and_then(|m| std::str::from_utf8(m.as_bytes()).ok())
                .and_then(|s| s.parse::<i64>().ok())
            {
                Some(v) => v,
                None => continue,
            };
            let r = reqs.or_insert_with(k, || Req {
                t0: ts,
                t1: ts,
                ..Default::default()
            });
            r.t1 = r.t1.max(ts);
            if r.t0 == 0 {
                r.t0 = ts;
            }
            let g = |re: &BRegex| -> i64 {
                re.captures(raw)
                    .and_then(|c| c.get(1))
                    .and_then(|m| std::str::from_utf8(m.as_bytes()).ok())
                    .and_then(|s| s.parse::<i64>().ok())
                    .unwrap_or(0)
            };
            r.tin += g(r_tin());
            r.tout += g(r_tout());
            r.cached += g(r_cached());
            r.calls += 1;
            if r.model.is_empty() {
                if let Some(m) = r_model().captures(raw) {
                    r.model = String::from_utf8_lossy(m.get(1).map(|x| x.as_bytes()).unwrap_or(b"")).to_string();
                }
            }
            mark(am, ts);
        }
    }
    off = off + cut as i64 + 1;
    Some(done(off, reqs, fph))
}

fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    if n.is_empty() || h.len() < n.len() {
        return None;
    }
    h.windows(n.len()).position(|w| w == n)
}

/// `scan(force)`：把窗口内的会话文件增量扫一遍。
pub fn scan(ctx: &Ctx, force: bool) -> State {
    let _g = SCAN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut d = if force { State::default() } else { load_cache(ctx) };
    let since_ms = now_ms() - KEEP_DAYS * 86_400_000;
    let keep = now_ms() - KEEP_DAYS * 86_400_000;
    let files = &mut d.files;
    let am = &mut d.am;

    let mut live: HashSet<String> = HashSet::new();
    for p in sess_files(since_ms) {
        let key = p.to_string_lossy().to_string();
        live.insert(key.clone());
        let prev = files.get(&key).cloned().flatten();
        let e = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            scan_file(&p, prev.as_ref(), am)
        }))
        .unwrap_or(None);
        files.insert(key, e);
    }
    let dead: Vec<String> = files
        .keys()
        .into_iter()
        .filter(|k| !live.contains(k))
        .collect();
    for k in dead {
        files.remove(&k);
    }
    // 每轮只留 KEEP_DAYS 内的
    let cut_ms = keep;
    let keys = files.keys();
    for k in keys {
        if let Some(Some(f)) = files.get_mut(&k) {
            let dead: Vec<String> = f
                .reqs
                .iter()
                .filter(|(_, r)| r.t0 < cut_ms)
                .map(|(k, _)| k.clone())
                .collect();
            for rid in dead {
                f.reqs.remove(&rid);
            }
        }
    }
    // 提示词桶跟着文件走，分钟格是全局的
    let cut_day = ymd(today() - chrono::Duration::days(KEEP_DAYS));
    let keys = files.keys();
    for k in keys {
        if let Some(Some(f)) = files.get_mut(&k) {
            let dead: Vec<String> = f
                .ph
                .iter()
                .filter(|(k, _)| key_day(k) < cut_day)
                .map(|(k, _)| k.clone())
                .collect();
            for kk in dead {
                f.ph.remove(&kk);
            }
        }
    }
    let dead: Vec<String> = am
        .iter()
        .filter(|(k, _)| key_day(k) < cut_day)
        .map(|(k, _)| k.clone())
        .collect();
    for k in dead {
        am.remove(&k);
    }
    d.at = now_secs();
    d
}

fn key_day(k: &str) -> String {
    k.chars().take(10).collect()
}

// ---------------------------------------------------------------- 按天折账
fn blank_day(d: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("d".into(), Value::String(d.to_string()));
    m.insert("ai".into(), json!(0.0));
    m.insert("human".into(), json!(0.0));
    for k in ["tok", "tin", "tout", "prompts", "chars", "links", "imgs", "reqs", "ptok"] {
        m.insert(k.into(), json!(0));
    }
    m.insert("has".into(), json!(0));
    let hours: Vec<Value> = (0..24)
        .map(|_| json!([0, 0, 0, 0, 0, 0, 0, 0, 0]))
        .collect();
    m.insert("hours".into(), Value::Array(hours));
    for k in ["apps", "agents", "models", "experts", "mcps"] {
        m.insert(k.into(), Value::Object(Map::new()));
    }
    m.insert("hist".into(), json!(vec![0i64; LENS.len() + 1]));
    m
}

fn day_rec<'a>(days: &'a mut Map<String, Value>, d: &str) -> &'a mut Map<String, Value> {
    if !days.get(d).map(|v| v.is_object()).unwrap_or(false) {
        days.insert(d.to_string(), Value::Object(blank_day(d)));
    }
    days.get_mut(d).and_then(|v| v.as_object_mut()).unwrap()
}

fn addh(days: &mut Map<String, Value>, d: &str, h: i64, col: usize, v: &Num) {
    let rec = day_rec(days, d);
    let hours = rec.get_mut("hours").and_then(|x| x.as_array_mut());
    let Some(hours) = hours else { return };
    let hi = h.clamp(0, 23) as usize;
    if hi >= hours.len() {
        return;
    }
    if let Some(row) = hours[hi].as_array_mut() {
        if col < row.len() {
            add_cell(&mut row[col], v);
        }
    }
}

fn agent_of<'a>(rec: &'a mut Map<String, Value>, name: &str) -> &'a mut Map<String, Value> {
    let agents = obj_mut(rec, "agents");
    if !agents.get(name).map(|v| v.is_object()).unwrap_or(false) {
        agents.insert(
            name.to_string(),
            json!({"prompts": 0, "reqs": 0, "secs": 0.0, "tin": 0, "tout": 0, "chars": 0}),
        );
    }
    agents.get_mut(name).and_then(|v| v.as_object_mut()).unwrap()
}

fn connector_names(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| {
                let nm = x
                    .as_object()
                    .map(|o| str_or(o.get("name")))
                    .unwrap_or_default();
                nm
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// `_fold`：把每轮的账按天折好。
pub fn fold(state: &State, since_day: &str, lib_dir: &Path) -> Map<String, Value> {
    let mut days: Map<String, Value> = Map::new();

    for (_, f) in state.files.iter() {
        let Some(f) = f else { continue };
        for (_, r) in f.reqs.iter() {
            let (t0, t1, tin, tout, calls) = (r.t0, r.t1, r.tin, r.tout, r.calls);
            let model = r.model.clone();
            let expert = r.expert.clone();
            let mcp = r.mcp.clone();
            if t0 == 0 {
                continue;
            }
            let d = day_of(t0);
            if d.is_empty() || d.as_str() < since_day {
                continue;
            }
            let dur = (t1 - t0).clamp(0, TURN_CAP_MS) as f64 / 1000.0;
            {
                let rw = day_rec(&mut days, &d);
                bump(rw, "reqs", 1);
                bump(rw, "tin", tin);
                bump(rw, "tout", tout);
                bump(rw, "tok", tin + tout);
            }
            {
                let rec = day_rec(&mut days, &d);
                let a = agent_of(rec, "WorkBuddy");
                bump(a, "reqs", 1);
                bump(a, "tin", tin);
                bump(a, "tout", tout);
                let cur = get_f(a, "secs");
                set_f(a, "secs", cur + dur);
            }
            if !model.is_empty() {
                let mk = model.trim().to_lowercase();
                let rw = day_rec(&mut days, &d);
                let models = obj_mut(rw, "models");
                if !models.get(&mk).map(|v| v.is_object()).unwrap_or(false) {
                    models.insert(
                        mk.clone(),
                        json!({"name": model.trim(), "calls": 0, "tin": 0, "tout": 0}),
                    );
                }
                if let Some(Value::Object(m)) = models.get_mut(&mk) {
                    bump(m, "calls", if calls == 0 { 1 } else { calls });
                    bump(m, "tin", tin);
                    bump(m, "tout", tout);
                }
            }
            if !expert.is_empty() {
                let rw = day_rec(&mut days, &d);
                let ex = obj_mut(rw, "experts");
                if !ex.get(&expert).map(|v| v.is_object()).unwrap_or(false) {
                    ex.insert(
                        expert.clone(),
                        json!({"name": expert, "reqs": 0, "secs": 0.0, "tok": 0}),
                    );
                }
                if let Some(Value::Object(e)) = ex.get_mut(&expert) {
                    bump(e, "reqs", 1);
                    let cur = get_f(e, "secs");
                    set_f(e, "secs", cur + dur);
                    bump(e, "tok", tin + tout);
                }
            }
            for srv in mcp.split(',').filter(|x| !x.is_empty()).take(4) {
                let rw = day_rec(&mut days, &d);
                let mc = obj_mut(rw, "mcps");
                if !mc.get(srv).map(|v| v.is_object()).unwrap_or(false) {
                    mc.insert(srv.to_string(), json!({"name": srv, "reqs": 0, "secs": 0.0, "tok": 0}));
                }
                if let Some(Value::Object(q)) = mc.get_mut(srv) {
                    bump(q, "reqs", 1);
                    let cur = get_f(q, "secs");
                    set_f(q, "secs", cur + dur);
                    bump(q, "tok", tin + tout);
                }
            }
            if t1 > t0 && (t1 - t0) > 3_600_000 {
                let span = (t1 - t0) as f64 / 1000.0;
                let mut cur = t0 as f64;
                while cur < t1 as f64 {
                    let Some(hs) = hour_start_ms(cur as i64) else { break };
                    let nxt = hs as f64 + 3_600_000.0;
                    let seg = (t1 as f64).min(nxt);
                    let frac = ((seg - cur).max(0.0)) / (span * 1000.0);
                    let hh = hour_index(cur as i64);
                    addh(&mut days, &d, hh, C_TOK, &Num::I(((tin + tout) as f64 * frac) as i64));
                    addh(&mut days, &d, hh, C_TIN, &Num::I((tin as f64 * frac) as i64));
                    addh(&mut days, &d, hh, C_TOUT, &Num::I((tout as f64 * frac) as i64));
                    cur = seg;
                }
            } else {
                let hh = hour_index(t0);
                addh(&mut days, &d, hh, C_TOK, &Num::I(tin + tout));
                addh(&mut days, &d, hh, C_TIN, &Num::I(tin));
                addh(&mut days, &d, hh, C_TOUT, &Num::I(tout));
            }
        }
    }

    // 提示词桶（WorkBuddy）
    for (_, f) in state.files.iter() {
        let Some(f) = f else { continue };
        for (key, b) in f.ph.iter() {
            let d: String = key.chars().take(10).collect();
            if d.as_str() < since_day {
                continue;
            }
            let h: i64 = key
                .chars()
                .skip(11)
                .take(2)
                .collect::<String>()
                .parse()
                .unwrap_or(0);
            let bi = |i: usize| -> i64 { b.get(i).copied().unwrap_or(0) };
            {
                let rw = day_rec(&mut days, &d);
                bump(rw, "prompts", bi(0));
                bump(rw, "chars", bi(1));
                bump(rw, "ptok", if b.len() > B_PTOK { bi(B_PTOK) } else { 0 });
                bump(rw, "links", bi(2));
                bump(rw, "imgs", bi(3));
            }
            addh(&mut days, &d, h, C_PROMPTS, &Num::I(bi(0)));
            addh(&mut days, &d, h, C_CHARS, &Num::I(bi(1)));
            addh(&mut days, &d, h, C_LINKS, &Num::I(bi(2)));
            addh(&mut days, &d, h, C_IMGS, &Num::I(bi(3)));
            {
                let rec = day_rec(&mut days, &d);
                let a = agent_of(rec, "WorkBuddy");
                bump(a, "prompts", bi(0));
                bump(a, "chars", bi(1));
                let rw = day_rec(&mut days, &d);
                let hist = arr_mut(rw, "hist");
                for i in 0..(LENS.len() + 1) {
                    if B_HIST + i < b.len() {
                        if i < hist.len() {
                            let cur = py_int(&hist[i]);
                            hist[i] = json!(cur + bi(B_HIST + i));
                        }
                    }
                }
            }
        }
    }

    // 别家 agent：只有轮次（和偶尔有的耗时），没有 token 流水
    let mut am = state.am.clone();
    let since_ms = midnight_ms(since_day);
    for it in other_agents(lib_dir, since_ms) {
        let obj = match it.as_object() {
            Some(o) => o,
            None => continue,
        };
        let t0 = py_int(obj.get("t0").unwrap_or(&Value::Null));
        if t0 == 0 {
            continue;
        }
        let d = day_of(t0);
        if d.is_empty() || d.as_str() < since_day {
            continue;
        }
        let client = {
            let c = str_or(obj.get("client"));
            if c.is_empty() { "其他".to_string() } else { c }
        };
        let tok = obj.get("tokens").cloned().unwrap_or(json!({}));
        let tin = py_int(tok.get("in").unwrap_or(&Value::Null));
        let tout = py_int(tok.get("out").unwrap_or(&Value::Null));
        let secs = crate::hostwork::util::py_double(obj.get("secs").unwrap_or(&Value::Null));
        {
            let rw = day_rec(&mut days, &d);
            bump(rw, "prompts", 1);
            bump(rw, "reqs", 1);
            bump(rw, "tin", tin);
            bump(rw, "tout", tout);
            bump(rw, "tok", tin + tout);
        }
        {
            let rec = day_rec(&mut days, &d);
            let a = agent_of(rec, &client);
            bump(a, "prompts", 1);
            bump(a, "reqs", 1);
            let cur = get_f(a, "secs");
            set_f(a, "secs", cur + secs);
            bump(a, "tin", tin);
            bump(a, "tout", tout);
        }
        let hh = hour_index(t0);
        addh(&mut days, &d, hh, C_PROMPTS, &Num::I(1));
        addh(&mut days, &d, hh, C_TOK, &Num::I(tin + tout));
        addh(&mut days, &d, hh, C_TIN, &Num::I(tin));
        addh(&mut days, &d, hh, C_TOUT, &Num::I(tout));
        mark(&mut am, t0);
        if secs > 0.0 {
            mark(&mut am, t0 + (secs * 1000.0) as i64);
        }
        let f = obj.get("facets").cloned().unwrap_or(json!({}));
        if truthy(f.get("expert").unwrap_or(&Value::Null)) {
            let name = str_or(f.get("expert"));
            let rw = day_rec(&mut days, &d);
            let ex = obj_mut(rw, "experts");
            if !ex.get(&name).map(|v| v.is_object()).unwrap_or(false) {
                ex.insert(name.clone(), json!({"name": name, "reqs": 0, "secs": 0.0, "tok": 0}));
            }
            if let Some(Value::Object(e)) = ex.get_mut(&name) {
                bump(e, "reqs", 1);
                let cur = get_f(e, "secs");
                set_f(e, "secs", cur + secs);
                bump(e, "tok", tin + tout);
            }
        }
        for c in connector_names(f.get("connectors")) {
            if c.is_empty() {
                continue;
            }
            let rw = day_rec(&mut days, &d);
            let mc = obj_mut(rw, "mcps");
            if !mc.get(&c).map(|v| v.is_object()).unwrap_or(false) {
                mc.insert(c.clone(), json!({"name": c, "reqs": 0, "secs": 0.0, "tok": 0}));
            }
            if let Some(Value::Object(q)) = mc.get_mut(&c) {
                bump(q, "reqs", 1);
                let cur = get_f(q, "secs");
                set_f(q, "secs", cur + secs);
                bump(q, "tok", tin + tout);
            }
        }
    }

    // AI 时长：分钟格的并集
    for (key, hx) in am.iter() {
        let d: String = key.chars().take(10).collect();
        if d.as_str() < since_day {
            continue;
        }
        let secs = bits_secs(hx);
        if secs <= 0.0 {
            continue;
        }
        {
            let rec = day_rec(&mut days, &d);
            let cur = get_f(rec, "ai");
            set_f(rec, "ai", cur + secs);
        }
        let h: i64 = key
            .chars()
            .skip(11)
            .take(2)
            .collect::<String>()
            .parse()
            .unwrap_or(0);
        addh(&mut days, &d, h, C_AI, &Num::F(secs));
    }

    // 专家名
    let mut label_of: Map<String, Value> = Map::new();
    let mut ids: Vec<String> = Vec::new();
    for (_, rec) in days.iter() {
        if let Some(Value::Object(ex)) = rec.get("experts") {
            for k in ex.keys() {
                if !ids.contains(k) {
                    ids.push(k.clone());
                }
            }
        }
    }
    for key in ids {
        let nm = hs::expert_name(&key);
        let label = if !nm.is_empty() {
            nm
        } else if key.starts_with("ex_") {
            key.chars().rev().take(6).collect::<Vec<_>>().into_iter().rev().collect()
        } else {
            key.clone()
        };
        label_of.insert(key, Value::String(label));
    }
    for (_, rec) in days.iter_mut() {
        if let Some(Value::Object(ex)) = rec.get_mut("experts") {
            for (k, e) in ex.iter_mut() {
                if let Some(l) = label_of.get(k) {
                    if let Some(o) = e.as_object_mut() {
                        o.insert("label".into(), l.clone());
                    }
                }
            }
        }
    }

    days
}

fn midnight_ms(day: &str) -> i64 {
    match NaiveDate::parse_from_str(day, "%Y-%m-%d") {
        Ok(d) => d
            .and_hms_opt(0, 0, 0)
            .and_then(|n| Local.from_local_datetime(&n).single())
            .map(|t| t.timestamp_millis())
            .unwrap_or(0),
        Err(_) => 0,
    }
}

/// `_other_agents`：TRAE / Cherry / Claude Code 的轮次（走 dash 的解析层）。
/// 顺序照 Python 的循环：先 TRAE，再 Cherry，最后 Claude。
fn other_agents(lib_dir: &Path, since_ms: i64) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    // TRAE 走的是 dash 的 `_trae_items`（不是 trae_work 那份）：不按 message_id
    // 去重、一行记忆一条，字段只有 t0 / client / secs / tokens / facets。
    out.extend(trae_mem_items(lib_dir, since_ms as f64));
    out.extend(hs::cherry_items(since_ms as f64));
    for it in hs::claude_items(since_ms as f64) {
        out.push(fill_t0(it));
    }
    out
}

/// `dash._trae_ts`：'2026-11-23 02:12:06' → 毫秒（本地时区）。
fn trae_ts(v: &Value) -> f64 {
    let raw = str_or(Some(v));
    let head: String = raw.chars().take(19).collect();
    match chrono::NaiveDateTime::parse_from_str(&head, "%Y-%m-%d %H:%M:%S") {
        Ok(d) => d
            .and_local_timezone(Local)
            .single()
            .map(|t| t.timestamp_millis() as f64)
            .unwrap_or(0.0),
        Err(_) => 0.0,
    }
}

/// `dash._trae_items` 的会话记忆那一段（workstat 只要 t0）。
fn trae_mem_items(lib_dir: &Path, since_ms: f64) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let home = home();
    for sub in [".trae-cn", ".trae"] {
        let root = home.join(sub).join("memory").join("projects");
        if !root.is_dir() {
            continue;
        }
        let rd = match std::fs::read_dir(&root) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for e1 in rd.flatten() {
            let d1 = e1.path();
            if !d1.is_dir() {
                continue;
            }
            let rd2 = match std::fs::read_dir(&d1) {
                Ok(r) => r,
                Err(_) => continue,
            };
            for e2 in rd2.flatten() {
                let d2 = e2.path();
                if !d2.is_dir() {
                    continue;
                }
                let rd3 = match std::fs::read_dir(&d2) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                for e3 in rd3.flatten() {
                    let p = e3.path();
                    let nm = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                    if !nm.starts_with("session_memory_") || !nm.ends_with(".jsonl") {
                        continue;
                    }
                    // Python 的 glob 不匹配点开头的名字
                    if nm.starts_with('.') {
                        continue;
                    }
                    let mt = std::fs::metadata(&p)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|x| x.as_secs_f64() * 1000.0)
                        .unwrap_or(0.0);
                    if mt < since_ms {
                        continue;
                    }
                    let text = match std::fs::read(&p) {
                        Ok(b) => String::from_utf8_lossy(&b).to_string(),
                        Err(_) => continue,
                    };
                    for line in text.split('\n') {
                        let line = line.trim();
                        if line.is_empty() {
                            continue;
                        }
                        let o: Value = match loads(line) {
                            Some(v) => v,
                            None => continue,
                        };
                        let obj = match o.as_object() {
                            Some(x) => x,
                            None => continue,
                        };
                        let ts = trae_ts(obj.get("message_summary_time").unwrap_or(&Value::Null));
                        if ts == 0.0 || ts < since_ms {
                            continue;
                        }
                        let intent = str_or(obj.get("intent"))
                            .split_whitespace()
                            .collect::<Vec<_>>()
                            .join(" ");
                        let acts: Vec<String> = match obj.get("actions") {
                            Some(Value::Array(a)) => a
                                .iter()
                                .filter(|x| truthy(x))
                                .map(|x| {
                                    crate::hostwork::util::py_str(x)
                                        .split_whitespace()
                                        .collect::<Vec<_>>()
                                        .join(" ")
                                })
                                .collect(),
                            _ => Vec::new(),
                        };
                        if intent.is_empty() && acts.is_empty() {
                            continue;
                        }
                        out.push(json!({
                            "t0": ts as i64,
                            "client": "TRAE",
                            "secs": 0,
                            "tokens": {"in": 0, "out": 0, "cached": 0, "total": 0},
                            "facets": {},
                        }));
                    }
                }
            }
        }
    }
    // 自述（Agent自述 / TRAE自述）：-本机工作- 下的两份，按 agent 分组
    for (name, fallback) in [
        ("Agent自述.jsonl", "未知"),
        ("TRAE自述.jsonl", "TRAE"),
    ] {
        let p = lib_dir.join("_本机工作").join(name);
        out.extend(hs::self_items_from(&p, since_ms, fallback));
    }
    out
}

fn fill_t0(mut it: Value) -> Value {
    if let Some(o) = it.as_object_mut() {
        if py_int(o.get("t0").unwrap_or(&Value::Null)) == 0 {
            let start = str_or(o.get("start"));
            if let Some(t) = local_ms(&start) {
                o.insert("t0".into(), json!(t));
            }
        }
    }
    it
}

fn local_ms(s: &str) -> Option<i64> {
    let head: String = s.chars().take(19).collect();
    let d = chrono::NaiveDateTime::parse_from_str(&head, "%Y-%m-%d %H:%M:%S").ok()?;
    Some(d.and_local_timezone(Local).single()?.timestamp_millis())
}

// ---------------------------------------------------------------- 屏幕并账
fn with_screen(days_map: &mut Map<String, Value>, sc: &Value, since_day: &str) {
    if !truthy(sc.get("ok").unwrap_or(&Value::Null)) {
        return;
    }
    let names = sc.get("names").and_then(|v| v.as_object()).cloned().unwrap_or_default();
    if let Some(Value::Object(sd)) = sc.get("days") {
        for (d, row) in sd {
            if d.is_empty() || d.as_str() < since_day {
                continue;
            }
            let total = crate::hostwork::util::py_double(row.get("total").unwrap_or(&Value::Null));
            let rec = day_rec(days_map, d);
            set_f(rec, "human", py_round1(total));
            rec.insert("has".into(), json!(1));
            let mut apps: Map<String, Value> = Map::new();
            if let Some(Value::Object(a)) = row.get("apps") {
                for (b, secs) in a {
                    let nm = names
                        .get(b)
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                        .or_else(|| {
                            screen::NAME_FIX
                                .iter()
                                .find(|(k, _)| k == b)
                                .map(|(_, v)| v.to_string())
                        })
                        .unwrap_or_else(|| b.clone());
                    let cur = crate::hostwork::util::py_double(apps.get(&nm).unwrap_or(&Value::Null));
                    apps.insert(
                        nm,
                        json!(cur + crate::hostwork::util::py_double(secs)),
                    );
                }
            }
            let mut ap = Map::new();
            for (k, v) in apps.iter() {
                ap.insert(k.clone(), json!(py_round1(crate::hostwork::util::py_double(v))));
            }
            rec.insert("apps".into(), Value::Object(ap));
        }
    }
    if let Some(Value::Object(sh)) = sc.get("hours") {
        for (d, hs) in sh {
            if d.is_empty() || d.as_str() < since_day {
                continue;
            }
            let rec = day_rec(days_map, d);
            let arr = hs.as_array().cloned().unwrap_or_default();
            if let Some(Value::Array(hours)) = rec.get_mut("hours") {
                for h in 0..24 {
                    let v = arr
                        .get(h)
                        .map(|x| crate::hostwork::util::py_double(x))
                        .unwrap_or(0.0);
                    if let Some(row) = hours.get_mut(h).and_then(|x| x.as_array_mut()) {
                        if row.len() > C_HUMAN {
                            row[C_HUMAN] = json!(py_round1(v));
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------- 热力图
fn heat(days_map: &Map<String, Value>, weeks: i64) -> Value {
    let today = today();
    let start = today - chrono::Duration::days(today.weekday().num_days_from_monday() as i64)
        - chrono::Duration::days((weeks - 1) * 7);
    let mut cells: Vec<Value> = Vec::new();
    let mut cur = start;
    while cur <= today {
        let ds = ymd(cur);
        let r = days_map.get(&ds).and_then(|v| v.as_object()).cloned().unwrap_or_default();
        cells.push(json!({
            "d": ds,
            "ai": py_round1(get_f(&r, "ai")),
            "human": py_round1(get_f(&r, "human")),
            "has": if truthy(r.get("has").unwrap_or(&Value::Null)) { 1 } else { 0 },
            "tok": get_i(&r, "tok"),
            "prompts": get_i(&r, "prompts"),
        }));
        cur += chrono::Duration::days(1);
    }
    json!({
        "from": ymd(start),
        "to": ymd(today),
        "cells": cells,
        "weeks": weeks,
    })
}

// ---------------------------------------------------------------- 永久存档
struct Arch {
    days: Map<String, Value>,
    at: f64,
    loaded: bool,
}

static ARCH: Mutex<Option<Arch>> = Mutex::new(None);

fn max_merge(a: &Value, b: &Value) -> Value {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            let mut out = x.clone();
            for (k, v) in y.iter() {
                let merged = match out.get(k) {
                    Some(old) => max_merge(old, v),
                    None => v.clone(),
                };
                out.insert(k.clone(), merged);
            }
            Value::Object(out)
        }
        (Value::Array(x), Value::Array(y)) => {
            let n = x.len().max(y.len());
            let mut out: Vec<Value> = Vec::with_capacity(n);
            for i in 0..n {
                let item = match (x.get(i), y.get(i)) {
                    (None, Some(yy)) => yy.clone(),
                    (Some(xx), None) => xx.clone(),
                    (Some(xx), Some(yy)) => max_merge(xx, yy),
                    (None, None) => Value::Null,
                };
                out.push(item);
            }
            Value::Array(out)
        }
        (Value::Bool(_), _) | (_, Value::Bool(_)) => {
            Value::Bool(truthy(a) || truthy(b))
        }
        (Value::Number(x), Value::Number(y)) => {
            if x.as_i64().is_some() && y.as_i64().is_some() {
                let ax = x.as_i64().unwrap_or(0);
                let ay = y.as_i64().unwrap_or(0);
                if ax >= ay { a.clone() } else { b.clone() }
            } else {
                let ax = x.as_f64().unwrap_or(0.0);
                let ay = y.as_f64().unwrap_or(0.0);
                if ax >= ay { a.clone() } else { b.clone() }
            }
        }
        _ => {
            if !truthy(b) && matches!(b, Value::Null | Value::String(_)) {
                a.clone()
            } else if !truthy(a) && matches!(a, Value::Null | Value::String(_)) {
                b.clone()
            } else {
                b.clone()
            }
        }
    }
}

fn load_archive(ctx: &Ctx) -> Map<String, Value> {
    let mut g = ARCH.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(a) = g.as_ref() {
        if a.loaded {
            return a.days.clone();
        }
    }
    let mut days: Map<String, Value> = Map::new();
    let mut at = 0.0;
    if let Some(raw) = read_json_py(&archive_path(ctx)) {
        if let Some(Value::Object(d)) = raw.get("days") {
            days = d.clone();
        }
        at = crate::hostwork::util::py_double(raw.get("at").unwrap_or(&Value::Null));
    }
    *g = Some(Arch { days: days.clone(), at, loaded: true });
    days
}

fn write_archive(ctx: &Ctx, days: &Map<String, Value>) {
    let mut payload = Map::new();
    payload.insert("v".into(), json!(1));
    payload.insert("at".into(), json!(now_secs()));
    payload.insert(
        "updated".into(),
        json!(Local::now().format("%Y-%m-%d %H:%M").to_string()),
    );
    payload.insert("cols".into(), json!(HOUR_COLS));
    payload.insert("day_cols".into(), json!(["ai", "human", "tok", "tin", "tout", "prompts", "chars", "links", "imgs", "reqs", "ptok"]));
    payload.insert(
        "note".into(),
        json!("每天一条，只增不减；workstat.py --rebuild 才按源重建"),
    );
    payload.insert("days".into(), Value::Object(days.clone()));
    let text = dumps_sorted(&Value::Object(payload));
    let _ = crate::jsonio::atomic_write(&archive_path(ctx), &text);
}

fn absorb(ctx: &Ctx, new_days: &Map<String, Value>, force: bool) -> Map<String, Value> {
    let mut days = load_archive(ctx);
    if new_days.is_empty() && !force {
        return days;
    }
    let mut g = ARCH.lock().unwrap_or_else(|e| e.into_inner());
    let arch = g.get_or_insert_with(|| Arch { days: Map::new(), at: 0.0, loaded: true });
    if arch.loaded && arch.days.is_empty() && !days.is_empty() {
        arch.days = days.clone();
    }
    for (d, rec) in new_days.iter() {
        let merged = match arch.days.get(d) {
            Some(old) if !force => max_merge(old, rec),
            _ => rec.clone(),
        };
        arch.days.insert(d.clone(), merged);
    }
    let now = now_secs();
    if force || now - arch.at >= ARCHIVE_SAVE_S {
        write_archive(ctx, &arch.days);
        arch.at = now;
    }
    days = arch.days.clone();
    days
}

// ---------------------------------------------------------------- view
fn row_out(rec: &Map<String, Value>, d: &str) -> Value {
    let mut r = Map::new();
    r.insert("d".into(), Value::String(d.to_string()));
    r.insert("ai".into(), json!(py_round1(get_f(rec, "ai"))));
    r.insert("human".into(), json!(py_round1(get_f(rec, "human"))));
    for k in ["tok", "tin", "tout", "prompts", "chars", "links", "imgs", "reqs", "ptok"] {
        r.insert(k.into(), json!(get_i(rec, k)));
    }
    Value::Object(r)
}

fn norm_day(s: &str) -> String {
    let t = s.trim();
    let ok = t.len() == 10
        && t.as_bytes()[4] == b'-'
        && t.as_bytes()[7] == b'-'
        && t.chars().enumerate().all(|(i, c)| {
            if i == 4 || i == 7 {
                true
            } else {
                c.is_ascii_digit()
            }
        });
    if ok && NaiveDate::parse_from_str(t, "%Y-%m-%d").is_ok() {
        t.to_string()
    } else {
        String::new()
    }
}

/// 统计模块没装上时的降级形状（`workstatmod is None` 那条分支）。
/// Rust 侧模块永远在，保留这个形状是为了让「契约」在这份代码里也有出处。
pub fn module_missing() -> Value {
    json!({
        "ok": false,
        "error": "统计模块没装上（workstat 未加载）",
        "rows": [], "hours": [], "agents": [], "models": [],
        "experts": [], "mcps": [], "heat": {"cells": []},
        "screen": {"ok": false, "why": "统计模块没装上"},
    })
}

pub fn error_shape(err: &str) -> Value {
    json!({
        "ok": false,
        "error": err,
        "rows": [], "hours": [], "agents": [], "models": [],
        "experts": [], "mcps": [], "heat": {"cells": []},
        "screen": {"ok": false, "why": "统计层出错了"},
    })
}

pub fn view(ctx: &Ctx, days_in: i64, day_in: &str, fresh: bool, rebuild: bool) -> Value {
    let d0 = if days_in == 0 { 30 } else { days_in };
    let days = d0.clamp(7, 180);
    let today = today();
    let today_s = ymd(today);
    let mut day = norm_day(day_in);
    if day.is_empty() {
        day = today_s.clone();
    }
    if day > today_s {
        day = today_s.clone();
    }

    screen::start_sampler(&ctx.library_dir);

    let fresh_ok = {
        let g = SCACHE.lock().unwrap_or_else(|e| e.into_inner());
        match g.as_ref() {
            Some(c) => now_secs() - c.at < SCAN_TTL,
            None => false,
        }
    };
    if fresh || !fresh_ok {
        let t0 = now_secs();
        let st = scan(ctx, false);
        save_cache(ctx, &st);
        let took = now_secs() - t0;
        let mut g = SCACHE.lock().unwrap_or_else(|e| e.into_inner());
        *g = Some(ScanCache { data: Arc::new(st), at: now_secs(), took });
    }
    let state = {
        let g = SCACHE.lock().unwrap_or_else(|e| e.into_inner());
        match g.as_ref() {
            Some(c) => c.data.clone(),
            None => Arc::new(State::default()),
        }
    };

    let heat_weeks = 26i64;
    let heat_from = ymd(
        today - chrono::Duration::days(today.weekday().num_days_from_monday() as i64)
            - chrono::Duration::days((heat_weeks - 1) * 7),
    );
    let win_from = ymd(today - chrono::Duration::days(days - 1));
    let since_day = if win_from < heat_from { win_from.clone() } else { heat_from.clone() };
    let since_date = NaiveDate::parse_from_str(&since_day, "%Y-%m-%d").unwrap_or(today);
    let screen_days = days.max((today - since_date).num_days() + 1);

    let mut bundle = fold(&state, &since_day, &ctx.runtime_dir);
    let sc = screen::screen(&ctx.library_dir, screen_days);
    with_screen(&mut bundle, &sc, &since_day);
    let days_map = absorb(ctx, &bundle, rebuild);
    let ht = heat(&days_map, heat_weeks);

    // 明细行
    let mut rows: Vec<Value> = Vec::new();
    for i in (0..days).rev() {
        let ds = ymd(today - chrono::Duration::days(i));
        let rec = days_map
            .get(&ds)
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        rows.push(row_out(&rec, &ds));
    }
    let sum_i = |k: &str| -> i64 {
        rows.iter().map(|r| get_i(r.as_object().unwrap_or(&Map::new()), k)).sum()
    };
    let sum_f = |k: &str| -> f64 {
        rows.iter().map(|r| get_f(r.as_object().unwrap_or(&Map::new()), k)).sum()
    };
    let mut tot = Map::new();
    tot.insert("ai".into(), json!(py_round1(sum_f("ai"))));
    tot.insert("human".into(), json!(py_round1(sum_f("human"))));
    for k in ["tok", "tin", "tout", "prompts", "chars", "links", "imgs", "reqs", "ptok"] {
        tot.insert(k.into(), json!(sum_i(k)));
    }
    let tot_i = |k: &str| get_i(&tot, k);
    let tot_f = |k: &str| get_f(&tot, k);
    let active_days = rows
        .iter()
        .filter(|r| {
            let o = r.as_object().unwrap();
            get_i(o, "prompts") != 0 || get_i(o, "reqs") != 0
        })
        .count() as i64;
    let human_days = rows
        .iter()
        .filter(|r| {
            let d = str_or(r.get("d"));
            days_map
                .get(&d)
                .and_then(|v| v.as_object())
                .map(|o| truthy(o.get("has").unwrap_or(&Value::Null)))
                .unwrap_or(false)
        })
        .count() as i64;

    // 分维度：窗口内那几天相加
    let mut ag_t: OrdMap<String, Map<String, Value>> = OrdMap::new();
    let mut md_t: OrdMap<String, Map<String, Value>> = OrdMap::new();
    let mut ex_t: OrdMap<String, Map<String, Value>> = OrdMap::new();
    let mut mc_t: OrdMap<String, Map<String, Value>> = OrdMap::new();
    let mut hist: Vec<i64> = vec![0; LENS.len() + 1];
    let mut dkeys: Vec<String> = days_map.keys().cloned().collect();
    dkeys.sort();
    for d in dkeys {
        if d.as_str() < win_from.as_str() {
            continue;
        }
        let rec = match days_map.get(&d).and_then(|v| v.as_object()) {
            Some(r) => r,
            None => continue,
        };
        if let Some(Value::Object(agents)) = rec.get("agents") {
            for (nm, a) in agents {
                let ao = a.as_object().cloned().unwrap_or_default();
                let t = ag_t.or_insert_with(nm.clone(), || {
                    let mut m = Map::new();
                    m.insert("name".into(), Value::String(nm.clone()));
                    m.insert("prompts".into(), json!(0));
                    m.insert("reqs".into(), json!(0));
                    m.insert("secs".into(), json!(0.0));
                    m.insert("tin".into(), json!(0));
                    m.insert("tout".into(), json!(0));
                    m.insert("chars".into(), json!(0));
                    m
                });
                for k in ["prompts", "reqs", "tin", "tout", "chars"] {
                    bump(t, k, get_i(&ao, k));
                }
                let cur = get_f(t, "secs");
                set_f(t, "secs", cur + get_f(&ao, "secs"));
            }
        }
        if let Some(Value::Object(models)) = rec.get("models") {
            for (mk, m) in models {
                let mo = m.as_object().cloned().unwrap_or_default();
                let t = md_t.or_insert_with(mk.clone(), || {
                    let mut x = Map::new();
                    x.insert("name".into(), Value::String(mk.clone()));
                    x.insert("calls".into(), json!(0));
                    x.insert("tin".into(), json!(0));
                    x.insert("tout".into(), json!(0));
                    x
                });
                if str_or(t.get("name")) == *mk && truthy(mo.get("name").unwrap_or(&Value::Null)) {
                    t.insert("name".into(), Value::String(str_or(mo.get("name"))));
                }
                for k in ["calls", "tin", "tout"] {
                    bump(t, k, get_i(&mo, k));
                }
            }
        }
        if let Some(Value::Object(ex)) = rec.get("experts") {
            for (ek, e) in ex {
                let eo = e.as_object().cloned().unwrap_or_default();
                let t = ex_t.or_insert_with(ek.clone(), || {
                    let mut x = Map::new();
                    let nm = str_or(eo.get("name"));
                    let lb = str_or(eo.get("label"));
                    x.insert("name".into(), Value::String(if nm.is_empty() { ek.clone() } else { nm }));
                    x.insert("label".into(), Value::String(if lb.is_empty() { ek.clone() } else { lb }));
                    x.insert("reqs".into(), json!(0));
                    x.insert("secs".into(), json!(0.0));
                    x.insert("tok".into(), json!(0));
                    x
                });
                bump(t, "reqs", get_i(&eo, "reqs"));
                let cur = get_f(t, "secs");
                set_f(t, "secs", cur + get_f(&eo, "secs"));
                bump(t, "tok", get_i(&eo, "tok"));
            }
        }
        if let Some(Value::Object(mc)) = rec.get("mcps") {
            for (qk, q) in mc {
                let qo = q.as_object().cloned().unwrap_or_default();
                let t = mc_t.or_insert_with(qk.clone(), || {
                    let mut x = Map::new();
                    x.insert("name".into(), Value::String(qk.clone()));
                    x.insert("reqs".into(), json!(0));
                    x.insert("secs".into(), json!(0.0));
                    x.insert("tok".into(), json!(0));
                    x
                });
                bump(t, "reqs", get_i(&qo, "reqs"));
                let cur = get_f(t, "secs");
                set_f(t, "secs", cur + get_f(&qo, "secs"));
                bump(t, "tok", get_i(&qo, "tok"));
            }
        }
        if let Some(Value::Array(h)) = rec.get("hist") {
            for i in 0..hist.len() {
                if i < h.len() {
                    hist[i] += py_int(&h[i]);
                }
            }
        }
    }

    // agents
    let mut agents: Vec<Value> = Vec::new();
    for (_, a) in ag_t.iter() {
        let reqs = get_i(a, "reqs");
        let prompts = get_i(a, "prompts");
        let tin = get_i(a, "tin");
        let tout = get_i(a, "tout");
        let secs = get_f(a, "secs");
        let chars = get_i(a, "chars");
        let n = reqs.max(1) as f64;
        let avg_sec = if reqs != 0 {
            json!(py_round1(secs / n))
        } else {
            json!(0)
        };
        let mut m = Map::new();
        m.insert("name".into(), Value::String(str_or(a.get("name"))));
        m.insert("prompts".into(), json!(prompts));
        m.insert("reqs".into(), json!(reqs));
        m.insert("secs".into(), json!(py_round1(secs)));
        m.insert("tin".into(), json!(tin));
        m.insert("tout".into(), json!(tout));
        m.insert("tok".into(), json!(tin + tout));
        m.insert("avg_sec".into(), avg_sec);
        m.insert(
            "avg_tok".into(),
            json!(py_round1((tin + tout) as f64 / ((prompts).max(reqs)).max(1) as f64)),
        );
        m.insert("avg_chars".into(), json!(py_round1(chars as f64 / prompts.max(1) as f64)));
        agents.push(Value::Object(m));
    }
    agents.sort_by(|a, b| {
        let ka = get_i(a.as_object().unwrap(), "tok") + get_i(a.as_object().unwrap(), "reqs") * 1000;
        let kb = get_i(b.as_object().unwrap(), "tok") + get_i(b.as_object().unwrap(), "reqs") * 1000;
        kb.cmp(&ka)
    });

    // models
    let mut models: Vec<Value> = Vec::new();
    let mut mvals: Vec<(String, Map<String, Value>)> =
        md_t.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    mvals.sort_by(|a, b| {
        let ka = get_i(&a.1, "tin") + get_i(&a.1, "tout");
        let kb = get_i(&b.1, "tin") + get_i(&b.1, "tout");
        kb.cmp(&ka)
    });
    for (_, mut m) in mvals {
        let tin = get_i(&m, "tin");
        let tout = get_i(&m, "tout");
        let calls = get_i(&m, "calls");
        m.insert("tok".into(), json!(tin + tout));
        m.insert("avg_tok".into(), json!(py_round1((tin + tout) as f64 / calls.max(1) as f64)));
        models.push(Value::Object(m));
    }

    // experts
    let mut evals: Vec<Map<String, Value>> = ex_t.iter().map(|(_, v)| v.clone()).collect();
    evals.sort_by(|a, b| get_i(b, "reqs").cmp(&get_i(a, "reqs")));
    let mut experts: Vec<Value> = Vec::new();
    for mut e in evals.into_iter().take(12) {
        let reqs = get_i(&e, "reqs");
        let secs = py_round1(get_f(&e, "secs"));
        let tok = get_i(&e, "tok");
        e.insert("secs".into(), json!(secs));
        e.insert("tok".into(), json!(tok));
        e.insert("avg_sec".into(), json!(py_round1(secs / reqs.max(1) as f64)));
        e.insert("avg_tok".into(), json!(py_round1(tok as f64 / reqs.max(1) as f64)));
        experts.push(Value::Object(e));
    }

    // mcps
    let mut qvals: Vec<Map<String, Value>> = mc_t.iter().map(|(_, v)| v.clone()).collect();
    qvals.sort_by(|a, b| get_i(b, "reqs").cmp(&get_i(a, "reqs")));
    let mut mcps: Vec<Value> = Vec::new();
    for mut q in qvals.into_iter().take(12) {
        let reqs = get_i(&q, "reqs");
        let secs = py_round1(get_f(&q, "secs"));
        let tok = get_i(&q, "tok");
        q.insert("secs".into(), json!(secs));
        q.insert("tok".into(), json!(tok));
        q.insert("avg_sec".into(), json!(py_round1(secs / reqs.max(1) as f64)));
        q.insert("avg_tok".into(), json!(py_round1(tok as f64 / reqs.max(1) as f64)));
        mcps.push(Value::Object(q));
    }

    let turn_secs: f64 = ag_t.iter().map(|(_, a)| get_f(a, "secs")).sum();

    // 当天 24 小时桶
    let drec = days_map.get(&day).and_then(|v| v.as_object()).cloned().unwrap_or_default();
    let hs_day = drec
        .get("hours")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_else(|| (0..24).map(|_| json!([0, 0, 0, 0, 0, 0, 0, 0, 0])).collect());
    let mut hours: Vec<Value> = Vec::new();
    for h in 0..24 {
        let mut o = Map::new();
        for (i, c) in HOUR_COLS.iter().enumerate() {
            let v = hs_day
                .get(h)
                .and_then(|x| x.as_array())
                .and_then(|a| a.get(i))
                .cloned()
                .unwrap_or(json!(0));
            o.insert(c.to_string(), v);
        }
        hours.push(Value::Object(o));
    }
    let mut hs_win = vec![vec![0.0f64; HOUR_COLS.len()]; 24];
    let mut windays = 0i64;
    for r in rows.iter() {
        let dd = str_or(r.get("d"));
        let drec = days_map.get(&dd).and_then(|v| v.as_object()).cloned().unwrap_or_default();
        let hh = match drec.get("hours").and_then(|v| v.as_array()) {
            Some(a) if !a.is_empty() => a.clone(),
            _ => continue,
        };
        windays += 1;
        for h in 0..24.min(hh.len()) {
            let grp = hh[h].as_array().cloned().unwrap_or_default();
            for i in 0..HOUR_COLS.len().min(grp.len()) {
                hs_win[h][i] += crate::hostwork::util::py_double(&grp[i]);
            }
        }
    }
    let mut hours_win: Vec<Value> = Vec::new();
    for h in 0..24 {
        let mut o = Map::new();
        for (i, c) in HOUR_COLS.iter().enumerate() {
            o.insert(c.to_string(), json!(py_round1(hs_win[h][i])));
        }
        hours_win.push(Value::Object(o));
    }
    let mut day_apps: Vec<(String, f64)> = drec
        .get("apps")
        .and_then(|v| v.as_object())
        .map(|o| {
            o.iter()
                .map(|(k, v)| (k.clone(), py_round1(crate::hostwork::util::py_double(v))))
                .collect()
        })
        .unwrap_or_default();
    day_apps.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let day_apps: Vec<Value> = day_apps
        .into_iter()
        .take(12)
        .map(|(k, v)| json!([k, v]))
        .collect();

    // 存档状态
    let arch_n = days_map.len() as i64;
    let arch_from = days_map.keys().min().cloned().unwrap_or_default();
    let arch_to = days_map.keys().max().cloned().unwrap_or_default();
    let arch_kb = std::fs::metadata(archive_path(ctx))
        .map(|m| (m.len() / 1024) as i64)
        .unwrap_or(0);
    let _ = arch_to;

    let day_apps_empty = day_apps.is_empty();
    let in_store = sc
        .get("days")
        .and_then(|v| v.as_object())
        .map(|d| d.contains_key(&day))
        .unwrap_or(false);
    let from_store = if in_store {
        0
    } else if !day_apps_empty {
        1
    } else {
        0
    };

    let mut out = Map::new();
    out.insert("ok".into(), json!(true));
    out.insert("at".into(), json!(Local::now().format("%H:%M:%S").to_string()));
    out.insert("today".into(), json!(today_s));
    out.insert("day".into(), Value::String(day.clone()));
    out.insert("days".into(), json!(days));
    out.insert("range".into(), json!([since_day, today_s]));
    out.insert("rows".into(), Value::Array(rows));
    out.insert("hours".into(), Value::Array(hours));
    out.insert("hours_win".into(), Value::Array(hours_win));
    out.insert("hours_win_days".into(), json!(windays));
    out.insert("heat".into(), ht);
    out.insert(
        "screen".into(),
        json!({
            "ok": truthy(sc.get("ok").unwrap_or(&Value::Null)),
            "why": str_or(sc.get("why")),
            "src": str_or(sc.get("src")),
            "need_fda": truthy(sc.get("need_fda").unwrap_or(&Value::Null)),
            "n_days": py_int(sc.get("n_days").unwrap_or(&Value::Null)),
            "span": sc.get("span").cloned().unwrap_or(json!(["", ""])),
            "day": day.clone(),
            "apps": day_apps,
            "names": sc.get("names").cloned().unwrap_or(json!({})),
            "exe": str_or(sc.get("exe")),
            "from_store": from_store,
        }),
    );
    out.insert(
        "store".into(),
        json!({
            "file": archive_path(ctx).to_string_lossy().to_string(),
            "days": arch_n,
            "from": arch_from,
            "to": arch_to,
            "kb": arch_kb,
            "day_has": if days_map.contains_key(&day) { 1 } else { 0 },
        }),
    );
    out.insert("agents".into(), Value::Array(agents));
    out.insert("models".into(), Value::Array(models));
    out.insert("experts".into(), Value::Array(experts));
    out.insert("mcps".into(), Value::Array(mcps));
    out.insert(
        "lens".into(),
        json!({"bounds": LENS, "hist": hist}),
    );
    out.insert("tot".into(), Value::Object(tot.clone()));
    let mut avg = Map::new();
    avg.insert("tok_per_prompt".into(), json!(py_round1(tot_f("tok") / (tot_i("prompts")).max(1) as f64)));
    avg.insert("out_per_prompt".into(), json!(py_round1(tot_f("tout") / (tot_i("prompts")).max(1) as f64)));
    avg.insert("tok_per_call".into(), json!(py_round1(tot_f("tok") / (tot_i("reqs")).max(1) as f64)));
    avg.insert(
        "sec_per_prompt".into(),
        json!(py_round1(turn_secs / (tot_i("prompts")).max(1) as f64)),
    );
    avg.insert(
        "sec_per_req".into(),
        json!(py_round1(turn_secs / (tot_i("reqs")).max(1) as f64)),
    );
    avg.insert(
        "chars_per_prompt".into(),
        json!(py_round1(tot_f("chars") / (tot_i("prompts")).max(1) as f64)),
    );
    avg.insert(
        "prompts_per_day".into(),
        json!(py_round1(tot_f("prompts") / days.max(1) as f64)),
    );
    avg.insert(
        "prompts_per_active_day".into(),
        json!(py_round1(tot_f("prompts") / active_days.max(1) as f64)),
    );
    avg.insert(
        "ai_hours_per_day".into(),
        json!(round_n(tot_f("ai") / 3600.0 / active_days.max(1) as f64, 2)),
    );
    avg.insert(
        "human_hours_per_day".into(),
        json!(round_n(
            tot_f("human") / 3600.0 / (if human_days != 0 { human_days } else { days }).max(1) as f64,
            2
        )),
    );
    out.insert("avg".into(), Value::Object(avg));
    out.insert("active_days".into(), json!(active_days));

    let (took, cache_at) = {
        let g = SCACHE.lock().unwrap_or_else(|e| e.into_inner());
        match g.as_ref() {
            Some(c) => (
                c.took,
                if c.data.at != 0.0 {
                    Local
                        .timestamp_opt(c.data.at as i64, 0)
                        .single()
                        .map(|t| t.format("%m-%d %H:%M").to_string())
                        .unwrap_or_default()
                } else {
                    String::new()
                },
            ),
            None => (0.0, String::new()),
        }
    };
    let req_count: i64 = state
        .files
        .iter()
        .map(|(_, f)| f.as_ref().map(|x| x.reqs.len() as i64).unwrap_or(0))
        .sum();
    out.insert(
        "built".into(),
        json!({
            "files": state.files.len(),
            "reqs": req_count,
            "scan_sec": py_round1(took),
            "cache_at": cache_at,
        }),
    );
    Value::Object(out)
}

