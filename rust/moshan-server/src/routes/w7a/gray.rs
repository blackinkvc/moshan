// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：`/api/gray/*` —— 灰色扫码下载的任务队列、二维码上传、批量存图。
// 不做：不改 gray.py 的页面（/gray HTML 与本泳道无关）。

#![allow(dead_code)]

use super::util::{self, py_round, s_of};
use crate::paths::Ctx;
use crate::routes::common::{body_obj, http_err, peek, q, CtxState};
use axum::extract::{Multipart, Path as AxPath, State};
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};

const MAX_LINES: usize = 2000;

fn gray_dir() -> PathBuf {
    let raw = std::env::var("GRAY_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "/Volumes/AI仓库/灰色".to_string());
    util::expanduser(&raw)
}

fn cli() -> PathBuf {
    gray_dir().join("qrget.sh")
}

fn out_root() -> PathBuf {
    gray_dir().join("输出")
}

fn bad_chars(s: &str) -> String {
    let re = regex::Regex::new("[\\\\/:*?\"<>|\\x00-\\x1f]").unwrap();
    re.replace_all(s, "").to_string()
}

fn clean_name(raw: &str) -> String {
    let s = bad_chars(raw.trim()).trim_matches(|c: char| c == '.' || c == ' ').to_string();
    let s = s.replace("..", "");
    util::trunc(&s, 60)
}

fn size_units(unit: &str) -> f64 {
    match unit.to_uppercase().as_str() {
        "B" => 1.0,
        "KB" => 1024.0,
        "MB" => 1024.0 * 1024.0,
        "GB" => 1024.0 * 1024.0 * 1024.0,
        _ => 1.0,
    }
}

// ---------------------------------------------------------------- 全局状态

struct Gray {
    tasks: Vec<(String, Value)>,
    queue: Vec<String>,
    active: Option<String>,
}

fn gray() -> &'static Mutex<Gray> {
    static G: OnceLock<Mutex<Gray>> = OnceLock::new();
    G.get_or_init(|| Mutex::new(Gray { tasks: Vec::new(), queue: Vec::new(), active: None }))
}

fn procs() -> &'static Mutex<Vec<(String, Arc<Mutex<Child>>)>> {
    static P: OnceLock<Mutex<Vec<(String, Arc<Mutex<Child>>)>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(Vec::new()))
}

fn task_get(gs: &Gray, tid: &str) -> Option<Value> {
    gs.tasks.iter().find(|(k, _)| k == tid).map(|(_, v)| v.clone())
}

fn task_mut<F: FnOnce(&mut Map<String, Value>)>(gs: &mut Gray, tid: &str, f: F) {
    if let Some((_, v)) = gs.tasks.iter_mut().find(|(k, _)| k == tid) {
        if let Some(m) = v.as_object_mut() {
            f(m);
        }
    }
}

fn prog_mut<F: FnOnce(&mut Map<String, Value>)>(t: &mut Map<String, Value>, f: F) {
    if let Some(Value::Object(p)) = t.get_mut("prog") {
        f(p);
    }
}

