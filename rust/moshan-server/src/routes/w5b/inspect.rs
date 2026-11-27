// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：巡检三件套（`moshan/selfcheck.py` + `server.py` 的同名路由）：
//       GET  /api/inspect/report  读一份报告（只认「报告/」下的 xxx.md）
//       POST /api/inspect/run     触发一轮：优先转给 8774，没起就跑本机基线
//       GET  /api/inspect/state   进度快照：同样优先问 8774
// 不做：不跑 OCR、不改配置。
//
// 为什么要「先 8774 再本机」：Python 就是这么写的 —— 装了 cuntu 的机器用那边
// 更细的端口矩阵，没装的（交付版 / Windows）退回本机这十项基线。两条路径都得在，
// 否则对拍时同一台机器上两边会走不同分支。
//
// 本机基线的已知分叉（8774 不在时才暴露）：内存/运行时长按 `vm_stat` 与
// `kern.boottime` 复刻 sysinfo 口径；「采集层」那项 Rust 没有 hostwork，
// 只能回 skip；「运行依赖」按同一个解释器的 import 结果判断。

use super::util::{clip, now_hms, now_ymd_hms, round1, str_or};
use crate::paths::Ctx;
use crate::routes::common::{http_err, peek, CtxState};
use axum::extract::State;
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const INSPECT_BASE: &str = "http://127.0.0.1:8774";


fn report_dir(ctx: &Ctx) -> PathBuf {
    crate::paths::rt_join(ctx, "10-系统记录").join("巡检").join("报告")
}


// ---------------------------------------------------------------- 报告

pub async fn report(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let nm = crate::routes::common::q(&uri, "name").trim().to_string();
    if !nm.ends_with(".md") || nm.contains('/') || nm.contains('\\') || nm.starts_with('.') {
        return http_err(400, "文件名不对");
    }
    let root = std::fs::canonicalize(report_dir(&ctx)).unwrap_or_else(|_| report_dir(&ctx));
    let p = match std::fs::canonicalize(root.join(&nm)) {
        Ok(p) => p,
        Err(_) => return http_err(404, "没有这份报告"),
    };
    if !p.starts_with(&root) {
        return http_err(400, "文件名不对");
    }
    if !p.is_file() {
        return http_err(404, "没有这份报告");
    }
    let text = match std::fs::read(&p) {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(_) => return http_err(404, "没有这份报告"),
    };
    Json(json!({ "ok": true, "name": nm, "text": text })).into_response()
}

// ---------------------------------------------------------------- 跑一轮

pub const STATUS_CN: [(&str, &str); 4] = [
    ("ok", "通过"),
    ("warn", "注意"),
    ("fail", "没过"),
    ("skip", "跳过"),
];

fn cn_of(st: &str) -> String {
    STATUS_CN
        .iter()
        .find(|(k, _)| *k == st)
        .map(|(_, v)| v.to_string())
        .unwrap_or_else(|| st.to_string())
}

fn state_json() -> Value {
    let s = STATE
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_else(st_default);
    json!({
        "ok": true,
        "local": true,
        "running": s.running,
        "started": s.started,
        "phase": s.phase,
        "done": s.done,
        "total": s.total,
        "checks": s.checks,
        "last": s.last,
        "report": s.report,
        "error": s.error,
    })
}

#[derive(Clone, Default)]
struct Round {
    running: bool,
    started: String,
    phase: String,
    done: usize,
    total: usize,
    checks: Vec<Value>,
    last: Value,
    report: String,
    error: String,
}

static STATE: Mutex<Option<Round>> = Mutex::new(None);
static LOCK: AtomicBool = AtomicBool::new(false);

fn st_default() -> Round {
    Round {
        running: false,
        started: String::new(),
        phase: String::new(),
        done: 0,
        total: 0,
        checks: Vec::new(),
        last: Value::Null,
        report: String::new(),
        error: String::new(),
    }
}

fn with_state<F: FnOnce(&mut Round)>(f: F) {
    if let Ok(mut g) = STATE.lock() {
        let s = g.get_or_insert_with(st_default);
        f(s);
    }
}

pub fn state() -> Value {
    state_json()
}

