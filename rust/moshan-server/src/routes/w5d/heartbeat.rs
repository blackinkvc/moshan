// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：`GET /api/heartbeat` + `POST /api/heartbeat/run` + 后台心跳循环。
//       系统心跳 = 纯代码自检 + 自愈（heartbeat.py 的 mac 分支），全程不调模型。
// 不做：`GET /api/heartbeat` 的只读状态投影在 live::heartbeat_value；本文件负责
//       「跑一轮」、调度、以及把结果落到 心跳/最新.json + 流水.jsonl + 磁盘.jsonl。
//
// 说明：heartbeat 的检查项全是 mac 常驻结构（gateway / launchd / node / 会话库），
// 落盘位置、字段顺序、文案都照着 Python 抄。自愈动作（拉起 gateway / 巡检 /
// App 监工）同样保留 —— 这是这条接口的语义，不能为了对拍把它阉掉。

#![allow(dead_code)]

use super::util::{self, py_round, s_of, trunc};
use crate::jsonio;
use crate::paths::Ctx;
use crate::routes::common::{peek, CtxState};
use axum::extract::State;
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const GW_PORT: i64 = 18789;
const INSPECT_PORT: i64 = 8774;
const OLLAMA_URL: &str = "http://127.0.0.1:11434/api/tags";
const BREW_NODE: &str = "/opt/homebrew/opt/node@22/bin/node";
const MIN_NODE: (i64, i64, i64) = (22, 22, 3);
const SESSION_WARN_BYTES: i64 = 80 * 1024;
const SESSION_KEEP_LINES: i64 = 30;
const SESSION_COMPACT_COOLDOWN: f64 = 600.0;
const RETRY_AFTER: f64 = 1800.0;

fn home() -> PathBuf {
    util::home()
}

fn openclaw() -> PathBuf {
    home().join(".openclaw")
}

fn lock_dir() -> PathBuf {
    openclaw().join("tmp").join("openclaw-501")
}

fn serve_sh() -> PathBuf {
    openclaw().join("serve-gateway.sh")
}

fn plist() -> PathBuf {
    home()
        .join("Library")
        .join("LaunchAgents")
        .join("com.local-ai.openclaw-gateway.plist")
}

fn agent_db() -> PathBuf {
    openclaw().join("agents").join("main").join("agent").join("openclaw-agent.sqlite")
}

fn openclaw_mjs() -> PathBuf {
    PathBuf::from("/opt/homebrew/lib/node_modules/openclaw/openclaw.mjs")
}

fn backup_dir() -> PathBuf {
    openclaw().join("backups")
}

fn log_dir() -> PathBuf {
    home().join("Library").join("Logs")
}

fn sys_dir(ctx: &Ctx) -> PathBuf {
    ctx.library_dir.join("10-系统记录")
}

fn hb_dir(ctx: &Ctx) -> PathBuf {
    sys_dir(ctx).join("心跳")
}

fn state_file(ctx: &Ctx) -> PathBuf {
    hb_dir(ctx).join("最新.json")
}

fn log_file(ctx: &Ctx) -> PathBuf {
    hb_dir(ctx).join("流水.jsonl")
}

fn disk_log(ctx: &Ctx) -> PathBuf {
    sys_dir(ctx).join("磁盘").join("磁盘.jsonl")
}

fn inspect_cmd(ctx: &Ctx) -> PathBuf {
    ctx.library_dir.join("_巡检").join("_系统巡检.command")
}

fn appwatch_cmd(ctx: &Ctx) -> PathBuf {
    sys_dir(ctx).join("_App监工.command")
}

fn appwatch_state_path() -> PathBuf {
    log_dir().join("cuntu-appwatch.state.json")
}

// ---------------------------------------------------------------- STATE
//
// 运行态在 crate::live（与 /api/dash、GET /api/heartbeat 共用一份）。
// Python 的 heartbeat.STATE 是有序字典，键名/顺序由 live 那份字面量固定。

fn st_get(k: &str) -> Value {
    crate::live::hb_get(k)
}

fn st_set(k: &str, v: Value) {
    crate::live::hb_set(k, v)
}

fn st_i64(k: &str) -> i64 {
    crate::live::hb_i64(k)
}

fn st_f64(k: &str) -> f64 {
    crate::live::hb_f64(k)
}

fn st_inc(k: &str) -> i64 {
    crate::live::hb_inc(k)
}

/// `_LOCK`：同一时刻只允许一轮自检。用 AtomicBool 而不是 Mutex：
/// 单轮 panic 不会留下毒锁，下一轮还能继续。
fn busy() -> &'static std::sync::atomic::AtomicBool {
    static B: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    &B
}

struct BusyGuard;
impl Drop for BusyGuard {
    fn drop(&mut self) {
        busy().store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

fn now_hms() -> String {
    util::local_fmt("%Y-%m-%d %H:%M:%S")
}

// ---------------------------------------------------------------- HTTP

/// loopback 探活：只发一个 HTTP/1.0 GET，够用且不引异步运行时。
fn http_ok(url: &str, timeout_s: f64) -> bool {
    let rest = url.strip_prefix("http://").unwrap_or(url);
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let addr = match hostport.parse() {
        Ok(a) => a,
        Err(_) => return false,
    };
    let timeout = Duration::from_secs_f64(timeout_s.max(0.05));
    let Ok(mut s) = TcpStream::connect_timeout(&addr, timeout) else { return false };
    let _ = s.set_read_timeout(Some(timeout));
    let _ = s.set_write_timeout(Some(timeout));
    let req = format!("GET {path} HTTP/1.0\r\nHost: {hostport}\r\nConnection: close\r\n\r\n");
    if s.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 512];
    let n = match s.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return false,
    };
    let head = String::from_utf8_lossy(&buf[..n]);
    let code = head
        .split_whitespace()
        .nth(1)
        .and_then(|x| x.parse::<i64>().ok())
        .unwrap_or(0);
    (200..400).contains(&code)
}

