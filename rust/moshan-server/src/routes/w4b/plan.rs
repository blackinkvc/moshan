// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：`moshan/plan.py` 的 Rust 版 —— AI 工作计划的读/写/建议（4 条路由）。
// 不做：不写 agent 总线（`_emit*` 走 agentbus，属于另一条泳道）；因此
//       `/api/plan/act` 里 `agent.export` 的 written/dup 计数恒为 0，
//       但「推过」的戳（agent_h / agent_at）照打，计划数据文件仍然一致。
//
// 字节口径：所有落盘都走 `jsonio::dumps(v, 1)`；字段顺序、`.strip()`、`[:300]`
// 截断、`round(x, 3)`、稳定排序全部照抄 Python —— 这份文件用户会直接打开看。

use super::dashrs;
use super::hostwork;
use super::util::{self, get, int0, s_or, truthy};
use crate::paths::Ctx;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const PLAN_DIR: &str = "19-AI工作计划";
pub const PLAN_FILE: &str = "计划.json";
const LOG_MAX: usize = 200;
const STATUS: [&str; 3] = ["active", "paused", "done"];

// ---------------------------------------------------------------- 路径

fn default_root() -> PathBuf {
    // 没有 cfg 时（MCP / 命令行）自己找：state 目录的 config.json → allowed_roots[0]。
    let state = crate::paths::state_dir();
    if let Some(cfg) = util::load_json(&state.join("config.json")) {
        if let Some(a) = cfg.get("allowed_roots").and_then(|x| x.as_array()) {
            if let Some(first) = a.first() {
                return PathBuf::from(s_or(first));
            }
        }
        let inbox = s_or(get(&cfg, "inbox_dir"));
        if !inbox.is_empty() {
            return parent_of(&inbox);
        }
    }
    util::home().join("Documents").join("AI项目")
}

/// `Path(x).parent`：空/相对单段时回 "."，别回空路径。
fn parent_of(raw: &str) -> PathBuf {
    let p = PathBuf::from(raw);
    match p.parent() {
        Some(par) if !par.as_os_str().is_empty() => par.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

pub fn root_of(ctx: &Ctx) -> PathBuf {
    if !ctx.cfg.is_object() {
        return default_root();
    }
    if let Some(a) = ctx.cfg.get("allowed_roots").and_then(|x| x.as_array()) {
        if let Some(first) = a.first() {
            let s = s_or(first);
            if !s.is_empty() {
                return PathBuf::from(s);
            }
        }
    }
    let inbox = s_or(get(&ctx.cfg, "inbox_dir"));
    if inbox.is_empty() {
        parent_of(".")
    } else {
        parent_of(&inbox)
    }
}

pub fn plan_dir(root: &Path) -> PathBuf {
    root.join(PLAN_DIR)
}

pub fn plan_path(root: &Path) -> PathBuf {
    plan_dir(root).join(PLAN_FILE)
}

// ---------------------------------------------------------------- id

fn token_hex(n: usize) -> String {
    use std::io::Read;
    let mut buf = vec![0u8; n];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok();
    if !ok {
        let mut x = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
            ^ (std::process::id() as u64) << 32;
        for b in buf.iter_mut() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = (x & 0xff) as u8;
        }
    }
    buf.iter().map(|b| format!("{:02x}", b)).collect()
}

fn uid(prefix: &str, taken: &mut HashSet<String>) -> String {
    for _ in 0..50 {
        let v = format!("{}{}", prefix, token_hex(3));
        if !taken.contains(&v) {
            taken.insert(v.clone());
            return v;
        }
    }
    format!("{}{}", prefix, token_hex(6))
}

fn now() -> String {
    util::now_iso()
}

// ---------------------------------------------------------------- 规范化

pub fn norm_node(n: &Value, taken: &mut HashSet<String>) -> Value {
    let n = if n.is_object() { n.clone() } else { json!({"t": util::pystr(n)}) };
    let mut tid = s_or(get(&n, "id"));
    if tid.is_empty() || taken.contains(&tid) {
        tid = uid("tk_", taken);
    } else {
        taken.insert(tid.clone());
    }
    let kids: Vec<Value> = n
        .get("children")
        .and_then(|c| c.as_array())
        .map(|a| a.iter().filter(|x| x.is_object()).cloned().collect())
        .unwrap_or_default();
    let t_raw = if truthy(get(&n, "t")) { s_or(get(&n, "t")) } else { s_or(get(&n, "title")) };
    json!({
        "id": tid,
        "t": util::cut(t_raw.trim(), 300),
        "done": truthy(get(&n, "done")),
        "at": s_or(get(&n, "at")),
        "due": util::cut(&s_or(get(&n, "due")), 10),
        "note": util::cut(&s_or(get(&n, "note")), 2000),
        "link": s_or(get(&n, "link")),
        "src": util::cut(&if truthy(get(&n, "src")) { s_or(get(&n, "src")) } else { "manual".into() }, 16),
        "tok": util::digits_or_zero(get(&n, "tok")),
        "agent_h": util::cut(&s_or(get(&n, "agent_h")), 16),
        "agent_at": util::cut(&s_or(get(&n, "agent_at")), 19),
        "children": kids.iter().map(|c| norm_node(c, taken)).collect::<Vec<_>>(),
    })
}

fn str_list(v: &Value, cap: usize, limit: usize, strip: bool) -> Vec<Value> {
    let arr = v.as_array().cloned().unwrap_or_default();
    let mut out = Vec::new();
    for x in arr {
        let s = util::pystr(&x);
        if s.trim().is_empty() {
            continue;
        }
        let s = if strip { s.trim().to_string() } else { s };
        out.push(Value::String(util::cut(&s, cap)));
        if out.len() >= limit {
            break;
        }
    }
    out
}

pub fn norm_project(p: &Value, taken: &mut HashSet<String>) -> Value {
    let p = if p.is_object() { p.clone() } else { json!({}) };
    let mut pid = s_or(get(&p, "id"));
    if pid.is_empty() || taken.contains(&pid) {
        pid = uid("pr_", taken);
    } else {
        taken.insert(pid.clone());
    }
    let m = if get(&p, "match").is_object() { get(&p, "match").clone() } else { json!({}) };
    let st_raw = if truthy(get(&p, "status")) { s_or(get(&p, "status")) } else { "active".into() };
    let st = if STATUS.contains(&st_raw.as_str()) { st_raw } else { "active".to_string() };
    let kids: Vec<Value> = p
        .get("tasks")
        .and_then(|t| t.as_array())
        .map(|a| a.iter().filter(|x| x.is_object()).cloned().collect())
        .unwrap_or_default();
    let name_raw = if truthy(get(&p, "name")) { s_or(get(&p, "name")) } else { "未命名计划".into() };
    let created = if truthy(get(&p, "created")) { s_or(get(&p, "created")) } else { now() };
    json!({
        "id": pid,
        "name": util::cut(name_raw.trim(), 80),
        "note": util::cut(&s_or(get(&p, "note")), 2000),
        "status": st,
        "created": created,
        "due": util::cut(&s_or(get(&p, "due")), 10),
        "match": {
            "spaces": str_list(get(&m, "spaces"), 120, 12, false),
            "projs": str_list(get(&m, "projs"), 120, 12, false),
            "keys": str_list(get(&m, "keys"), 40, 12, true),
        },
        "tasks": kids.iter().map(|t| norm_node(t, taken)).collect::<Vec<_>>(),
    })
}

pub fn norm_doc(d: &Value) -> Value {
    let d = if d.is_object() { d.clone() } else { json!({}) };
    let mut taken: HashSet<String> = HashSet::new();
    let projects: Vec<Value> = d
        .get("projects")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter(|p| p.is_object())
                .map(|p| norm_project(p, &mut taken))
                .collect()
        })
        .unwrap_or_default();
    let mut inbox: Vec<Value> = Vec::new();
    if let Some(items) = d.get("inbox").and_then(|x| x.as_array()) {
        for it in items {
            if !it.is_object() {
                continue;
            }
            let mut iid = s_or(get(it, "id"));
            if iid.is_empty() || taken.contains(&iid) {
                iid = uid("ib_", &mut taken);
            } else {
                taken.insert(iid.clone());
            }
            let t_raw = if truthy(get(it, "t")) { s_or(get(it, "t")) } else { s_or(get(it, "text")) };
            let at = if truthy(get(it, "at")) { s_or(get(it, "at")) } else { now() };
            inbox.push(json!({
                "id": iid,
                "t": util::cut(t_raw.trim(), 300),
                "at": at,
                "note": util::cut(&s_or(get(it, "note")), 500),
                "agent_h": util::cut(&s_or(get(it, "agent_h")), 16),
                "agent_at": util::cut(&s_or(get(it, "agent_at")), 19),
            }));
        }
    }
    let mut log: Vec<Value> = d
        .get("log")
        .and_then(|x| x.as_array())
        .map(|a| a.iter().filter(|x| x.is_object()).cloned().collect())
        .unwrap_or_default();
    if log.len() > LOG_MAX {
        log = log[log.len() - LOG_MAX..].to_vec();
    }
    json!({
        "v": 1,
        "rev": int0(get(&d, "rev")),
        "updated_at": s_or(get(&d, "updated_at")),
        "projects": projects,
        "inbox": inbox,
        "log": log,
    })
}