/// `run()`：抢不到锁说明上一轮还在跑，回 started:false（HTTP 仍是 200）。
pub fn run_local(ctx: &Ctx, port: Option<u16>, heal: bool) -> Value {
    if LOCK.swap(true, Ordering::SeqCst) {
        return json!({ "ok": true, "started": false, "note": "上一轮还在跑" });
    }
    with_state(|s| {
        *s = Round {
            running: true,
            started: now_hms(),
            phase: "检查中".to_string(),
            done: 0,
            total: 10,
            checks: Vec::new(),
            last: Value::Null,
            report: String::new(),
            error: String::new(),
        };
    });
    // 巡检只写 10-系统记录/_本机工作，走运行根。
    let cfg_root = ctx.runtime_dir.clone();
    let rt = std::thread::Builder::new().name("inspect-round".into());
    let _ = rt.spawn(move || {
        work(&cfg_root, port, heal);
        LOCK.store(false, Ordering::SeqCst);
        with_state(|s| {
            s.running = false;
            s.phase.clear();
        });
    });
    json!({ "ok": true, "started": true, "local": true })
}

fn work(root: &Path, port: Option<u16>, heal: bool) {
    let mut rows: Vec<Value> = Vec::new();
    if heal {
        for sub in ["10-系统记录/巡检/报告", "10-系统记录/巡检", "_本机工作"] {
            let _ = std::fs::create_dir_all(root.join(sub));
        }
    }
    for (label, key) in checks_list() {
        let (st, detail) = one(key, root, port);
        rows.push(json!({
            "name": label,
            "status": st,
            "detail": detail,
            "cn": cn_of(&st),
        }));
        let snapshot = rows.clone();
        let n = snapshot.len();
        with_state(|s| {
            s.done = n;
            s.checks = snapshot;
        });
    }
    let text = render(&rows, port, root);
    let d = root.join("10-系统记录").join("巡检").join("报告");
    let _ = std::fs::create_dir_all(&d);
    let name = format!("{}.md", chrono::Local::now().format("%Y-%m-%d_%H%M%S"));
    let p = d.join(&name);
    let write_ok = std::fs::write(&p, text.as_bytes()).is_ok();
    let at = now_ymd_hms();
    let n_ok = rows.iter().filter(|r| r["status"] == "ok").count();
    let n_warn = rows.iter().filter(|r| r["status"] == "warn").count();
    let n_fail = rows.iter().filter(|r| r["status"] == "fail").count();
    if write_ok {
        let hist = root.join("10-系统记录").join("巡检").join("历史.jsonl");
        if let Some(parent) = hist.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Python 是 `json.dumps({...}, ensure_ascii=False) + "\n"` —— 分隔符带空格
        let line = super::util::dumps_py_default(&json!({
            "at": at.clone(), "ok": n_ok, "warn": n_warn, "fail": n_fail,
            "report": name.clone(),
        })) + "\n";
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&hist) {
            let _ = f.write_all(line.as_bytes());
        }
        with_state(|s| {
            s.last = json!({
                "at": at,
                "report": name,
                "fail": rows.iter().filter(|r| r["status"] == "fail").map(|r| r["name"].clone()).collect::<Vec<_>>(),
                "warn": rows.iter().filter(|r| r["status"] == "warn").map(|r| r["name"].clone()).collect::<Vec<_>>(),
            });
            s.report = s.last.get("report").and_then(|v| v.as_str()).unwrap_or("").to_string();
        });
    }
}

fn checks_list() -> Vec<(&'static str, &'static str)> {
    vec![
        ("数据目录可写", "dir"),
        ("磁盘剩余", "disk"),
        ("内存压力", "mem"),
        ("运行时长", "uptime"),
        ("Python 可用", "python"),
        ("https 证书", "tls"),
        ("本机服务", "port"),
        ("采集层", "sources"),
        ("运行依赖", "deps"),
        ("外网 GitHub", "net"),
    ]
}

fn one(key: &str, root: &Path, port: Option<u16>) -> (String, String) {
    let r = match key {
        "dir" => check_dir(root),
        "disk" => check_disk(root),
        "mem" => check_mem(),
        "uptime" => check_uptime(),
        "python" => check_python(),
        "tls" => check_tls(),
        "port" => check_port(port),
        "sources" => check_sources(),
        "deps" => check_deps(),
        "net" => check_net(),
        _ => ("skip".to_string(), String::new()),
    };
    (r.0, clip(&r.1, 180))
}

