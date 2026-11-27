// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：`moshan/agent_link.py` 的 Rust 版 —— 把别的 Agent 接进中台的四步链：
//       认人（scan）→ 装说明（install / prompt）→ 收回报（report）→ 按回执导入（import）。
// 不做：`auto_install`（启动时自动投放，没有 HTTP 路由在用）；不猜别家的会话格式
//       （Python 也不猜，只列目录里有什么）。
//
// 与 Python 对齐的几处硬口径：
//   · links.json 用 `indent=2`（`_save_links`），worklog.json 用 `indent=1`；
//   · scan 结果缓存 20 秒，键是 `<root>|<time//20>`，没有 force 参数；
//   · 说明书正文由 `_PROMPT.format(agent=..., base=...)` 生成，这里用
//     `prompt_template.md` 的两个占位符替换 —— 8 家逐字节已核过；
//   · `import` 只认 trae 一家，别的给「不做日志导入」的原话。

use super::bus;
use super::util::{self, collapse, cut, pystr, s_or, truthy};
use crate::paths::Ctx;
use crate::routes::common::{peek, query_map, CtxState};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{Local, NaiveDateTime, TimeZone};
use serde_json::{json, Map, Value};
use sha1::{Digest, Sha1};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub fn router() -> Router<Arc<Ctx>> {
    Router::new()
        .route("/api/agent/link/prompt", get(prompt_route))
        .route("/api/agent/link/scan", get(scan_route))
        .route("/api/agent/link/import", post(import_route))
        .route("/api/agent/link/install", post(install_route))
        .route("/api/agent/link/report", post(report_route))
}

const V: i64 = 1;
const MAX_REPORT_PATHS: usize = 40;
const SCAN_TTL: i64 = 20;

// ---------------------------------------------------------------- 认人表

struct AgentSpec {
    id: &'static str,
    name: &'static str,
    kind: &'static str,
    note: &'static str,
    darwin: &'static [(&'static str, &'static str)],
    win: &'static [(&'static str, &'static str)],
    linux: &'static [(&'static str, &'static str)],
    any: &'static [(&'static str, &'static str)],
    skill: (&'static str, &'static str),
}

const AGENTS: &[AgentSpec] = &[
    AgentSpec {
        id: "trae",
        name: "Trae",
        kind: "skill",
        note: "对话正文在加密库里，但 memory/ 下的会话记忆是明文",
        darwin: &[
            ("home", ".trae-cn"),
            ("appsupport", "Trae CN"),
            ("appsupport", "Trae"),
        ],
        win: &[("home", ".trae-cn"), ("appdata", "Trae CN"), ("appdata", "Trae")],
        linux: &[("home", ".trae-cn")],
        any: &[],
        skill: ("home", ".trae-cn/skills/pocket-bridge/SKILL.md"),
    },
    AgentSpec {
        id: "cursor",
        name: "Cursor",
        kind: "rule",
        note: "规则是 .cursor/rules/*.mdc，alwaysApply 才会每轮带上",
        darwin: &[("home", ".cursor"), ("appsupport", "Cursor")],
        win: &[("home", ".cursor"), ("appdata", "Cursor")],
        linux: &[("home", ".cursor")],
        any: &[],
        skill: ("home", ".cursor/rules/pocket-bridge.mdc"),
    },
    AgentSpec {
        id: "claude",
        name: "Claude Code",
        kind: "skill",
        note: "技能目录 ~/.claude/skills/",
        darwin: &[],
        win: &[],
        linux: &[],
        any: &[("home", ".claude")],
        skill: ("home", ".claude/skills/pocket-bridge/SKILL.md"),
    },
    AgentSpec {
        id: "workbuddy",
        name: "WorkBuddy",
        kind: "skill",
        note: "技能目录 ~/.workbuddy/skills/",
        darwin: &[],
        win: &[],
        linux: &[],
        any: &[("home", ".workbuddy")],
        skill: ("home", ".workbuddy/skills/pocket-bridge/SKILL.md"),
    },
    AgentSpec {
        id: "codex",
        name: "Codex",
        kind: "skill",
        note: "会话在 ~/.codex/sessions 下按年月日分目录，rollout-*.jsonl；技能目录 ~/.codex/skills/",
        darwin: &[],
        win: &[],
        linux: &[],
        any: &[("home", ".codex")],
        skill: ("home", ".codex/skills/pocket-bridge/SKILL.md"),
    },
    AgentSpec {
        id: "gemini",
        name: "Gemini CLI",
        kind: "skill",
        note: "配置与技能都在 ~/.gemini/",
        darwin: &[],
        win: &[],
        linux: &[],
        any: &[("home", ".gemini")],
        skill: ("home", ".gemini/skills/pocket-bridge/SKILL.md"),
    },
    AgentSpec {
        id: "opencode",
        name: "OpenCode",
        kind: "skill",
        note: "技能目录是单数 ~/.config/opencode/skill/",
        darwin: &[],
        win: &[],
        linux: &[],
        any: &[("home", ".config/opencode"), ("home", ".opencode")],
        skill: ("home", ".config/opencode/skill/pocket-bridge/SKILL.md"),
    },
    AgentSpec {
        id: "deepseek",
        name: "DeepSeek Harness",
        kind: "skill",
        note: "自建 harness，目录不固定：装没装看 ~/.deepseek*，没有就用「自报路径」接进来",
        darwin: &[],
        win: &[],
        linux: &[],
        any: &[
            ("home", ".deepseek"),
            ("home", ".deepseek-harness"),
            ("home", ".config/deepseek"),
        ],
        skill: ("home", ".deepseek/skills/pocket-bridge/SKILL.md"),
    },
];

fn plat() -> &'static str {
    if cfg!(target_os = "windows") {
        "win"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    }
}