pub fn load(root: &Path) -> Value {
    norm_doc(&util::load_json(&plan_path(root)).unwrap_or(json!({})))
}

pub fn save(root: &Path, doc: &Value) -> Value {
    let mut doc = norm_doc(doc);
    let rev = int0(get(&doc, "rev")) + 1;
    let stamp = now();
    if let Some(o) = doc.as_object_mut() {
        o.insert("rev".to_string(), json!(rev));
        o.insert("updated_at".to_string(), json!(stamp));
        if let Some(Value::Array(log)) = o.get_mut("log") {
            if log.len() > LOG_MAX {
                let cut = log.len() - LOG_MAX;
                log.drain(0..cut);
            }
        }
    }
    let _ = util::write_json(&plan_path(root), &doc, 1);
    let _ = export_md(root, &doc);
    doc
}

fn log_act(doc: &mut Value, act: &str, name: &str, detail: &str) {
    let entry = json!({
        "at": now(),
        "act": act,
        "name": util::cut(name, 120),
        "detail": util::cut(detail, 200),
    });
    if let Some(o) = doc.as_object_mut() {
        let arr = o.entry("log").or_insert_with(|| json!([]));
        if let Some(a) = arr.as_array_mut() {
            a.push(entry);
        }
    }
}

// ---------------------------------------------------------------- 树工具

pub fn walk<'a>(nodes: &'a [Value], depth: usize, out: &mut Vec<(&'a Value, usize)>) {
    for n in nodes {
        out.push((n, depth));
        if let Some(kids) = n.get("children").and_then(|c| c.as_array()) {
            walk(kids, depth + 1, out);
        }
    }
}

fn all_nodes<'a>(nodes: &'a [Value]) -> Vec<&'a Value> {
    let mut v = Vec::new();
    walk(nodes, 0, &mut v);
    v.into_iter().map(|(n, _)| n).collect()
}

fn count(nodes: &[Value]) -> (i64, i64) {
    let mut done = 0;
    let mut total = 0;
    for n in all_nodes(nodes) {
        total += 1;
        if truthy(get(n, "done")) {
            done += 1;
        }
    }
    (done, total)
}

pub fn find_node<'a>(nodes: &'a [Value], tid: &str) -> Option<&'a Value> {
    for n in nodes {
        if n.get("id").and_then(|x| x.as_str()) == Some(tid) {
            return Some(n);
        }
        if let Some(kids) = n.get("children").and_then(|c| c.as_array()) {
            if let Some(hit) = find_node(kids, tid) {
                return Some(hit);
            }
        }
    }
    None
}

fn find_node_mut<'a>(nodes: &'a mut Vec<Value>, tid: &str) -> Option<&'a mut Value> {
    let pos = nodes
        .iter()
        .position(|n| n.get("id").and_then(|x| x.as_str()) == Some(tid));
    if let Some(i) = pos {
        return nodes.get_mut(i);
    }
    for n in nodes.iter_mut() {
        if let Some(kids) = n.get_mut("children").and_then(|c| c.as_array_mut()) {
            if let Some(hit) = find_node_mut(kids, tid) {
                return Some(hit);
            }
        }
    }
    None
}

fn find_parent_list_mut<'a>(nodes: &'a mut Vec<Value>, tid: &str) -> Option<&'a mut Vec<Value>> {
    if nodes.iter().any(|n| n.get("id").and_then(|x| x.as_str()) == Some(tid)) {
        return Some(nodes);
    }
    for n in nodes.iter_mut() {
        if let Some(kids) = n.get_mut("children").and_then(|c| c.as_array_mut()) {
            if let Some(hit) = find_parent_list_mut(kids, tid) {
                return Some(hit);
            }
        }
    }
    None
}

fn find_project<'a>(doc: &'a Value, pid: &str) -> Option<&'a Value> {
    doc.get("projects")
        .and_then(|p| p.as_array())?
        .iter()
        .find(|p| p.get("id").and_then(|x| x.as_str()) == Some(pid))
}

fn project_index(doc: &Value, pid: &str) -> Option<usize> {
    let arr = doc.get("projects").and_then(|p| p.as_array())?;
    arr.iter()
        .position(|p| p.get("id").and_then(|x| x.as_str()) == Some(pid))
}

fn project_mut<'a>(doc: &'a mut Value, pid: &str) -> Option<&'a mut Value> {
    let i = project_index(doc, pid)?;
    doc.get_mut("projects")?.as_array_mut()?.get_mut(i)
}

fn tasks_mut<'a>(p: &'a mut Value) -> Option<&'a mut Vec<Value>> {
    // `p["tasks"]` 在规范化后一定存在且是数组
    p.get_mut("tasks").and_then(|t| t.as_array_mut())
}

fn flatten_names(nodes: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    fn rec(nodes: &[Value], depth: usize, out: &mut Vec<Value>) {
        for n in nodes {
            out.push(json!({
                "id": s_or(get(n, "id")),
                "t": s_or(get(n, "t")),
                "depth": depth,
                "done": truthy(get(n, "done")),
            }));
            if let Some(kids) = n.get("children").and_then(|c| c.as_array()) {
                rec(kids, depth + 1, out);
            }
        }
    }
    rec(nodes, 0, &mut out);
    out
}

fn all_ids(doc: &Value) -> HashSet<String> {
    let mut out = HashSet::new();
    if let Some(ps) = doc.get("projects").and_then(|p| p.as_array()) {
        for p in ps {
            out.insert(s_or(get(p, "id")));
            if let Some(ts) = p.get("tasks").and_then(|t| t.as_array()) {
                for n in all_nodes(ts) {
                    out.insert(s_or(get(n, "id")));
                }
            }
        }
    }
    if let Some(ib) = doc.get("inbox").and_then(|x| x.as_array()) {
        for x in ib {
            out.insert(s_or(get(x, "id")));
        }
    }
    out
}

// ---------------------------------------------------------------- 打分

fn re_space() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"[\s\u{3000}]+").unwrap())
}

fn re_punct() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r#"[·、，,。.：:；;（）()\[\]【】「」"'“”‘’/\\|_\-—–~!！?？]+"#).unwrap())
}

fn re_cjk() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"[\u{4e00}-\u{9fff}]").unwrap())
}

fn re_word() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"[^a-z0-9]+").unwrap())
}

pub fn norm_text(s: &str) -> String {
    let low = s.to_lowercase();
    let a = re_space().replace_all(&low, "");
    re_punct().replace_all(&a, "").to_string()
}

fn grams(s: &str) -> HashSet<String> {
    let n = norm_text(s);
    if n.is_empty() {
        return HashSet::new();
    }
    let chars: Vec<char> = n.chars().collect();
    if re_cjk().is_match(&n) {
        let mut out: HashSet<String> = HashSet::new();
        for i in 0..chars.len().saturating_sub(1).max(1) {
            if i + 2 <= chars.len() {
                out.insert(chars[i..i + 2].iter().collect());
            } else if i < chars.len() {
                out.insert(chars[i..].iter().collect());
            }
        }
        for c in &chars {
            out.insert(c.to_string());
        }
        out
    } else {
        re_word()
            .split(&n)
            .filter(|w| !w.is_empty())
            .map(|w| w.to_string())
            .collect()
    }
}

pub fn sim(a: &str, b: &str) -> f64 {
    let ga = grams(a);
    let gb = grams(b);
    if ga.is_empty() || gb.is_empty() {
        return 0.0;
    }
    let inter = ga.intersection(&gb).count() as f64;
    inter / (ga.len().min(gb.len()) as f64)
}

fn num(v: &Value) -> i64 {
    util::as_f64(v).map(|f| f as i64).unwrap_or(0)
}