fn check_dir(root: &Path) -> (String, String) {
    if !root.is_dir() {
        return ("fail".into(), format!("数据根不存在：{}", root.display()));
    }
    let probe = root.join(".moshan-probe");
    let ok = std::fs::write(&probe, format!("{}", unix_now_f()))
        .and_then(|_| std::fs::remove_file(&probe))
        .is_ok();
    if !ok {
        return ("fail".into(), format!("数据根不可写：{}", root.display()));
    }
    ("ok".into(), root.display().to_string())
}

fn check_disk(root: &Path) -> (String, String) {
    let target = if root.exists() {
        root.to_path_buf()
    } else {
        PathBuf::from("/")
    };
    let u = crate::storage::usage(&target);
    let total = u.get("total").and_then(|v| v.as_u64()).unwrap_or(0) as f64;
    let free = u.get("free").and_then(|v| v.as_u64()).unwrap_or(0) as f64;
    if total == 0.0 {
        return ("skip".into(), "读不到磁盘用量：No such file or directory (os error 2)".into());
    }
    let gb = 1073741824.0;
    let (free_gb, total_gb) = (free / gb, total / gb);
    let st = if free_gb < 0.5 {
        "fail"
    } else if free_gb < 5.0 {
        "warn"
    } else {
        "ok"
    };
    (st.into(), format!("剩余 {free_gb:.1} GB / 共 {total_gb:.1} GB"))
}

fn unix_now_f() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// `sysinfo._mem()` 的 darwin 口径：sysctl hw.memsize + vm_stat 的 free/inactive。
fn mem_sample() -> Option<(f64, f64, f64)> {
    let total = run_cmd("/usr/sbin/sysctl", &["-n", "hw.memsize"])
        .and_then(|s| s.trim().parse::<f64>().ok())?;
    let vm = run_cmd("/usr/bin/vm_stat", &[]).unwrap_or_default();
    let page = re_num(&vm, r"page size of (\d+) bytes").unwrap_or(4096) as f64;
    let free = re_num(&vm, r"Pages free:\s+(\d+)").unwrap_or(0) as f64;
    let inact = re_num(&vm, r"Pages inactive:\s+(\d+)").unwrap_or(0) as f64;
    let avail = (free + inact) * page;
    let used = (total - avail).max(0.0);
    if total <= 0.0 {
        return None;
    }
    Some((used * 100.0 / total, used / 1073741824.0, total / 1073741824.0))
}

fn re_num(text: &str, pat: &str) -> Option<i64> {
    let rx = regex::Regex::new(pat).ok()?;
    rx.captures(text)?.get(1)?.as_str().parse().ok()
}

fn check_mem() -> (String, String) {
    match mem_sample() {
        None => ("skip".into(), "读不到内存：OSError: 取不到内存用量".into()),
        Some((pct, used, total)) => {
            let pct = round1(pct);
            let used = round1(used);
            let total = round1(total);
            let st = if pct >= 90.0 { "warn" } else { "ok" };
            (
                st.into(),
                format!(
                    "已用 {} GB / 共 {} GB（{}%）",
                    py_num(used),
                    py_num(total),
                    py_num(pct)
                ),
            )
        }
    }
}

/// Python `str(float)`：整数值带 `.0`，其余最短表示。
pub fn py_num(x: f64) -> String {
    if x.fract() == 0.0 && x.is_finite() {
        format!("{x:.1}")
    } else {
        format!("{x}")
    }
}

fn boot_time() -> Option<i64> {
    let out = run_cmd("/usr/sbin/sysctl", &["-n", "kern.boottime"])?;
    re_num(&out, r"sec = (\d+)")
}

fn check_uptime() -> (String, String) {
    let Some(boot) = boot_time() else {
        return ("skip".into(), "读不到运行时长".into());
    };
    let h = round1((unix_now_f() - boot as f64) / 3600.0);
    let st = if h > 24.0 * 14.0 { "warn" } else { "ok" };
    let tail = if st == "warn" {
        "（两周多没重启，建议重启一次）"
    } else {
        ""
    };
    (st.into(), format!("开机 {h:.1} 小时{tail}"))
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let p = dir.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// `pyexe.resolve(None, venv)` 的候选顺序（POSIX 分支）。
fn resolve_python() -> Option<PathBuf> {
    let venv = std::env::var("MOSHAN_CUNTU")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join("Documents")
                .join("cuntu")
        });
    let mut cands: Vec<PathBuf> = Vec::new();
    if venv.is_dir() {
        cands.push(venv.join("bin").join("python"));
    }
    for n in ["python3", "python"] {
        if let Some(p) = which(n) {
            cands.push(p);
        }
    }
    for d in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"] {
        for n in ["python3", "python"] {
            let p = PathBuf::from(d).join(n);
            if p.is_file() {
                cands.push(p);
            }
        }
    }
    cands.into_iter().find(|p| p.is_file())
}

