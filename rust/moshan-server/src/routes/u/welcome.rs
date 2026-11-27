// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：新手引导 8 条路由（页面 / 环境快照 / 拉模型 / 写配置 / 网关探测 / 选目录）。
// 不做：不启动引导的自动检查线程（Python 那套在 about.py 里，与这 8 条无关）。

use crate::jsonio;
use crate::paths::Ctx;
use crate::routes::common::{body_obj, http_err, q, s};
use crate::routes::u::util::{html, which};
use axum::extract::State;
use axum::http::Uri;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

type CtxState = State<Arc<Ctx>>;

const WELCOME_HTML: &str = include_str!("assets/welcome.txt");
const OLLAMA_DEFAULT: &str = "http://127.0.0.1:11434";
const PULL_LOG_LIMIT: usize = 400;

// ---------------------------------------------------------------- 拉取状态

#[derive(Clone)]
struct Pull {
    running: bool,
    model: String,
    lines: Vec<String>,
    ok: Option<bool>,
    error: String,
}

impl Default for Pull {
    fn default() -> Self {
        Self {
            running: false,
            model: String::new(),
            lines: Vec::new(),
            ok: None,
            error: String::new(),
        }
    }
}

fn pull_state() -> &'static Arc<Mutex<Pull>> {
    static ST: OnceLock<Arc<Mutex<Pull>>> = OnceLock::new();
    ST.get_or_init(|| Arc::new(Mutex::new(Pull::default())))
}

fn drain_lines<R: std::io::Read>(src: R, st: &Arc<Mutex<Pull>>) {
    use std::io::{BufRead, BufReader};
    for line in BufReader::new(src).lines().map_while(Result::ok) {
        append_line(st, &line);
    }
}

fn append_line(st: &Arc<Mutex<Pull>>, line: &str) {
    let mut p = st.lock().unwrap();
    p.lines.push(line.trim_end().to_string());
    if p.lines.len() > PULL_LOG_LIMIT {
        let drop = p.lines.len() - PULL_LOG_LIMIT;
        p.lines.drain(0..drop);
    }
}

// ---------------------------------------------------------------- 页面

pub async fn page(State(ctx): CtxState) -> Response {
    let body = WELCOME_HTML
        .replace("__VERSION__", &format!("v{}", crate::paths::VERSION))
        .replace("__TOKEN__", &ctx.token);
    html(200, body, &[("cache-control", "no-store")])
}

// ---------------------------------------------------------------- 环境快照

fn describe(ctx: &Ctx) -> Value {
    let mut d = Map::new();
    d.insert("app_dir".into(), json!(ctx.app_dir.to_string_lossy()));
    d.insert("state_dir".into(), json!(ctx.state_dir.to_string_lossy()));
    d.insert("library_dir".into(), json!(ctx.library_dir.to_string_lossy()));
    d.insert(
        "config".into(),
        json!(ctx.state_dir.join("config.json").to_string_lossy()),
    );
    d.insert("frozen".into(), json!(ctx.frozen));
    Value::Object(d)
}

fn os_name() -> &'static str {
    if cfg!(windows) {
        "nt"
    } else {
        "posix"
    }
}