/// `usage_of(proj, host)`
pub fn usage_of(proj: &Value, host: &Value) -> Value {
    let m = if get(proj, "match").is_object() { get(proj, "match").clone() } else { json!({}) };
    let mut want: HashSet<String> = HashSet::new();
    for key in ["spaces", "projs"] {
        if let Some(a) = m.get(key).and_then(|x| x.as_array()) {
            for x in a {
                want.insert(util::pystr(x));
            }
        }
    }
    let keys: Vec<String> = m
        .get("keys")
        .and_then(|x| x.as_array())
        .map(|a| a.iter().map(util::pystr).filter(|s| !s.trim().is_empty()).collect())
        .unwrap_or_default();
    let nm = s_or(get(proj, "name"));
    let mut hits: Vec<(String, Value)> = Vec::new();
    for (kind, arr) in [
        ("spaces", host.get("spaces").cloned().unwrap_or(json!([]))),
        ("projs", host.get("projs").cloned().unwrap_or(json!([]))),
    ] {
        for it in arr.as_array().cloned().unwrap_or_default() {
            let k = s_or(get(&it, "k"));
            let n = s_or(get(&it, "n"));
            let mut ok = want.contains(&k);
            if !ok {
                for kk in &keys {
                    let nk = norm_text(kk);
                    if nk.chars().count() >= 2 && norm_text(&n).contains(&nk) {
                        ok = true;
                        break;
                    }
                }
            }
            if !ok && norm_text(&nm).chars().count() >= 2 {
                let a = norm_text(&nm);
                let b = norm_text(&n);
                ok = !a.is_empty() && (b.contains(&a) || a.contains(&b) || sim(&nm, &n) >= 0.7);
            }
            if ok {
                let item = json!({"k": k, "n": n, "kind": kind, "tok": int0(get(&it, "tok"))});
                match hits.iter_mut().find(|(hk, _)| *hk == k) {
                    Some(slot) => slot.1 = item,
                    None => hits.push((k, item)),
                }
            }
        }
    }
    let mut items: Vec<Value> = hits.into_iter().map(|(_, v)| v).collect();
    items.sort_by(|a, b| num(get(b, "tok")).cmp(&num(get(a, "tok"))));
    let tok: i64 = items.iter().map(|x| num(get(x, "tok"))).sum();
    let n = items.len();
    json!({"tok": tok, "items": items.iter().take(6).cloned().collect::<Vec<_>>(), "n": n})
}

/// `score_project(text, proj)`
pub fn score_project(text: &str, proj: &Value) -> f64 {
    let mut best = sim(text, &s_or(get(proj, "name")));
    if let Some(keys) = get(proj, "match").get("keys").and_then(|k| k.as_array()) {
        for k in keys {
            best = best.max(sim(text, &util::pystr(k)) * 1.1);
        }
    }
    if let Some(tasks) = proj.get("tasks").and_then(|t| t.as_array()) {
        for n in all_nodes(tasks) {
            best = best.max(sim(text, &s_or(get(n, "t"))) * 1.05);
        }
    }
    util::py_round(best, 3)
}

/// `best_task(text, proj)`
fn best_task(text: &str, proj: &Value) -> Option<Value> {
    let head = norm_text(text);
    let a = grams(text);
    // (权重, 命中字数, 深度, 标题 gram 数, node)
    let mut pool: Vec<(i64, i64, usize, usize, Value)> = Vec::new();

    fn rec(
        nodes: &[Value],
        d: usize,
        head: &str,
        a: &HashSet<String>,
        pool: &mut Vec<(i64, i64, usize, usize, Value)>,
    ) {
        for n in nodes {
            if !truthy(get(n, "done")) {
                let t = s_or(get(n, "t"));
                let b = grams(&t);
                if !b.is_empty() {
                    let ov = a.intersection(&b).count() as i64;
                    let nt = norm_text(&t);
                    let hit = nt.chars().count() >= 3 && head.contains(&nt);
                    if hit || (ov as f64 / b.len() as f64) >= 0.55 {
                        pool.push((ov + if hit { 2 } else { 0 }, ov, d, b.len(), n.clone()));
                    }
                }
            }
            if let Some(kids) = n.get("children").and_then(|c| c.as_array()) {
                rec(kids, d + 1, head, a, pool);
            }
        }
    }
    if let Some(tasks) = proj.get("tasks").and_then(|t| t.as_array()) {
        rec(tasks, 0, &head, &a, &mut pool);
    }
    if pool.is_empty() {
        return None;
    }
    // Python: sort(key=(w, d, lb), reverse=True) —— 三个键都倒序，稳定
    pool.sort_by(|x, y| {
        y.0.cmp(&x.0)
            .then_with(|| y.2.cmp(&x.2))
            .then_with(|| y.3.cmp(&x.3))
    });
    let (_w, ov, _d, lb, node) = &pool[0];
    let score = util::py_round((*ov as f64 / (*lb).max(1) as f64).min(1.0), 3);
    Some(json!({
        "id": s_or(get(node, "id")),
        "t": s_or(get(node, "t")),
        "score": score,
    }))
}

/// `candidates(root, ids)`
pub fn candidates(root: &Path, ids: &[String]) -> Vec<Value> {
    let doc = load(root);
    let want: HashSet<&str> = ids.iter().map(|s| s.as_str()).collect();
    let projects: Vec<Value> = doc
        .get("projects")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for it in doc.get("inbox").and_then(|x| x.as_array()).cloned().unwrap_or_default() {
        let iid = s_or(get(&it, "id"));
        if !want.is_empty() && !want.contains(iid.as_str()) {
            continue;
        }
        let text = s_or(get(&it, "t"));
        let mut ranked: Vec<(f64, Value)> =
            projects.iter().map(|p| (score_project(&text, p), p.clone())).collect();
        ranked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        let top = ranked.first().cloned();
        let mut hint = json!({
            "proj": "", "proj_name": "", "task": "", "task_id": "",
            "new_task": true, "score": 0.0, "why": "",
        });
        let (top_score, top_proj) = match &top {
            Some((s, p)) => (*s, Some(p.clone())),
            None => (0.0, None),
        };
        if let Some(p) = top_proj {
            if top_score >= 0.45 {
                let pname = s_or(get(&p, "name"));
                set(&mut hint, "proj", json!(s_or(get(&p, "id"))));
                set(&mut hint, "proj_name", json!(pname));
                set(&mut hint, "score", json!(top_score));
                set(&mut hint, "why", json!(format!("与「{}」的标题/关键词最接近", pname)));
                if let Some(bt) = best_task(&text, &p) {
                    set(&mut hint, "task", json!(text));
                    set(&mut hint, "task_id", json!(s_or(get(&bt, "id"))));
                    set(&mut hint, "new_task", json!(false));
                    set(&mut hint, "score", json!(util::as_f64(get(&bt, "score")).unwrap_or(0.0)));
                    set(
                        &mut hint,
                        "why",
                        json!(format!("挂到「{}」下面", s_or(get(&bt, "t")))),
                    );
                } else {
                    set(&mut hint, "task", json!(text));
                    set(
                        &mut hint,
                        "why",
                        json!(format!("归到「{}」，但项目里没有对应子任务，建议新建", pname)),
                    );
                }
            } else {
                set(&mut hint, "task", json!(text));
                set(&mut hint, "why", json!(no_project_why(&doc)));
            }
        } else {
            set(&mut hint, "task", json!(text));
            set(&mut hint, "why", json!(no_project_why(&doc)));
        }
        let mut item = it.clone();
        if let Some(o) = item.as_object_mut() {
            o.insert("hint".to_string(), hint);
        }
        out.push(item);
    }
    out
}

fn no_project_why(doc: &Value) -> &'static str {
    let has = doc
        .get("projects")
        .and_then(|p| p.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    if has {
        "没有足够接近的项目，建议新建一个计划项目"
    } else {
        "还没有任何计划项目"
    }
}

fn set(v: &mut Value, k: &str, val: Value) {
    if let Some(o) = v.as_object_mut() {
        o.insert(k.to_string(), val);
    }
}

/// `_th(t)`：标题短指纹（事件 id 用）。
fn th(t: &str) -> String {
    use sha1::{Digest, Sha1};
    let mut h = Sha1::new();
    h.update(norm_text(t).as_bytes());
    let d = h.finalize();
    d.iter().map(|b| format!("{:02x}", b)).collect::<String>()[..6].to_string()
}

// ---------------------------------------------------------------- act

struct Bad(String);

fn need_project<'a>(doc: &'a Value, pid: &Value) -> Result<&'a Value, Bad> {
    match find_project(doc, &s_or(pid)) {
        Some(p) => Ok(p),
        None => Err(Bad("找不到这个计划项目".into())),
    }
}

fn val_or_empty(v: &Value) -> Value {
    if v.is_null() {
        Value::String(String::new())
    } else {
        v.clone()
    }
}

pub fn act(ctx: &Ctx, root: &Path, body: &Value) -> Value {
    let b = if body.is_object() { body.clone() } else { json!({}) };
    let action = s_or(get(&b, "action")).trim().to_string();
    let mut doc = load(root);
    match dispatch(ctx, root, &mut doc, &action, &b) {
        Ok(v) => v,
        Err(Bad(msg)) => {
            let rev = int0(get(&doc, "rev"));
            json!({"ok": false, "error": msg, "rev": rev})
        }
    }
}