/// `_homes()`：非 Windows 只有家目录本身（去重按小写）。
fn homes() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut push = |p: PathBuf, out: &mut Vec<PathBuf>| {
        let k = p.display().to_string().to_lowercase();
        if seen.insert(k) {
            out.push(p);
        }
    };
    push(util::home(), &mut out);
    if cfg!(target_os = "windows") {
        let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string());
        let users = PathBuf::from(format!("{}/Users", drive));
        if let Ok(rd) = std::fs::read_dir(&users) {
            for e in rd.flatten() {
                let d = e.path();
                let low = d
                    .file_name()
                    .map(|s| s.to_string_lossy().to_lowercase())
                    .unwrap_or_default();
                if d.is_dir()
                    && ![
                        "public",
                        "default",
                        "default user",
                        "all users",
                        "defaultapppool",
                    ]
                    .contains(&low.as_str())
                {
                    push(d, &mut out);
                }
            }
        }
    }
    out
}

fn root_of(kind: &str) -> Option<PathBuf> {
    match kind {
        "home" => Some(util::home()),
        "appdata" => std::env::var_os("APPDATA").map(PathBuf::from),
        "appsupport" => Some(util::home().join("Library").join("Application Support")),
        "localappdata" => std::env::var_os("LOCALAPPDATA").map(PathBuf::from),
        _ => None,
    }
}

/// `_expand(spec)`：home/appdata/localappdata 按每个家目录推，其余按固定根。
fn expand(spec: (&'static str, &'static str)) -> Vec<PathBuf> {
    let (kind, sub) = spec;
    if matches!(kind, "home" | "appdata" | "localappdata") {
        let sub_dir = match kind {
            "appdata" => "AppData/Roaming",
            "localappdata" => "AppData/Local",
            _ => "",
        };
        let mut out = Vec::new();
        for h in homes() {
            let base = if sub_dir.is_empty() {
                h.clone()
            } else {
                h.join(sub_dir)
            };
            out.push(if sub.is_empty() { base } else { base.join(sub) });
        }
        return out;
    }
    match root_of(kind) {
        Some(base) => vec![if sub.is_empty() { base } else { base.join(sub) }],
        None => Vec::new(),
    }
}

fn roots_of(a: &AgentSpec) -> Vec<PathBuf> {
    let table = match plat() {
        "darwin" => a.darwin,
        "win" => a.win,
        _ => a.linux,
    };
    let spec = if table.is_empty() { a.any } else { table };
    let mut out = Vec::new();
    for s in spec {
        out.extend(expand(*s));
    }
    out
}

/// `_skill_path`：挑那个「首段目录真存在」的家目录；都不存在退回第一个。
fn skill_path(a: &AgentSpec) -> Option<PathBuf> {
    let cands = expand(a.skill);
    for p in &cands {
        for h in homes() {
            let Ok(rel) = p.strip_prefix(&h) else { continue };
            if let Some(first) = rel.iter().next() {
                if h.join(first).exists() {
                    return Some(p.clone());
                }
            }
        }
    }
    cands.into_iter().next()
}

// ---------------------------------------------------------------- 目录量体

fn fmt_hm(secs: f64) -> String {
    if secs <= 0.0 {
        return String::new();
    }
    util::strftime_secs(secs as i64, "%Y-%m-%d %H:%M")
}

fn mtime_secs(md: &std::fs::Metadata) -> f64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// `Path.rglob(pat)` 的产出顺序（3.13 实测，不是朴素 DFS）：
///
///   1. 先把 base 的**直接子项**按 scandir 顺序过一遍；
///   2. 再用一个**栈**下钻：弹出目录 → 依次「产出它每个子目录的匹配子项、把该子目录入栈」，
///      所以同级目录里**后出现的先展开**（`rg2` 那棵树里 b/y 先于 a/x）。
///
/// 顺序是契约的一部分：cap=5000 截断时先数到哪一批直接决定 files/mb 两个数。
/// `*/*` 是另一种口径（`**` 后面跟两段），单独走 two 分支。
struct RgWalk<'a> {
    i: usize,
    cap: usize,
    stopped: bool,
    f: &'a mut dyn FnMut(&Path),
}

impl RgWalk<'_> {
    fn emit(&mut self, p: &Path) {
        if self.stopped {
            return;
        }
        if self.i >= self.cap {
            self.stopped = true;
            return;
        }
        self.i += 1;
        (self.f)(p);
    }
}

fn scan_paths(dir: &Path) -> Vec<PathBuf> {
    match std::fs::read_dir(dir) {
        Ok(rd) => rd.flatten().map(|e| e.path()).collect(),
        Err(_) => Vec::new(),
    }
}