/// `_push`：日志 + 进度解析。
fn push(t: &mut Map<String, Value>, line: &str) {
    let mut lines: Vec<Value> = match t.get("lines") {
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    };
    lines.push(Value::String(line.to_string()));
    if lines.len() > MAX_LINES {
        let cut = lines.len() - MAX_LINES;
        lines.drain(..cut);
    }
    t.insert("lines".into(), Value::Array(lines));

    if let Some(m) = regex::Regex::new(r"保存位置：(.+)$").unwrap().captures(line) {
        t.insert("out".into(), Value::String(m.get(1).map(|x| x.as_str().trim().to_string()).unwrap_or_default()));
    }

    let body = regex::Regex::new(r"^\[\d{2}:\d{2}:\d{2}\]\s*")
        .unwrap()
        .replace(line, "")
        .into_owned();
    let re_total = regex::Regex::new(r"共 (\d+) 个目录 / (\d+) 个文件 / ([\d.]+)\s*(B|KB|MB|GB)").unwrap();
    let re_file = regex::Regex::new(r"\[(\d+)/(\d+)\]\s+([\d.]+)%\s+([\d.]+)\s*(B|KB|MB|GB)").unwrap();
    if let Some(m) = re_total.captures(&body) {
        let files_total: i64 = m.get(2).and_then(|x| x.as_str().parse().ok()).unwrap_or(0);
        let bytes_total = m.get(3).and_then(|x| x.as_str().parse::<f64>().ok()).unwrap_or(0.0)
            * size_units(m.get(4).map(|x| x.as_str()).unwrap_or("B"));
        let now = util::now_f();
        prog_mut(t, |p| {
            p.insert("files_total".into(), json!(files_total));
            p.insert("bytes_total".into(), json!(bytes_total));
            p.insert("phase".into(), Value::String("download".into()));
            let t0 = p.get("t0").map(|v| util::f64_of(Some(v))).unwrap_or(0.0);
            if t0 == 0.0 {
                p.insert("t0".into(), json!(now));
            }
        });
        return;
    }
    if let Some(m) = re_file.captures(&body) {
        let files_done: i64 = m.get(1).and_then(|x| x.as_str().parse().ok()).unwrap_or(0);
        let files_batch: i64 = m.get(2).and_then(|x| x.as_str().parse().ok()).unwrap_or(0);
        let add = m.get(4).and_then(|x| x.as_str().parse::<f64>().ok()).unwrap_or(0.0)
            * size_units(m.get(5).map(|x| x.as_str()).unwrap_or("B"));
        let now = util::now_f();
        prog_mut(t, |p| {
            p.insert("files_done".into(), json!(files_done));
            p.insert("files_batch".into(), json!(files_batch));
            let cur = p.get("bytes_done").map(|v| util::f64_of(Some(v))).unwrap_or(0.0);
            p.insert("bytes_done".into(), json!(cur + add));
            let t0 = p.get("t0").map(|v| util::f64_of(Some(v))).unwrap_or(0.0);
            if t0 == 0.0 {
                p.insert("t0".into(), json!(now));
            }
            p.insert("phase".into(), Value::String("download".into()));
        });
        return;
    }
    if body.contains("保存位置：") {
        prog_mut(t, |p| {
            p.insert("phase".into(), Value::String("done".into()));
        });
    }
}

fn snapshot(t: &Value) -> Value {
    let p = t.get("prog").cloned().unwrap_or_else(|| Value::Object(Map::new()));
    let files_total = util::i64_of(p.get("files_total"));
    let files_batch = util::i64_of(p.get("files_batch"));
    let files_done = util::i64_of(p.get("files_done"));
    let bytes_total = util::f64_of(p.get("bytes_total"));
    let bytes_done = util::f64_of(p.get("bytes_done"));
    let phase = s_of(p.get("phase"));
    let t0 = util::f64_of(p.get("t0"));
    let denom = if files_batch != 0 { files_batch } else { files_total };
    let mut pct = if denom != 0 { 100.0 * files_done as f64 / denom as f64 } else { 0.0 };
    if phase == "done" {
        pct = 100.0;
    }
    let elapsed = if t0 != 0.0 { util::now_f() - t0 } else { 0.0 };
    let speed = if elapsed > 1.0 && bytes_done != 0.0 {
        (bytes_done / elapsed) as i64
    } else {
        0
    };
    let eta = if phase == "download" && files_done > 0 && denom > files_done && elapsed > 0.0 {
        Some((elapsed / files_done as f64 * (denom - files_done) as f64) as i64)
    } else {
        None
    };
    obj(vec![
        ("phase", Value::String(phase)),
        ("pct", json!(py_round(pct.min(100.0), 1))),
        ("files_total", json!(if files_total != 0 { files_total } else { denom })),
        ("files_batch", json!(if files_batch != 0 { files_batch } else { denom })),
        ("files_done", json!(files_done)),
        ("bytes_total", json!(bytes_total as i64)),
        ("bytes_done", json!(bytes_done as i64)),
        ("speed", json!(speed)),
        ("eta", eta.map(|x| json!(x)).unwrap_or(Value::Null)),
        ("elapsed", json!(elapsed as i64)),
    ])
}

fn sv(s: &str) -> Value {
    Value::String(s.to_string())
}

fn obj(pairs: Vec<(&str, Value)>) -> Value {
    let mut m = Map::new();
    for (k, v) in pairs {
        m.insert(k.to_string(), v);
    }
    Value::Object(m)
}