fn pid_alive(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    crate::hostwork::platform::proc_alive(pid)
}

// ---------------------------------------------------------------- 检查

fn node_version(exe: &str) -> Option<(i64, i64, i64)> {
    let argv = vec![exe.to_string(), "--version".into()];
    let (_, out, err) = util::run_cmd(&argv, 6, None)?;
    let re = regex::Regex::new(r"(\d+)\.(\d+)\.(\d+)").unwrap();
    let text = format!("{out}{err}");
    let c = re.captures(&text)?;
    Some((
        c.get(1)?.as_str().parse().ok()?,
        c.get(2)?.as_str().parse().ok()?,
        c.get(3)?.as_str().parse().ok()?,
    ))
}

fn check_node() -> Value {
    let brew = node_version(BREW_NODE);
    let brew_ok = brew.map(|v| v >= MIN_NODE).unwrap_or(false);
    let path_node = util::which("node")
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let path_ver = if path_node.is_empty() { None } else { node_version(&path_node) };
    let path_ok = path_ver.map(|v| v >= MIN_NODE).unwrap_or(false);
    let launcher = std::fs::read_to_string(serve_sh()).unwrap_or_default();
    let launcher_ok = launcher.contains(BREW_NODE) && launcher.contains("exec \"$NODE_BIN\"");
    let tuple = |v: Option<(i64, i64, i64)>| match v {
        Some((a, b, c)) => json!([a, b, c]),
        None => Value::Null,
    };
    json!({
        "brew": tuple(brew),
        "brew_ok": brew_ok,
        "path": tuple(path_ver),
        "path_ok": path_ok,
        "path_exe": path_node,
        "launcher_ok": launcher_ok,
        "launcher_exists": !launcher.is_empty(),
        "min": [MIN_NODE.0, MIN_NODE.1, MIN_NODE.2],
    })
}

#[derive(Clone)]
struct LockItem {
    file: PathBuf,
    pid: Value,
    alive: bool,
}

fn scan_locks() -> Vec<LockItem> {
    let d = lock_dir();
    if !d.is_dir() {
        return Vec::new();
    }
    let mut files: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&d) {
        for ent in rd.flatten() {
            let name = ent.file_name().to_string_lossy().to_string();
            if name.starts_with("gateway.") && name.ends_with(".lock") {
                files.push(ent.path());
            }
        }
    }
    files.sort();
    files
        .into_iter()
        .map(|f| {
            let data = util::read_json(&f).unwrap_or(Value::Null);
            let pid = data.get("pid").cloned().unwrap_or(Value::Null);
            let alive = pid_alive(util::i64_of(Some(&pid)));
            LockItem { file: f, pid, alive }
        })
        .collect()
}

fn clean_stale_locks(items: &[LockItem]) -> Vec<String> {
    let mut removed = Vec::new();
    for it in items {
        if it.alive {
            continue;
        }
        let stem = util::name_of(&it.file);
        let mut cands: Vec<PathBuf> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(lock_dir()) {
            for ent in rd.flatten() {
                let n = ent.file_name().to_string_lossy().to_string();
                if n.starts_with(&stem) {
                    cands.push(ent.path());
                }
            }
        }
        cands.sort();
        for c in cands {
            let ok = if c.is_dir() { std::fs::remove_dir_all(&c).is_ok() } else { std::fs::remove_file(&c).is_ok() };
            if ok {
                removed.push(util::name_of(&c));
            }
        }
    }
    removed
}

fn check_launchd() -> Value {
    let p = plist();
    let plist_exists = p.exists();
    let mut lint_ok = false;
    if plist_exists {
        let argv = vec!["/usr/bin/plutil".to_string(), "-lint".into(), p.to_string_lossy().to_string()];
        lint_ok = matches!(util::run_cmd(&argv, 6, None), Some((0, _, _)));
    }
    let argv = vec![
        "/bin/launchctl".to_string(),
        "print".into(),
        "gui/501/com.local-ai.openclaw-gateway".into(),
    ];
    let loaded = match util::run_cmd(&argv, 6, None) {
        Some((0, out, _)) => out.contains("state = running"),
        _ => false,
    };
    json!({"plist_exists": plist_exists, "lint_ok": lint_ok, "loaded": loaded})
}

fn check_session() -> Value {
    let db = agent_db();
    let mut out = json!({"db_exists": db.exists(), "sid": "", "events": 0, "bytes": 0});
    if !db.exists() {
        return out;
    }
    let sql = "SELECT session_id, count(*), sum(length(event_json)) FROM transcript_events \
               GROUP BY session_id ORDER BY count(*) DESC LIMIT 1";
    let argv = vec![
        "sqlite3".to_string(),
        "-readonly".into(),
        "-separator".into(),
        "\t".into(),
        db.to_string_lossy().to_string(),
        sql.into(),
    ];
    if let Some((0, stdout, _)) = util::run_cmd(&argv, 6, None) {
        let line = stdout.lines().next().unwrap_or("");
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() >= 3 {
            if let Some(o) = out.as_object_mut() {
                o.insert("sid".into(), Value::String(parts[0].to_string()));
                o.insert("events".into(), json!(parts[1].parse::<i64>().unwrap_or(0)));
                o.insert("bytes".into(), json!(parts[2].parse::<i64>().unwrap_or(0)));
            }
        }
    }
    out
}

