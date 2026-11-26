// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：会话流水解析层 —— 「每轮请求干了什么」的唯一口径。对应 moshan/dash.py 的
//       _ask_of / _call_args / _step_text / _expert_name / _note_call / _session_reqs /
//       _proj_of / ctx_labels / _req_item / _tail_entry / _live_rid / _wb_items /
//       _cherry_items / _cli_reqs / _claude_items / _trae_items / _self_items_from，
//       以及 hostwork.py 的 _task_title / _session_index / rounds_items 的组装。
// 不做：不写任何客户端的数据；sqlite 一律 mode=ro（客户端的库可能正开着）。
//
// 为什么这份要单独一个文件：工作台左卡（pocket 的账）、右卡（本机 AI 客户端的账）、
// /api/rounds、/api/tasks 全从这一份出。口径分两处写，字段名和截断规则迟早漂移 ——
// Python 那边就是靠 _req_item 一份函数服务四家客户端（WorkBuddy / Cherry / Claude / TRAE）。
//
// 逐字节的坑都在细节里：_clip 是「截到 n-1 再加 …」、_brief 是「整 n 个再加 …」、
// sources 的顺序（tool → ai → mcp → mode → expert → skill）、facets 里 connectors 排序
// 是 (-n, name)、detail 上限 160（按字符不是字节）。下面逐条照抄，别凭印象简化。
#![allow(unused_imports, dead_code)]

use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, UNIX_EPOCH};

use super::platform;
use super::util::{fnum, int_of, load_json, ms_to_str, now_ymd, py_str, str_or, truthy};

const WL_SESSION_TAIL: u64 = 64 * 1024 * 1024;
const WL_STEPS_MEM: usize = 80;
const WL_STEPS_SEND: usize = 18;
const LIVE_TAIL: u64 = 48 * 1024;
const LIVE_STALE_MS: f64 = 300.0 * 1000.0;
const LIVE_FRESH_S: f64 = 7200.0;

// ------------------------------------------------------------------ 正则

fn re(p: &str) -> Regex {
    Regex::new(p).expect("内置正则必须能编译")
}

/// 词典驱动的正则（标题抽取那一百来条）：编译一次就留着，别每条记录重编。
fn re_cached(p: &str) -> Regex {
    static CACHE: OnceLock<Mutex<HashMap<String, Regex>>> = OnceLock::new();
    let m = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut g = m.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(r) = g.get(p) {
        return r.clone();
    }
    let r = re(p);
    g.insert(p.to_string(), r.clone());
    r
}

fn rx_ask() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r"(?s)<user_query>(.*?)</user_query>"))
}
fn rx_sr() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r"(?is)<system-reminder[\s\S]*?</system-reminder>"))
}
fn rx_img_ph() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r"@image#\d+:\S+"))
}
fn rx_tag() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r"<[^>]+>"))
}
fn rx_ws() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r"\s+"))
}
fn rx_model() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r#""requestModelName"\s*:\s*"([^"]{1,80})""#))
}
fn rx_rid() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r#""conversationRequestId"\s*:\s*"([A-Za-z0-9-]{8,64})""#))
}
fn rx_mode() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r"You are now in ([A-Za-z][A-Za-z ]{2,14}?) mode"))
}
fn rx_expert() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r"(?s)<current-expert>[\s\S]{0,200}?(ex_[A-Za-z0-9]{6,})"))
}
fn rx_date_layer() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        re(r"\d{4}[-_]\d{1,2}[-_]\d{1,2}(?:[-_ T]\d{1,2}[-:]\d{2}(?:[-:]\d{2})?)?")
    })
}
fn rx_cd() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r"^cd\s+\S+\s*&&\s*"))
}
fn rx_http() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r"^https?://"))
}
fn rx_attach() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r"@\S+?:\S*"))
}

// ------------------------------------------------------------------ 文本工具

/// `_clip`：截到 n-1 再补省号（n 按字符算）。
pub fn clip(s: &str, n: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= n {
        return s.to_string();
    }
    if n == 0 {
        return String::new();
    }
    let mut out: String = chars[..n - 1].iter().collect();
    out.push('…');
    out
}