fn scan_dirs(dir: &Path) -> Vec<PathBuf> {
    scan_paths(dir)
        .into_iter()
        .filter(|p| {
            // rglob 默认不跟指向目录的符号链接（entry.is_dir(follow_symlinks=False)）
            std::fs::symlink_metadata(p)
                .map(|m| m.is_dir())
                .unwrap_or(false)
        })
        .collect()
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// 按 `Path.rglob(pat)` 的口径逐个产出，最多 cap 个。返回是否被 cap 截断。
fn rglob_each(base: &Path, pat: &str, cap: usize, f: &mut dyn FnMut(&Path)) -> bool {
    let two = pat == "*/*";
    let mut st = RgWalk { i: 0, cap, stopped: false, f };
    if two {
        // `**/*/*`：先给每个一级子目录的**全部**直接子项
        for d in scan_dirs(base) {
            for c in scan_paths(&d) {
                st.emit(&c);
                if st.stopped {
                    return true;
                }
            }
        }
    } else {
        for c in scan_paths(base) {
            if name_match(pat, &file_name(&c)) {
                st.emit(&c);
                if st.stopped {
                    return true;
                }
            }
        }
    }
    let mut stack = vec![base.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in scan_dirs(&dir) {
            if two {
                for g in scan_dirs(&e) {
                    for c in scan_paths(&g) {
                        st.emit(&c);
                        if st.stopped {
                            return true;
                        }
                    }
                }
            } else {
                for c in scan_paths(&e) {
                    if name_match(pat, &file_name(&c)) {
                        st.emit(&c);
                        if st.stopped {
                            return true;
                        }
                    }
                }
            }
            stack.push(e);
        }
    }
    st.stopped
}

/// `_walk_stats(p, cap=5000)`：文件数 / 体积 / 最新修改；走不完按走到的算。
fn walk_stats(p: &Path, cap: usize) -> Value {
    if !p.exists() {
        return json!({"files": 0, "mb": 0.0, "newest": "", "stopped": false});
    }
    let mut files: i64 = 0;
    let mut size: i64 = 0;
    let mut newest: f64 = 0.0;
    let stopped = rglob_each(p, "*", cap, &mut |f| {
        // `f.is_file()` 跟符号链接（Path.is_file 默认跟），所以这里也 use metadata
        let Ok(md) = std::fs::metadata(f) else { return };
        if md.is_file() {
            files += 1;
            size += md.len() as i64;
            let m = mtime_secs(&md);
            if m > newest {
                newest = m;
            }
        }
    });
    let mb = util::py_round(size as f64 / 1048576.0, 1);
    let mut o = Map::new();
    o.insert("files".into(), json!(files));
    o.insert("mb".into(), json!(mb));
    o.insert("stopped".into(), json!(stopped));
    o.insert("newest".into(), json!(fmt_hm(newest)));
    Value::Object(o)
}

/// 只支持 `*` 的单段文件名匹配（本文件只用到 `session_memory_*.jsonl` / `*.jsonl` / `*.json`）。
fn name_match(pat: &str, name: &str) -> bool {
    let parts: Vec<&str> = pat.split('*').collect();
    if parts.len() == 1 {
        return pat == name;
    }
    if !name.starts_with(parts[0]) {
        return false;
    }
    let mut rest = &name[parts[0].len()..];
    let last = parts.len() - 1;
    for (i, seg) in parts.iter().enumerate().skip(1) {
        if seg.is_empty() {
            continue;
        }
        let Some(pos) = rest.find(seg) else { return false };
        if i == last && rest.len() != pos + seg.len() {
            return false;
        }
        rest = &rest[pos + seg.len()..];
    }
    true
}

/// `_count(pattern, base, cap=5000)`：数匹配条数与最新时间。
/// 顺序沿用 `Path.rglob`（见 rglob_each 的头注释），cap 数的是**产出的匹配项**。
fn count(pattern: &str, base: &Path, cap: usize) -> (i64, String) {
    let mut n: i64 = 0;
    let mut newest: f64 = 0.0;
    rglob_each(base, pattern, cap, &mut |f| {
        n += 1;
        if let Ok(md) = std::fs::metadata(f) {
            let m = mtime_secs(&md);
            if m > newest {
                newest = m;
            }
        }
    });
    (n, fmt_hm(newest))
}

// ---------------------------------------------------------------- 证据点

fn trae_evidence() -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for base in roots_of(&AGENTS[0]) {
        if base.file_name().map(|s| s == ".trae-cn").unwrap_or(false) {
            let mem = base.join("memory").join("projects");
            if mem.exists() {
                let (n, at) = count("session_memory_*.jsonl", &mem, 5000);
                out.push(json!({
                    "key": "memory", "label": "会话记忆",
                    "path": mem.display().to_string(),
                    "what": "intent/actions/outcome/learned，明文，能直接导入",
                    "n": n, "at": at,
                }));
            }
            for (sub, label) in [
                ("mcps", "MCP 任务"),
                ("skills", "技能"),
                ("builtin", "内置映射"),
            ] {
                let p = base.join(sub);
                if p.exists() {
                    out.push(json!({
                        "key": sub, "label": label,
                        "path": p.display().to_string(), "what": "辅助",
                    }));
                }
            }
        } else {
            let cmd = base
                .join("ModularData")
                .join("ai-agent")
                .join("command-execution");
            if cmd.exists() {
                let (n, at) = count("*/*", &cmd, 2000);
                out.push(json!({
                    "key": "commands", "label": "命令执行",
                    "path": cmd.display().to_string(),
                    "what": "每条命令的 cwd / 起止时间，能算「真干了多久」",
                    "n": n, "at": at,
                }));
            }
            let db = base.join("ModularData").join("ai-agent").join("database.db");
            if db.exists() {
                let size = std::fs::metadata(&db).map(|m| m.len()).unwrap_or(0);
                let head: Vec<u8> = std::fs::read(&db)
                    .map(|b| b.into_iter().take(4).collect())
                    .unwrap_or_default();
                let hex: String = head.iter().map(|b| format!("{:02x}", b)).collect();
                let what = if head.is_empty() {
                    "整库加密，读不到正文".to_string()
                } else {
                    format!("整库加密（文件头 {}），读不到正文，只能拿体积和时间", hex)
                };
                out.push(json!({
                    "key": "db", "label": "对话库",
                    "path": db.display().to_string(),
                    "what": what, "locked": true,
                    "mb": util::py_round(size as f64 / 1048576.0, 1),
                }));
            }
        }
    }
    out
}