fn dispatch(ctx: &Ctx, root: &Path, doc: &mut Value, action: &str, b: &Value) -> Result<Value, Bad> {
    let nows = now();
    let host = hostwork::host_projects(ctx);

    // ---- 项目
    if action == "proj.add" || action == "project.add" {
        let name = s_or(get(b, "name")).trim().to_string();
        if name.is_empty() {
            return Err(Bad("项目名不能为空".into()));
        }
        let mut taken = all_ids(doc);
        let proj = norm_project(
            &json!({"name": name, "note": val_or_empty(get(b, "note")),
                    "due": val_or_empty(get(b, "due")),
                    "match": {"keys": val_or_empty(get(b, "keys"))}}),
            &mut taken,
        );
        let pid = s_or(get(&proj, "id"));
        let mut proj = proj;
        set(&mut proj, "created", json!(nows));
        if let Some(arr) = doc.get_mut("projects").and_then(|x| x.as_array_mut()) {
            arr.insert(0, proj);
        }
        log_act(doc, "proj.add", &name, "");
        *doc = save(root, doc);
        return Ok(json!({"ok": true, "doc": doc, "id": pid}));
    }

    if action == "proj.del" || action == "project.del" {
        let p = need_project(doc, get(b, "id"))?.clone();
        let pid = s_or(get(&p, "id"));
        let pname = s_or(get(&p, "name"));
        if let Some(arr) = doc.get_mut("projects").and_then(|x| x.as_array_mut()) {
            arr.retain(|x| x.get("id").and_then(|v| v.as_str()) != Some(pid.as_str()));
        }
        log_act(doc, "proj.del", &pname, "");
        let saved = save(root, doc);
        *doc = saved.clone();
        return Ok(json!({"ok": true, "doc": saved}));
    }

    if action == "proj.rename" || action == "project.rename" {
        let p = need_project(doc, get(b, "id"))?.clone();
        let pid = s_or(get(&p, "id"));
        let name = s_or(get(b, "name")).trim().to_string();
        if name.is_empty() {
            return Err(Bad("项目名不能为空".into()));
        }
        let old = s_or(get(&p, "name"));
        if let Some(pm) = project_mut(doc, &pid) {
            set(pm, "name", json!(util::cut(&name, 80)));
        }
        log_act(doc, "proj.rename", &name, &format!("原：{}", old));
        let saved = save(root, doc);
        *doc = saved.clone();
        return Ok(json!({"ok": true, "doc": saved}));
    }

    if action == "proj.link" || action == "project.link" {
        let p = need_project(doc, get(b, "id"))?.clone();
        let pid = s_or(get(&p, "id"));
        let mut spaces: Option<Vec<Value>> = None;
        let mut projs: Option<Vec<Value>> = None;
        let mut keys: Option<Vec<Value>> = None;
        if b.get("spaces").is_some() {
            spaces = Some(str_list(get(b, "spaces"), 120, 12, false));
        }
        if b.get("projs").is_some() {
            projs = Some(str_list(get(b, "projs"), 120, 12, false));
        }
        if b.get("keys").is_some() {
            keys = Some(str_list(get(b, "keys"), 40, 12, true));
        }
        let mut logdetail = String::new();
        if let Some(pm) = project_mut(doc, &pid) {
            if !get(pm, "match").is_object() {
                set(pm, "match", json!({"spaces": [], "projs": [], "keys": []}));
            }
            if let Some(m) = pm.get_mut("match") {
                if let Some(v) = spaces {
                    set(m, "spaces", json!(v));
                }
                if let Some(v) = projs {
                    set(m, "projs", json!(v));
                }
                if let Some(v) = keys {
                    set(m, "keys", json!(v));
                }
                let mut all: Vec<String> = Vec::new();
                for k in ["spaces", "projs", "keys"] {
                    if let Some(a) = m.get(k).and_then(|x| x.as_array()) {
                        for x in a {
                            all.push(util::pystr(x));
                        }
                    }
                }
                logdetail = all.join(",");
            }
        }
        let pname = s_or(get(&p, "name"));
        log_act(doc, "proj.link", &pname, &logdetail);
        let _ = save(root, doc);
        let reloaded = load(root);
        *doc = reloaded.clone();
        let usage = usage_of(&reloaded.get("projects").and_then(|x| x.as_array())
            .and_then(|a| a.iter().find(|x| x.get("id").and_then(|v| v.as_str()) == Some(pid.as_str())))
            .cloned().unwrap_or(json!({})), &host);
        return Ok(json!({"ok": true, "doc": reloaded, "usage": usage}));
    }

    if action == "proj.status" || action == "project.status" {
        let p = need_project(doc, get(b, "id"))?.clone();
        let pid = s_or(get(&p, "id"));
        let st = if truthy(get(b, "status")) { s_or(get(b, "status")) } else { "active".into() };
        if !STATUS.contains(&st.as_str()) {
            return Err(Bad("状态只能是 active / paused / done".into()));
        }
        if let Some(pm) = project_mut(doc, &pid) {
            set(pm, "status", json!(st));
        }
        let pname = s_or(get(&p, "name"));
        log_act(doc, "proj.status", &pname, &st);
        let saved = save(root, doc);
        *doc = saved.clone();
        return Ok(json!({"ok": true, "doc": saved}));
    }

    // ---- 任务
    if action == "task.add" || action == "task.addChild" {
        let p = need_project(doc, get(b, "proj"))?.clone();
        let pid = s_or(get(&p, "id"));
        let title = s_or(get(b, "title")).trim().to_string();
        if title.is_empty() {
            return Err(Bad("任务名不能为空".into()));
        }
        let mut taken = all_ids(doc);
        let src = if truthy(get(b, "src")) { s_or(get(b, "src")) } else { "manual".into() };
        let node = norm_node(
            &json!({"t": title, "due": val_or_empty(get(b, "due")),
                    "note": val_or_empty(get(b, "note")), "src": src}),
            &mut taken,
        );
        let nid = s_or(get(&node, "id"));
        let parent = s_or(get(b, "parent"));
        if !parent.is_empty() {
            let found = project_mut(doc, &pid)
                .and_then(|pm| tasks_mut(pm))
                .and_then(|ts| find_node_mut(ts, &parent))
                .map(|par| {
                    if !get(par, "children").is_array() {
                        set(par, "children", json!([]));
                    }
                    if let Some(kids) = par.get_mut("children").and_then(|c| c.as_array_mut()) {
                        kids.push(node.clone());
                    }
                });
            if found.is_none() {
                return Err(Bad("找不到这个父任务（可能刚被删了，刷新一下）".into()));
            }
        } else if let Some(pm) = project_mut(doc, &pid).and_then(|pm| tasks_mut(pm)) {
            pm.push(node.clone());
        }
        let pname = s_or(get(&p, "name"));
        log_act(doc, "task.add", &title, &pname);
        stamp_push(doc, &pid, &[nid.clone()], &nows);
        let saved = save(root, doc);
        *doc = saved.clone();
        return Ok(json!({"ok": true, "doc": saved, "id": nid}));
    }

    if action == "task.toggle" || action == "task.done" {
        let p = need_project(doc, get(b, "proj"))?.clone();
        let pid = s_or(get(&p, "id"));
        let tid = s_or(get(b, "id"));
        let want = b.get("done").cloned().unwrap_or(Value::Null);
        let mut did: Option<(String, bool, String)> = None;
        if let Some(pm) = project_mut(doc, &pid).and_then(|pm| tasks_mut(pm)) {
            if let Some(n) = find_node_mut(pm, &tid) {
                let prev = truthy(get(n, "done"));
                let next = if want.is_null() { !prev } else { truthy(&want) };
                set(n, "done", json!(next));
                set(n, "at", json!(if next { nows.clone() } else { String::new() }));
                if next {
                    set(n, "tok", json!(int0(get(&usage_of(&p, &host), "tok"))));
                }
                did = Some((s_or(get(n, "t")), next, s_or(get(n, "id"))));
            }
        }
        let Some((title, done, _nid)) = did else {
            return Err(Bad("找不到这条任务".into()));
        };
        log_act(doc, "task.toggle", &title, if done { "done" } else { "undone" });
        let saved = save(root, doc);
        *doc = saved.clone();
        return Ok(json!({"ok": true, "doc": saved}));
    }

    if action == "task.rename" || action == "task.edit" {
        let p = need_project(doc, get(b, "proj"))?.clone();
        let pid = s_or(get(&p, "id"));
        let tid = s_or(get(b, "id"));
        let has_title = b.get("title").is_some();
        let title = s_or(get(b, "title")).trim().to_string();
        if has_title && title.is_empty() {
            return Err(Bad("任务名不能为空".into()));
        }
        let mut logtitle = String::new();
        let mut found = false;
        if let Some(pm) = project_mut(doc, &pid).and_then(|pm| tasks_mut(pm)) {
            if let Some(n) = find_node_mut(pm, &tid) {
                found = true;
                if has_title {
                    set(n, "t", json!(util::cut(&title, 300)));
                }
                for k in ["due", "note", "link"] {
                    if b.get(k).is_some() {
                        let cap = if k == "note" { 2000 } else { 120 };
                        set(n, k, json!(util::cut(&s_or(get(b, k)), cap)));
                    }
                }
                logtitle = s_or(get(n, "t"));
            }
        }
        if !found {
            return Err(Bad("找不到这条任务".into()));
        }
        log_act(doc, "task.edit", &logtitle, "");
        let saved = save(root, doc);
        *doc = saved.clone();
        return Ok(json!({"ok": true, "doc": saved}));
    }

    if action == "task.del" || action == "task.remove" {
        let p = need_project(doc, get(b, "proj"))?.clone();
        let pid = s_or(get(&p, "id"));
        let tid = s_or(get(b, "id"));
        let mut removed_title = String::new();
        let mut ok = false;
        if let Some(pm) = project_mut(doc, &pid).and_then(|pm| tasks_mut(pm)) {
            if let Some(lst) = find_parent_list_mut(pm, &tid) {
                if let Some(pos) = lst.iter().position(|x| x.get("id").and_then(|v| v.as_str()) == Some(tid.as_str())) {
                    let hit = lst.remove(pos);
                    removed_title = s_or(get(&hit, "t"));
                    ok = true;
                }
            }
        }
        if !ok {
            return Err(Bad("找不到这条任务".into()));
        }
        log_act(doc, "task.del", &removed_title, "");
        let saved = save(root, doc);
        *doc = saved.clone();
        return Ok(json!({"ok": true, "doc": saved}));
    }

    if action == "task.move" {
        let p = need_project(doc, get(b, "proj"))?.clone();
        let pid = s_or(get(&p, "id"));
        let tid = s_or(get(b, "id"));
        let dest_id = s_or(get(b, "parent"));
        // 先把「不能拖进自己的子树」判掉（读不成环）
        if let Some(pm) = project_mut(doc, &pid).and_then(|pm| tasks_mut(pm)) {
            if find_node(pm, &tid).is_none() {
                return Err(Bad("找不到这条任务".into()));
            }
            if find_parent_list_mut(pm, &tid).is_none() {
                return Err(Bad("这条任务不在这个项目里".into()));
            }
            if let Some(dest) = find_node(pm, &dest_id) {
                let inside = s_or(get(dest, "id")) == tid
                    || dest
                        .get("children")
                        .and_then(|c| c.as_array())
                        .map(|kids| find_node(kids, &tid).is_some())
                        .unwrap_or(false);
                if inside {
                    return Err(Bad("不能把任务拖进它自己的子任务里".into()));
                }
            }
        }
        let mut node = Value::Null;
        if let Some(pm) = project_mut(doc, &pid).and_then(|pm| tasks_mut(pm)) {
            if let Some(lst) = find_parent_list_mut(pm, &tid) {
                if let Some(pos) = lst.iter().position(|x| x.get("id").and_then(|v| v.as_str()) == Some(tid.as_str())) {
                    node = lst.remove(pos);
                }
            }
        }
        if let Some(pm) = project_mut(doc, &pid).and_then(|pm| tasks_mut(pm)) {
            if let Some(dest) = find_node_mut(pm, &dest_id) {
                if !get(dest, "children").is_array() {
                    set(dest, "children", json!([]));
                }
                if let Some(kids) = dest.get_mut("children").and_then(|c| c.as_array_mut()) {
                    kids.push(node.clone());
                }
            } else {
                pm.push(node.clone());
            }
        }
        log_act(doc, "task.move", &s_or(get(&node, "t")), "");
        let saved = save(root, doc);
        *doc = saved.clone();
        return Ok(json!({"ok": true, "doc": saved}));
    }

    // ---- 收件箱
    if action == "inbox.add" {
        let text = s_or(get(b, "text")).trim().to_string();
        if text.is_empty() {
            return Err(Bad("写点什么再添加".into()));
        }
        let mut taken = all_ids(doc);
        let iid = uid("ib_", &mut taken);
        let title = util::cut(&text, 300);
        let item = json!({
            "id": iid,
            "t": title,
            "at": nows,
            "note": util::cut(&s_or(get(b, "note")), 500),
            // _push_inbox_now 会立刻给这条打上「推过」的戳（总线没迁，只打戳）
            "agent_h": th(&title),
            "agent_at": nows,
        });
        if let Some(arr) = doc.get_mut("inbox").and_then(|x| x.as_array_mut()) {
            arr.push(item.clone());
        }
        log_act(doc, "inbox.add", &text, "");
        let saved = save(root, doc);
        *doc = saved.clone();
        return Ok(json!({"ok": true, "doc": saved, "id": iid}));
    }

    if action == "inbox.del" {
        let inbox_len = doc.get("inbox").and_then(|x| x.as_array()).map(|a| a.len()).unwrap_or(0);
        let want = s_or(get(b, "id"));
        if let Some(arr) = doc.get_mut("inbox").and_then(|x| x.as_array_mut()) {
            arr.retain(|x| x.get("id").and_then(|v| v.as_str()) != Some(want.as_str()));
        }
        let left = doc.get("inbox").and_then(|x| x.as_array()).map(|a| a.len()).unwrap_or(0);
        log_act(doc, "inbox.del", &s_or(get(b, "t")), "");
        let saved = save(root, doc);
        *doc = saved.clone();
        return Ok(json!({"ok": true, "doc": saved, "removed": inbox_len - left}));
    }

    if action == "inbox.assign" || action == "inbox.apply" {
        let iid = s_or(get(b, "id"));
        let item = doc
            .get("inbox")
            .and_then(|x| x.as_array())
            .and_then(|a| a.iter().find(|x| x.get("id").and_then(|v| v.as_str()) == Some(iid.as_str())))
            .cloned();
        let Some(item) = item else {
            return Err(Bad("收件箱里已经没有这条了".into()));
        };
        let mut pid = s_or(get(b, "proj"));
        let parent = s_or(get(b, "parent"));
        let title = {
            let t = s_or(get(b, "title"));
            let t = if t.trim().is_empty() { s_or(get(&item, "t")) } else { t };
            t.trim().to_string()
        };
        let mut node: Option<Value> = None;
        let new_proj_name: String;
        if pid.is_empty() {
            let nm = split_first_clause(&title);
            let mut taken = all_ids(doc);
            let proj = norm_project(
                &json!({"name": util::cut(&nm, 24), "note": "", "match": {"keys": []}}),
                &mut taken,
            );
            let mut proj = proj;
            set(&mut proj, "created", json!(nows));
            pid = s_or(get(&proj, "id"));
            new_proj_name = s_or(get(&proj, "name"));
            if let Some(arr) = doc.get_mut("projects").and_then(|x| x.as_array_mut()) {
                arr.insert(0, proj);
            }
        } else {
            let p = need_project(doc, &json!(pid))?.clone();
            if !parent.is_empty() {
                let mut taken = all_ids(doc);
                let n = norm_node(&json!({"t": title, "src": "inbox"}), &mut taken);
                let mut ok = false;
                if let Some(pm) = project_mut(doc, &pid).and_then(|pm| tasks_mut(pm)) {
                    if let Some(host_node) = find_node_mut(pm, &parent) {
                        if !get(host_node, "children").is_array() {
                            set(host_node, "children", json!([]));
                        }
                        if let Some(kids) = host_node.get_mut("children").and_then(|c| c.as_array_mut()) {
                            kids.push(n.clone());
                            ok = true;
                        }
                    }
                }
                if !ok {
                    return Err(Bad("找不到这个子任务（可能刚被删了）".into()));
                }
                node = Some(n);
            } else {
                let mut taken = all_ids(doc);
                let n = norm_node(&json!({"t": title, "src": "inbox"}), &mut taken);
                if let Some(pm) = project_mut(doc, &pid).and_then(|pm| tasks_mut(pm)) {
                    pm.push(n.clone());
                }
                node = Some(n);
            }
            new_proj_name = s_or(get(&p, "name"));
        }
        if let Some(arr) = doc.get_mut("inbox").and_then(|x| x.as_array_mut()) {
            arr.retain(|x| x.get("id").and_then(|v| v.as_str()) != Some(iid.as_str()));
        }
        log_act(doc, "inbox.assign", &title, &new_proj_name);
        if let Some(n) = &node {
            stamp_push(doc, &pid, &[s_or(get(n, "id"))], &nows);
        }
        let saved = save(root, doc);
        *doc = saved.clone();
        return Ok(json!({"ok": true, "doc": saved, "proj": pid}));
    }

    if action == "inbox.clear" {
        let n = doc.get("inbox").and_then(|x| x.as_array()).map(|a| a.len()).unwrap_or(0);
        set(doc, "inbox", json!([]));
        log_act(doc, "inbox.clear", "", &format!("{} 条", n));
        let saved = save(root, doc);
        *doc = saved.clone();
        return Ok(json!({"ok": true, "doc": saved, "removed": n}));
    }

    // ---- 导入
    if action == "import" {
        let text = s_or(get(b, "text"));
        if text.trim().is_empty() {
            return Err(Bad("没有可导入的内容".into()));
        }
        let parsed = parse_plan(&text, &s_or(get(b, "name")));
        let ptasks = parsed.get("tasks").and_then(|x| x.as_array()).cloned().unwrap_or_default();
        if ptasks.is_empty() {
            return Err(Bad("没解析出任何任务：内容里没有标题、列表或编号行".into()));
        }
        if !truthy(get(b, "apply")) {
            return Ok(json!({"ok": true, "dry": true, "parsed": parsed}));
        }
        let pname = s_or(get(&parsed, "name"));
        let mut pid = s_or(get(b, "proj"));
        let mode = if truthy(get(b, "mode")) { s_or(get(b, "mode")) } else { "merge".into() };
        let mut proj_idx = if pid.is_empty() { None } else { project_index(doc, &pid) };
        if proj_idx.is_none() {
            if let Some(arr) = doc.get("projects").and_then(|x| x.as_array()) {
                if let Some(pos) = arr.iter().position(|x| s_or(get(x, "name")) == pname) {
                    if mode == "merge" {
                        proj_idx = Some(pos);
                    }
                }
            }
        }
        let added;
        if let Some(i) = proj_idx {
            let mut taken = all_ids(doc);
            let src: Vec<Value> = ptasks.iter().map(|t| norm_node(t, &mut taken)).collect();
            if mode == "replace" {
                added = src.len() as i64;
                if let Some(pm) = doc.get_mut("projects").and_then(|x| x.as_array_mut()).and_then(|a| a.get_mut(i)) {
                    set(pm, "tasks", json!(src));
                }
            } else {
                let mut have: Vec<(String, Value)> = doc
                    .get("projects")
                    .and_then(|x| x.as_array())
                    .and_then(|a| a.get(i))
                    .and_then(|p| p.get("tasks"))
                    .and_then(|t| t.as_array())
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|n| (key_of(&n), n))
                    .collect();
                let mut acc = 0;
                for s in src {
                    merge_one(&mut have, s, &mut acc, &nows);
                }
                added = acc;
                let merged: Vec<Value> = have.into_iter().map(|(_, v)| v).collect();
                if let Some(pm) = doc.get_mut("projects").and_then(|x| x.as_array_mut()).and_then(|a| a.get_mut(i)) {
                    set(pm, "tasks", json!(merged));
                }
            }
            pid = doc
                .get("projects")
                .and_then(|x| x.as_array())
                .and_then(|a| a.get(i))
                .map(|p| s_or(get(p, "id")))
                .unwrap_or_default();
        } else {
            let mut taken = all_ids(doc);
            let mut proj = norm_project(
                &json!({"name": pname, "note": s_or(get(b, "note")),
                        "match": {"keys": [pname.clone()]}}),
                &mut taken,
            );
            set(&mut proj, "created", json!(nows));
            let tasks: Vec<Value> = ptasks.iter().map(|t| norm_node(t, &mut taken)).collect();
            // Python: sum(1 for _n,_s,_p in walk(proj["tasks"])) —— 连子任务一起数
            added = all_nodes(&tasks).len() as i64;
            set(&mut proj, "tasks", json!(tasks));
            pid = s_or(get(&proj, "id"));
            if let Some(arr) = doc.get_mut("projects").and_then(|x| x.as_array_mut()) {
                arr.insert(0, proj);
            }
        }
        log_act(doc, "import", &pname, &format!("{} 条", added));
        let saved = save(root, doc);
        *doc = saved.clone();
        return Ok(json!({"ok": true, "doc": saved, "proj": pid, "added": added, "parsed": parsed}));
    }

    // ---- 推给 agent 总线（总线未迁，只打戳）
    if action == "agent.export" || action == "agent.push" {
        let pid = s_or(get(b, "proj"));
        let tid = s_or(get(b, "id"));
        let scope = if truthy(get(b, "scope")) { s_or(get(b, "scope")) } else { "open".into() };
        let mut jobs: Vec<(String, Vec<String>)> = Vec::new();
        if !pid.is_empty() && !tid.is_empty() {
            let p = need_project(doc, &json!(pid))?.clone();
            let node = p
                .get("tasks")
                .and_then(|t| t.as_array())
                .and_then(|a| find_node(a, &tid));
            let Some(node) = node else {
                return Err(Bad("找不到这条任务".into()));
            };
            jobs.push((s_or(get(&p, "id")), vec![s_or(get(node, "id"))]));
        } else if !pid.is_empty() {
            let p = need_project(doc, &json!(pid))?.clone();
            let ids = p
                .get("tasks")
                .and_then(|t| t.as_array())
                .map(|a| {
                    all_nodes(a)
                        .into_iter()
                        .filter(|n| scope == "all" || !truthy(get(n, "done")))
                        .map(|n| s_or(get(n, "id")))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            jobs.push((s_or(get(&p, "id")), ids));
        } else {
            for p in doc.get("projects").and_then(|x| x.as_array()).cloned().unwrap_or_default() {
                let ids = p
                    .get("tasks")
                    .and_then(|t| t.as_array())
                    .map(|a| {
                        all_nodes(a)
                            .into_iter()
                            .filter(|n| scope == "all" || !truthy(get(n, "done")))
                            .map(|n| s_or(get(n, "id")))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                if !ids.is_empty() {
                    jobs.push((s_or(get(&p, "id")), ids));
                }
            }
        }
        let mut tasks = 0;
        for (p, nodes) in &jobs {
            tasks += nodes.len();
            stamp_push(doc, p, nodes, &nows);
        }
        log_act(doc, "agent.export", &scope, "0 条");
        let saved = save(root, doc);
        *doc = saved.clone();
        return Ok(json!({"ok": true, "doc": saved, "written": 0, "dup": 0, "tasks": tasks}));
    }

    Err(Bad(format!(
        "不认识的 action：{}",
        if action.is_empty() { "（空）" } else { action }
    )))
}

fn key_of(n: &Value) -> String {
    format!("{}\u{0}{}", s_or(get(n, "t")).trim(), truthy(get(n, "done")))
}

fn merge_one(have: &mut Vec<(String, Value)>, s: Value, added: &mut i64, nows: &str) {
    let k = key_of(&s);
    if let Some(slot) = have.iter_mut().find(|(hk, _)| *hk == k) {
        let tgt = &mut slot.1;
        if truthy(get(&s, "done")) && !truthy(get(tgt, "done")) {
            set(tgt, "done", json!(true));
            set(tgt, "at", json!(nows));
        }
        if !get(tgt, "children").is_array() {
            set(tgt, "children", json!([]));
        }
        let mut kids: Vec<(String, Value)> = tgt
            .get("children")
            .and_then(|c| c.as_array())
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|n| (key_of(&n), n))
            .collect();
        for sc in s.get("children").and_then(|c| c.as_array()).cloned().unwrap_or_default() {
            merge_one(&mut kids, sc, added, nows);
        }
        let merged: Vec<Value> = kids.into_iter().map(|(_, v)| v).collect();
        set(tgt, "children", json!(merged));
    } else {
        let cnt = 1 + s
            .get("children")
            .and_then(|c| c.as_array())
            .map(|a| all_nodes(a).len())
            .unwrap_or(0);
        have.push((k, s));
        *added += cnt as i64;
    }
}

fn split_first_clause(title: &str) -> String {
    static R: OnceLock<Regex> = OnceLock::new();
    let re = R.get_or_init(|| Regex::new(r"[，,。.；;：:！!？?\n]").unwrap());
    let first = re.split(title).next().unwrap_or("").trim().to_string();
    if first.is_empty() {
        title.to_string()
    } else {
        first
    }
}

/// `_push_now`：推过且没改名的不再打戳。总线未迁，只改 agent_h/agent_at。
fn stamp_push(doc: &mut Value, pid: &str, node_ids: &[String], nows: &str) {
    let Some(pm) = project_mut(doc, pid) else { return };
    let Some(tasks) = tasks_mut(pm) else { return };
    for nid in node_ids {
        if let Some(n) = find_node_mut(tasks, nid) {
            let h = th(&s_or(get(n, "t")));
            if s_or(get(n, "agent_h")) == h && truthy(get(n, "agent_at")) {
                continue;
            }
            set(n, "agent_h", json!(h));
            set(n, "agent_at", json!(nows));
        }
    }
}

// ---------------------------------------------------------------- 视图

fn short_name(k: &str, n: &str) -> String {
    let mut s = n.to_string();
    if k.starts_with("proj:") {
        for pre in [
            "Users-pamphile-Documents-AI项目-",
            "Users-pamphile-Documents-",
            "Users-pamphile-WorkBuddy-",
            "Users-pamphile-",
        ] {
            if s.starts_with(pre) {
                s = s[pre.len()..].to_string();
                break;
            }
        }
    }
    if s.is_empty() {
        n.to_string()
    } else {
        s
    }
}

pub fn payload(ctx: &Ctx, root: &Path) -> Value {
    let doc = load(root);
    let host = hostwork::host_projects(ctx);
    let mut projects: Vec<Value> = Vec::new();
    for p in doc.get("projects").and_then(|x| x.as_array()).cloned().unwrap_or_default() {
        let tasks = p.get("tasks").and_then(|t| t.as_array()).cloned().unwrap_or_default();
        let (done, total) = count(&tasks);
        let u = usage_of(&p, &host);
        let mut on = 0;
        let mut p2 = p.clone();
        if let Some(ts) = p2.get_mut("tasks").and_then(|t| t.as_array_mut()) {
            for n in ts.iter_mut() {
                apply_agent_flag(n, &mut on);
            }
        }
        let pct = if total != 0 { util::py_round_int(done as f64 * 100.0 / total as f64) } else { 0 };
        let mut out = p2;
        set(&mut out, "stat", json!({"done": done, "total": total, "pct": pct}));
        set(&mut out, "usage", u);
        set(&mut out, "agent", json!({"on": on, "total": total}));
        projects.push(out);
    }
    let today = util::today();
    let mut done_today = 0;
    for p in &projects {
        if let Some(ts) = p.get("tasks").and_then(|t| t.as_array()) {
            for n in all_nodes(ts) {
                if truthy(get(n, "done")) && s_or(get(n, "at")).starts_with(&today) {
                    done_today += 1;
                }
            }
        }
    }
    let all_done: i64 = projects.iter().map(|p| int0(get(get(p, "stat"), "done"))).sum();
    let all_total: i64 = projects.iter().map(|p| int0(get(get(p, "stat"), "total"))).sum();
    let pct = if all_total != 0 {
        util::py_round_int(all_done as f64 * 100.0 / all_total as f64)
    } else {
        0
    };
    let inbox = candidates(root, &[]);
    let inbox_n = doc.get("inbox").and_then(|x| x.as_array()).map(|a| a.len()).unwrap_or(0);
    let host_out = json!({
        "spaces": host.get("spaces").and_then(|x| x.as_array()).cloned().unwrap_or_default()
            .iter().map(|x| json!({
                "k": s_or(get(x, "k")),
                "n": s_or(get(x, "n")),
                "sn": short_name(&s_or(get(x, "k")), &s_or(get(x, "n"))),
                "tok": int0(get(x, "tok")),
            })).collect::<Vec<_>>(),
        "projs": host.get("projs").and_then(|x| x.as_array()).cloned().unwrap_or_default()
            .iter().map(|x| json!({
                "k": s_or(get(x, "k")),
                "n": s_or(get(x, "n")),
                "sn": short_name(&s_or(get(x, "k")), &s_or(get(x, "n"))),
                "tok": int0(get(x, "tok")),
            })).collect::<Vec<_>>(),
        "at": s_or(get(&host, "at")),
        "err": s_or(get(&host, "err")),
    });
    json!({
        "ok": true,
        "rev": int0(get(&doc, "rev")),
        "updated_at": s_or(get(&doc, "updated_at")),
        "projects": projects,
        "inbox": inbox,
        "inbox_n": inbox_n,
        "stat": {"done": all_done, "total": all_total, "done_today": done_today, "pct": pct},
        "host": host_out,
        "dir": plan_dir(root).to_string_lossy().to_string(),
    })
}

fn apply_agent_flag(n: &mut Value, on: &mut i64) {
    let synced = truthy(get(n, "agent_at")) && s_or(get(n, "agent_h")) == th(&s_or(get(n, "t")));
    let a = synced || truthy(get(n, "done"));
    set(n, "a", json!(a));
    if a {
        *on += 1;
    }
    if let Some(kids) = n.get_mut("children").and_then(|c| c.as_array_mut()) {
        for k in kids.iter_mut() {
            apply_agent_flag(k, on);
        }
    }
}

// ---------------------------------------------------------------- 解析导入

fn re_head() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^(#{1,6})\s+(.*)$").unwrap())
}
fn re_item() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^(\s*)(?:[-*+•]|\d+[.)])\s+(.*)$").unwrap())
}
fn re_box() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^\[([ xX✓✔])\]\s*(.*)$").unwrap())
}
fn re_bold() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\*\*(.+?)\*\*").unwrap())
}
fn re_code() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"`(.+?)`").unwrap())
}