fn run_cmd(bin: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(bin).args(args).output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn check_python() -> (String, String) {
    let Some(py) = resolve_python() else {
        return (
            "warn".into(),
            "找不到 Python：试过 python3 / python（PATH、/opt/homebrew/bin、/usr/local/bin、/usr/bin）"
                .into(),
        );
    };
    let ver = std::process::Command::new(&py)
        .arg("--version")
        .output()
        .ok()
        .map(|o| {
            let s = if o.stdout.is_empty() { o.stderr } else { o.stdout };
            String::from_utf8_lossy(&s)
                .trim()
                .lines()
                .next()
                .unwrap_or("")
                .to_string()
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "版本未知".to_string());
    ("ok".into(), format!("{ver}（{}）", py.display()))
}

fn check_tls() -> (String, String) {
    let Some(py) = resolve_python() else {
        return ("fail".into(), "缺 certifi：https 请求会 CERTIFICATE_VERIFY_FAILED".into());
    };
    let out = std::process::Command::new(&py)
        .args(["-c", "import certifi;print(certifi.where())"])
        .output();
    let Ok(out) = out else {
        return ("fail".into(), "缺 certifi：https 请求会 CERTIFICATE_VERIFY_FAILED".into());
    };
    if !out.status.success() {
        return ("fail".into(), "缺 certifi：https 请求会 CERTIFICATE_VERIFY_FAILED".into());
    }
    let ca = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let p = PathBuf::from(&ca);
    if !p.is_file() {
        return ("fail".into(), format!("证书文件不存在：{ca}"));
    }
    let n = std::fs::read_to_string(&p)
        .map(|t| t.matches("-----BEGIN CERTIFICATE-----").count())
        .unwrap_or(0);
    let st = if n > 0 { "ok" } else { "fail" };
    (st.into(), format!("根证书 {n} 张（{ca}）"))
}

fn check_port(port: Option<u16>) -> (String, String) {
    let Some(port) = port else {
        return ("skip".into(), "不知道自己的端口".into());
    };
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    match TcpStream::connect_timeout(&addr, Duration::from_secs(3)) {
        Ok(_) => ("ok".into(), format!("127.0.0.1:{port} 在听")),
        Err(e) => (
            "fail".into(),
            format!("127.0.0.1:{port} 连不上：{}", os_err(&e)),
        ),
    }
}

fn os_err(e: &std::io::Error) -> String {
    match e.raw_os_error() {
        Some(n) => format!("[Errno {n}] {}", e),
        None => e.to_string(),
    }
}

fn check_sources() -> (String, String) {
    // Python 问的是 hostwork.sources_report()；Rust 这条泳道没有采集层。
    ("skip".into(), "采集层不可用：Rust 侧未迁 hostwork".into())
}

fn check_deps() -> (String, String) {
    let Some(py) = resolve_python() else {
        return ("fail".into(), "缺依赖：fastapi、uvicorn、certifi".into());
    };
    let mut bad: Vec<&str> = Vec::new();
    for m in ["fastapi", "uvicorn", "certifi"] {
        let ok = std::process::Command::new(&py)
            .args(["-c", &format!("import {m}")])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            bad.push(m);
        }
    }
    if bad.is_empty() {
        ("ok".into(), "fastapi / uvicorn / certifi 都在".into())
    } else {
        ("fail".into(), format!("缺依赖：{}", bad.join("、")))
    }
}

fn check_net() -> (String, String) {
    let t0 = Instant::now();
    match TcpStream::connect_timeout(
        &match "api.github.com:443".parse() {
            Ok(a) => a,
            Err(_) => return ("warn".into(), "连不上 api.github.com：解析失败（这台机器适合 update.mode=push）".into()),
        },
        Duration::from_secs(4),
    ) {
        Ok(_) => {
            let ms = t0.elapsed().as_millis() as i64;
            ("ok".into(), format!("api.github.com:443 可达（{ms} ms）"))
        }
        Err(e) => (
            "warn".into(),
            format!(
                "连不上 api.github.com：{}（这台机器适合 update.mode=push）",
                os_err(&e)
            ),
        ),
    }
}

fn render(rows: &[Value], port: Option<u16>, root: &Path) -> String {
    let now = now_ymd_hms();
    let n_ok = rows.iter().filter(|r| r["status"] == "ok").count();
    let n_warn = rows.iter().filter(|r| r["status"] == "warn").count();
    let n_fail = rows.iter().filter(|r| r["status"] == "fail").count();
    let plat = if cfg!(windows) {
        "Windows"
    } else if cfg!(target_os = "macos") {
        "macOS"
    } else {
        "POSIX"
    };
    let port_s = port.map(|p| p.to_string()).unwrap_or_else(|| "未知".to_string());
    let mut lines: Vec<String> = vec![
        format!("# 巡检报告 {now}"),
        String::new(),
        format!("- 平台：{plat}"),
        format!("- 数据根：{}", root.display()),
        format!("- 端口：{port_s}"),
        format!("- 结果：{n_ok} 项通过 / {n_warn} 项注意 / {n_fail} 项没过"),
        String::new(),
        "| 检查项 | 结果 | 说明 |".to_string(),
        "|---|---|---|".to_string(),
    ];
    for r in rows {
        lines.push(format!(
            "| {} | {} | {} |",
            str_or(r.get("name")),
            str_or(r.get("cn")),
            str_or(r.get("detail"))
        ));
    }
    let worse: Vec<&Value> = rows
        .iter()
        .filter(|r| r["status"] == "fail" || r["status"] == "warn")
        .collect();
    if !worse.is_empty() {
        lines.push(String::new());
        lines.push("## 要看一眼的".to_string());
        lines.push(String::new());
        for r in worse {
            lines.push(format!(
                "- **{}**（{}）：{}",
                str_or(r.get("name")),
                str_or(r.get("cn")),
                str_or(r.get("detail"))
            ));
        }
    }
    lines.join("\n") + "\n"
}

// ---------------------------------------------------------------- 8774 转发

async fn http_body(url: &str, body: Option<&str>, timeout: u64) -> Option<Value> {
    let c = reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout))
        .no_proxy()
        .build()
        .ok()?;
    let mut req = match body {
        Some(_) => c.post(url),
        None => c.get(url),
    };
    if let Some(b) = body {
        req = req.header("Content-Type", "application/json").body(b.to_string());
    }
    let resp = req.send().await.ok()?;
    let text = resp.text().await.ok()?;
    let text = if text.trim().is_empty() { "{}".to_string() } else { text };
    serde_json::from_str(&text).ok()
}