fn evidence_generic(a: &AgentSpec) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for base in roots_of(a) {
        if !base.exists() {
            continue;
        }
        for (sub, label) in [
            ("sessions", "会话记录"),
            ("projects", "项目会话"),
            ("chats", "对话"),
            ("history", "历史"),
            ("logs", "日志"),
        ] {
            let p = base.join(sub);
            if !p.is_dir() {
                continue;
            }
            let (mut n, mut at) = count("*.jsonl", &p, 5000);
            if n == 0 {
                let (n2, at2) = count("*.json", &p, 5000);
                n = n2;
                at = at2;
            }
            out.push(json!({
                "key": sub, "label": label,
                "path": p.display().to_string(),
                "what": "明文，能读；格式各家不同，导入靠它自报", "n": n, "at": at,
            }));
        }
        for (fname, label) in [
            ("history.jsonl", "历史记录"),
            ("config.toml", "配置"),
            ("config.json", "配置"),
            ("state.json", "状态"),
        ] {
            let p = base.join(fname);
            if !p.exists() {
                continue;
            }
            let at = std::fs::metadata(&p)
                .map(|m| fmt_hm(mtime_secs(&m)))
                .unwrap_or_default();
            out.push(json!({
                "key": format!("file-{}", fname), "label": label,
                "path": p.display().to_string(),
                "what": "配置文件，能看出它把数据放哪", "at": at,
            }));
        }
    }
    out
}

// ---------------------------------------------------------------- links.json

fn load_links(root: &Path) -> Map<String, Value> {
    match util::load_json(&bus::links_path(root)) {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    }
}

fn save_links(root: &Path, d: &Map<String, Value>) {
    let p = bus::links_path(root);
    let _ = util::ensure_parent(&p);
    let tmp = p.with_file_name("links.json.tmp");
    if super::write_json_indent(&tmp, &Value::Object(d.clone()), 2).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}

fn now_hm() -> String {
    util::strftime_now("%Y-%m-%d %H:%M")
}

// ---------------------------------------------------------------- scan

static SCAN_CACHE: Mutex<Option<(String, Value)>> = Mutex::new(None);

fn scan(root: &Path) -> Value {
    let key = format!("{}|{}", root.display(), util::now_secs() / SCAN_TTL);
    if let Ok(g) = SCAN_CACHE.lock() {
        if let Some((k, v)) = g.as_ref() {
            if *k == key {
                return v.clone();
            }
        }
    }
    let links = load_links(root);
    let mut items: Vec<Value> = Vec::new();
    for a in AGENTS {
        let mut roots: Vec<Value> = Vec::new();
        for r in roots_of(a) {
            if !r.exists() {
                roots.push(json!({"path": r.display().to_string(), "exists": false}));
                continue;
            }
            let st = walk_stats(&r, 5000);
            let mut m = Map::new();
            m.insert("path".into(), json!(r.display().to_string()));
            m.insert("exists".into(), json!(true));
            if let Some(o) = st.as_object() {
                for (k, v) in o {
                    m.insert(k.clone(), v.clone());
                }
            }
            roots.push(Value::Object(m));
        }
        let sp = skill_path(a);
        let mut skill = Map::new();
        skill.insert(
            "path".into(),
            json!(sp.as_ref().map(|p| p.display().to_string()).unwrap_or_default()),
        );
        let installed = sp.as_ref().map(|p| p.exists()).unwrap_or(false);
        skill.insert("installed".into(), json!(installed));
        let mut at = String::new();
        if installed {
            if let Some(p) = &sp {
                if let Ok(md) = std::fs::metadata(p) {
                    at = fmt_hm(mtime_secs(&md));
                }
            }
        }
        skill.insert("at".into(), json!(at));

        let ev = if a.id == "trae" {
            trae_evidence()
        } else {
            evidence_generic(a)
        };
        let evidence: Vec<Value> = ev
            .iter()
            .map(|e| {
                json!({
                    "key": util::get(e, "key"),
                    "label": util::get(e, "label"),
                    "path": util::get(e, "path"),
                    "what": s_or(util::get(e, "what")),
                    "locked": truthy(util::get(e, "locked")),
                    "n": e.get("n").cloned().unwrap_or(Value::Null),
                    "at": s_or(util::get(e, "at")),
                })
            })
            .collect();
        let rec = links.get(a.id).cloned().unwrap_or(Value::Null);
        let report = rec.get("report").cloned().unwrap_or(Value::Null);
        let import = rec.get("import").cloned().unwrap_or(Value::Null);
        let mut m = Map::new();
        m.insert("id".into(), json!(a.id));
        m.insert("name".into(), json!(a.name));
        m.insert("kind".into(), json!(a.kind));
        m.insert("note".into(), json!(a.note));
        m.insert(
            "found".into(),
            json!(roots
                .iter()
                .any(|x| x.get("exists").and_then(|v| v.as_bool()).unwrap_or(false))),
        );
        m.insert("roots".into(), Value::Array(roots));
        m.insert("skill".into(), Value::Object(skill));
        m.insert("evidence".into(), Value::Array(evidence));
        m.insert(
            "report".into(),
            if truthy(&report) { report } else { json!({}) },
        );
        m.insert(
            "import".into(),
            if truthy(&import) { import } else { json!({}) },
        );
        items.push(Value::Object(m));
    }
    let mut out = Map::new();
    out.insert("ok".into(), json!(true));
    out.insert("v".into(), json!(V));
    out.insert("platform".into(), json!(plat()));
    out.insert("now".into(), json!(now_hm()));
    out.insert("agents".into(), Value::Array(items));
    let val = Value::Object(out);
    if let Ok(mut g) = SCAN_CACHE.lock() {
        *g = Some((key, val.clone()));
    }
    val
}

// ---------------------------------------------------------------- 说明书

const PROMPT_TPL: &str = include_str!("prompt_template.md");

fn prompt_of(agent_id: &str, base: &str) -> String {
    match AGENTS.iter().find(|a| a.id == agent_id) {
        Some(a) => PROMPT_TPL
            .replace("@@A@@", a.name)
            .replace("@@B@@", base.trim_end_matches('/')),
        None => String::new(),
    }
}