fn clean_line(s: &str, keep_indent: bool) -> String {
    let s = s.replace('\u{200b}', "");
    let s = s.trim_end();
    let s = re_bold().replace_all(s, "$1").to_string();
    let s = re_code().replace_all(&s, "$1").to_string();
    if keep_indent {
        s
    } else {
        s.trim().to_string()
    }
}

fn parse_plan(text: &str, name: &str) -> Value {
    let mut name = name.trim().to_string();
    let raw_lines: Vec<&str> = text.lines().collect();
    let lines: Vec<&str> = raw_lines.into_iter().filter(|l| !l.trim().is_empty()).collect();
    let mut units: Vec<usize> = Vec::new();
    for l in &lines {
        let cl = clean_line(l, true);
        if let Some(m) = re_item().captures(&cl) {
            let g = m.get(1).map(|x| x.as_str()).unwrap_or("");
            if !g.is_empty() {
                units.push(g.replace('\t', "  ").chars().count());
            }
        }
    }
    let unit = units.into_iter().min().unwrap_or(2).max(1) as i64;

    // 树用「arena + 下标」搭：节点先全部放进 nodes，父子只存下标。
    // 直接往 stack 里的 Value 塞 children 是错的 —— stack 拿的是克隆，
    // 改它不会回流到已经放进 roots 的那一份（子任务会整层消失，踩过一次）。
    let mut nodes: Vec<Value> = Vec::new();
    let mut kids: Vec<Vec<usize>> = Vec::new();
    let mut roots: Vec<usize> = Vec::new();
    let mut stack: Vec<(i64, usize)> = Vec::new();
    let mut head_depth: i64 = 0;

    fn attach(
        nodes: &mut Vec<Value>,
        kids: &mut Vec<Vec<usize>>,
        roots: &mut Vec<usize>,
        stack: &mut Vec<(i64, usize)>,
        depth: i64,
        node: Value,
    ) {
        while let Some(top) = stack.last() {
            if top.0 >= depth {
                stack.pop();
            } else {
                break;
            }
        }
        let idx = nodes.len();
        nodes.push(node);
        kids.push(Vec::new());
        match stack.last() {
            Some(top) => kids[top.1].push(idx),
            None => roots.push(idx),
        }
        stack.push((depth, idx));
    }

    for raw in &lines {
        let line_raw = clean_line(raw, true);
        let line = line_raw.trim().to_string();
        if line.is_empty() {
            continue;
        }
        if let Some(mh) = re_head().captures(&line) {
            let level = mh.get(1).map(|x| x.as_str().chars().count()).unwrap_or(0) as i64;
            let title = clean_line(mh.get(2).map(|x| x.as_str()).unwrap_or(""), false);
            if title.is_empty() {
                continue;
            }
            if level == 1 && name.is_empty() {
                name = title;
                head_depth = 0;
                stack.clear();
                continue;
            }
            head_depth = level;
            let node = json!({"t": title, "done": false, "due": "", "note": "",
                              "src": "import", "children": []});
            attach(&mut nodes, &mut kids, &mut roots, &mut stack, level, node);
            continue;
        }
        if let Some(mi) = re_item().captures(&line_raw) {
            let indent = mi
                .get(1)
                .map(|x| x.as_str().replace('\t', "  ").chars().count())
                .unwrap_or(0) as i64;
            let mut body = clean_line(mi.get(2).map(|x| x.as_str()).unwrap_or(""), false);
            let mut done = false;
            if let Some(mb) = re_box().captures(&body) {
                let g = mb.get(1).map(|x| x.as_str()).unwrap_or("").trim().to_lowercase();
                done = g == "x" || g == "✓" || g == "✔";
                body = clean_line(mb.get(2).map(|x| x.as_str()).unwrap_or(""), false);
            }
            if body.is_empty() {
                continue;
            }
            let depth = head_depth + 1 + indent / unit;
            let node = json!({"t": body, "done": done, "due": "", "note": "",
                              "src": "import", "children": []});
            attach(&mut nodes, &mut kids, &mut roots, &mut stack, depth, node);
            continue;
        }
        if line.chars().count() <= 120 && !line.ends_with('：') && !line.ends_with(':') {
            let depth = head_depth + 1;
            let body0 = clean_line(line.trim_start_matches(['-', '*', '+', ' '].as_ref()), false);
            let mut body = body0;
            let mut done = false;
            if let Some(mb) = re_box().captures(&body) {
                let g = mb.get(1).map(|x| x.as_str()).unwrap_or("").trim().to_lowercase();
                done = g == "x" || g == "✓" || g == "✔";
                body = clean_line(mb.get(2).map(|x| x.as_str()).unwrap_or(""), false);
            }
            if !body.is_empty() {
                let node = json!({"t": body, "done": done, "due": "", "note": "",
                                  "src": "import", "children": []});
                attach(&mut nodes, &mut kids, &mut roots, &mut stack, depth, node);
            }
        }
    }

    // 下标树 → 真树（倒序装，子节点先成型）
    fn build(nodes: &[Value], kids: &[Vec<usize>], idx: usize) -> Value {
        let mut n = nodes[idx].clone();
        let children: Vec<Value> = kids[idx].iter().map(|i| build(nodes, kids, *i)).collect();
        set(&mut n, "children", json!(children));
        n
    }
    let root_nodes: Vec<Value> = roots.iter().map(|i| build(&nodes, &kids, *i)).collect();
    let mut taken: HashSet<String> = HashSet::new();
    let tasks: Vec<Value> = root_nodes.iter().map(|t| norm_node(t, &mut taken)).collect();
    let (done, total) = count(&tasks);
    json!({
        "name": if name.is_empty() { "导入的计划".to_string() } else { name },
        "tasks": tasks,
        "stats": {"done": done, "total": total},
    })
}