fn queue_position(tid: &str) -> i64 {
    let gs = gray().lock().unwrap();
    match gs.queue.iter().position(|x| x == tid) {
        Some(i) => i as i64 + if gs.active.is_some() { 2 } else { 1 },
        None => 0,
    }
}

/// `_launch_next`：空闲就取队头；所有启动都走这里，保证串行。
fn launch_next() {
    let (tid, cmd, env) = {
        let mut gs = gray().lock().unwrap();
        if gs.active.is_some() {
            return;
        }
        let mut picked: Option<(String, Value)> = None;
        while !gs.queue.is_empty() {
            let tid = gs.queue.remove(0);
            if let Some(t) = task_get(&gs, &tid) {
                if !t.get("cancelled").and_then(|v| v.as_bool()).unwrap_or(false) {
                    picked = Some((tid, t));
                    break;
                }
            }
        }
        let Some((tid, t)) = picked else { return };
        gs.active = Some(tid.clone());
        let now = util::now_f();
        task_mut(&mut gs, &tid, |m| {
            m.insert("state".into(), sv("running"));
            m.insert("running".into(), json!(true));
            m.insert("started".into(), json!(now));
            prog_mut(m, |p| {
                p.insert("phase".into(), sv("list"));
                p.insert("t0".into(), json!(0.0));
            });
        });
        let cmd: Vec<String> = match t.get("cmd") {
            Some(Value::Array(a)) => a.iter().map(|x| s_of(Some(x))).collect(),
            _ => Vec::new(),
        };
        let env: Vec<(String, String)> = match t.get("_env") {
            Some(Value::Object(o)) => o
                .iter()
                .map(|(k, v)| (k.clone(), s_of(Some(v))))
                .collect(),
            _ => Vec::new(),
        };
        (tid, cmd, env)
    };
    if cmd.is_empty() {
        return;
    }
    // Python 那边是 stderr=STDOUT：两股输出合成一条流。这里自己开一根管道做同样的事，
    // 免得两个 reader 线程把先后顺序抖乱。
    let (rd, w1, w2) = match merged_pipe() {
        Ok(v) => v,
        Err(e) => {
            let mut gs = gray().lock().unwrap();
            task_mut(&mut gs, &tid, |m| {
                m.insert("running".into(), json!(false));
                m.insert("state".into(), sv("error"));
                m.insert("code".into(), json!(-1));
                push(m, &format!("启动失败：{e}"));
            });
            if gs.active.as_deref() == Some(tid.as_str()) {
                gs.active = None;
            }
            drop(gs);
            launch_next();
            return;
        }
    };
    let mut c = Command::new(&cmd[0]);
    c.args(&cmd[1..])
        .current_dir(gray_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::from(w1))
        .stderr(Stdio::from(w2));
    for (k, v) in env {
        c.env(k, v);
    }
    match c.spawn() {
        Ok(child) => {
            let shared = Arc::new(Mutex::new(child));
            procs().lock().unwrap().push((tid.clone(), shared.clone()));
            let tid2 = tid.clone();
            std::thread::spawn(move || consume(&tid2, rd, shared));
        }
        Err(e) => {
            {
                let mut gs = gray().lock().unwrap();
                task_mut(&mut gs, &tid, |m| {
                    m.insert("running".into(), json!(false));
                    m.insert("state".into(), sv("error"));
                    m.insert("code".into(), json!(-1));
                    push(m, &format!("启动失败：{e}"));
                });
                if gs.active.as_deref() == Some(tid.as_str()) {
                    gs.active = None;
                }
            }
            launch_next();
        }
    }
}

/// 一根 pipe，读端给本进程，写端给子进程的 stdout 与 stderr（各自 dup 一份）。
/// 写端设 CLOEXEC：别的子进程不该继承它，否则读端永远等不到 EOF。
fn merged_pipe() -> std::io::Result<(std::fs::File, std::fs::File, std::fs::File)> {
    use std::os::unix::io::FromRawFd;
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    unsafe {
        libc::fcntl(fds[1], libc::F_SETFD, libc::FD_CLOEXEC);
    }
    unsafe {
        let rd = std::fs::File::from_raw_fd(fds[0]);
        let w1 = std::fs::File::from_raw_fd(fds[1]);
        let w2 = w1.try_clone()?;
        Ok((rd, w1, w2))
    }
}