pub async fn run_api(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: axum::body::Bytes,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    // Python: `payload: dict` 必填 —— 空体 422 missing、坏 JSON 422 json_invalid
    let body = match super::util::strict_body(&body) {
        Ok(v) => v,
        Err((code, b)) => {
            return (axum::http::StatusCode::from_u16(code).unwrap(), Json(b)).into_response()
        }
    };
    let heal = super::util::truthy(body.get("heal").unwrap_or(&Value::Null));
    let payload = super::util::dumps_py_default(&json!({ "heal": heal }));
    if let Some(v) = http_body(&format!("{INSPECT_BASE}/api/repair"), Some(&payload), 30).await {
        return Json(v).into_response();
    }
    // `request.url.port or CFG.get("port")`：端口取本次请求的 Host，其次取配置
    let port = request_port(&headers).or_else(|| {
        ctx.cfg
            .get("port")
            .and_then(|v| v.as_u64())
            .filter(|p| *p > 0)
            .map(|p| p as u16)
    });
    Json(run_local(&ctx, port, heal)).into_response()
}

fn request_port(headers: &HeaderMap) -> Option<u16> {
    let host = headers.get(axum::http::header::HOST)?.to_str().ok()?;
    let tail = host.rsplit(':').next()?;
    if tail == host {
        return None;
    }
    tail.parse().ok()
}

pub async fn state_api(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    if let Some(st) = http_body(&format!("{INSPECT_BASE}/api/state"), None, 10).await {
        let repair = st.get("repair").cloned().unwrap_or(Value::Null);
        let repair = if super::util::truthy(&repair) {
            repair
        } else {
            Value::Object(Map::new())
        };
        return Json(json!({ "ok": true, "repair": repair })).into_response();
    }
    Json(state()).into_response()
}

// 说明：没跑过一轮时 STATE 为 None，`state_json()` 会回落到与 Python 相同的初值
// （running=false、started=""、total=0、last=null）。