// ---------------------------------------------------------------- AI 归位

pub fn triage_prompt(items: &[Value], projects: &[Value]) -> String {
    let mut lines: Vec<String> = vec![
        "你是任务归类助手。把每条「自由记录」归到下面某个计划的某个子任务；".to_string(),
        "如果都不合适，就提出新建子任务或新建计划。只能从给出的编号里选。".to_string(),
        String::new(),
    ];
    lines.push("【计划与子任务】".to_string());
    for p in projects {
        lines.push(format!("- 项目 {}｜{}", s_or(get(p, "id")), s_or(get(p, "name"))));
        let tasks = p.get("tasks").and_then(|t| t.as_array()).cloned().unwrap_or_default();
        for t in flatten_names(&tasks) {
            let depth = int0(get(&t, "depth")) as usize;
            lines.push(format!(
                "    - 子任务 {}｜{}{}",
                s_or(get(&t, "id")),
                "  ".repeat(depth),
                s_or(get(&t, "t"))
            ));
        }
    }
    lines.push(String::new());
    lines.push("【待归位的自由记录】".to_string());
    for it in items {
        lines.push(format!("- 记录 {}｜{}", s_or(get(it, "id")), s_or(get(it, "t"))));
    }
    lines.push(String::new());
    lines.push("只输出 JSON 对象，不要解释：".to_string());
    lines.push(
        "{\"items\":[{\"id\":\"记录id\",\"proj\":\"项目id或空\",\"parent\":\"子任务id或空\",\"title\":\"若新建，给出任务标题\",\"new_proj\":\"若新建项目，给出项目名\"}]}"
            .to_string(),
    );
    lines.push("规则：挂到已有子任务就填 parent；归到项目但没对应子任务就 parent 留空并给 title；".to_string());
    lines.push("都不合适就 proj 留空并给 new_proj。".to_string());
    lines.join("\n")
}