fn consume(tid: &str, rd: std::fs::File, child: Arc<Mutex<Child>>) {
    for raw in BufReader::new(rd).lines().map_while(Result::ok) {
        let line = raw.trim_end_matches('\n').to_string();
        if line.trim().is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("@@QR ") {
            if let Ok(v) = serde_json::from_str::<Value>(rest) {
                let mut gs = gray().lock().unwrap();
                task_mut(&mut gs, tid, |m| {
                    m.insert("qr".into(), v);
                });
            }
            continue;
        }
        let mut gs = gray().lock().unwrap();
        task_mut(&mut gs, tid, |m| push(m, &line));
    }
    let code = {
        use std::os::unix::process::ExitStatusExt;
        let mut ch = child.lock().unwrap();
        match ch.wait() {
            Ok(st) => match st.code() {
                Some(c) => c,
                // Python 的 Popen.returncode 对信号退出是负数（-15）
                None => -st.signal().unwrap_or(0),
            },
            Err(_) => -1,
        }
    };
    procs().lock().unwrap().retain(|(k, _)| k != tid);
    let mut gs = gray().lock().unwrap();
    let cancelled = task_get(&gs, tid)
        .map(|t| t.get("stopped").and_then(|v| v.as_bool()).unwrap_or(false))
        .unwrap_or(false);
    task_mut(&mut gs, tid, |m| {
        m.insert("code".into(), json!(code));
        m.insert("running".into(), json!(false));
        let stopped = m.get("stopped").and_then(|v| v.as_bool()).unwrap_or(false);
        m.insert("state".into(), sv(if stopped { "cancelled" } else if code == 0 { "done" } else { "error" }));
        let phase = m
            .get("prog")
            .and_then(|p| p.get("phase"))
            .map(|v| s_of(Some(v)))
            .unwrap_or_default();
        if phase != "done" {
            let nxt = if code == 0 && !stopped { "done" } else { "error" };
            prog_mut(m, |p| {
                p.insert("phase".into(), sv(nxt));
            });
        }
        if code != 0 {
            let lines: Vec<String> = match m.get("lines") {
                Some(Value::Array(a)) => a.iter().map(|x| s_of(Some(x))).collect(),
                _ => Vec::new(),
            };
            let tail: Vec<String> = lines.iter().rev().take(5).cloned().collect();
            if !tail.iter().any(|x| x.contains("失败")) {
                push(m, &format!("任务结束（退出码 {code}）"));
            }
        }
    });
    let _ = cancelled;
    if gs.active.as_deref() == Some(tid) {
        gs.active = None;
    }
    drop(gs);
    launch_next();
}

// ---------------------------------------------------------------- 路由

pub async fn status(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let cli = cli();
    let ready = cli.exists() && util::is_exec_file(&cli);
    let (running, task_id, queue) = {
        let gs = gray().lock().unwrap();
        let active = gs.active.as_ref().and_then(|id| task_get(&gs, id));
        let running = active
            .as_ref()
            .map(|t| t.get("running").and_then(|v| v.as_bool()).unwrap_or(false))
            .unwrap_or(false);
        (running, if running { gs.active.clone().unwrap_or_default() } else { String::new() }, gs.queue.len())
    };
    Json(obj(vec![
        ("ok", json!(ready && out_root().exists())),
        ("root", sv(&gray_dir().to_string_lossy())),
        ("out", sv(&out_root().to_string_lossy())),
        ("running", json!(running)),
        ("task", sv(&task_id)),
        ("queue", json!(queue)),
        ("mounted", json!(gray_dir().exists())),
    ]))
    .into_response()
}

fn qr_dir(ctx: &Ctx) -> PathBuf {
    let base = if ctx.inbox().is_empty() {
        util::home().join("Documents")
    } else {
        PathBuf::from(ctx.inbox())
    };
    base.join("_灰色二维码")
}

fn image_name(fname: &str) -> String {
    let raw = if fname.is_empty() {
        format!("图片-{}", util::now_f() as i64)
    } else {
        util::name_of(Path::new(fname))
    };
    let clean = bad_chars(&raw).trim().to_string();
    if clean.is_empty() {
        format!("图片-{}", util::now_f() as i64)
    } else {
        clean
    }
}