/// `_brief`：留整 n 个字符再补省号。
pub fn brief(s: &str, n: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= n {
        return s.to_string();
    }
    let mut out: String = chars[..n].iter().collect();
    out.push('…');
    out
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

/// `_secs(a,b)`：两个「YYYY-MM-DD HH:MM:SS」之间差几秒（负数归零，解析不了给 None）。
pub fn secs(a: &str, b: &str) -> Option<i64> {
    let pa = chrono::NaiveDateTime::parse_from_str(a.get(..19)?, "%Y-%m-%d %H:%M:%S").ok()?;
    let pb = chrono::NaiveDateTime::parse_from_str(b.get(..19)?, "%Y-%m-%d %H:%M:%S").ok()?;
    Some((pb - pa).num_seconds().max(0))
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn mtime_s(p: &Path) -> f64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// `Path.glob("*/*.jsonl")` 的一层通配（按 readdir 顺序，不排序）。
fn glob_two(p: &Path, suffix: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let d1 = match std::fs::read_dir(p) {
        Ok(r) => r,
        Err(_) => return out,
    };
    for e1 in d1.flatten() {
        if !e1.path().is_dir() {
            continue;
        }
        if let Ok(d2) = std::fs::read_dir(e1.path()) {
            for e2 in d2.flatten() {
                let f = e2.path();
                if f.is_file() && f.to_string_lossy().ends_with(suffix) {
                    out.push(f);
                }
            }
        }
    }
    out
}

// ------------------------------------------------------------------ 账页

#[derive(Clone, Default)]
pub struct Req {
    pub t0: f64,
    pub t1: f64,
    pub ask: String,
    pub model: String,
    pub cwd: String,
    pub files: Vec<String>,
    pub calls: i64,
    pub steps: Vec<(f64, String)>,
    pub steps_n: i64,
    pub mcp: Vec<(String, i64)>,
    pub mode: String,
    pub expert: String,
    pub skills: Vec<String>,
    pub note: String,
    pub tin: i64,
    pub tout: i64,
    pub cached: i64,
    pub sid: String,
}

pub fn new_req(ts: f64, cwd: &str) -> Req {
    Req {
        t0: ts,
        t1: ts,
        cwd: cwd.to_string(),
        ..Default::default()
    }
}

fn mcp_inc(m: &mut Vec<(String, i64)>, k: &str) {
    for (name, n) in m.iter_mut() {
        if name == k {
            *n += 1;
            return;
        }
    }
    m.push((k.to_string(), 1));
}

fn mcp_get(m: &[(String, i64)], k: &str) -> i64 {
    m.iter().find(|(n, _)| n == k).map(|(_, v)| *v).unwrap_or(0)
}

fn push_unique(v: &mut Vec<String>, s: &str) {
    if !v.iter().any(|x| x == s) {
        v.push(s.to_string());
    }
}

/// `_ask_of`：用户消息里真正的那句话。
pub fn ask_of(text: &str, plain_ok: bool) -> String {
    let src: String = if let Some(c) = rx_ask().captures(text) {
        c.get(1).map(|m| m.as_str().to_string()).unwrap_or_default()
    } else if plain_ok {
        rx_sr().replace_all(text, " ").to_string()
    } else {
        return String::new();
    };
    let q = rx_img_ph().replace_all(&src, "");
    let q = rx_tag().replace_all(&q, " ");
    clip(rx_ws().replace_all(&q, " ").trim(), 320)
}

/// `_call_args`：工具调用参数（dict 原样，JSON 串解一次，其余当空）。
pub fn call_args(d: &Value) -> Map<String, Value> {
    match d.get("arguments") {
        Some(Value::Object(m)) => m.clone(),
        Some(Value::String(s)) => {
            let t = s.trim();
            if !t.starts_with('{') {
                return Map::new();
            }
            match serde_json::from_str::<Value>(t) {
                Ok(Value::Object(m)) => m,
                _ => Map::new(),
            }
        }
        _ => Map::new(),
    }
}

const FILE_TOOLS: [&str; 4] = ["Write", "Edit", "MultiEdit", "NotebookEdit"];

/// `_step_text`：一条 function_call → 一行「Edit dash.html」。
pub fn step_text(name: &str, args: &Map<String, Value>) -> String {
    let sval = |k: &str| -> String { str_or(args.get(k)) };
    let mut name = name.to_string();
    let mut tgt = String::new();
    if name == "Bash" || name == "PowerShell" || name == "execute_command" {
        let cmd = rx_ws().replace_all(&sval("command"), " ").trim().to_string();
        let cmd = rx_cd().replace(&cmd, "").to_string();
        let out = if cmd.is_empty() {
            name.clone()
        } else {
            format!("$ {}", cmd)
        };
        return out.chars().take(64).collect();
    }
    if FILE_TOOLS.contains(&name.as_str()) || name == "Read" {
        let fp = if truthy(args.get("file_path").unwrap_or(&Value::Null)) {
            sval("file_path")
        } else {
            sval("path")
        };
        let fp = fp.trim_end_matches('/');
        tgt = fp.split('/').last().unwrap_or("").to_string();
    } else if name == "Grep" || name == "search_data" {
        let pat = if truthy(args.get("pattern").unwrap_or(&Value::Null)) {
            sval("pattern")
        } else {
            sval("query")
        };
        tgt = if pat.is_empty() {
            String::new()
        } else {
            format!("/{}/", pat)
        };
    } else if name == "Glob" {
        tgt = sval("pattern");
    } else if name == "WebSearch" {
        tgt = sval("query");
    } else if name == "WebFetch" {
        let u = rx_http().replace(&sval("url"), "").to_string();
        tgt = u.split('/').next().unwrap_or("").to_string();
    } else if name == "Skill" {
        tgt = if truthy(args.get("skill").unwrap_or(&Value::Null)) {
            sval("skill")
        } else {
            sval("name")
        };
    } else if name == "Agent" || name == "Task" {
        tgt = if truthy(args.get("subagent_type").unwrap_or(&Value::Null)) {
            sval("subagent_type")
        } else {
            sval("description")
        };
    } else if name.starts_with("mcp__") {
        let parts: Vec<&str> = name.split("__").filter(|x| !x.is_empty()).collect();
        name = if parts.len() > 1 {
            parts[1..].join("·")
        } else {
            name.clone()
        };
    }
    if name.is_empty() {
        return String::new();
    }
    let raw = if tgt.is_empty() {
        name.clone()
    } else {
        format!("{} {}", name, tgt)
    };
    let out = rx_ws().replace_all(&raw, " ").trim().to_string();
    out.chars().take(64).collect()
}

/// `_note_call`：一次工具调用记进这一轮。
pub fn note_call(b: &mut Req, nm: &str, args: &Map<String, Value>, ts: f64) {
    b.calls += 1;
    if nm == "Skill" {
        let sk = str_or(args.get("skill")).trim().to_string();
        if !sk.is_empty() {
            push_unique(&mut b.skills, &sk);
        }
    }
    if nm.starts_with("mcp__") {
        let seg: Vec<&str> = nm.split("__").filter(|x| !x.is_empty()).collect();
        let srv = if seg.len() > 1 {
            seg[1].to_string()
        } else {
            nm.get(5..).unwrap_or("").to_string()
        };
        if !srv.is_empty() {
            mcp_inc(&mut b.mcp, &srv);
        }
    }
    if FILE_TOOLS.contains(&nm) {
        let fp = if truthy(args.get("file_path").unwrap_or(&Value::Null)) {
            str_or(args.get("file_path"))
        } else {
            str_or(args.get("path"))
        };
        let base = fp.trim_end_matches('/').split('/').last().unwrap_or("").to_string();
        if !base.is_empty() {
            push_unique(&mut b.files, &base);
        }
    }
    let txt = step_text(nm, args);
    if !txt.is_empty() {
        b.steps.push((ts, txt));
        b.steps_n += 1;
        if b.steps.len() > WL_STEPS_MEM {
            b.steps.drain(0..WL_STEPS_MEM / 4);
        }
    }
}

// ------------------------------------------------------------------ 专家名

struct ExpertCache {
    mtime: f64,
    map: HashMap<String, String>,
}

static EXPERT: Mutex<Option<ExpertCache>> = Mutex::new(None);

/// `_expert_name`：ex_hXSIP3lgdCnU → 全栈自动化脚本工程师。
pub fn expert_name(eid: &str) -> String {
    if eid.is_empty() {
        return String::new();
    }
    let home = match std::env::var("HOME") {
        Ok(h) => PathBuf::from(h),
        Err(_) => return eid.chars().rev().take(6).collect::<Vec<_>>().into_iter().rev().collect(),
    };
    let wb = home.join(".workbuddy");
    let mut files: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(wb.join("storage")) {
        for e in rd.flatten() {
            let p = e.path().join("global").join("expert.json");
            if p.is_file() {
                files.push(p);
            }
        }
    }
    files.sort_by(|a, b| mtime_s(b).partial_cmp(&mtime_s(a)).unwrap_or(std::cmp::Ordering::Equal));
    if let Some(p) = files.first() {
        let mt = if p.exists() { mtime_s(p) } else { -1.0 };
        let mut guard = EXPERT.lock().unwrap_or_else(|e| e.into_inner());
        let stale = match guard.as_ref() {
            Some(c) => c.mtime != mt,
            None => true,
        };
        if stale {
            let mut amap: HashMap<String, String> = HashMap::new();
            if let Some(Value::Object(raw)) = load_json(p) {
                if let Some(Value::Array(rec)) = raw.get("recents") {
                    for row in rec {
                        if let Value::Object(o) = row {
                            let rid = str_or(o.get("id"));
                            let nm = str_or(o.get("name"));
                            if !rid.is_empty() && !nm.is_empty() {
                                amap.insert(rid, nm);
                            }
                        }
                    }
                }
            }
            if let Some(Value::Object(bmap)) =
                load_json(&wb.join("app").join("cache").join("experts").join("expert-bundle-map.json"))
            {
                for (rid, slug) in bmap.iter() {
                    let sl = py_str(slug);
                    if rid.starts_with("ex_") && !sl.is_empty() {
                        amap.entry(rid.clone()).or_insert(sl);
                    }
                }
            }
            *guard = Some(ExpertCache { mtime: mt, map: amap });
        }
        if let Some(c) = guard.as_ref() {
            if let Some(v) = c.map.get(eid) {
                return v.clone();
            }
        }
    }
    let chars: Vec<char> = eid.chars().collect();
    let start = chars.len().saturating_sub(6);
    chars[start..].iter().collect()
}

// ------------------------------------------------------------------ 项目名

const CONTAINER_LAYERS: [&str; 10] = [
    "spaces", "space", "projects", "project", "sessions", "conversations", "chats",
    "workspaces", "workspace",
    "",
];

/// `_proj_of`：cwd → 一个能认出来的项目名（剥时间戳层与容器层）。
pub fn proj_of(cwd: &str) -> String {
    let mut parts: Vec<&str> = cwd.trim_end_matches('/').split('/').filter(|p| !p.is_empty()).collect();
    for _ in 0..3 {
        let last = match parts.last() {
            Some(l) => *l,
            None => break,
        };
        let is_date = rx_date_layer()
            .find(last)
            .map(|m| m.as_str() == last)
            .unwrap_or(false);
        if is_date || CONTAINER_LAYERS.contains(&last.to_lowercase().as_str()) {
            parts.pop();
        } else {
            break;
        }
    }
    parts.last().map(|s| s.to_string()).unwrap_or_default()
}

/// `ctx_labels`：各 agent 自己定义维度名。
pub fn ctx_labels(client: &str) -> Value {
    let pairs: Vec<(&str, &str)> = match client {
        "WorkBuddy" => vec![("mode", "模式"), ("expert", "专家"), ("skills", "技能"), ("connectors", "连接器")],
        "Claude Code" => vec![("mode", "权限模式"), ("agent", "子代理"), ("skills", "技能"), ("connectors", "MCP")],
        "Cherry Studio" => vec![("skills", "技能"), ("connectors", "MCP")],
        "TRAE" => vec![("agent", "代理"), ("skills", "技能"), ("connectors", "MCP")],
        _ => vec![("mode", "模式"), ("expert", "专家"), ("skills", "技能"), ("connectors", "连接器")],
    };
    let mut m = Map::new();
    for (k, v) in pairs {
        m.insert(k.to_string(), Value::String(v.to_string()));
    }
    Value::Object(m)
}

// ------------------------------------------------------------------ 一轮 → 一行

/// `_req_item`：一轮账页 → 卡片上的一行（四家客户端共用）。
pub fn req_item(b: &Req, rid: &str, client: &str, tag: &str, status: &str) -> Option<Value> {
    if b.calls == 0 && b.ask.is_empty() {
        return None;
    }
    let start = ms_to_str(b.t0);
    let mut end = ms_to_str(b.t1);
    if start.is_empty() {
        return None;
    }
    let run = status == "run";
    let secs_v: Option<i64>;
    if run {
        end = String::new();
        secs_v = Some(((now_secs() * 1000.0 - b.t0) / 1000.0).max(0.0) as i64);
    } else {
        secs_v = secs(&start, &end);
    }
    let mut bits: Vec<String> = Vec::new();
    let note = b.note.split_whitespace().collect::<Vec<_>>().join(" ");
    if !note.is_empty() {
        bits.push(clip(&note, 60));
    }
    let mode = b.mode.clone();
    if !mode.is_empty() {
        bits.push(mode.clone());
    }
    let ename = expert_name(&b.expert);
    if !ename.is_empty() {
        bits.push(format!("专家 {}", brief(&ename, 4)));
    }
    let proj = proj_of(&b.cwd);
    if !proj.is_empty() && proj != client && proj != "WorkBuddy" {
        bits.push(proj.clone());
    }
    let mut mcp_sorted: Vec<(String, i64)> = b.mcp.clone();
    mcp_sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    if !mcp_sorted.is_empty() {
        let top: Vec<String> = mcp_sorted
            .iter()
            .take(2)
            .map(|(k, v)| if *v > 1 { format!("{}×{}", k, v) } else { k.clone() })
            .collect();
        bits.push(format!(
            "MCP {}{}",
            top.join("、"),
            if b.mcp.len() > 2 { "…" } else { "" }
        ));
    }
    let skills: Vec<String> = b.skills.iter().filter(|s| !s.is_empty()).cloned().collect();
    if !skills.is_empty() {
        bits.push(format!(
            "技能 {}{}",
            skills.iter().take(2).cloned().collect::<Vec<_>>().join("、"),
            if skills.len() > 2 { "…" } else { "" }
        ));
    }
    let mut rest: Vec<String> = Vec::new();
    if b.calls != 0 {
        rest.push(format!("{} 步", b.calls));
    }
    if !b.files.is_empty() {
        let mut s = b.files.iter().take(2).cloned().collect::<Vec<_>>().join("、");
        if b.files.len() > 2 {
            s.push_str(&format!(" 等{}个", b.files.len()));
        }
        rest.push(s);
    }
    let task = if !b.ask.is_empty() {
        b.ask.clone()
    } else if !b.files.is_empty() {
        format!("改 {}", b.files[0])
    } else {
        "会话".to_string()
    };
    let mut src: Vec<Value> = vec![json!({"kind": "tool", "name": client})];
    if !b.model.is_empty() {
        src.push(json!({"kind": "ai", "name": b.model}));
    }
    for (srv, _) in b.mcp.iter().take(3) {
        src.push(json!({"kind": "mcp", "name": srv}));
    }
    if !mode.is_empty() {
        src.push(json!({"kind": "mode", "name": mode}));
    }
    if !ename.is_empty() {
        src.push(json!({"kind": "expert", "name": ename}));
    }
    for sk in skills.iter().take(3) {
        src.push(json!({"kind": "skill", "name": sk}));
    }
    let mut facets = Map::new();
    if !mode.is_empty() {
        facets.insert("mode".into(), Value::String(mode.clone()));
    }
    if !ename.is_empty() {
        facets.insert("expert".into(), Value::String(ename.clone()));
    }
    if !skills.is_empty() {
        facets.insert(
            "skills".into(),
            Value::Array(skills.iter().take(6).map(|s| Value::String(s.clone())).collect()),
        );
    }
    if !mcp_sorted.is_empty() {
        facets.insert(
            "connectors".into(),
            Value::Array(
                mcp_sorted
                    .iter()
                    .take(6)
                    .map(|(k, v)| json!({"name": k, "n": v}))
                    .collect(),
            ),
        );
    }
    let tail_start = b.steps.len().saturating_sub(WL_STEPS_SEND);
    let kids: Vec<Value> = b.steps[tail_start..]
        .iter()
        .map(|(t, txt)| {
            json!({"start": ms_to_str(*t), "detail": txt.chars().take(48).collect::<String>()})
        })
        .collect();
    let mut all_bits = bits.clone();
    all_bits.extend(rest.iter().cloned());
    let detail: String = all_bits.join(" · ").chars().take(160).collect();
    let rest_s: String = rest.join(" · ").chars().take(160).collect();

    let mut m = Map::new();
    m.insert("id".into(), Value::String(format!("{}{}", tag, rid_tail(rid, 6))));
    m.insert("sid".into(), Value::String(b.sid.clone()));
    m.insert("task".into(), Value::String(clip(&task, 320)));
    m.insert("detail".into(), Value::String(detail));
    m.insert("rest".into(), Value::String(rest_s));
    m.insert("status".into(), Value::String(status.to_string()));
    m.insert("start".into(), Value::String(start));
    m.insert("end".into(), Value::String(end));
    m.insert(
        "secs".into(),
        match secs_v {
            Some(v) => json!(v),
            None => Value::Null,
        },
    );
    m.insert("t0".into(), json!(b.t0 as i64));
    m.insert("sources".into(), Value::Array(src));
    m.insert("facets".into(), Value::Object(facets));
    m.insert("ctx_labels".into(), ctx_labels(client));
    m.insert("client".into(), Value::String(client.to_string()));
    m.insert("children".into(), Value::Array(kids.clone()));
    m.insert("more_steps".into(), json!((b.steps_n - kids.len() as i64).max(0)));
    m.insert("project".into(), Value::String(proj));
    m.insert(
        "tokens".into(),
        json!({"in": b.tin, "out": b.tout, "cached": b.cached, "total": b.tin + b.tout}),
    );
    Some(Value::Object(m))
}

fn rid_tail(rid: &str, n: usize) -> String {
    let chars: Vec<char> = rid.chars().collect();
    let start = chars.len().saturating_sub(n);
    chars[start..].iter().collect()
}

// ------------------------------------------------------------------ 增量缓存

struct SCache {
    size: u64,
    reqs: HashMap<String, Req>,
}

static SESS_CACHE: Mutex<Option<HashMap<String, SCache>>> = Mutex::new(None);
static CLI_CACHE: Mutex<Option<HashMap<String, (u64, String, HashMap<String, Req>)>>> = Mutex::new(None);

fn read_from(p: &Path, start: u64) -> Option<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(p).ok()?;
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// `_session_reqs`：一个 WorkBuddy 会话文件里「每轮请求干了什么」。
pub fn session_reqs(path: &Path, _since_ms: f64) -> HashMap<String, Req> {
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if size == 0 {
        return HashMap::new();
    }
    let key = path.to_string_lossy().to_string();
    let mut guard = SESS_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    let mut c = match map.remove(&key) {
        Some(c) if c.size <= size => c,
        _ => SCache { size: 0, reqs: HashMap::new() },
    };
    if c.size >= size {
        let out = c.reqs.clone();
        map.insert(key, c);
        return out;
    }
    let start = if c.size > 0 { c.size } else { size.saturating_sub(WL_SESSION_TAIL) };
    let raw = match read_from(path, start) {
        Some(b) => b,
        None => {
            let out = c.reqs.clone();
            map.insert(key, c);
            return out;
        }
    };
    let consumed = start + raw.len() as u64;
    let mut raw: &[u8] = &raw;
    if start > 0 {
        match raw.iter().position(|b| *b == b'\n') {
            Some(i) => raw = &raw[i + 1..],
            None => raw = &[],
        }
    }
    let text = String::from_utf8_lossy(raw);
    for ln in text.split('\n') {
        if !ln.contains("\"function_call\"") && !ln.contains("\"message\"") {
            if let Some(m) = rx_model().captures(ln) {
                if let Some(r) = rx_rid().captures(ln) {
                    let rid = r.get(1).unwrap().as_str().to_string();
                    if let Some(b0) = c.reqs.get_mut(&rid) {
                        if b0.model.is_empty() {
                            b0.model = m.get(1).unwrap().as_str().to_string();
                        }
                    }
                }
            }
            continue;
        }
        let d: Value = match serde_json::from_str(ln) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if !d.is_object() {
            continue;
        }
        let ts = match d.get("timestamp") {
            Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
            _ => continue,
        };
        let pd = match d.get("providerData") {
            Some(Value::Object(m)) => m.clone(),
            _ => Map::new(),
        };
        let rid = str_or(pd.get("conversationRequestId"));
        if rid.is_empty() {
            continue;
        }
        let cwd = str_or(d.get("cwd"));
        let b = c.reqs.entry(rid.clone()).or_insert_with(|| new_req(ts, &cwd));
        if ts < b.t0 {
            b.t0 = ts;
        }
        if ts > b.t1 {
            b.t1 = ts;
        }
        if b.model.is_empty() {
            let mv = if truthy(pd.get("requestModelName").unwrap_or(&Value::Null)) {
                str_or(pd.get("requestModelName"))
            } else {
                str_or(pd.get("requestModelId"))
            };
            if !mv.is_empty() {
                b.model = mv;
            }
        }
        if let Some(Value::Object(u)) = pd.get("usage") {
            let tin = match u.get("inputTokens") {
                Some(v) if truthy(v) => int_of(Some(v)),
                _ => int_of(u.get("input_tokens")),
            };
            let tout = match u.get("outputTokens") {
                Some(v) if truthy(v) => int_of(Some(v)),
                _ => int_of(u.get("output_tokens")),
            };
            b.tin += tin;
            b.tout += tout;
            if let Some(Value::Array(det)) = u.get("inputTokensDetails") {
                if let Some(Value::Object(d0)) = det.first() {
                    b.cached += int_of(d0.get("cached_tokens"));
                }
            }
        }
        if d.get("type").and_then(|v| v.as_str()) == Some("message") {
            if d.get("role").and_then(|v| v.as_str()) == Some("user") {
                let mut txt = String::new();
                if let Some(Value::Array(parts)) = d.get("content") {
                    for x in parts {
                        if let Value::Object(o) = x {
                            txt.push_str(&str_or(o.get("text")));
                        }
                    }
                }
                if !txt.is_empty() && b.mode.is_empty() {
                    if let Some(m) = rx_mode().captures(&txt) {
                        b.mode = py_title(m.get(1).unwrap().as_str().trim());
                    }
                }
                if !txt.is_empty() && b.expert.is_empty() {
                    if let Some(e) = rx_expert().captures(&txt) {
                        b.expert = e.get(1).unwrap().as_str().to_string();
                    }
                }
                if b.ask.is_empty() {
                    let q = ask_of(&txt, false);
                    if !q.is_empty() {
                        b.ask = q;
                    }
                }
            }
        } else {
            let args = call_args(&d);
            let nm = str_or(d.get("name"));
            note_call(b, nm.trim(), &args, ts);
        }
    }
    c.size = consumed;
    let out = c.reqs.clone();
    map.insert(key, c);
    if map.len() > 16 {
        let keys: Vec<String> = map.keys().cloned().collect();
        for k in keys.iter().take(keys.len() - 16) {
            map.remove(k);
        }
    }
    out
}

// ------------------------------------------------------------------ 实时（在跑还是答完了）

struct TailEntry {
    typ: String,
    role: String,
    rid: String,
    ts: f64,
}

const LIVE_SKIP: [&str; 7] = [
    "session-meta", "ai-title", "file-history-snapshot", "token_count", "usage", "compact",
    "checkpoint",
];

fn tail_entry(path: &Path) -> Option<TailEntry> {
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let raw = read_from(path, size.saturating_sub(LIVE_TAIL))?;
    for chunk in raw.split(|b| *b == b'\n').rev() {
        let s = chunk.strip_prefix(b"\n").unwrap_or(chunk);
        let s = s.strip_suffix(b"\r").unwrap_or(s);
        if !s.starts_with(b"{") {
            continue;
        }
        let d: Value = match serde_json::from_str(&String::from_utf8_lossy(s)) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if !d.is_object() {
            continue;
        }
        let t = str_or(d.get("type"));
        if t.is_empty() || LIVE_SKIP.contains(&t.as_str()) {
            continue;
        }
        let pd = d.get("providerData").cloned().unwrap_or_else(|| json!({}));
        return Some(TailEntry {
            typ: t,
            role: str_or(d.get("role")),
            rid: str_or(pd.get("conversationRequestId")),
            ts: d.get("timestamp").and_then(|v| v.as_f64()).unwrap_or(0.0),
        });
    }
    None
}

/// `_live_rid`：这个会话现在跑的是哪一轮。
pub fn live_rid(path: &Path) -> (String, bool, f64) {
    let e = match tail_entry(path) {
        Some(e) => e,
        None => return (String::new(), false, 0.0),
    };
    let now = now_secs() * 1000.0;
    let mut done = if e.typ == "message" && e.role == "assistant" {
        true
    } else if e.ts > 0.0 && (now - e.ts) > LIVE_STALE_MS {
        true
    } else {
        false
    };
    if done && e.ts > 0.0 && (now - e.ts) < 2000.0 {
        done = false;
    }
    (e.rid, !done, e.ts)
}

// ------------------------------------------------------------------ 四家客户端

/// `_wb_items`：WorkBuddy 每轮会话一条。
pub fn wb_items(since_ms: f64) -> Vec<Value> {
    let home = match std::env::var("HOME") {
        Ok(h) => PathBuf::from(h),
        Err(_) => return Vec::new(),
    };
    let base = home.join(".workbuddy").join("projects");
    let files = glob_two(&base, ".jsonl");
    let mut out = Vec::new();
    let now = now_secs();
    for p in files {
        let mt = mtime_s(&p);
        if mt * 1000.0 < since_ms {
            continue;
        }
        let reqs = session_reqs(&p, since_ms);
        if reqs.is_empty() {
            continue;
        }
        let live = if (now - mt) < LIVE_FRESH_S {
            live_rid(&p)
        } else {
            (String::new(), false, 0.0)
        };
        let stem = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let last_rid = reqs
            .iter()
            .max_by(|a, b| a.1.t1.partial_cmp(&b.1.t1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(k, _)| k.clone())
            .unwrap_or_default();
        let target = if live.0.is_empty() { last_rid } else { live.0.clone() };
        let mut rids: Vec<(String, Req)> = reqs.into_iter().collect();
        rids.sort_by(|a, b| a.1.t0.partial_cmp(&b.1.t0).unwrap_or(std::cmp::Ordering::Equal));
        for (rid, mut b) in rids {
            if b.t1 < since_ms {
                continue;
            }
            b.sid = stem.clone();
            let st = if live.1 && rid == target { "run" } else { "ok" };
            let tag = format!("h{}", stem.chars().take(8).collect::<String>());
            if let Some(it) = req_item(&b, &rid, "WorkBuddy", &tag, st) {
                out.push(it);
            }
        }
    }
    out
}

/// `_cherry_text`：message.data → 纯文本。
fn cherry_text(raw: &Value) -> String {
    let s = match raw {
        Value::String(s) => s.clone(),
        Value::Null => "{}".to_string(),
        other => py_str(other),
    };
    let d: Value = match serde_json::from_str(&s) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };
    let parts = match d.get("parts") {
        Some(Value::Array(a)) => a.clone(),
        _ => return String::new(),
    };
    let mut txts: Vec<String> = Vec::new();
    for p in parts {
        if let Value::Object(o) = p {
            if o.get("type").and_then(|v| v.as_str()) == Some("text") {
                txts.push(str_or(o.get("text")));
            }
        }
    }
    let txt = txts.join(" ");
    let txt = rx_attach().replace_all(&txt, " ");
    rx_ws().replace_all(&txt, " ").trim().to_string()
}

/// `_cherry_model`：<uuid>::provider/model:quant → model。
fn cherry_model(mid: &str) -> String {
    let s = match mid.split_once("::") {
        Some((_, b)) => b,
        None => mid,
    };
    let s = s.split('/').last().unwrap_or("");
    let s = s.split(':').next().unwrap_or("");
    s.trim().to_string()
}

/// `_cherry_items`：Cherry Studio 的话题，一个话题一条。
pub fn cherry_items(since_ms: f64) -> Vec<Value> {
    let home = match std::env::var("HOME") {
        Ok(h) => PathBuf::from(h),
        Err(_) => return Vec::new(),
    };
    let rels: [Vec<&str>; 3] = [
        vec!["Library", "Application Support", "CherryStudio", "Data", "cherrystudio.sqlite"],
        vec!["AppData", "Roaming", "CherryStudio", "Data", "cherrystudio.sqlite"],
        vec![".config", "CherryStudio", "Data", "cherrystudio.sqlite"],
    ];
    let mut dbs: Vec<PathBuf> = Vec::new();
    for rel in rels.iter() {
        let mut p = home.clone();
        for seg in rel {
            p = p.join(seg);
        }
        if p.exists() && !dbs.contains(&p) {
            dbs.push(p);
        }
    }
    if dbs.is_empty() {
        return Vec::new();
    }
    let sql = format!(
        "select topic_id, role, data, model_id, created_at, status from message \
         where deleted_at is null and topic_id in ( \
         select topic_id from message where deleted_at is null and created_at >= {} \
         group by topic_id) order by created_at",
        since_ms as i64
    );
    let rows = match platform::sqlite_rows(&dbs[0], &sql, Duration::from_millis(1500)) {
        Some(r) => r,
        None => return Vec::new(),
    };
    struct TB {
        t0: f64,
        t1: f64,
        ask: String,
        model: String,
        n: i64,
        fail: bool,
    }
    let mut by: Vec<(String, TB)> = Vec::new();
    for r in rows {
        let tid = str_or(r.get("topic_id"));
        if tid.is_empty() {
            continue;
        }
        let created = r.get("created_at").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let idx = match by.iter().position(|(k, _)| *k == tid) {
            Some(i) => i,
            None => {
                by.push((
                    tid.clone(),
                    TB { t0: created, t1: created, ask: String::new(), model: String::new(), n: 0, fail: false },
                ));
                by.len() - 1
            }
        };
        let b = &mut by[idx].1;
        if created < b.t0 {
            b.t0 = created;
        }
        if created > b.t1 {
            b.t1 = created;
        }
        let role = str_or(r.get("role"));
        if role == "user" && b.ask.is_empty() {
            b.ask = clip(&cherry_text(r.get("data").unwrap_or(&Value::Null)), 320);
        } else if role == "assistant" && b.model.is_empty() {
            b.model = cherry_model(&str_or(r.get("model_id")));
        }
        if created >= since_ms {
            b.n += 1;
            if role == "assistant" && str_or(r.get("status")) == "error" {
                b.fail = true;
            }
        }
    }
    let mut out = Vec::new();
    for (tid, b) in by {
        if b.n == 0 {
            continue;
        }
        let start = ms_to_str(b.t0.max(since_ms));
        let end = ms_to_str(b.t1);
        if start.is_empty() || end.is_empty() {
            continue;
        }
        let mut src = vec![json!({"kind": "tool", "name": "Cherry Studio"})];
        if !b.model.is_empty() {
            src.push(json!({"kind": "ai", "name": b.model}));
        }
        let mut m = Map::new();
        m.insert(
            "id".into(),
            Value::String(format!("c{}", tid.replace('-', "").chars().take(10).collect::<String>())),
        );
        m.insert(
            "task".into(),
            Value::String(clip(if b.ask.is_empty() { "Cherry 对话" } else { &b.ask }, 320)),
        );
        m.insert("detail".into(), Value::String(format!("{} 条消息", b.n).chars().take(160).collect::<String>()));
        m.insert("status".into(), Value::String(if b.fail { "fail" } else { "ok" }.into()));
        m.insert("start".into(), Value::String(start.clone()));
        m.insert("end".into(), Value::String(end.clone()));
        m.insert(
            "secs".into(),
            match secs(&start, &end) {
                Some(v) => json!(v),
                None => Value::Null,
            },
        );
        m.insert("sources".into(), Value::Array(src));
        m.insert("client".into(), Value::String("Cherry Studio".into()));
        out.push(Value::Object(m));
    }
    out
}

/// `_iso_ms`：ISO 时间串 → 毫秒；已经是数字就原样用。
pub fn iso_ms(v: &Value) -> f64 {
    match v {
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        _ => {
            let s = str_or(Some(v)).trim().to_string();
            if s.is_empty() {
                return 0.0;
            }
            let t = s.replace('Z', "+00:00");
            match chrono::DateTime::parse_from_rfc3339(&t) {
                Ok(d) => d.timestamp_millis() as f64,
                Err(_) => match chrono::DateTime::parse_from_str(&t, "%Y-%m-%dT%H:%M:%S%.f%z") {
                    Ok(d) => d.timestamp_millis() as f64,
                    Err(_) => 0.0,
                },
            }
        }
    }
}

/// `_cli_reqs`：通用 JSONL（Claude Code 这类）→ {轮 key: 账页}。
pub fn cli_reqs(path: &Path) -> HashMap<String, Req> {
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if size == 0 {
        return HashMap::new();
    }
    let key = format!("cli:{}", path.to_string_lossy());
    let mut guard = CLI_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    let mut c = match map.remove(&key) {
        Some((sz, cur, reqs)) if sz <= size => (sz, cur, reqs),
        _ => (0u64, String::new(), HashMap::new()),
    };
    if c.0 >= size {
        let out = c.2.clone();
        map.insert(key, c);
        return out;
    }
    let start = if c.0 > 0 { c.0 } else { size.saturating_sub(WL_SESSION_TAIL) };
    let raw = match read_from(path, start) {
        Some(b) => b,
        None => {
            let out = c.2.clone();
            map.insert(key, c);
            return out;
        }
    };
    let consumed = start + raw.len() as u64;
    let mut raw: &[u8] = &raw;
    if start > 0 {
        match raw.iter().position(|b| *b == b'\n') {
            Some(i) => raw = &raw[i + 1..],
            None => raw = &[],
        }
    }
    let text = String::from_utf8_lossy(raw);
    let mut cur = c.1.clone();
    for ln in text.split('\n') {
        if !ln.contains("\"message\"") {
            continue;
        }
        let d: Value = match serde_json::from_str(ln) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let msg = match d.get("message") {
            Some(Value::Object(m)) => m.clone(),
            _ => continue,
        };
        let blocks: Vec<Value> = match msg.get("content") {
            Some(Value::Array(a)) => a.clone(),
            _ => Vec::new(),
        };
        let ts = iso_ms(d.get("timestamp").unwrap_or(&Value::Null));
        let role = {
            let r = str_or(msg.get("role"));
            if r.is_empty() {
                str_or(d.get("type"))
            } else {
                r
            }
        };
        if role == "user" {
            if !blocks.is_empty()
                && blocks
                    .iter()
                    .all(|x| x.get("type").and_then(|v| v.as_str()) == Some("tool_result"))
            {
                continue;
            }
            let txt: String = blocks
                .iter()
                .filter(|x| x.get("type").and_then(|v| v.as_str()) == Some("text"))
                .map(|x| str_or(x.get("text")))
                .collect();
            if txt.trim().is_empty() {
                continue;
            }
            cur = str_or(d.get("uuid"));
            if cur.is_empty() {
                cur = format!("{}", ts as i64);
            }
            let cwd = str_or(d.get("cwd"));
            let b = c.2.entry(cur.clone()).or_insert_with(|| new_req(ts, &cwd));
            if ts < b.t0 {
                b.t0 = ts;
            }
            if ts > b.t1 {
                b.t1 = ts;
            }
            if b.ask.is_empty() {
                b.ask = ask_of(&txt, true);
            }
            continue;
        }
        let b = match c.2.get_mut(&cur) {
            Some(b) => b,
            None => continue,
        };
        if ts > b.t1 {
            b.t1 = ts;
        }
        if b.model.is_empty() {
            b.model = str_or(msg.get("model"));
        }
        for x in blocks.iter() {
            if x.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                let args = match x.get("input") {
                    Some(Value::Object(m)) => m.clone(),
                    _ => Map::new(),
                };
                let nm = str_or(x.get("name"));
                note_call(b, &nm, &args, ts);
            }
        }
    }
    c.0 = consumed;
    c.1 = cur;
    let out = c.2.clone();
    map.insert(key, c);
    if map.len() > 16 {
        let keys: Vec<String> = map.keys().cloned().collect();
        for k in keys.iter().take(keys.len() - 16) {
            map.remove(k);
        }
    }
    out
}

/// `_claude_items`：Claude Code 每轮一条。
pub fn claude_items(since_ms: f64) -> Vec<Value> {
    let home = match std::env::var("HOME") {
        Ok(h) => PathBuf::from(h),
        Err(_) => return Vec::new(),
    };
    let base = home.join(".claude").join("projects");
    if !base.exists() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for p in glob_two(&base, ".jsonl") {
        if mtime_s(&p) * 1000.0 < since_ms {
            continue;
        }
        let stem = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let mut rows: Vec<(String, Req)> = cli_reqs(&p).into_iter().collect();
        rows.sort_by(|a, b| a.1.t0.partial_cmp(&b.1.t0).unwrap_or(std::cmp::Ordering::Equal));
        for (rid, mut b) in rows {
            if b.t1 < since_ms {
                continue;
            }
            b.sid = stem.clone();
            let tag = format!("a{}", stem.chars().take(8).collect::<String>());
            if let Some(it) = req_item(&b, &rid, "Claude Code", &tag, "ok") {
                out.push(it);
            }
        }
    }
    out
}

/// `_self_items_from`：agent 自述的一段（TRAE自述 / Agent自述）。
pub fn self_items_from(p: &Path, since_ms: f64, fallback: &str) -> Vec<Value> {
    let mut out = Vec::new();
    if !p.exists() || mtime_s(p) * 1000.0 < since_ms {
        return out;
    }
    let text = match std::fs::read_to_string(p) {
        Ok(t) => t,
        Err(_) => return out,
    };
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(400);
    for ln in &lines[start..] {
        let ln = ln.trim();
        if ln.is_empty() {
            continue;
        }
        let o: Value = match serde_json::from_str(ln) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if !o.is_object() {
            continue;
        }
        let at = o.get("at").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let ts = at * 1000.0;
        if ts == 0.0 || ts < since_ms {
            continue;
        }
        let task = str_or(o.get("task")).split_whitespace().collect::<Vec<_>>().join(" ");
        if task.is_empty() {
            continue;
        }
        let who = {
            let w = str_or(o.get("agent")).trim().to_string();
            if w.is_empty() { fallback.to_string() } else { w }
        };
        let mut b = new_req(ts, &str_or(o.get("project")));
        b.t1 = ts;
        b.ask = clip(&task, 320);
        b.note = str_or(o.get("summary")).split_whitespace().collect::<Vec<_>>().join(" ");
        b.model = str_or(o.get("model"));
        b.tin = int_of(o.get("tin")).max(0);
        b.tout = int_of(o.get("tout")).max(0);
        b.cached = int_of(o.get("cached")).max(0);
        b.calls = int_of(o.get("calls")).max(0);
        if b.calls == 0 {
            b.calls = 1;
        }
        let rid = {
            use sha1::{Digest, Sha1};
            let mut h = Sha1::new();
            h.update(format!("{}|{}|{}", who, py_str(o.get("at").unwrap_or(&Value::Null)), task).as_bytes());
            format!("trself{}", hex10(&h.finalize()))
        };
        if let Some(mut it) = req_item(&b, &rid, &who, &rid, "ok") {
            if let Value::Object(m) = &mut it {
                m.insert("source".into(), Value::String("自述".into()));
            }
            out.push(it);
        }
    }
    out
}

fn hex10(d: &[u8]) -> String {
    let mut s = String::new();
    for b in d.iter() {
        s.push_str(&format!("{:02x}", b));
    }
    s.chars().take(10).collect()
}

// ------------------------------------------------------------------ TRAE（记忆 + 自述）

/// trae_work._item 的同形账页（TRAE 的 ctx_labels 只有 agent/skills/connectors）。
fn trae_item(rid: &str, task: &str, detail: &str, start_ms: f64, end_ms: f64,
             children: Vec<Value>, source: &str, evidence: &str) -> Value {
    let a = ms_to_str(start_ms);
    let b = ms_to_str(end_ms);
    let kids: Vec<Value> = children.iter().rev().take(18).rev().cloned().collect();
    let mut m = Map::new();
    m.insert("id".into(), Value::String(format!("tr{}", rid_tail(rid, 10))));
    m.insert("task".into(), Value::String(clip(task, 320)));
    m.insert("detail".into(), Value::String(clip(detail, 160)));
    m.insert(
        "rest".into(),
        Value::String(clip(if evidence.is_empty() { detail } else { evidence }, 160)),
    );
    m.insert("status".into(), Value::String("ok".into()));
    m.insert("start".into(), Value::String(a.clone()));
    m.insert("end".into(), Value::String(b.clone()));
    m.insert(
        "secs".into(),
        match secs(&a, &b) {
            Some(v) => json!(v),
            None => Value::Null,
        },
    );
    m.insert(
        "sources".into(),
        json!([{"kind": "tool", "name": "TRAE"}, {"kind": "code", "name": source}]),
    );
    m.insert("facets".into(), json!({}));
    m.insert(
        "ctx_labels".into(),
        json!({"agent": "代理", "skills": "技能", "connectors": "MCP"}),
    );
    m.insert("client".into(), Value::String("TRAE".into()));
    m.insert("children".into(), Value::Array(kids.clone()));
    m.insert("more_steps".into(), json!((children.len() as i64 - kids.len() as i64).max(0)));
    m.insert("project".into(), Value::String(source.to_string()));
    m.insert("tokens".into(), json!({"in": 0, "out": 0, "cached": 0, "total": 0}));
    Value::Object(m)
}

fn trae_ep(s: &str) -> f64 {
    match chrono::NaiveDateTime::parse_from_str(s.get(..19).unwrap_or(""), "%Y-%m-%d %H:%M:%S") {
        Ok(d) => d.and_local_timezone(chrono::Local).single().map(|t| t.timestamp_millis() as f64).unwrap_or(0.0),
        Err(_) => 0.0,
    }
}

fn proj_from_memdir(name: &str) -> String {
    let head = name.split("--p2-").next().unwrap_or("");
    let segs: Vec<&str> = head.split('-').filter(|s| !s.is_empty()).collect();
    match segs.last() {
        Some(last) if last.len() >= 2 && last.is_ascii() => last.to_string(),
        _ => "TRAE".to_string(),
    }
}

/// trae_work.items：会话记忆 + 自述（命令证据/进程时段/OCR 补录另有一份实现，见文档）。
pub fn trae_items(root: &Path, since_ms: f64) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    // —— 会话记忆（明文 jsonl）——
    // 照 `trae_work._mem_rows()`：只认 MEM_ROOT，按 `message_id`（缺则 ts+intent）
    // 去重——同一条记忆可能落进两个目录，不去重会多出一截「会话数」。
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for home in home_dirs() {
        let root = home.join(".trae-cn").join("memory").join("projects");
        if !root.is_dir() {
            continue;
        }
        let mut files: Vec<PathBuf> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&root) {
            for e in rd.flatten() {
                let d2 = e.path();
                if !d2.is_dir() {
                    continue;
                }
                if let Ok(rd2) = std::fs::read_dir(&d2) {
                    for e2 in rd2.flatten() {
                        let d3 = e2.path();
                        let nm3 = d3.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                        // 只认 `[0-9]*` 那一层
                        if !nm3.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
                            continue;
                        }
                        if let Ok(rd3) = std::fs::read_dir(&d3) {
                            for e3 in rd3.flatten() {
                                let f = e3.path();
                                let nm = f.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                                if nm.starts_with("session_memory_") && nm.ends_with(".jsonl") {
                                    files.push(f);
                                }
                            }
                        }
                    }
                }
            }
        }
        files.sort();
        for p in files {
            if mtime_s(&p) * 1000.0 < since_ms {
                continue;
            }
            let text = match std::fs::read(&p) {
                Ok(b) => String::from_utf8_lossy(&b).to_string(),
                Err(_) => continue,
            };
            let proj = proj_from_memdir(
                &p.parent()
                    .and_then(|x| x.parent())
                    .and_then(|x| x.file_name())
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default(),
            );
            for ln in text.lines() {
                let ln = ln.trim();
                if ln.is_empty() {
                    continue;
                }
                let o: Value = match serde_json::from_str(ln) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if !o.is_object() {
                    continue;
                }
                let ts = str_or(o.get("message_summary_time"));
                if ts.is_empty() {
                    continue;
                }
                let raw_intent = str_or(o.get("intent"));
                let intent = raw_intent.trim().to_string();
                let key = {
                    let mid = str_or(o.get("message_id"));
                    if mid.is_empty() { format!("{}{}", ts, raw_intent) } else { mid }
                };
                if seen.contains(&key) {
                    continue;
                }
                seen.insert(key);
                let end_ms = trae_ep(&ts);
                if end_ms == 0.0 || end_ms < since_ms {
                    continue;
                }
                let acts: Vec<String> = match o.get("actions") {
                    Some(Value::Array(a)) => a
                        .iter()
                        .map(|x| py_str(x).trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect(),
                    _ => Vec::new(),
                };
                let learned: Vec<String> = match o.get("learned") {
                    Some(Value::Array(a)) => a
                        .iter()
                        .map(|x| py_str(x).trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect(),
                    _ => Vec::new(),
                };
                let outcome = str_or(o.get("outcome")).split_whitespace().collect::<Vec<_>>().join(" ");
                let kids: Vec<Value> = acts
                    .iter()
                    .take(8)
                    .map(|a| json!({"start": ts, "detail": clip(a, 120)}))
                    .collect();
                let detail = if outcome.is_empty() {
                    "只有意图，没留下结果".to_string()
                } else {
                    format!("结果：{}", outcome)
                };
                let ev = if learned.is_empty() {
                    "取自 TRAE 会话记忆（明文），不是猜的".to_string()
                } else {
                    format!("经验：{}", learned.iter().take(2).cloned().collect::<Vec<_>>().join(" / "))
                };
                let sid: String = str_or(o.get("message_id")).chars().take(8).collect();
                let rid = if sid.is_empty() {
                    let n = end_ms as i64;
                    let s = format!("{}", n);
                    let tail: String = {
                        let c: Vec<char> = s.chars().collect();
                        let st = c.len().saturating_sub(8);
                        c[st..].iter().collect()
                    };
                    format!("mem{}", tail)
                } else {
                    format!("mem{}", sid)
                };
                let mut it = trae_item(
                    &rid,
                    if intent.is_empty() { "（TRAE 未写意图）" } else { &intent },
                    &detail,
                    end_ms,
                    end_ms,
                    kids,
                    "TRAE 记忆",
                    &ev,
                );
                if let Value::Object(m) = &mut it {
                    m.insert("project".into(), Value::String(proj.clone()));
                }
                out.push(it);
            }
        }
    }
    // —— 自述（Agent自述 / TRAE自述）——
    for (p, fallback) in [
        (root.join("_本机工作").join("Agent自述.jsonl"), "未知"),
        (root.join("_本机工作").join("TRAE自述.jsonl"), "TRAE"),
    ] {
        if !p.exists() {
            continue;
        }
        let text = match std::fs::read_to_string(&p) {
            Ok(t) => t,
            Err(_) => continue,
        };
        for ln in text.lines() {
            let ln = ln.trim();
            if ln.is_empty() {
                continue;
            }
            let o: Value = match serde_json::from_str(ln) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let t = o.get("at").and_then(|v| v.as_f64()).unwrap_or(0.0) * 1000.0;
            if t < since_ms {
                continue;
            }
            let task = str_or(o.get("task")).trim().to_string();
            if task.is_empty() {
                continue;
            }
            let who = {
                let w = str_or(o.get("agent")).trim().to_string();
                if w.is_empty() { fallback.to_string() } else { w }
            };
            let summary = str_or(o.get("summary")).split_whitespace().collect::<Vec<_>>().join(" ");
            let proj = str_or(o.get("project")).trim().to_string();
            let rid = format!(
                "note{}",
                format!("{}", t as i64).chars().rev().take(8).collect::<String>().chars().rev().collect::<String>()
            );
            let detail = if summary.is_empty() {
                format!("{} 主动上报，没写摘要", who)
            } else {
                summary
            };
            let mut it = trae_item(
                &rid,
                &task,
                &detail,
                t,
                t,
                Vec::new(),
                &format!("{} 自述", who),
                &if proj.is_empty() { format!("{} 收尾自报", who) } else { proj.clone() },
            );
            if let Value::Object(m) = &mut it {
                if !proj.is_empty() {
                    m.insert("project".into(), Value::String(proj));
                }
            }
            out.push(it);
        }
    }
    out.sort_by(|a, b| {
        let ka = format!("{}{}", str_or(a.get("end")), str_or(a.get("start")));
        let kb = format!("{}{}", str_or(b.get("end")), str_or(b.get("start")));
        kb.cmp(&ka)
    });
    out
}