/// `apply_ai_assign`：每项独立成败，一条坏不影响别的。
pub fn apply_ai_assign(ctx: &Ctx, root: &Path, items: &[Value]) -> Value {
    let mut ok = 0;
    let mut bad: Vec<Value> = Vec::new();
    for it in items {
        let body = json!({
            "action": "inbox.assign",
            "id": it.get("id").cloned().unwrap_or(Value::Null),
            "proj": it.get("proj").cloned().unwrap_or(Value::Null),
            "parent": it.get("parent").cloned().unwrap_or(Value::Null),
            "title": it.get("title").cloned().unwrap_or(Value::Null),
        });
        let r = act(ctx, root, &body);
        if truthy(get(&r, "ok")) {
            ok += 1;
        } else {
            bad.push(json!({"id": it.get("id").cloned().unwrap_or(Value::Null),
                            "error": r.get("error").cloned().unwrap_or(Value::Null)}));
        }
    }
    json!({"ok": true, "applied": ok, "failed": bad})
}

// ---------------------------------------------------------------- markdown 镜像

pub fn export_md(root: &Path, doc: &Value) -> std::io::Result<()> {
    let mut out: Vec<String> = vec![
        "# AI 工作计划".to_string(),
        String::new(),
        format!(
            "_更新：{}　rev {}_",
            s_or(get(doc, "updated_at")),
            int0(get(doc, "rev"))
        ),
        String::new(),
    ];
    for p in doc.get("projects").and_then(|x| x.as_array()).cloned().unwrap_or_default() {
        let tasks = p.get("tasks").and_then(|t| t.as_array()).cloned().unwrap_or_default();
        let (done, total) = count(&tasks);
        out.push(format!("## {}（{}/{}）", s_or(get(&p, "name")), done, total));
        let note = s_or(get(&p, "note"));
        if !note.is_empty() {
            out.push(format!("> {}", note));
        }
        fn dump(nodes: &[Value], d: usize, out: &mut Vec<String>) {
            for n in nodes {
                let bx = if truthy(get(n, "done")) { "x" } else { " " };
                let due = if truthy(get(n, "due")) {
                    format!("  ·  截止 {}", s_or(get(n, "due")))
                } else {
                    String::new()
                };
                out.push(format!("{}- [{}] {}{}", "  ".repeat(d), bx, s_or(get(n, "t")), due));
                if let Some(kids) = n.get("children").and_then(|c| c.as_array()) {
                    dump(kids, d + 1, out);
                }
            }
        }
        dump(&tasks, 0, &mut out);
        out.push(String::new());
    }
    let inbox = doc.get("inbox").and_then(|x| x.as_array()).cloned().unwrap_or_default();
    if !inbox.is_empty() {
        out.push("## 待归位".to_string());
        out.push(String::new());
        for x in &inbox {
            out.push(format!("- {}", s_or(get(x, "t"))));
        }
        out.push(String::new());
    }
    let p = plan_dir(root).join("计划.md");
    util::ensure_dir(p.parent().unwrap_or(Path::new(".")));
    std::fs::write(&p, out.join("\n"))
}