fn save_upload(data: &[u8], dest: &Path, limit: u64, max_mb: i64) -> Result<u64, String> {
    if data.len() as u64 > limit {
        return Err(format!("超过 {max_mb}MB 上限"));
    }
    std::fs::write(dest, data).map_err(|e| e.to_string())?;
    Ok(data.len() as u64)
}

pub async fn qr(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    mp: Multipart,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    if !gray_dir().exists() {
        return http_err(400, "移动硬盘没挂上（找不到 灰色 目录）");
    }
    let dir = qr_dir(&ctx);
    let _ = std::fs::create_dir_all(&dir);
    let max_mb = ctx.max_upload_mb() as i64;
    let limit = (max_mb as u64).saturating_mul(1024 * 1024);
    let mut mp = mp;
    let mut got: Option<(String, Vec<u8>)> = None;
    while let Ok(Some(field)) = mp.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        if name != "file" && got.is_some() {
            continue;
        }
        let fname = field.file_name().unwrap_or("").to_string();
        let data = match field.bytes().await {
            Ok(b) => b.to_vec(),
            Err(_) => return http_err(400, &format!("超过 {max_mb}MB 上限")),
        };
        got = Some((fname, data));
        if name == "file" {
            break;
        }
    }
    let Some((fname, data)) = got else {
        return http_err(400, "移动硬盘没挂上（找不到 灰色 目录）");
    };
    let safe = image_name(&fname);
    let dest = util::unique_path(&dir, &safe);
    match save_upload(&data, &dest, limit, max_mb) {
        Ok(n) => Json(obj(vec![
            ("path", sv(&dest.to_string_lossy())),
            ("name", sv(&util::name_of(&dest))),
            ("size", sv(&util::human(n as i64))),
            ("bytes", json!(n)),
        ]))
        .into_response(),
        Err(msg) => http_err(400, &msg),
    }
}

pub async fn run(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: Option<Json<Value>>,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let b = body_obj(body);
    let name = clean_name(&s_of(b.get("name")));
    if name.is_empty() {
        return http_err(400, "请先填文件夹名");
    }
    let target = s_of(b.get("path")).trim().to_string();
    if target.is_empty() {
        return http_err(400, "还没放二维码");
    }
    let mut qr_size = 0u64;
    let arg = if target.starts_with("http://") || target.starts_with("https://") {
        target.clone()
    } else {
        let p = Path::new(&target);
        if !p.is_file() {
            return http_err(400, "二维码没收到：文件不见了，请重新放一次");
        }
        qr_size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        target.clone()
    };
    if !cli().exists() {
        return http_err(400, "找不到灰色 CLI（移动硬盘没挂上？）");
    }
    let mut cmd: Vec<String> = vec![
        "/bin/zsh".into(),
        cli().to_string_lossy().to_string(),
        arg,
        "--name".into(),
        name.clone(),
    ];
    if let Some(lim) = b.get("limit") {
        if lim.is_i64() || lim.is_u64() {
            let n = util::i64_of(Some(lim));
            if n > 0 && n <= 20 {
                cmd.push("--limit".into());
                cmd.push(n.to_string());
            }
        }
    }
    let mut env = Map::new();
    for (k, v) in std::env::vars() {
        env.insert(k, Value::String(v));
    }
    env.insert("QRGET_GUI".into(), sv("1"));
    env.insert("FORCE_COLOR".into(), sv("0"));

    let tid = format!("{:x}{}", util::epoch_ms_i64(), util::rand_hex(2));
    let task = obj(vec![
        ("id", sv(&tid)),
        ("name", sv(&name)),
        ("state", sv("queued")),
        ("running", json!(false)),
        ("code", Value::Null),
        ("lines", json!([])),
        ("qr", Value::Null),
        ("out", sv("")),
        ("started", json!(0.0)),
        ("cmd", Value::Array(cmd.iter().map(|x| sv(x)).collect())),
        ("env", Value::Object(env.clone())),
        ("cancelled", json!(false)),
        (
            "prog",
            obj(vec![
                ("phase", sv("queued")),
                ("files_total", json!(0)),
                ("files_batch", json!(0)),
                ("files_done", json!(0)),
                ("bytes_total", json!(0.0)),
                ("bytes_done", json!(0.0)),
                ("t0", json!(0.0)),
            ]),
        ),
    ]);
    {
        let mut gs = gray().lock().unwrap();
        let mut t = task.clone();
        if let Some(m) = t.as_object_mut() {
            m.insert("_env".into(), Value::Object(env));
        }
        gs.tasks.push((tid.clone(), t));
        gs.queue.push(tid.clone());
        // 只留最近 6 个任务的日志：`for old in list(_TASKS)[:-6]`
        if gs.tasks.len() > 6 {
            let old: Vec<String> = gs.tasks[..gs.tasks.len() - 6].iter().map(|(k, _)| k.clone()).collect();
            for tid in old {
                if !gs.queue.contains(&tid) && gs.active.as_deref() != Some(tid.as_str()) {
                    gs.tasks.retain(|(k, _)| *k != tid);
                }
            }
        }
    }
    let position = queue_position(&tid);
    launch_next();
    let running = {
        let gs = gray().lock().unwrap();
        task_get(&gs, &tid)
            .map(|t| s_of(t.get("state")) == "running")
            .unwrap_or(false)
    };
    let qr_size_s = if qr_size != 0 { util::human(qr_size as i64) } else { "链接".to_string() };
    Json(obj(vec![
        ("id", sv(&tid)),
        ("name", sv(&name)),
        ("queued", json!(!running)),
        ("position", json!(position)),
        ("qr_received", json!(true)),
        ("qr_size", sv(&qr_size_s)),
    ]))
    .into_response()
}

