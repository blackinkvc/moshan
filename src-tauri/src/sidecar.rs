// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：拉起 / 复用本机服务端（moshan-server，Rust），等到 /api/ping 真的应答再返回地址。
// 不做：不写配置、不选端口、不解析业务响应——端口与令牌由服务自举时写进 config.json。

use crate::paths;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 子进程句柄：只存一个，退出时统一收尸。Mutex 只是为了满足 static 的 Sync 要求。
static CHILD: Mutex<Option<Child>> = Mutex::new(None);

#[derive(Debug, serde::Serialize)]
pub struct BootError {
    pub stage: String,
    pub message: String,
    pub log_tail: String,
}

fn err(stage: &str, message: impl Into<String>) -> BootError {
    BootError {
        stage: stage.to_string(),
        message: message.into(),
        log_tail: log_tail(),
    }
}

/// 直接讲 HTTP/1.1 探活，不引 reqwest：目标是 127.0.0.1 明文端口，
/// 拉一整套 TLS 栈只为了读一行状态码不划算，也会让包大十几兆。
fn ping(port: u16, token: &str, want_inbox: &str) -> bool {
    let Ok(addr) = format!("127.0.0.1:{port}").parse::<SocketAddr>() else {
        return false;
    };
    let Ok(mut sock) = TcpStream::connect_timeout(&addr, Duration::from_millis(600)) else {
        return false;
    };
    let _ = sock.set_read_timeout(Some(Duration::from_millis(800)));
    let req = format!(
        "GET /api/ping HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Token: {token}\r\nConnection: close\r\n\r\n"
    );
    if sock.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 4096];
    let mut got = Vec::new();
    loop {
        match sock.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                got.extend_from_slice(&buf[..n]);
                if got.len() >= 4096 {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    if got.len() <= 12 {
        return false;
    }
    let text = String::from_utf8_lossy(&got);
    if !(text.starts_with("HTTP/1.1 200") || text.starts_with("HTTP/1.0 200")) {
        return false;
    }
    // 200 只说明「有个服务在」，还得是「这台机器该用的那份」。`inbox` 不同就说明
    // 端口被别的实例占着（常见于对拍/测试留下的临时数据根），此时必须自己另起一个，
    // 否则页面会连到别人的数据上。
    if want_inbox.is_empty() {
        return true;
    }
    match text.split_once("\r\n\r\n") {
        Some((_, body)) => match serde_json::from_str::<serde_json::Value>(body.trim()) {
            Ok(v) => v.get("inbox").and_then(|x| x.as_str()) == Some(want_inbox),
            Err(_) => false,
        },
        None => false,
    }
}

/// 首页落点与 launcher.py 一致：首启进 /welcome，之后进带令牌的中台。
fn url_for(port: u16, token: &str) -> String {
    if paths::welcome_done().exists() {
        format!("http://127.0.0.1:{port}/dash?k={token}")
    } else {
        format!("http://127.0.0.1:{port}/welcome")
    }
}

fn wait_ready(timeout: Duration) -> Option<String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(cfg) = paths::read_config() {
            if let Some(port) = cfg.plain() {
                if ping(port, &cfg.token, &cfg.inbox_dir) {
                    return Some(url_for(port, &cfg.token));
                }
            }
        }
        std::thread::sleep(Duration::from_millis(350));
    }
    None
}

/// 找可执行体：显式覆盖 → 与主程序同目录（打包后 Tauri 把 externalBin 放这里，
/// 名字会去掉三元组后缀）→ 开发态 binaries/moshan-<triple>。
///
/// **没有「退回 python3 launcher.py」这一档**：服务端已全量 Rust，静默回退会让
/// 开发态跑起一个 Python 版服务（接口/页面与发布物不一致），看着像在测新版、
/// 其实测的是旧实现。宁可报错让人先 stage，也不要这种回退。
fn program() -> Result<(PathBuf, Vec<String>), BootError> {
    if let Some(p) = std::env::var_os("MOSHAN_SIDECAR") {
        return Ok((PathBuf::from(p), vec![]));
    }

    let exe = std::env::current_exe().map_err(|e| err("locate", e.to_string()))?;
    let dir = exe
        .parent()
        .ok_or_else(|| err("locate", "无法定位可执行文件目录"))?
        .to_path_buf();
    let bare = if cfg!(windows) { "moshan.exe" } else { "moshan" };

    let mut candidates = vec![dir.join(bare)];
    if let Some(triple) = option_env!("TAURI_ENV_TARGET_TRIPLE") {
        let named = if cfg!(windows) {
            format!("moshan-{triple}.exe")
        } else {
            format!("moshan-{triple}")
        };
        candidates.push(dir.join(&named));
        candidates.push(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("binaries")
                .join(named),
        );
    }
    for cand in candidates {
        if cand.is_file() {
            return Ok((cand, vec![]));
        }
    }

    Err(err(
        "locate",
        format!(
            "找不到 sidecar。打包后应位于 {}；开发态请先跑 scripts/stage-sidecar.sh",
            dir.join(bare).display()
        ),
    ))
}

fn log_tail() -> String {
    let path = paths::log_file();
    let Ok(raw) = std::fs::read(&path) else {
        return String::new();
    };
    // 只取最后 4KB：日志可能有几兆，一次性塞进 IPC 会把错误面板撑爆。
    let start = raw.len().saturating_sub(4096);
    String::from_utf8_lossy(&raw[start..]).to_string()
}

/// 启动或复用本机服务，返回应加载的地址。
///
/// 先探活再拉进程，是有意为之：用户可能已经把网页版开在浏览器里，
/// 此时再起一个只会在端口上撞车（launcher 会顺延，但两个实例会各写各的账）。
pub fn boot() -> Result<String, BootError> {
    if let Some(cfg) = paths::read_config() {
        if let Some(port) = cfg.plain() {
            // 端口上有应答、且应答的那份实例用的就是本机这份收件箱，才算「已经在跑」。
            if ping(port, &cfg.token, &cfg.inbox_dir) {
                return Ok(url_for(port, &cfg.token));
            }
        }
    }

    let (program, args) = program()?;
    let mut cmd = Command::new(&program);
    cmd.args(&args)
        // 关键：壳自己有窗口，再让 launcher 唤起浏览器就成了「双击弹两个」。
        .env("MOSHAN_NO_BROWSER", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let child = cmd.spawn().map_err(|e| {
        err(
            "spawn",
            format!("启动 {} 失败：{e}", program.display()),
        )
    })?;
    *CHILD.lock().unwrap() = Some(child);

    wait_ready(Duration::from_secs(90)).ok_or_else(|| {
        err(
            "ready",
            "服务 90 秒内没有应答 /api/ping（首启解包较慢，可看下方日志）",
        )
    })
}

pub fn shutdown() {
    if let Some(mut child) = CHILD.lock().unwrap().take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}