fn backup_agent_db() -> String {
    if std::fs::create_dir_all(backup_dir()).is_err() {
        return String::new();
    }
    let ts = util::local_fmt("%Y%m%d-%H%M%S");
    let dst = backup_dir().join(format!("openclaw-agent-{ts}.sqlite"));
    if std::fs::copy(agent_db(), &dst).is_err() {
        return String::new();
    }
    let mut olds: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(backup_dir()) {
        for ent in rd.flatten() {
            let n = ent.file_name().to_string_lossy().to_string();
            if n.starts_with("openclaw-agent-") && n.ends_with(".sqlite") {
                olds.push(ent.path());
            }
        }
    }
    olds.sort();
    let keep_from = olds.len().saturating_sub(10);
    for old in olds.into_iter().take(keep_from) {
        let _ = std::fs::remove_file(old);
    }
    dst.to_string_lossy().to_string()
}

fn compact_session(keep: i64) -> Value {
    if !openclaw_mjs().exists() {
        return json!({"ok": false, "error": format!("找不到 {}", openclaw_mjs().to_string_lossy())});
    }
    let argv = vec![
        BREW_NODE.to_string(),
        openclaw_mjs().to_string_lossy().to_string(),
        "sessions".into(),
        "compact".into(),
        "agent:main:main".into(),
        "--max-lines".into(),
        keep.to_string(),
        "--timeout".into(),
        "30000".into(),
        "--json".into(),
    ];
    let Some((rc, out, err)) = util::run_cmd(&argv, 60, None) else {
        return json!({"ok": false, "error": "启动失败"});
    };
    let tail = if out.len() > 2000 { &out[out.len() - 2000..] } else { &out };
    let body = serde_json::from_str::<Value>(if tail.trim().is_empty() { "{}" } else { tail })
        .unwrap_or_else(|_| json!({}));
    let ok = rc == 0 && body.get("ok") == Some(&json!(true));
    let detail = if ok { String::new() } else {
        let s = if err.trim().is_empty() { out } else { err };
        util::trunc(&s, 200)
    };
    json!({"ok": ok, "kept": body.get("kept").cloned().unwrap_or(Value::Null),
           "error": if ok { Value::Null } else { Value::String(detail) }})
}

fn start_proc(cmd: &[String], log: &Path) -> Value {
    if let Some(parent) = log.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(out) = std::fs::OpenOptions::new().create(true).append(true).open(log) else {
        return json!({"ok": false, "error": format!("写不了日志 {}", log.to_string_lossy())});
    };
    let mut c = Command::new(&cmd[0]);
    c.args(&cmd[1..])
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(Stdio::null());
    match c.spawn() {
        Ok(child) => json!({"ok": true, "pid": child.id()}),
        Err(e) => json!({"ok": false, "error": trunc(&e.to_string(), 200)}),
    }
}

fn start_gateway() -> Value {
    if !serve_sh().exists() {
        return json!({"ok": false, "error": format!("找不到启动脚本 {}", serve_sh().to_string_lossy())});
    }
    start_proc(
        &["/bin/bash".to_string(), serve_sh().to_string_lossy().to_string()],
        &log_dir().join("openclaw-gateway.log"),
    )
}

fn start_inspect(ctx: &Ctx) -> Value {
    let c = inspect_cmd(ctx);
    if !c.exists() {
        return json!({"ok": false, "error": format!("找不到 {}", c.to_string_lossy())});
    }
    start_proc(
        &["/bin/bash".to_string(), c.to_string_lossy().to_string()],
        &log_dir().join("cuntu-syscheck.log"),
    )
}

fn start_appwatch(ctx: &Ctx) -> Value {
    let c = appwatch_cmd(ctx);
    if !c.exists() {
        return json!({"ok": false, "error": format!("找不到 {}", c.to_string_lossy())});
    }
    start_proc(
        &["/bin/bash".to_string(), c.to_string_lossy().to_string()],
        &log_dir().join("cuntu-appwatch.log"),
    )
}