/// `/api/plan/act` 的那两个判断：`r.get("ok") and not r.get("dry")`。
pub fn is_ok_view(r: &Value) -> bool {
    truthy(get(r, "ok")) && !truthy(get(r, "dry"))
}

/// 供路由用：`planmod.payload(planmod.root_of(CFG))`
pub fn payload_ctx(ctx: &Ctx) -> Value {
    payload(ctx, &ctx.runtime_dir)
}

/// 本机模型建议（`/api/plan/triage` 的 AI 分支）。
pub async fn ai_assign(
    ctx: &Ctx,
    root: &Path,
    items: &mut [Value],
) -> (bool, String, String) {
    // (是否跑了模型, 模型名, note)
    let weak: Vec<Value> = items
        .iter()
        .filter(|x| {
            util::as_f64(get(get(x, "hint"), "score")).unwrap_or(0.0) < 0.45
        })
        .cloned()
        .collect();
    if weak.is_empty() {
        return (false, String::new(), "代码已经能归位，不用问模型".to_string());
    }
    let doc = load(root);
    let model = dashrs::local_model_cfg(ctx);
    let projects = doc.get("projects").and_then(|x| x.as_array()).cloned().unwrap_or_default();
    let prompt = triage_prompt(&weak, &projects);
    let raw = match dashrs::llm_json(&prompt, &model, 600, 90).await {
        Ok(r) => r,
        Err(e) => {
            let kind = if e.is_empty() { "URLError".to_string() } else { "URLError".to_string() };
            return (false, String::new(), format!("模型没答上来（{}），先按代码建议", kind));
        }
    };
    let got = dashrs::grab_json(&raw);
    let arr = got.get("items").and_then(|x| x.as_array()).cloned().unwrap_or_default();
    let known_p: HashSet<String> = projects.iter().map(|p| s_or(get(p, "id"))).collect();
    let mut known_t: HashSet<String> = HashSet::new();
    for p in &projects {
        if let Some(ts) = p.get("tasks").and_then(|t| t.as_array()) {
            for n in all_nodes(ts) {
                known_t.insert(s_or(get(n, "id")));
            }
        }
    }
    let mut hit = 0;
    let weak_ids: HashSet<String> = weak.iter().map(|x| s_or(get(x, "id"))).collect();
    for it in arr {
        if !it.is_object() {
            continue;
        }
        let rid = s_or(get(&it, "id"));
        if !weak_ids.contains(&rid) {
            continue;
        }
        let Some(pos) = items.iter().position(|x| s_or(get(x, "id")) == rid) else {
            continue;
        };
        let mut pid = s_or(get(&it, "proj"));
        let mut par = s_or(get(&it, "parent"));
        // 模型给的 id 必须是真实存在的；编出来的当没看见，保留代码建议
        if !pid.is_empty() && !known_p.contains(&pid) {
            pid.clear();
        }
        if !par.is_empty() && !known_t.contains(&par) {
            par.clear();
        }
        let title = {
            let t = s_or(get(&it, "title"));
            if t.is_empty() {
                s_or(get(&items[pos], "t"))
            } else {
                t
            }
        };
        let hint = match items[pos].get_mut("hint") {
            Some(h) => h,
            None => continue,
        };
        let why = if par.is_empty() {
            "模型判断：新建一条子任务".to_string()
        } else {
            format!("模型判断：挂到「{}」", par)
        };
        set(hint, "proj", json!(pid));
        set(hint, "task_id", json!(par));
        set(hint, "task", json!(util::cut(&title, 300)));
        set(hint, "new_task", json!(par.is_empty()));
        set(hint, "score", json!(0.0));
        set(hint, "why", json!(why));
        set(hint, "ai", json!(true));
        set(hint, "new_proj", json!(util::cut(&s_or(get(&it, "new_proj")), 80)));
        hit += 1;
    }
    let note = if hit > 0 {
        format!("模型给了 {} 条判断", hit)
    } else {
        "模型没给出可用判断，先按代码建议".to_string()
    };
    (true, model, note)
}