async fn detect_ollama(base: &str) -> Value {
    let exe = which("ollama");
    let mut running = false;
    let mut models: Vec<String> = Vec::new();
    let url = format!("{}/api/tags", base.trim_end_matches('/'));
    if let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_millis(2500))
        .build()
    {
        if let Ok(resp) = client.get(&url).send().await {
            if let Ok(txt) = resp.text().await {
                if let Ok(v) = serde_json::from_str::<Value>(&txt) {
                    running = true;
                    if let Some(list) = v.get("models").and_then(|m| m.as_array()) {
                        for m in list {
                            if let Some(n) = m.get("name").and_then(|x| x.as_str()) {
                                if !n.is_empty() {
                                    models.push(n.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    let mut version = String::new();
    if let Some(exe_path) = exe.clone() {
        let out = tokio::task::spawn_blocking(move || {
            std::process::Command::new(exe_path).arg("--version").output()
        })
        .await
        .ok()
        .and_then(|r| r.ok());
        if let Some(o) = out {
            let so = String::from_utf8_lossy(&o.stdout).to_string();
            let se = String::from_utf8_lossy(&o.stderr).to_string();
            let raw = if !so.is_empty() { so } else { se };
            let trimmed = raw.trim();
            if let Some(first) = trimmed.lines().next() {
                version = first.chars().take(60).collect();
            }
        }
    }

    let mut o = Map::new();
    o.insert("installed".into(), json!(exe.is_some()));
    o.insert("running".into(), json!(running));
    o.insert("base".into(), json!(base));
    o.insert("models".into(), json!(models));
    o.insert("version".into(), json!(version));
    o.insert("exe".into(), json!(exe.unwrap_or_default()));
    Value::Object(o)
}

pub async fn status(State(ctx): CtxState) -> Response {
    let ollama = detect_ollama(OLLAMA_DEFAULT).await;
    let first_run = !ctx.state_dir.join("welcome.done").exists();
    let mut out = Map::new();
    out.insert("first_run".into(), json!(first_run));
    out.insert("ollama".into(), ollama);
    out.insert("platform".into(), json!(os_name()));
    out.insert("dirs".into(), describe(&ctx));
    Json(Value::Object(out)).into_response()
}

// ---------------------------------------------------------------- 拉模型

pub async fn pull_get() -> Response {
    let st = pull_state().lock().unwrap().clone();
    let tail: Vec<String> = st
        .lines
        .iter()
        .rev()
        .take(12)
        .rev()
        .cloned()
        .collect();
    let mut out = Map::new();
    out.insert("running".into(), json!(st.running));
    out.insert("model".into(), json!(st.model));
    out.insert("ok".into(), match st.ok {
        Some(b) => json!(b),
        None => Value::Null,
    });
    out.insert("error".into(), json!(st.error));
    out.insert("tail".into(), json!(tail));
    out.insert("lines".into(), json!(st.lines.len()));
    Json(Value::Object(out)).into_response()
}

pub async fn pull_post(
    State(ctx): CtxState,
    body: Option<Json<Value>>,
) -> Response {
    let body = body_obj(body);
    let model = {
        let from_body = s(&body, "model").trim().to_string();
        if !from_body.is_empty() {
            from_body
        } else {
            ctx.cfg
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string()
        }
    };
    if model.is_empty() {
        return http_err(400, "缺少模型名");
    }
    let base = ctx
        .cfg
        .get("ollama_base")
        .and_then(|v| v.as_str())
        .unwrap_or(OLLAMA_DEFAULT)
        .to_string();

    let st = pull_state();
    {
        let mut p = st.lock().unwrap();
        if p.running {
            return Json(json!({"started": false, "reason": "已有拉取任务在进行"}))
                .into_response();
        }
        *p = Pull {
            running: true,
            model: model.clone(),
            lines: Vec::new(),
            ok: None,
            error: String::new(),
        };
    }
    let exe = which("ollama");
    let Some(exe) = exe else {
        {
            let mut p = st.lock().unwrap();
            p.running = false;
            p.ok = Some(false);
            p.error = "未安装 Ollama".to_string();
        }
        return http_err(400, "未检测到 ollama 命令，请先安装 Ollama 再拉取模型");
    };

    let st2 = st.clone();
    let m = model.clone();
    std::thread::spawn(move || {
        append_line(&st2, &format!("$ ollama pull {m}"));
        let mut cmd = std::process::Command::new(&exe);
        cmd.args(["pull", &m]);
        if std::env::var_os("OLLAMA_HOST").is_none() {
            cmd.env(
                "OLLAMA_HOST",
                base.replace("http://", "").replace("https://", ""),
            );
        }
        // Python 是 `stderr=subprocess.STDOUT`：进度条在 stderr 上，必须合流，
        // 否则 tail 里只剩第一行。这里自己开一条 pipe，两个 fd 都指过去。
        #[cfg(all(unix, not(feature = "sim_windows")))]
        let merged = unsafe {
            let mut fds = [0i32; 2];
            if libc::pipe(fds.as_mut_ptr()) == 0 {
                Some((fds[0], fds[1]))
            } else {
                None
            }
        };
        #[cfg(not(all(unix, not(feature = "sim_windows"))))]
        let merged: Option<(i32, i32)> = None;

        // unix：两条 fd 指向同一条管道，tail 才能看到进度条（stderr）与正文交织。
        // Windows：没有可移植的 dup-into-另一个进程的办法，退回分开的两条管道。
        #[cfg(all(unix, not(feature = "sim_windows")))]
        let read_end = if let Some((rfd, wfd)) = merged {
            use std::os::unix::io::FromRawFd;
            let w2 = unsafe { libc::dup(wfd) };
            cmd.stdout(unsafe { std::process::Stdio::from_raw_fd(w2) });
            cmd.stderr(unsafe { std::process::Stdio::from_raw_fd(wfd) });
            Some(unsafe { std::fs::File::from_raw_fd(rfd) })
        } else {
            cmd.stdout(std::process::Stdio::piped());
            cmd.stderr(std::process::Stdio::piped());
            None
        };
        #[cfg(not(all(unix, not(feature = "sim_windows"))))]
        let read_end: Option<std::fs::File> = {
            cmd.stdout(std::process::Stdio::piped());
            cmd.stderr(std::process::Stdio::piped());
            None
        };
        // cmd 在 spawn 后必须立刻 drop：它持有我们 from_raw_fd 出来的写端，
        // 不释放父进程这一份就永远等不到 EOF（tail 会卡在最后一行）。
        let spawned = {
            let child = cmd.spawn();
            drop(cmd);
            child
        };
        match spawned {
            Ok(mut child) => {
                match read_end {
                    Some(f) => drain_lines(f, &st2),
                    None => {
                        if let Some(o) = child.stdout.take() {
                            drain_lines(o, &st2);
                        }
                    }
                }
                let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
                let mut p = st2.lock().unwrap();
                p.running = false;
                p.ok = Some(code == 0);
                p.error = if code == 0 {
                    String::new()
                } else {
                    format!("ollama pull 退出码 {code}")
                };
            }
            Err(e) => {
                let mut p = st2.lock().unwrap();
                p.running = false;
                p.ok = Some(false);
                p.error = e.to_string();
                drop(p);
                append_line(&st2, &format!("[错误] {e}"));
            }
        }
    });

    Json(json!({"started": true, "model": model})).into_response()
}

// ---------------------------------------------------------------- 写配置

fn py_str_list(v: &Value) -> Option<Vec<String>> {
    v.as_array().map(|a| {
        a.iter()
            .map(|x| match x {
                Value::String(s) => s.clone(),
                Value::Null => "None".to_string(),
                other => other.to_string(),
            })
            .filter(|s| !s.trim().is_empty())
            .collect()
    })
}

pub async fn apply(State(ctx): CtxState, body: Option<Json<Value>>) -> Response {
    let body = body_obj(body);
    let p = ctx.state_dir.join("config.json");
    let mut cur: Value = if p.exists() {
        jsonio::read_json(&p).unwrap_or_else(|| ctx.cfg.clone())
    } else {
        ctx.cfg.clone()
    };
    if !cur.is_object() {
        cur = json!({});
    }

    let obj = cur.as_object_mut().unwrap();
    if let Some(feats) = body.get("features").and_then(py_str_list) {
        obj.insert("features".into(), json!(feats));
    }
    obj.insert("use_llm".into(), json!(crate::routes::common::b(&body, "use_llm")));
    let model = s(&body, "model").trim().to_string();
    if !model.is_empty() {
        obj.insert("model".into(), json!(model));
    }

    let mut moved = false;
    let want = s(&body, "library_dir").trim().to_string();
    let old = obj
        .get("library_dir")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if !want.is_empty() && want != old {
        match std::fs::create_dir_all(&want) {
            Ok(_) => {}
            Err(e) => {
                return Json(json!({
                    "ok": false,
                    "error": format!("这个目录建不出来：{e}"),
                }))
                .into_response()
            }
        }
        let target = PathBuf::from(&want);
        obj.insert("library_dir".into(), json!(target.to_string_lossy()));
        obj.insert(
            "allowed_roots".into(),
            json!([target.to_string_lossy().to_string()]),
        );
        obj.insert(
            "trash_dir".into(),
            json!(target.join(".回收站").to_string_lossy()),
        );
        obj.insert("dav_root".into(), json!(target.to_string_lossy()));
        obj.insert(
            "inbox_dir".into(),
            json!(target.join("07-收件箱").to_string_lossy()),
        );
        moved = true;
    }

    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let text = jsonio::dumps(&cur, 2);
    if std::fs::write(&p, text).is_err() {
        return Json(json!({"ok": false, "error": "写配置失败"}))
            .into_response();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
    }
    let lib = cur
        .get("library_dir")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| ctx.library_dir.to_string_lossy().to_string());
    Json(json!({"ok": true, "moved_hint": moved, "library_dir": lib})).into_response()
}

pub async fn finish(State(ctx): CtxState) -> Response {
    let _ = std::fs::create_dir_all(&ctx.state_dir);
    let at = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%z").to_string();
    let body = format!("{{\"at\": \"{at}\"}}");
    let _ = std::fs::write(ctx.state_dir.join("welcome.done"), body);
    Json(json!({"ok": true, "dirs": describe(&ctx)})).into_response()
}

// ---------------------------------------------------------------- 网关探测

fn urllib_error_text(e: &reqwest::Error) -> String {
    if e.is_connect() {
        // Python: urllib.error.URLError(ConnectionRefusedError(61, 'Connection refused'))
        return "<urlopen error [Errno 61] Connection refused>".to_string();
    }
    if e.is_timeout() {
        return "timed out".to_string();
    }
    e.to_string()
}

async fn probe_gateway(base: &str, key: &str) -> Value {
    if base.is_empty() {
        return json!({"ok": false, "reason": "未配置网关地址", "models": []});
    }
    let url = format!("{}/v1/models", base.trim_end_matches('/'));
    let token = if key.is_empty() { "none" } else { key };
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_millis(4000))
        .build()
    {
        Ok(c) => c,
        Err(_) => {
            return json!({"ok": false, "reason": "连接失败：", "models": []})
        }
    };
    match client
        .get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
    {
        Ok(resp) => {
            // urllib 对 >=400 直接抛 HTTPError：`HTTP Error 401: Unauthorized`。
            let code = resp.status().as_u16();
            if code >= 400 {
                let reason = resp.status().canonical_reason().unwrap_or("");
                return json!({
                    "ok": false,
                    "reason": format!("连接失败：HTTP Error {code}: {reason}"),
                    "models": [],
                });
            }
            match resp.text().await {
            Ok(txt) => match serde_json::from_str::<Value>(&txt) {
                Ok(v) => {
                    let mut models: Vec<String> = Vec::new();
                    if let Some(arr) = v.get("data").and_then(|x| x.as_array()) {
                        for m in arr {
                            if let Some(id) = m.get("id").and_then(|x| x.as_str()) {
                                if !id.is_empty() {
                                    models.push(id.to_string());
                                }
                            }
                        }
                    }
                    models.truncate(50);
                    json!({"ok": true, "reason": "已连通", "models": models})
                }
                Err(e) => json!({
                    "ok": false,
                    "reason": format!("连接失败：{e}"),
                    "models": [],
                }),
            },
                Err(e) => json!({
                    "ok": false,
                    "reason": format!("连接失败：{}", urllib_error_text(&e)),
                    "models": [],
                }),
            }
        }
        Err(e) => json!({
            "ok": false,
            "reason": format!("连接失败：{}", urllib_error_text(&e)),
            "models": [],
        }),
    }
}

pub async fn gateway_test(
    State(ctx): CtxState,
    body: Option<Json<Value>>,
) -> Response {
    let body = body_obj(body);
    let base = {
        let b = s(&body, "base").to_string();
        if b.is_empty() {
            ctx.cfg
                .get("gateway_base")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        } else {
            b
        }
    };
    let key = {
        let k = s(&body, "key").to_string();
        if k.is_empty() {
            ctx.cfg
                .get("gateway_api_key")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        } else {
            k
        }
    };
    Json(probe_gateway(&base, &key).await).into_response()
}

// ---------------------------------------------------------------- 选目录

pub async fn pickdir() -> Response {
    if !cfg!(target_os = "macos") {
        return Json(json!({
            "ok": false,
            "error": "这个系统没有原生选择器，直接填路径",
        }))
        .into_response();
    }
    let out = tokio::task::spawn_blocking(|| {
        std::process::Command::new("osascript")
            .args([
                "-e",
                "POSIX path of (choose folder with prompt \"选一个目录放墨山的数据\")",
            ])
            .output()
    })
    .await
    .ok();
    let out = match out {
        Some(Ok(o)) => o,
        Some(Err(e)) => {
            return Json(json!({"ok": false, "error": e.to_string()})).into_response();
        }
        None => {
            return Json(json!({"ok": false, "error": "选择器没能启动"}))
                .into_response()
        }
    };
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let p = stdout.trim().trim_end_matches('/').to_string();
    if !out.status.success() || p.is_empty() {
        return Json(json!({"ok": false, "cancelled": true, "error": "已取消"}))
            .into_response();
    }
    Json(json!({"ok": true, "path": p})).into_response()
}

pub fn _unused(uri: &Uri) -> String {
    q(uri, "x")
}