/// `_rule_wrap`：Cursor 的 .mdc 要 frontmatter，内容一样、头不一样。
fn rule_wrap(agent_id: &str, body: &str) -> String {
    let is_rule = AGENTS
        .iter()
        .find(|a| a.id == agent_id)
        .map(|a| a.kind == "rule")
        .unwrap_or(false);
    if !is_rule {
        return body.to_string();
    }
    // body.split("---", 2)[-1].lstrip("\n")
    let b = match body.splitn(3, "---").nth(2) {
        Some(x) => x.trim_start_matches('\n'),
        None => body,
    };
    format!(
        "---\ndescription: 本机个人中台的接入说明（pocket 中台）\nglobs:\nalwaysApply: true\n---\n\n{}",
        b
    )
}

fn link_base(ctx: &Ctx) -> String {
    let cfg = &ctx.cfg;
    let mut port: i64 = 0;
    for key in ["local_port", "port"] {
        let v = util::get(cfg, key);
        if truthy(&v) {
            if let Some(f) = util::fnum(&v) {
                if f.is_finite() {
                    port = f as i64;
                    break;
                }
            }
        }
    }
    if port == 0 {
        port = 8323;
    }
    format!("http://127.0.0.1:{}", port)
}

fn install(root: &Path, agent_id: &str, base: &str) -> Value {
    let Some(a) = AGENTS.iter().find(|a| a.id == agent_id) else {
        return json!({"ok": false, "error": format!("不认识这个 agent：{}", agent_id)});
    };
    let Some(p) = skill_path(a) else {
        return json!({"ok": false, "error": "这台机器的路径拼不出来（缺 APPDATA？）"});
    };
    let txt = rule_wrap(agent_id, &prompt_of(agent_id, base));
    let _ = util::ensure_parent(&p);
    if let Err(e) = std::fs::write(&p, txt.as_bytes()) {
        return json!({"ok": false,
                      "error": format!("写不进去：{}", util::py_os_err_path(&e, &p)),
                      "path": p.display().to_string()});
    }
    let mut links = load_links(root);
    let bytes = txt.as_bytes().len() as i64;
    let mut prompt = Map::new();
    prompt.insert("at".into(), json!(now_hm()));
    prompt.insert("path".into(), json!(p.display().to_string()));
    prompt.insert("base".into(), json!(base));
    prompt.insert("bytes".into(), json!(bytes));
    let rec = links
        .entry(agent_id.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(m) = rec.as_object_mut() {
        m.insert("prompt".into(), Value::Object(prompt));
    }
    save_links(root, &links);
    json!({"ok": true, "path": p.display().to_string(), "agent": agent_id, "bytes": bytes})
}

// ---------------------------------------------------------------- 收回报

fn report(root: &Path, body: &Value) -> Value {
    let b = body.as_object().cloned().unwrap_or_default();
    let agent_id = {
        let s = s_or(b.get("agent").unwrap_or(&Value::Null)).trim().to_string();
        if s.is_empty() {
            "unknown".to_string()
        } else {
            s
        }
    };
    let mut raw: Vec<Value> = match b.get("paths") {
        Some(Value::Object(m)) => m
            .iter()
            .map(|(k, v)| json!({"key": k, "path": v}))
            .collect(),
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    };
    raw.truncate(MAX_REPORT_PATHS);
    let mut paths: Vec<Value> = Vec::new();
    for x in raw {
        match x {
            Value::String(s) => {
                paths.push(json!({"key": "", "path": s, "exists": Path::new(&s).exists()}));
            }
            Value::Object(m) => {
                let p = s_or(m.get("path").unwrap_or(&Value::Null)).trim().to_string();
                if p.is_empty() {
                    continue;
                }
                paths.push(json!({
                    "key": s_or(m.get("key").unwrap_or(&Value::Null)),
                    "path": p,
                    "what": s_or(m.get("what").unwrap_or(&Value::Null)),
                    "exists": Path::new(&p).exists(),
                }));
            }
            _ => {}
        }
    }
    let note = cut(&s_or(b.get("note").unwrap_or(&Value::Null)), 600);
    let via = {
        let v = s_or(b.get("via").unwrap_or(&Value::Null));
        if v.is_empty() {
            "skill".to_string()
        } else {
            v
        }
    };
    let platform = {
        let v = s_or(b.get("platform").unwrap_or(&Value::Null));
        if v.is_empty() {
            plat().to_string()
        } else {
            v
        }
    };
    let mut rec = Map::new();
    rec.insert("at".into(), json!(now_hm()));
    rec.insert("via".into(), json!(cut(&via, 20)));
    rec.insert("platform".into(), json!(cut(&platform, 20)));
    rec.insert("paths".into(), Value::Array(paths.clone()));
    rec.insert("note".into(), json!(note));

    let mut links = load_links(root);
    let entry = links
        .entry(agent_id.clone())
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(m) = entry.as_object_mut() {
        m.insert("report".into(), Value::Object(rec));
    }
    save_links(root, &links);

    let alive = paths
        .iter()
        .filter(|p| p.get("exists").and_then(|v| v.as_bool()).unwrap_or(false))
        .count() as i64;
    let text = if note.is_empty() {
        "agent 回报了数据目录，中台按它自己说的位置取数。".to_string()
    } else {
        note.clone()
    };
    let ev = json!({
        "ns": "系统", "type": "note", "src": "agent_link",
        "id": format!("link-report-{}-{}", agent_id, util::now_secs()),
        "title": format!("{} 接上了中台（{} 个路径）", agent_id, alive),
        "text": text,
        "data": {"agent": agent_id, "paths": paths},
        "tags": ["接入"],
    });
    let _ = bus::add_event(root, &ev, "agent_link");
    json!({"ok": true, "agent": agent_id, "paths": paths.len(), "alive": alive})
}

// ---------------------------------------------------------------- 导入

fn proj_hint(name: &str) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"--p2-[0-9a-f]+$").unwrap());
    let s = re.replace(name, "").trim_matches('-').to_string();
    if s.is_empty() {
        return String::new();
    }
    let cand = format!("/{}", s.replace('-', "/"));
    if Path::new(&cand).exists() {
        cand
    } else {
        String::new()
    }
}