pub async fn task(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    AxPath(tid): AxPath<String>,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let t = {
        let gs = gray().lock().unwrap();
        task_get(&gs, &tid)
    };
    let Some(t) = t else {
        return http_err(404, "没有这个任务");
    };
    let pos = queue_position(&tid);
    Json(obj(vec![
        ("id", sv(&tid)),
        ("name", t.get("name").cloned().unwrap_or_else(|| sv(""))),
        ("lines", t.get("lines").cloned().unwrap_or_else(|| json!([]))),
        ("qr", t.get("qr").cloned().unwrap_or(Value::Null)),
        ("out", t.get("out").cloned().unwrap_or_else(|| sv(""))),
        ("running", json!(t.get("running").and_then(|v| v.as_bool()).unwrap_or(false))),
        ("code", t.get("code").cloned().unwrap_or(Value::Null)),
        ("state", t.get("state").cloned().unwrap_or_else(|| sv(""))),
        ("queue_pos", json!(pos)),
        ("progress", snapshot(&t)),
    ]))
    .into_response()
}

pub async fn stop(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let mut stopped_queued = 0i64;
    let active;
    {
        let mut gs = gray().lock().unwrap();
        let queued: Vec<String> = gs.queue.clone();
        for tid in queued {
            task_mut(&mut gs, &tid, |m| {
                m.insert("cancelled".into(), json!(true));
                m.insert("state".into(), sv("cancelled"));
                push(m, "已取消排队");
            });
            gs.queue.retain(|x| *x != tid);
            stopped_queued += 1;
        }
        active = gs.active.clone();
    }
    let t = {
        let gs = gray().lock().unwrap();
        active.as_ref().and_then(|id| task_get(&gs, id))
    };
    let Some(t) = t else {
        return Json(obj(vec![
            ("ok", json!(true)),
            ("stopped", json!(false)),
            ("cancelled", json!(stopped_queued)),
        ]))
        .into_response();
    };
    if !t.get("running").and_then(|v| v.as_bool()).unwrap_or(false) {
        return Json(obj(vec![
            ("ok", json!(true)),
            ("stopped", json!(false)),
            ("cancelled", json!(stopped_queued)),
        ]))
        .into_response();
    }
    let child = {
        let p = procs().lock().unwrap();
        active
            .as_deref()
            .and_then(|id| p.iter().find(|(k, _)| k == id).map(|(_, c)| c.clone()))
    };
    if let Some(child) = child {
        // Python 是 proc.terminate()（SIGTERM），等 6 秒还不走才 kill。
        // 这里不抓着子进程锁等：读输出那条线程要拿同一把锁收尾（写 code/state/补尾行），
        // Python 那边也是 _consume 先落定、stop() 再补「已手动停止」，顺序得一致。
        let pid = { child.lock().unwrap().id() as i32 };
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        let finished = || -> bool {
            let gs = gray().lock().unwrap();
            match task_get(&gs, active.as_deref().unwrap_or("")) {
                Some(t) => !t.get("code").map(|v| v.is_null()).unwrap_or(true),
                None => true,
            }
        };
        let t0 = std::time::Instant::now();
        loop {
            if finished() {
                break;
            }
            if t0.elapsed().as_secs_f64() > 6.0 {
                let _ = child.lock().unwrap().kill();
                let k0 = std::time::Instant::now();
                while !finished() && k0.elapsed().as_secs_f64() < 2.0 {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    {
        let mut gs = gray().lock().unwrap();
        let tid = active.clone().unwrap_or_default();
        task_mut(&mut gs, &tid, |m| {
            m.insert("running".into(), json!(false));
            m.insert("stopped".into(), json!(true));
            push(m, "已手动停止（已下好的文件保留，重跑同一条会增量补齐）");
        });
    }
    Json(obj(vec![
        ("ok", json!(true)),
        ("stopped", json!(true)),
        ("cancelled", json!(stopped_queued)),
    ]))
    .into_response()
}

pub async fn images(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    mp: Multipart,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let name = clean_name(&q(&uri, "name"));
    // FastAPI 先校验请求体（缺 files 就是 422），才轮到 handler 里的 name/挂盘判断，
    // 所以这两个错误要先让位给「一个 files part 都没有」。
    if name.is_empty() || !gray_dir().exists() {
        let mut mp = mp;
        let mut seen = false;
        while let Ok(Some(field)) = mp.next_field().await {
            if field.name().unwrap_or("") == "files" && !seen {
                seen = true;
            }
            let _ = field.bytes().await;
        }
        if !seen {
            return util::validation_missing("files");
        }
        if name.is_empty() {
            return http_err(400, "请先填文件夹名");
        }
        return http_err(400, "移动硬盘没挂上（找不到 灰色 目录）");
    }
    let target = out_root().join(&name);
    let max_mb = ctx.max_upload_mb() as i64;
    let limit = (max_mb as u64).saturating_mul(1024 * 1024);
    let mut saved: Vec<Value> = Vec::new();
    let mut failed: Vec<Value> = Vec::new();
    let mut batch: u64 = 0;
    let mut seen = false;
    let mut mp = mp;
    while let Ok(Some(field)) = mp.next_field().await {
        if field.name().unwrap_or("") != "files" {
            continue;
        }
        if !seen {
            seen = true;
            let _ = std::fs::create_dir_all(&target);
        }
        let fname = field.file_name().unwrap_or("").to_string();
        let ctype = field.content_type().unwrap_or("").to_lowercase();
        if !ctype.is_empty() && !ctype.starts_with("image/") {
            failed.push(obj(vec![("name", sv(&fname)), ("error", sv("只收图片"))]));
            continue;
        }
        let data = match field.bytes().await {
            Ok(b) => b.to_vec(),
            Err(e) => {
                failed.push(obj(vec![
                    ("name", sv(&fname)),
                    ("error", sv(&util::trunc(&e.to_string(), 120))),
                ]));
                continue;
            }
        };
        let dest = util::unique_path(&target, &image_name(&fname));
        match save_upload(&data, &dest, limit, max_mb) {
            Ok(n) => {
                if batch + n > 2 * 1024 * 1024 * 1024 {
                    let _ = std::fs::remove_file(&dest);
                    failed.push(obj(vec![
                        ("name", sv(&fname)),
                        ("error", sv("这一批太大，请分几次传")),
                    ]));
                    continue;
                }
                batch += n;
                saved.push(obj(vec![
                    ("name", sv(&util::name_of(&dest))),
                    ("size", sv(&util::human(n as i64))),
                    ("path", sv(&dest.to_string_lossy())),
                ]));
            }
            Err(msg) => failed.push(obj(vec![("name", sv(&fname)), ("error", sv(&msg))])),
        }
    }
    if !seen {
        return util::validation_missing("files");
    }
    Json(obj(vec![
        ("saved", Value::Array(saved)),
        ("failed", Value::Array(failed)),
        ("dir", sv(&target.to_string_lossy())),
    ]))
    .into_response()
}