fn home_dirs() -> Vec<PathBuf> {
    match std::env::var("HOME") {
        Ok(h) => vec![PathBuf::from(h)],
        Err(_) => Vec::new(),
    }
}

// ------------------------------------------------------------------ 任务短标题

const TITLE_ACTIONS: [(&str, &str); 13] = [
    ("移到|挪到|放在|放到|插回|拔掉|弹出|换位置|移动位置|拖动|调大小|缩放", "调整"),
    ("谁在做|谁做的|是不是.*做的|到底是谁", "核查"),
    ("确认|核查|查下|查一下|检查下|检查一下|排查|看看为什么|看下为什么|查清", "核查"),
    ("修复|修好|修正|改正|恢复|解决", "修复"),
    ("新增|增加|添加|加上|加一个|加个|新建|做一个|做成", "新增"),
    ("移除|去掉|删掉|删除|拿掉|不用再|取消", "移除"),
    ("改成|改为|换成|调整|修改|优化|重做|重新设计|重构|第二行", "调整"),
    ("补充|填进去|录入|补录|记录下|也记录|也要显示", "补充"),
    ("设计|先别做|先分析|想想|想个办法|出个方案|给个思路", "设计"),
    ("显示|展示", "显示"),
    ("记录|追踪|监控|统计", "记录"),
    ("下载|抓取|爬取|拉取|采集|刮削", "采集"),
    ("导入|迁移|放到|转移", "迁移"),
];