fn wait_ready(port: i64, seconds: u64) -> (bool, f64) {
    let t0 = Instant::now();
    while t0.elapsed().as_secs() < seconds {
        if http_ok(&format!("http://127.0.0.1:{port}/"), 2.0) {
            return (true, py_round(t0.elapsed().as_secs_f64(), 1));
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    (false, py_round(t0.elapsed().as_secs_f64(), 1))
}

fn wait_inspect(seconds: u64) -> (bool, f64) {
    let t0 = Instant::now();
    while t0.elapsed().as_secs() < seconds {
        if http_ok(&format!("http://127.0.0.1:{INSPECT_PORT}/api/state"), 1.0) {
            return (true, py_round(t0.elapsed().as_secs_f64(), 1));
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    (false, py_round(t0.elapsed().as_secs_f64(), 1))
}

fn wait_appwatch(seconds: u64) -> (bool, f64) {
    let t0 = Instant::now();
    while t0.elapsed().as_secs() < seconds {
        if appwatch_state().get("ok") == Some(&json!(true)) {
            return (true, py_round(t0.elapsed().as_secs_f64(), 1));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    (false, py_round(t0.elapsed().as_secs_f64(), 1))
}

fn appwatch_state() -> Value {
    let p = appwatch_state_path();
    let Ok(d) = std::fs::read_to_string(&p)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .ok_or(())
    else {
        return json!({"ok": false, "why": "state.json 读不出来（还没跑过？）"});
    };
    let pid = util::i64_of(d.get("pid"));
    if pid <= 0 {
        return json!({"ok": false, "why": "state.json 里没有 pid"});
    }
    if !pid_alive(pid) {
        return json!({"ok": false, "why": format!("pid {pid} 已经没了")});
    }
    let age = std::fs::metadata(&p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map(|x| x.as_secs_f64())
        .unwrap_or(0.0);
    if age > 90.0 {
        return json!({"ok": false, "why": format!("pid {pid} 在，但 {}s 没刷过心跳（可能卡住了）", age as i64)});
    }
    json!({"ok": true, "pid": pid, "at": d.get("at").cloned().unwrap_or(Value::Null),
           "apps": d.get("apps").cloned().unwrap_or(Value::Null),
           "ax": d.get("ax").map(|v| v.as_bool().unwrap_or(false)).unwrap_or(false),
           "age": age as i64})
}

fn appwatch_tally(ctx: &Ctx, hours: i64) -> Value {
    let mut out = json!({"crash": 0, "hang": 0, "kill": 0, "names": []});
    let p = sys_dir(ctx).join("App事件.jsonl");
    let Ok(text) = std::fs::read_to_string(&p) else { return out };
    let all: Vec<&str> = text.split('\n').collect();
    let start = all.len().saturating_sub(4000);
    let cut = util::now_f() - hours as f64 * 3600.0;
    let mut names: Vec<String> = Vec::new();
    for ln in &all[start..] {
        let Ok(r) = serde_json::from_str::<Value>(ln) else { continue };
        let key = match s_of(r.get("event")).as_str() {
            "app_crash" => "crash",
            "app_hang" => "hang",
            "app_kill" => "kill",
            _ => continue,
        };
        let ts = s_of(r.get("ts"));
        let ts = &ts[..ts.len().min(19)];
        let Ok(dt) = chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%d %H:%M:%S") else { continue };
        let t = dt.and_local_timezone(chrono::Local).single().map(|x| x.timestamp() as f64).unwrap_or(0.0);
        if t < cut {
            continue;
        }
        let n = util::i64_of(out.get(key)) + 1;
        if let Some(o) = out.as_object_mut() {
            o.insert(key.to_string(), json!(n));
        }
        if names.len() < 3 && !s_of(r.get("app")).is_empty() {
            names.push(s_of(r.get("app")));
        }
    }
    if let Some(o) = out.as_object_mut() {
        o.insert("names".into(), Value::Array(names.into_iter().map(|n| Value::String(n)).collect()));
    }
    out
}

struct PushRow {
    at: String,
    id: String,
    ok: bool,
    name: String,
    error: String,
}

fn push_rows(path: &Path) -> Vec<PushRow> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    let mut out = Vec::new();
    for ln in text.split('\n') {
        let Ok(r) = serde_json::from_str::<Value>(ln) else { continue };
        out.push(PushRow {
            at: s_of(r.get("at")),
            id: s_of(r.get("id")),
            ok: r.get("ok").and_then(|v| v.as_bool()).unwrap_or(false),
            name: s_of(r.get("name")),
            error: s_of(r.get("error")),
        });
    }
    out
}

fn push_summary_at(path: &Path, hours: i64) -> Value {
    let rows = push_rows(path);
    if rows.is_empty() {
        return json!({"hours": hours, "total": 0, "failed": 0, "ok": 0, "error": "",
                      "at": "", "since": "", "names": [], "ids": []});
    }
    let cut = util::now_f() - hours.max(1) as f64 * 3600.0;
    let mut last: Vec<(String, usize)> = Vec::new();
    let mut first: Vec<(String, String)> = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&r.at) else { continue };
        if (dt.timestamp() as f64) < cut {
            continue;
        }
        let key = if r.id.is_empty() { r.at.clone() } else { r.id.clone() };
        if let Some(slot) = last.iter_mut().find(|(k, _)| *k == key) {
            slot.1 = i;
        } else {
            last.push((key.clone(), i));
        }
        if !first.iter().any(|(k, _)| *k == key) {
            first.push((key, r.at.clone()));
        }
    }
    let mut bad: Vec<&PushRow> = last
        .iter()
        .map(|(_, i)| &rows[*i])
        .filter(|r| !r.ok)
        .collect();
    bad.sort_by(|a, b| a.at.cmp(&b.at));
    let good = last.iter().filter(|(_, i)| rows[*i].ok).count();
    let since = bad
        .iter()
        .map(|r| {
            let key = if r.id.is_empty() { r.at.clone() } else { r.id.clone() };
            first.iter().find(|(k, _)| *k == key).map(|(_, v)| v.clone()).unwrap_or_default()
        })
        .min()
        .unwrap_or_default();
    let names: Vec<Value> = bad.iter().rev().take(3).rev().map(|r| Value::String(r.name.clone())).collect();
    let ids: Vec<Value> = bad.iter().take(20).map(|r| Value::String(r.id.clone())).collect();
    json!({
        "hours": hours,
        "total": last.len(),
        "failed": bad.len(),
        "ok": good,
        "error": bad.last().map(|r| r.error.clone()).unwrap_or_default(),
        "at": bad.last().map(|r| r.at.clone()).unwrap_or_default(),
        "since": if bad.is_empty() { String::new() } else { since },
        "names": names,
        "ids": ids,
    })
}

fn check_transfer_push(ctx: &Ctx) -> Value {
    let name = "传输助手 → 手机";
    let p = ctx.library_dir.join("14-传输").join("推送失败.jsonl");
    if !p.exists() {
        return json!({"id": "transfer_push", "level": "warn", "ok": true, "name": name,
                      "detail": "近 6 小时没往手机推过东西（没发就不用推）"});
    }
    let s = push_summary_at(&p, 6);
    let total = util::i64_of(s.get("total"));
    let bad = util::i64_of(s.get("failed"));
    if total == 0 {
        return json!({"id": "transfer_push", "level": "warn", "ok": true, "name": name,
                      "detail": "近 6 小时没往手机推过东西（没发就不用推）"});
    }
    if bad == 0 {
        return json!({"id": "transfer_push", "level": "warn", "ok": true, "name": name,
                      "detail": format!("近 6 小时推了 {total} 条，全到了")});
    }
    let names = s
        .get("names")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter(|x| !s_of(Some(x)).is_empty()).take(2).map(|x| s_of(Some(x))).collect::<Vec<_>>().join("、"))
        .unwrap_or_default();
    let at = s_of(s.get("at"));
    let err = s_of(s.get("error"));
    let mut detail = format!("近 6 小时 {bad}/{total} 条没推到手机");
    if !names.is_empty() {
        detail.push_str(&format!("（{names}）"));
    }
    if !at.is_empty() {
        detail.push_str(&format!("；最近一次 {}", &at[at.len().min(11)..at.len().min(16)]));
    }
    if !err.is_empty() {
        detail.push_str(&format!("；原因：{}", trunc(&err, 90)));
    }
    json!({"id": "transfer_push", "level": "warn", "ok": false, "name": name, "detail": detail})
}

fn disk_sample(ctx: &Ctx) -> Value {
    let u = crate::storage::usage(&sys_dir(ctx));
    let total = util::i64_of(u.get("total")) as f64;
    let used = util::i64_of(u.get("used")) as f64;
    let free = util::i64_of(u.get("free")) as f64;
    if total <= 0.0 {
        return json!({});
    }
    json!({
        "at": now_hms(),
        "used_pct": py_round(used * 100.0 / total, 1),
        "free_gb": py_round(free / 1e9, 1),
        "total_gb": py_round(total / 1e9, 1),
    })
}

/// `json.dumps(v, ensure_ascii=False)`：单行，但分隔符是 `", "` / `": "`。
fn py_json_compact(v: &Value) -> String {
    match v {
        Value::Array(a) => {
            let items: Vec<String> = a.iter().map(py_json_compact).collect();
            format!("[{}]", items.join(", "))
        }
        Value::Object(o) => {
            let items: Vec<String> = o
                .iter()
                .map(|(k, x)| format!("{}: {}", serde_json::to_string(k).unwrap_or_default(), py_json_compact(x)))
                .collect();
            format!("{{{}}}", items.join(", "))
        }
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

fn append_jsonl(p: &Path, v: &Value) {
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
        let _ = writeln!(f, "{}", py_json_compact(v));
    }
}

fn save_state(ctx: &Ctx, result: &Value) {
    let p = state_file(ctx);
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = p.with_file_name(format!(
        "{}.tmp",
        p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
    ));
    if std::fs::write(&tmp, jsonio::dumps(result, 2)).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}

fn cfg_of(cfg: &Value) -> Value {
    let d = cfg.get("heartbeat").cloned().unwrap_or_else(|| json!({}));
    let int_or = |k: &str, def: i64| -> i64 {
        match d.get(k) {
            Some(Value::Null) | None => def,
            Some(v) => util::i64_of(Some(v)),
        }
    };
    json!({
        "enabled": d.get("enabled") != Some(&json!(false)),
        "interval": int_or("interval_sec", 120).max(30),
        "cooldown": int_or("cooldown_sec", 180).max(30),
        "max_restarts": int_or("max_restarts", 3).max(1),
        "heal": d.get("heal") != Some(&json!(false)),
        "port": int_or("gateway_port", GW_PORT),
    })
}

fn join3(v: &Value) -> String {
    match v.as_array() {
        Some(a) => a.iter().map(|x| util::py_str(x)).collect::<Vec<_>>().join("."),
        None => String::new(),
    }
}

fn do_run_once(ctx: &Ctx, heal: bool) -> Value {
    let c = cfg_of(&ctx.cfg);
    let port = util::i64_of(c.get("port"));
    let do_heal = c.get("heal") != Some(&json!(false)) && heal;
    if busy().swap(true, std::sync::atomic::Ordering::SeqCst) {
        return json!({"ok": false, "error": "正在自检，等这次结束", "at": now_hms()});
    }
    let _guard = BusyGuard;
    st_set("running", json!(true));
    let mut checks: Vec<Value> = Vec::new();
    let mut healed: Vec<String> = Vec::new();
    st_set("consecutive_offline", json!(st_i64("consecutive_offline")));

    let mut online = http_ok(&format!("http://127.0.0.1:{port}/"), 2.0);
    let mut locks = scan_locks();
    let mut stale: Vec<LockItem> = locks.iter().filter(|x| !s_of(Some(&x.pid)).is_empty() && !x.alive).cloned().collect();
    let stale_pids: Vec<Value> = stale.iter().map(|x| x.pid.clone()).collect();
    let node = check_node();
    let launchd = check_launchd();
    let ollama_ok = http_ok(OLLAMA_URL, 2.0);

    if !stale.is_empty() {
        if do_heal {
            let removed = clean_stale_locks(&stale);
            if !removed.is_empty() {
                healed.push(format!(
                    "清理陈旧锁 {} 个文件（{}）",
                    removed.len(),
                    removed.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
                ));
                append_jsonl(
                    &log_file(ctx),
                    &json!({"at": now_hms(), "event": "stale_locks_removed", "pids": stale_pids, "files": removed}),
                );
                locks = scan_locks();
                stale = locks.iter().filter(|x| !s_of(Some(&x.pid)).is_empty() && !x.alive).cloned().collect();
            }
        } else {
            healed.push(format!("[仅检查] 发现 {} 个陈旧锁，--no-heal 未清理", stale.len()));
        }
    }

    let mut gw_action = String::new();
    if !online {
        let n = st_i64("consecutive_offline") + 1;
        st_set("consecutive_offline", json!(n));
        let now_ts = util::now_f();
        let last_restart = st_f64("last_restart_at");
        let cooldown = util::i64_of(c.get("cooldown")) as f64;
        let max_restarts = util::i64_of(c.get("max_restarts"));
        let cooling = now_ts - last_restart < cooldown;
        let exhausted = n > max_restarts;
        if do_heal && !cooling && !exhausted {
            let r = start_gateway();
            if r.get("ok") == Some(&json!(true)) {
                st_set("last_restart_at", json!(now_ts));
                st_set("restart_count", json!(st_i64("restart_count") + 1));
                let (ready, sec) = wait_ready(port, 50);
                if ready {
                    online = true;
                    st_set("consecutive_offline", json!(0));
                    st_set("needs_attention", json!(false));
                    let pid = util::i64_of(r.get("pid"));
                    healed.push(format!("gateway 已拉起并就绪（{sec}s，pid {pid}）"));
                    gw_action = format!("已拉起，{sec}s 就绪");
                } else {
                    gw_action = format!("已拉起但 {sec}s 内未就绪，下轮再看");
                    healed.push(gw_action.clone());
                }
            } else {
                gw_action = format!("拉起失败：{}", s_of(r.get("error")));
            }
        } else {
            if cooling {
                let left = cooldown - (now_ts - last_restart);
                gw_action = format!("冷却中，{}s 后可重试", left as i64);
            }
            if exhausted {
                st_set("needs_attention", json!(true));
                gw_action = format!("连续 {n} 轮掉线，已停止自动重试，需人工查看");
            }
        }
    } else {
        st_set("consecutive_offline", json!(0));
        st_set("needs_attention", json!(false));
    }

    checks.push(json!({
        "id": "gateway_online", "level": "critical", "ok": online,
        "name": format!("微信助手 gateway（:{port}）"),
        "detail": format!("{}{}", if online { "HTTP 200，在线" } else { "端口不通" },
                          if gw_action.is_empty() { String::new() } else { format!("；{gw_action}") }),
    }));

    let mut insp_ok = http_ok(&format!("http://127.0.0.1:{INSPECT_PORT}/api/state"), 1.5);
    let mut insp_action = String::new();
    if !insp_ok {
        if do_heal {
            let r = start_inspect(ctx);
            if r.get("ok") == Some(&json!(true)) {
                let (ready, sec) = wait_inspect(12);
                insp_ok = ready;
                insp_action = if ready { format!("已拉起，{sec}s 就绪") } else { "已拉起但 12s 内没就绪，下轮再看".into() };
                if ready {
                    healed.push(format!("巡检服务已拉起（{sec}s，pid {}）", util::i64_of(r.get("pid"))));
                    append_jsonl(&log_file(ctx), &json!({"at": now_hms(), "event": "inspect_restarted",
                                                          "pid": r.get("pid"), "sec": sec}));
                }
            } else {
                insp_action = format!("拉起失败：{}", s_of(r.get("error")));
            }
        } else {
            insp_action = "--no-heal 未拉起".into();
        }
    }
    checks.push(json!({
        "id": "inspect_alive", "level": "warn", "ok": insp_ok,
        "name": format!("系统巡检（:{INSPECT_PORT}）"),
        "detail": format!("{}{}", if insp_ok { "HTTP 200，常驻在跑" } else { "端口不通" },
                          if insp_action.is_empty() { String::new() } else { format!("；{insp_action}") }),
    }));

    checks.push(check_transfer_push(ctx));

    let mut aw = appwatch_state();
    let mut aw_action = String::new();
    if aw.get("ok") != Some(&json!(true)) {
        if do_heal {
            let r = start_appwatch(ctx);
            if r.get("ok") == Some(&json!(true)) {
                let (ready, sec) = wait_appwatch(10);
                aw = appwatch_state();
                aw_action = if ready { format!("已拉起，{sec}s 就绪") } else { "已拉起但 10s 内还没刷心跳，下轮再看".into() };
                if ready {
                    healed.push(format!("App 监工已拉起（{sec}s，pid {}）", util::i64_of(r.get("pid"))));
                    append_jsonl(&log_file(ctx), &json!({"at": now_hms(), "event": "appwatch_restarted",
                                                          "pid": r.get("pid"), "sec": sec}));
                }
            } else {
                aw_action = format!("拉起失败：{}", s_of(r.get("error")));
            }
        } else {
            aw_action = "--no-heal 未拉起".into();
        }
    }
    let tally = appwatch_tally(ctx, 24);
    let mut detail = if aw.get("ok") == Some(&json!(true)) {
        format!(
            "{} 个 App 在监 · 辅助功能{}",
            util::i64_of(aw.get("apps")),
            if aw.get("ax").and_then(|v| v.as_bool()).unwrap_or(false) { "可用（能判卡住）" } else { "未授权（只记开关/闪退）" }
        )
    } else {
        let w = s_of(aw.get("why"));
        if w.is_empty() { "没在跑".into() } else { w }
    };
    let mut bits: Vec<String> = Vec::new();
    for (k, label) in [("crash", "闪退"), ("hang", "卡住"), ("kill", "强制退出")] {
        let n = util::i64_of(tally.get(k));
        if n != 0 {
            bits.push(format!("{label} {n}"));
        }
    }
    if !bits.is_empty() {
        detail.push_str(&format!("；近 24 小时 {}", bits.join(" / ")));
        let names: Vec<String> = tally
            .get("names")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().take(3).map(|x| s_of(Some(x))).collect())
            .unwrap_or_default();
        if !names.is_empty() {
            detail.push_str(&format!("（{}）", names.join("、")));
        }
    }
    if !aw_action.is_empty() {
        detail.push_str(&format!("；{aw_action}"));
    }
    checks.push(json!({
        "id": "appwatch_alive", "level": "warn", "ok": aw.get("ok") == Some(&json!(true)),
        "name": "App 监工（开/关/闪退/卡住）", "detail": detail,
    }));

    let mut ndetail = format!(
        "brew node {} ≥ {}：{}",
        {
            let b = node.get("brew").cloned().unwrap_or(Value::Null);
            if b.is_null() { "缺失".to_string() } else { join3(&b) }
        },
        format!("{}.{}.{}", MIN_NODE.0, MIN_NODE.1, MIN_NODE.2),
        if node.get("brew_ok") == Some(&json!(true)) { "通过" } else { "不通过" }
    );
    if node.get("path_ok") != Some(&json!(true)) {
        let path_exe = s_of(node.get("path_exe"));
        if path_exe.is_empty() {
            ndetail.push_str("；当前环境 PATH 里没有 node（launchd 下常见）—— 启动脚本已显式写死 brew node 绝对路径");
        } else {
            let path_v = node.get("path").cloned().unwrap_or(Value::Null);
            ndetail.push_str(&format!(
                "；PATH 里的 node {}（{path_exe}）低于门槛 —— 启动脚本已显式绕开",
                join3(&path_v)
            ));
        }
    }
    if node.get("launcher_ok") != Some(&json!(true)) {
        ndetail.push_str("；⚠ 启动脚本未显式指向 brew node，下次启动可能复发版本问题");
    }
    checks.push(json!({
        "id": "node_version", "level": "critical",
        "ok": node.get("brew_ok") == Some(&json!(true)) && node.get("launcher_ok") == Some(&json!(true)),
        "name": "Node 版本与启动脚本", "detail": ndetail,
    }));

    let lock_detail = if !stale.is_empty() {
        format!("{} 个锁文件，仍有 {} 个 pid 已死", locks.len(), stale.len())
    } else if !stale_pids.is_empty() {
        format!("已清理 {} 个孤儿锁（pids {}）；现 {} 个锁文件均有效",
                stale_pids.len(),
                stale_pids.iter().map(util::py_str).collect::<Vec<_>>().join(", "),
                locks.len())
    } else {
        format!("{} 个锁文件，无陈旧锁", locks.len())
    };
    checks.push(json!({
        "id": "stale_locks", "level": "warn", "ok": stale.is_empty(),
        "name": "陈旧锁", "detail": lock_detail,
    }));

    let ld = if launchd.get("loaded") == Some(&json!(true)) {
        "已由 launchd 托管（running）".to_string()
    } else if launchd.get("plist_exists") != Some(&json!(true)) {
        "⚠ plist 缺失，登录自启不会生效".to_string()
    } else if launchd.get("lint_ok") != Some(&json!(true)) {
        "⚠ plist 格式非法，登录自启不会生效".to_string()
    } else {
        "未加载（当前沙箱无法 bootstrap；心跳兜底，下次登录自动加载）".to_string()
    };
    checks.push(json!({
        "id": "launchd", "level": "info",
        "ok": launchd.get("plist_exists") == Some(&json!(true)) && launchd.get("lint_ok") == Some(&json!(true)),
        "name": "LaunchAgent 常驻配置", "detail": ld,
    }));

    checks.push(json!({
        "id": "ollama", "level": "warn", "ok": ollama_ok,
        "name": "Ollama（11434）",
        "detail": if ollama_ok { "在线" } else { "不在线（摘要/简报会降级，不影响 gateway）" },
    }));

    let mut sess = check_session();
    let mut sess_bytes = util::i64_of(sess.get("bytes"));
    let mut sess_kb = sess_bytes as f64 / 1024.0;
    let mut sess_action = String::new();
    if sess.get("db_exists") == Some(&json!(true)) && sess_bytes > SESSION_WARN_BYTES {
        let cooling = util::now_f() - st_f64("last_compact_at") < SESSION_COMPACT_COOLDOWN;
        if do_heal && online && !cooling {
            let bk = backup_agent_db();
            let r = compact_session(SESSION_KEEP_LINES);
            if r.get("ok") == Some(&json!(true)) {
                st_set("last_compact_at", json!(util::now_f()));
                sess = check_session();
                sess_bytes = util::i64_of(sess.get("bytes"));
                sess_kb = sess_bytes as f64 / 1024.0;
                sess_action = format!(
                    "已备份 {} 并截断到最近 {} 条（{} 条 / {:.0}KB）",
                    Path::new(&bk).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
                    util::i64_of(r.get("kept")),
                    util::i64_of(sess.get("events")),
                    sess_kb
                );
                healed.push(format!("主会话过大，已截断压缩：{sess_action}"));
                append_jsonl(&log_file(ctx), &json!({"at": now_hms(), "event": "session_compacted",
                                                      "backup": bk, "kept": r.get("kept"), "bytes_after": sess_bytes}));
            } else {
                sess_action = format!("截断失败：{}，需人工 /new 或 /compact", s_of(r.get("error")));
            }
        } else if cooling {
            sess_action = "体积超阈值，但距上次截断不足 10 分钟，下轮再看".into();
        } else if !online {
            sess_action = "体积超阈值，gateway 不在线无法压缩".into();
        } else {
            sess_action = "体积超阈值（--no-heal 未处理）".into();
        }
    }
    let sess_ok = !(sess.get("db_exists") == Some(&json!(true)) && sess_bytes > SESSION_WARN_BYTES);
    checks.push(json!({
        "id": "session_size", "level": "warn", "ok": sess_ok,
        "name": "微信主会话体积",
        "detail": format!("{}{}",
            if sess.get("db_exists") != Some(&json!(true)) {
                "无会话库".to_string()
            } else {
                format!("{} 条 / {:.0}KB（阈值 {}KB，4B 窗口有限）",
                        util::i64_of(sess.get("events")), sess_kb, SESSION_WARN_BYTES / 1024)
            },
            if sess_action.is_empty() { String::new() } else { format!("；{sess_action}") }),
    }));

    let ok = checks
        .iter()
        .filter(|x| s_of(x.get("level")) == "critical")
        .all(|x| x.get("ok") == Some(&json!(true)));
    let result = json!({
        "at": now_hms(),
        "ok": ok,
        "checks": checks,
        "healed": healed,
        "needs_attention": st_get("needs_attention"),
        "restart_count": st_get("restart_count"),
        "interval_sec": util::i64_of(c.get("interval")),
    });
    st_set("last_at", result.get("at").cloned().unwrap_or(Value::Null));
    st_set("last", result.clone());
    save_state(ctx, &result);
    append_jsonl(&log_file(ctx), &json!({"at": result.get("at"), "event": "heartbeat", "ok": ok,
                                          "online": online, "stale": stale.len(), "healed": healed,
                                          "needs_attention": st_get("needs_attention")}));
    let sample = disk_sample(ctx);
    if sample.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
        append_jsonl(&disk_log(ctx), &sample);
    }
    st_set("running", json!(false));
    result
}

// ---------------------------------------------------------------- 路由

fn parse_bool_query(uri: &Uri, key: &str, default: bool) -> bool {
    let raw = crate::routes::common::q(uri, key);
    if raw.trim().is_empty() {
        return default;
    }
    !matches!(raw.trim().to_lowercase().as_str(), "0" | "false" | "no" | "off" | "f" | "n")
}

pub async fn run(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let heal = parse_bool_query(&uri, "heal", true);
    if st_get("running").as_bool().unwrap_or(false) {
        return Json(json!({"ok": false, "error": "正在自检，等这次结束"})).into_response();
    }
    let ctx2 = ctx.clone();
    std::thread::spawn(move || {
        let _ = do_run_once(&ctx2, heal);
    });
    crate::storage::log(&ctx, json!({"event": "heartbeat_manual", "heal": heal}));
    Json(json!({"ok": true, "started": true})).into_response()
}

// 让未使用的 helper 在单特性编译时不报警（这些函数在 windows 分支/后续收编时会用到）。
#[allow(dead_code)]
fn _touch(_: &Value, _: &Map<String, Value>) {}

// ---------------------------------------------------------------- 只读状态 / 调度

fn truthy_or(v: Option<&Value>, default: bool) -> bool {
    match v {
        None => default,
        Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0) != 0.0,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// `GET /api/heartbeat`（server.py:953）
pub async fn state_route(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let cfg = ctx.cfg.get("heartbeat").cloned().unwrap_or_else(|| json!({}));
    let interval = match cfg.get("interval_sec") {
        Some(Value::Null) | None => 120,
        Some(v) => util::i64_of(Some(v)),
    };
    Json(json!({
        "enabled": truthy_or(cfg.get("enabled"), true),
        "interval_sec": interval,
        "heal": truthy_or(cfg.get("heal"), true),
        "state": crate::live::heartbeat_value(),
    }))
    .into_response()
}

/// 后台循环。Python 的 `scheduler_loop`：启动立即跑一次，跑完才睡 `interval`。
pub fn start(ctx: std::sync::Arc<Ctx>) {
    let c = cfg_of(&ctx.cfg);
    // Python 的 `_start_heartbeat`：`heartbeat.enabled is False` 时连线程都不起。
    if c.get("enabled") != Some(&json!(true)) {
        eprintln!("[moshan-rs] heartbeat.enabled=false，未启动心跳循环");
        return;
    }
    let raw_interval = ctx
        .cfg
        .get("heartbeat")
        .and_then(|v| v.get("interval_sec"))
        .map(|v| util::i64_of(Some(v)))
        .unwrap_or(120);
    crate::storage::log(
        &ctx,
        json!({"event": "heartbeat_scheduler", "interval_sec": raw_interval}),
    );
    std::thread::Builder::new()
        .name("moshan-heartbeat".into())
        .spawn(move || loop {
            let interval = util::i64_of(cfg_of(&ctx.cfg).get("interval")).max(30) as u64;
            let t0 = Instant::now();
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                do_run_once(&ctx, true)
            }));
            match r {
                Ok(out) => {
                    let online = out
                        .get("checks")
                        .and_then(|v| v.as_array())
                        .and_then(|a| {
                            a.iter()
                                .find(|c| c.get("id").and_then(|v| v.as_str()) == Some("gateway_online"))
                        })
                        .and_then(|c| c.get("ok").cloned())
                        .unwrap_or(Value::Null);
                    crate::storage::log(
                        &ctx,
                        json!({
                            "event": "heartbeat_done",
                            "ok": truthy_or(out.get("ok"), false),
                            "online": online,
                            "healed": out.get("healed").cloned().unwrap_or_else(|| json!([])),
                            "needs_attention": truthy_or(out.get("needs_attention"), false),
                            "error": out.get("error").and_then(|v| v.as_str()).unwrap_or(""),
                        }),
                    );
                }
                Err(_) => {
                    st_set("running", json!(false));
                    eprintln!("[moshan-rs] heartbeat 单轮异常，已跳过（下一轮继续）");
                    crate::storage::log(
                        &ctx,
                        json!({"event": "heartbeat_done", "ok": false, "error": "单轮异常"}),
                    );
                }
            }
            let secs = t0.elapsed().as_secs_f64();
            if secs > interval as f64 {
                eprintln!(
                    "[moshan-rs] heartbeat 单轮耗时 {:.1}s，超过间隔 {}s；本轮结束，立即进入下一轮等待",
                    secs, interval
                );
            }
            std::thread::sleep(Duration::from_secs(interval));
        })
        .ok();
}