fn trae_memory_dirs(root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let links = load_links(root);
    let rep = links
        .get("trae")
        .and_then(|v| v.get("report"))
        .cloned()
        .unwrap_or(Value::Null);
    if let Some(paths) = rep.get("paths").and_then(|v| v.as_array()) {
        for p in paths {
            let q = PathBuf::from(s_or(p.get("path").unwrap_or(&Value::Null)));
            if q.exists() && q.is_dir() && q.display().to_string().contains("memory") {
                let proj = q.join("projects");
                out.push(if proj.exists() { proj } else { q });
            }
        }
    }
    if out.is_empty() {
        for base in roots_of(&AGENTS[0]) {
            let mem = base.join("memory").join("projects");
            if mem.exists() {
                out.push(mem);
            }
        }
    }
    out
}

fn mem_ts(d: &Value, fallback: f64) -> f64 {
    let raw = cut(&s_or(d.get("message_summary_time").unwrap_or(&Value::Null)).trim(), 19);
    if !raw.is_empty() {
        if let Ok(ndt) = NaiveDateTime::parse_from_str(&raw, "%Y-%m-%d %H:%M:%S") {
            if let Some(dt) = Local.from_local_datetime(&ndt).single() {
                return dt.timestamp() as f64;
            }
        }
    }
    fallback
}

/// `[collapse(str(x)) for x in (v or []) if str(x).strip()][:cap]`
fn str_list(v: &Value, cap: usize) -> Vec<String> {
    let mut out = Vec::new();
    match v {
        Value::Array(a) => {
            for x in a {
                let s = collapse(&pystr(x));
                if !s.is_empty() {
                    out.push(s);
                }
                if out.len() >= cap {
                    break;
                }
            }
        }
        Value::String(s) => {
            for ch in s.chars() {
                let t = collapse(&ch.to_string());
                if !t.is_empty() {
                    out.push(t);
                }
                if out.len() >= cap {
                    break;
                }
            }
        }
        _ => {}
    }
    out
}

fn day_re_ok(name: &str) -> bool {
    name.len() == 8 && name.bytes().all(|b| b.is_ascii_digit())
}

fn sha1_16(seed: &str) -> String {
    let mut h = Sha1::new();
    h.update(seed.as_bytes());
    h.finalize()
        .iter()
        .take(8)
        .map(|b| format!("{:02x}", b))
        .collect()
}