const TITLE_TOPICS_SPEC: [&str; 69] = [
    "外置磁盘", "外置盘", "外挂的磁盘", "移动硬盘", "项目token", "token", "点数", "折算",
    "显存", "耗电", "功耗", "网速", "网络", "磁盘", "内存", "大模型颜色", "模型颜色",
    "任务清单", "工作流水", "逐轮记录", "具体工作记录", "工作记录", "磁盘使用",
    "指标列", "资源视图", "媒体墙", "电影墙", "海报墙", "电视墙", "海报", "我的观看",
    "打分", "评分", "收藏", "打tag", "存图", "存文章", "文章", "图片", "ocr",
    "微信", "链接", "下载工作流", "文件传输", "记账", "行程", "待办", "日记", "备忘",
    "关注追踪", "巡检", "日报周报", "报告", "封面", "标签", "分类", "回收站", "command z",
    "claude code", "cherry studio", "deepseek", "4b模型", "4b", "nas", "zip", "github",
    "模型", "专家", "模式",
];

const TITLE_TOPICS_GENERIC: [&str; 20] = [
    "agent 监控", "agent监控", "本机工作", "口袋工作", "工作台", "页面", "卡片", "布局", "视图", "表格", "大小位置",
    "任务名", "项目名称", "日期", "顶区", "agent", "workbuddy", "pocket", "trae", "技能",
];

