// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：首启自举 —— 建目录骨架、发令牌、选端口、把结果写回 config.json。
//       对齐 `moshan/launcher.py` + `paths.ensure_layout()`，让服务**不依赖 Python**
//       也能独立起来（桌面壳只负责起进程，配置由这里落）。
// 不做：不拉模型、不装 CA（那两个是欢迎向导里"点了才做"的动作）。
//       也不在显式指定了端口（MOSHAN_RS_PORT）时改配置 —— 那是测试与多实例的口径。

use crate::paths;
use serde_json::Value;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::path::PathBuf;

const PORT_SCAN_START: u16 = 8323;
const PORT_SCAN_END: u16 = 8399;
const PLAIN_PORT: u16 = 8322; // 明文备用口，避开它
const MAIN_PORT: u16 = 8321; // 局域网 https 主口，避开它

fn env_flag(key: &str) -> bool {
    match std::env::var(key) {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            !v.is_empty() && v != "0" && v != "false" && v != "no"
        }
        Err(_) => false,
    }
}

/// 端口能不能独占。**不设 SO_REUSEADDR**：那会让「已被别人监听」也 bind 成功，
/// 于是误判为空闲、把服务绑到别人端口上（Python 侧踩过，注释里留着这段教训）。
fn port_free(port: u16) -> bool {
    TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).is_ok()
}

fn pick_port(prefer: u16, skip: &[u16]) -> u16 {
    if prefer != 0 && !skip.contains(&prefer) && port_free(prefer) {
        return prefer;
    }
    for p in PORT_SCAN_START..=PORT_SCAN_END {
        if skip.contains(&p) || p == prefer {
            continue;
        }
        if port_free(p) {
            return p;
        }
    }
    prefer.max(PORT_SCAN_START)
}

/// 32 位十六进制令牌。用系统随机源；取不到就退回时间+进程号（本机回环场景足够）。
fn new_token() -> String {
    let mut buf = [0u8; 16];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok();
    if !ok {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seed = t ^ (std::process::id() as u128) << 32;
        buf[..16].copy_from_slice(&seed.to_le_bytes());
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

fn ensure_dirs() {
    let state = paths::state_dir();
    let lib = paths::library_dir(&Value::Null);
    let mut dirs: Vec<PathBuf> = vec![
        state.clone(),
        state.join("logs"),
        state.join("versions"),
        state.join("certs"),
        lib.clone(),
        lib.join("收件箱"),
        lib.join(".回收站"),
    ];
    // 工作台目录按 config 走；读不到配置时落在默认库下的 09-个人工作台。
    let cfg = paths::read_config_value();
    dirs.push(paths::workbench_dir_for(&cfg));
    dirs.push(paths::workbench_dir_for(&cfg).join("wallpapers"));
    for d in dirs {
        let _ = std::fs::create_dir_all(&d);
    }
}

/// 自举并返回实际使用的端口。调用方随后才 `Ctx::load()`（它要读这份刚写好的配置）。
pub fn prepare() -> u16 {
    ensure_dirs();

    let path = paths::config_file();
    let mut cfg = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| Value::Object(Default::default()));

    let mut changed = false;
    let token = cfg
        .get("token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if token.is_empty() {
        if let Some(o) = cfg.as_object_mut() {
            o.insert("token".into(), Value::String(new_token()));
        }
        changed = true;
    }

    let prefer = cfg
        .get("local_port")
        .and_then(|v| v.as_u64())
        .unwrap_or(PORT_SCAN_START as u64) as u16;
    let port = pick_port(prefer, &[PLAIN_PORT, MAIN_PORT]);
    if prefer != port {
        if let Some(o) = cfg.as_object_mut() {
            o.insert("local_port".into(), Value::from(port));
        }
        changed = true;
    }

    if changed || !path.exists() {
        let text = crate::jsonio::dumps(&cfg, 2);
        if crate::jsonio::atomic_write(&path, &text).is_ok() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
        }
    }

    // 只有显式要求才开浏览器：桌面壳自己有窗口，服务端常驻进程也不该弹窗。
    if env_flag("MOSHAN_OPEN") {
        let token = cfg
            .get("token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let first = !path.parent().map(|p| p.join("welcome_done").exists()).unwrap_or(false);
        let url = if first {
            format!("http://127.0.0.1:{port}/welcome")
        } else {
            format!("http://127.0.0.1:{port}/dash?k={token}")
        };
        open_browser(&url);
    }
    port
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(windows)]
    let mut cmd = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    let _ = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// 把一行状态打到 stderr（桌面壳把它收进日志）。
pub fn announce(port: u16, token: &str) {
    eprintln!("[moshan-rs] 就绪 http://127.0.0.1:{port}/  令牌长度={}", token.len());
    let _ = std::io::stderr().flush();
}