fn import_trae(root: &Path, days: i64) -> (Value, Vec<Value>) {
    let since = util::now_secs() as f64 - (days.max(1) as f64) * 86400.0;
    let dirs = trae_memory_dirs(root);
    if dirs.is_empty() {
        return (
            json!({"ok": false, "error": "找不到 Trae 的 memory/projects",
                   "hint": "先点「装说明」，在 Trae 里让它核对一次路径"}),
            Vec::new(),
        );
    }
    let mut scanned = 0i64;
    let mut imported = 0i64;
    let mut dup = 0i64;
    let mut bad = 0i64;
    let mut projects: HashSet<String> = HashSet::new();
    let mut wl: Vec<Value> = Vec::new();

    for base in &dirs {
        let Ok(rd) = std::fs::read_dir(base) else { continue };
        let mut proj_dirs: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        proj_dirs.sort();
        for proj in proj_dirs {
            let proj_name = proj
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let Ok(rd2) = std::fs::read_dir(&proj) else { continue };
            let mut day_dirs: Vec<PathBuf> = rd2
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.is_dir()
                        && p.file_name()
                            .map(|s| day_re_ok(&s.to_string_lossy()))
                            .unwrap_or(false)
                })
                .collect();
            day_dirs.sort();
            for day in day_dirs {
                let day_name = day
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                let Ok(rd3) = std::fs::read_dir(&day) else { continue };
                let mut files: Vec<PathBuf> = rd3
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        p.is_file()
                            && p.file_name()
                                .map(|s| {
                                    let n = s.to_string_lossy();
                                    n.starts_with("session_memory_") && n.ends_with(".jsonl")
                                })
                                .unwrap_or(false)
                    })
                    .collect();
                files.sort();
                for f in files {
                    let Ok(md) = std::fs::metadata(&f) else { continue };
                    let mtime = mtime_secs(&md);
                    if mtime < since {
                        continue;
                    }
                    let bytes = match std::fs::read(&f) {
                        Ok(b) => b,
                        Err(_) => continue,
                    };
                    let text = String::from_utf8_lossy(&bytes).to_string();
                    for ln in text.lines() {
                        let s = ln.trim();
                        if s.is_empty() {
                            continue;
                        }
                        let d: Value = match serde_json::from_str(s) {
                            Ok(v) => v,
                            Err(_) => {
                                bad += 1;
                                continue;
                            }
                        };
                        if !d.is_object() {
                            bad += 1;
                            continue;
                        }
                        scanned += 1;
                        let intent = collapse(&s_or(d.get("intent").unwrap_or(&Value::Null)));
                        let outcome = collapse(&s_or(d.get("outcome").unwrap_or(&Value::Null)));
                        let actions = str_list(d.get("actions").unwrap_or(&Value::Null), 8);
                        let learned = str_list(d.get("learned").unwrap_or(&Value::Null), 6);
                        if intent.is_empty() && outcome.is_empty() && actions.is_empty() {
                            bad += 1;
                            continue;
                        }
                        let stem = f
                            .file_stem()
                            .map(|x| x.to_string_lossy().to_string())
                            .unwrap_or_default();
                        let sid = stem.replace("session_memory_", "");
                        let seed = format!(
                            "{}|{}|{}|{}",
                            proj_name,
                            day_name,
                            sid,
                            cut(&intent, 60)
                        );
                        let key = sha1_16(&seed);
                        let mtime = std::fs::metadata(&f)
                            .map(|m| mtime_secs(&m))
                            .unwrap_or_else(|_| util::now_secs() as f64);
                        let tsec = mem_ts(&d, mtime);
                        let mut txt = outcome.clone();
                        if !learned.is_empty() {
                            txt = cut(
                                &format!(
                                    "{}{}学到：{}",
                                    txt,
                                    if txt.is_empty() { "" } else { "\n" },
                                    learned.join("；")
                                ),
                                bus::MAX_TEXT,
                            );
                        }
                        let day_s = format!(
                            "{}-{}-{}",
                            &day_name[0..4],
                            &day_name[4..6],
                            &day_name[6..]
                        );
                        let title = {
                            let t = if !intent.is_empty() {
                                intent.clone()
                            } else if !outcome.is_empty() {
                                outcome.clone()
                            } else {
                                "（Trae 没写意图）".to_string()
                            };
                            cut(&t, 120)
                        };
                        let rec = json!({
                            "ns": "工作", "type": "task", "src": "agent_link",
                            "id": format!("trae-mem-{}", key),
                            "ts": (tsec * 1000.0) as i64,
                            "title": title,
                            "text": txt,
                            "data": {"agent": "TRAE", "session": sid, "day": day_s,
                                     "project": proj_name,
                                     "project_path": proj_hint(&proj_name),
                                     "actions": actions, "learned": learned,
                                     "source": "trae-memory"},
                            "tags": ["Trae", "会话记忆"],
                        });
                        let r = bus::add_event(root, &rec, "agent_link");
                        imported += r.get("written").and_then(|v| v.as_i64()).unwrap_or(0);
                        dup += r.get("dup").and_then(|v| v.as_i64()).unwrap_or(0);
                        projects.insert(proj_name.clone());
                        let at = util::strftime_secs(tsec as i64, "%Y-%m-%d %H:%M:%S");
                        let task = {
                            let t = if !intent.is_empty() {
                                intent.clone()
                            } else if !outcome.is_empty() {
                                outcome.clone()
                            } else {
                                "Trae 会话".to_string()
                            };
                            cut(&t, 80)
                        };
                        let detail = cut(
                            &format!(
                                "Trae 会话记忆 · {}{}",
                                day_s,
                                if outcome.is_empty() {
                                    String::new()
                                } else {
                                    format!(" · {}", cut(&outcome, 90))
                                }
                            ),
                            200,
                        );
                        wl.push(json!({
                            "id": format!("tr-{}", key),
                            "task": task,
                            "detail": detail,
                            "status": "ok",
                            "start": at, "end": at,
                            "code": "", "ai": "TRAE", "client": "TRAE",
                            "created": at,
                        }));
                    }
                }
            }
        }
    }
    let res = json!({
        "ok": true, "agent": "trae", "scanned": scanned, "written": imported,
        "dup": dup, "bad": bad, "projects": projects.len(),
        "dirs": dirs.iter().map(|d| d.display().to_string()).collect::<Vec<_>>(),
        "path": bus::events_path(root).display().to_string(),
    });
    (res, wl)
}

/// `_worklog_add` → (新增条数, 重复条数)
fn worklog_add(root: &Path, recs: &[Value]) -> (i64, i64) {
    let p = root.join("_工作台").join("worklog.json");
    let d = if p.exists() {
        util::load_json(&p).unwrap_or_else(|| json!({}))
    } else {
        json!({})
    };
    let mut items: Vec<Value> = d
        .get("items")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let have: HashSet<String> = items
        .iter()
        .filter(|x| x.is_object())
        .map(|x| s_or(x.get("id").unwrap_or(&Value::Null)))
        .collect();
    let fresh: Vec<Value> = recs
        .iter()
        .filter(|r| {
            let id = s_or(r.get("id").unwrap_or(&Value::Null));
            !have.contains(&id)
        })
        .cloned()
        .collect();
    if fresh.is_empty() {
        return (0, recs.len() as i64);
    }
    items.extend(fresh.iter().cloned());
    if items.len() > 800 {
        let cut_at = items.len() - 800;
        items.drain(0..cut_at);
    }
    let now = util::strftime_now("%Y-%m-%d %H:%M:%S");
    let payload = json!({"items": items, "updated_at": now});
    let _ = util::ensure_parent(&p);
    let tmp = p.with_file_name("worklog.json.tmp");
    let ok = super::write_json_indent(&tmp, &payload, 1).is_ok()
        && std::fs::rename(&tmp, &p).is_ok();
    if !ok {
        return (0, recs.len() as i64);
    }
    (fresh.len() as i64, recs.len() as i64 - fresh.len() as i64)
}