fn rx_title_stop() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        re(r#"https?://\S+|@[^\s，。；、]{2,}|#[^\s，。；、]{2,}|[（(][^）)]{0,40}[）)]|^\s*\d+[)）]\s*"#)
    })
}

/// `_task_title`：一句用户输入 → 短标题（「调整·外置盘」）。
pub fn task_title(text: &str) -> String {
    let mut s = text.trim().to_string();
    if s.is_empty() {
        return "未命名任务".to_string();
    }
    s = rx_title_stop().replace_all(&s, " ").to_string();
    s = re(r#"["“”'‘’《》<>.。，、；;：:！!？?~…\-—_/\\]+"#).replace_all(&s, " ").to_string();
    s = rx_ws().replace_all(&s, " ").trim().to_string();
    if s.is_empty() {
        return "未命名任务".to_string();
    }
    let low = s.to_lowercase();
    let mut action = String::new();
    for (pat, label) in TITLE_ACTIONS.iter() {
        if re_cached(pat).is_match(&low) {
            action = label.to_string();
            break;
        }
    }
    let pick = |words: &[&str]| -> String {
        let mut hits: Vec<(usize, isize, String)> = Vec::new();
        for w in words {
            if let Some(i) = low.find(&w.to_lowercase()) {
                hits.push((i, -(w.chars().count() as isize), w.to_string()));
            }
        }
        hits.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        hits.first().map(|x| x.2.clone()).unwrap_or_default()
    };
    let mut topic = pick(&TITLE_TOPICS_SPEC);
    if topic.is_empty() {
        topic = pick(&TITLE_TOPICS_GENERIC);
    }
    let title = if !topic.is_empty() && !action.is_empty() {
        if action == "核查" && re(r"谁在做|谁做的|是不是.*做的|到底是谁|找到的吗").is_match(&low) {
            topic = "任务归属".to_string();
        }
        format!("{}·{}", action, topic)
    } else if !topic.is_empty() {
        topic
    } else if !action.is_empty() {
        format!("{}·{}", action, s.chars().take(10).collect::<String>())
    } else {
        let s2 = re(r"^(然后|接着|另外|就是|这个|那个|你看|你看下|帮我|我想|我现在|现在|如果|有个)")
            .replace(&s, "")
            .to_string();
        s2.trim().chars().take(14).collect()
    };
    let title = rx_ws().replace_all(&title, "").to_string();
    if title.chars().count() > 18 {
        format!("{}…", title.chars().take(18).collect::<String>())
    } else {
        title
    }
}

// ------------------------------------------------------------------ 会话索引（空间 / 名字 / 生死）

struct SidxCache {
    at: f64,
    rows: Map<String, Value>,
}

static SIDX: Mutex<Option<SidxCache>> = Mutex::new(None);

/// hostwork._session_index：{session_id: {title, deleted, deleted_at, space}}。
pub fn session_index(ttl: f64) -> Map<String, Value> {
    let now = now_secs();
    {
        let g = SIDX.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = g.as_ref() {
            if !c.rows.is_empty() && now - c.at < ttl {
                return c.rows.clone();
            }
        }
    }
    let home = match std::env::var("HOME") {
        Ok(h) => PathBuf::from(h),
        Err(_) => return Map::new(),
    };
    let db = home.join(".workbuddy").join("workbuddy.db");
    let mut rows: Map<String, Value> = Map::new();
    let sql = "SELECT id, title, custom_title, deleted_at, space_id FROM sessions";
    let sp_sql = "SELECT id, name FROM spaces";
    if let Some(sp) = platform::sqlite_rows(&db, sp_sql, Duration::from_millis(1500)) {
        let mut spaces: HashMap<String, String> = HashMap::new();
        for r in sp {
            spaces.insert(str_or(r.get("id")), str_or(r.get("name")));
        }
        if let Some(rs) = platform::sqlite_rows(&db, sql, Duration::from_millis(1500)) {
            for r in rs {
                let sid = str_or(r.get("id"));
                let title = {
                    let c = str_or(r.get("custom_title"));
                    if c.trim().is_empty() { str_or(r.get("title")) } else { c }
                };
                let del = int_of(r.get("deleted_at"));
                let spid = str_or(r.get("space_id"));
                rows.insert(
                    sid,
                    json!({
                        "title": title.trim(),
                        "deleted": del > 0,
                        "deleted_at": del,
                        "space": spaces.get(&spid).cloned().unwrap_or_default(),
                    }),
                );
            }
        }
    }
    if !rows.is_empty() {
        let mut g = SIDX.lock().unwrap_or_else(|e| e.into_inner());
        *g = Some(SidxCache { at: now, rows: rows.clone() });
    }
    rows
}

// ------------------------------------------------------------------ 汇总：本机的活

pub const WL_HOST_DAYS: i64 = 2;
pub const WL_HOST_LIMIT: usize = 80;

/// `_agent_items`：本机上所有 AI 客户端的活，合成一份（hostwork.rounds_items 口径）。
pub fn agent_items(root: &Path, days: i64) -> Vec<Value> {
    let since_ms = (now_secs() - days as f64 * 86400.0) * 1000.0;
    let mut items: Vec<Value> = Vec::new();
    items.extend(wb_items(since_ms));
    items.extend(cherry_items(since_ms));
    items.extend(claude_items(since_ms));
    items.extend(trae_items(root, since_ms));
    items.sort_by(|a, b| {
        let ka = {
            let e = str_or(a.get("end"));
            if e.is_empty() { str_or(a.get("start")) } else { e }
        };
        let kb = {
            let e = str_or(b.get("end"));
            if e.is_empty() { str_or(b.get("start")) } else { e }
        };
        kb.cmp(&ka)
    });
    let keep_keys = [
        "id", "sid", "task", "detail", "rest", "status", "start", "end", "secs", "t0",
        "sources", "facets", "ctx_labels", "client", "children", "more_steps", "project",
        "tokens",
    ];
    let sidx = session_index(20.0);
    let mut out: Vec<Value> = Vec::new();
    for it in items.iter().take(WL_HOST_LIMIT) {
        let mut m = Map::new();
        for k in keep_keys {
            if let Some(v) = it.get(k) {
                m.insert(k.to_string(), v.clone());
            }
        }
        m.insert("title".into(), Value::String(task_title(&str_or(it.get("task")))));
        let meta = sidx.get(&str_or(it.get("sid"))).cloned().unwrap_or_else(|| json!({}));
        m.insert("space".into(), Value::String(str_or(meta.get("space"))));
        m.insert("sess_title".into(), Value::String(str_or(meta.get("title"))));
        m.insert(
            "sess_gone".into(),
            Value::Bool(meta.get("deleted").map(truthy).unwrap_or(false)),
        );
        out.push(Value::Object(m));
    }
    out
}