fn do_import(root: &Path, agent_id: &str, days: i64) -> Value {
    let days = days.clamp(1, 365);
    if agent_id != "trae" {
        let nm = AGENTS
            .iter()
            .find(|a| a.id == agent_id)
            .map(|a| a.name.to_string())
            .unwrap_or_else(|| agent_id.to_string());
        return json!({
            "ok": false,
            "error": format!("{} 不做日志导入", nm),
            "hint": format!(
                "让它按说明书用 /api/agent/note 自报（agent 填 {}），记录会直接进「AI 工作」卡；数据目录靠 /api/agent/link/report 回报。",
                nm
            ),
        });
    }
    let (mut res, mut wl) = import_trae(root, days);
    if res.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        wl.sort_by(|a, b| {
            s_or(a.get("start").unwrap_or(&Value::Null))
                .cmp(&s_or(b.get("start").unwrap_or(&Value::Null)))
        });
        let keep = wl.len().saturating_sub(60);
        let recs: Vec<Value> = wl.into_iter().skip(keep).collect();
        let (n, dupn) = worklog_add(root, &recs);
        if let Some(m) = res.as_object_mut() {
            m.insert("to_worklog".into(), json!(n));
            m.insert("worklog_dup".into(), json!(dupn));
        }
    }
    let mut links = load_links(root);
    let prev = links
        .get(agent_id)
        .and_then(|v| v.get("import"))
        .cloned()
        .unwrap_or(Value::Null);
    let prev_written = prev.get("written").and_then(|v| v.as_i64()).unwrap_or(0);
    let ok = res.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    let written = if ok {
        res.get("written").and_then(|v| v.as_i64()).unwrap_or(0) + prev_written
    } else {
        prev_written
    };
    let mut imp = Map::new();
    imp.insert("at".into(), json!(now_hm()));
    imp.insert("written".into(), json!(written));
    imp.insert(
        "last".into(),
        json!({
            "written": res.get("written").cloned().unwrap_or(Value::Null),
            "dup": res.get("dup").cloned().unwrap_or(Value::Null),
            "scanned": res.get("scanned").cloned().unwrap_or(Value::Null),
            "days": days,
        }),
    );
    let entry = links
        .entry(agent_id.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(m) = entry.as_object_mut() {
        m.insert("import".into(), Value::Object(imp));
    }
    save_links(root, &links);
    res
}

// ---------------------------------------------------------------- 路由

async fn scan_route(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let root = super::data_root(&ctx);
    let out = tokio::task::spawn_blocking(move || scan(&root))
        .await
        .unwrap_or_else(|_| json!({"ok": false, "error": "RuntimeError: 扫描失败", "agents": []}));
    Json(out).into_response()
}

async fn prompt_route(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    // 参数缺省才是 "trae"；显式 `?agent=` 是空串，Python 会给 404。
    let agent = query_map(&uri)
        .into_iter()
        .find(|(k, _)| k == "agent")
        .map(|(_, v)| v)
        .unwrap_or_else(|| "trae".to_string());
    let txt = prompt_of(&agent, &link_base(&ctx));
    if txt.is_empty() {
        return super::plain_text(
            404,
            format!("不认识这个 agent：{}", agent),
            "text/plain; charset=utf-8",
        );
    }
    super::plain_text(200, txt, "text/markdown; charset=utf-8")
}

async fn install_route(State(ctx): CtxState, headers: HeaderMap, uri: Uri, body: Bytes) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let b = match super::parse_body(&body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let agent = {
        let s = s_or(b.get("agent").unwrap_or(&Value::Null));
        if s.is_empty() {
            "trae".to_string()
        } else {
            s
        }
    };
    let root = super::data_root(&ctx);
    let base = link_base(&ctx);
    let out = tokio::task::spawn_blocking(move || install(&root, &agent, &base))
        .await
        .unwrap_or_else(|_| json!({"ok": false, "error": "RuntimeError: 写说明失败"}));
    Json(out).into_response()
}

async fn report_route(State(ctx): CtxState, headers: HeaderMap, uri: Uri, body: Bytes) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let payload: Value = match super::parse_json_opt(&body) {
        Some(v) => v,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"ok": false, "error": "body 不是 JSON"})),
            )
                .into_response()
        }
    };
    if !payload.is_object() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": "body 要是对象"})),
        )
            .into_response();
    }
    let root = super::data_root(&ctx);
    let out = tokio::task::spawn_blocking(move || report(&root, &payload))
        .await
        .unwrap_or_else(|_| json!({"ok": false, "error": "RuntimeError: 回执写不进去"}));
    Json(out).into_response()
}

async fn import_route(State(ctx): CtxState, headers: HeaderMap, uri: Uri, body: Bytes) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    // `body: dict | None = None`：没给就是 {}，给了不是对象仍然 422。
    let b: Value = if body.is_empty() {
        json!({})
    } else {
        match serde_json::from_slice::<Value>(&body) {
            Ok(Value::Object(m)) => Value::Object(m),
            Ok(Value::Null) => json!({}),
            Ok(v) => return super::dict_type_body(&v),
            Err(e) => return super::json_invalid_body(&e),
        }
    };
    let agent = {
        let s = s_or(b.get("agent").unwrap_or(&Value::Null));
        if s.is_empty() {
            "trae".to_string()
        } else {
            s
        }
    };
    // `int(b.get("days") or 30)`：默认 30；认不出是 ValueError → FastAPI 的 500 纯文本。
    let days = {
        let v = b.get("days").cloned().unwrap_or(Value::Null);
        if !truthy(&v) {
            30
        } else {
            match &v {
                Value::Number(n) => n.as_f64().map(|f| f as i64).unwrap_or(0),
                Value::Bool(b) => *b as i64,
                Value::String(s) => match s.trim().parse::<i64>() {
                    Ok(i) => i,
                    Err(_) => {
                        return super::plain_text(
                            500,
                            "Internal Server Error".to_string(),
                            "text/plain; charset=utf-8",
                        )
                    }
                },
                _ => {
                    return super::plain_text(
                        500,
                        "Internal Server Error".to_string(),
                        "text/plain; charset=utf-8",
                    )
                }
            }
        }
    };
    let root = super::data_root(&ctx);
    let out = tokio::task::spawn_blocking(move || do_import(&root, &agent, days))
        .await
        .unwrap_or_else(|_| json!({"ok": false, "error": "RuntimeError: 导入失败"}));
    Json(out).into_response()
}
