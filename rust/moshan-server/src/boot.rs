// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：首启自举 —— 建目录骨架、发令牌、选端口、把结果写回 config.json。
//       对齐 `moshan/launcher.py` + `paths.ensure_layout()`，让服务**不依赖 Python**
//       也能独立起来（桌面壳只负责起进程，配置由这里落）。
// 不做：不拉模型、不装 CA（那两个是欢迎向导里"点了才做"的动作）。
//       也不在显式指定了端口（MOSHAN_RS_PORT）时改配置 —— 那是测试与多实例的口径。

use crate::paths;
use serde_json::Value;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;

const PORT_SCAN_START: u16 = 8323;
const PORT_SCAN_END: u16 = 8399;
const MAIN_SCAN_START: u16 = 8321;
const MAIN_SCAN_END: u16 = 8399;
const PLAIN_PORT: u16 = 8322; // 明文备用口，避开它
const MAIN_PORT: u16 = 8321; // 局域网 https 主口

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
///
/// 主端口要按 0.0.0.0 探（launcher 就是这么做的）：只探 127.0.0.1 会漏掉
/// 「别的进程已占 0.0.0.0:8321」这种最常见的情况；再补一次 127.0.0.1 ——
/// macOS 允许「通配已绑 + 具体地址再绑」，别的进程占着 127.0.0.1:8323 时
/// 光探 0.0.0.0 会误判空闲，结果本机入口被人家抢走。
fn port_free_on(host: &str, port: u16) -> bool {
    TcpListener::bind((host, port)).is_ok()
}

fn pick_port_on<F>(prefer: u16, start: u16, end: u16, skip: &[u16], free: F) -> u16
where
    F: Fn(u16) -> bool,
{
    if prefer != 0 && !skip.contains(&prefer) && free(prefer) {
        return prefer;
    }
    for p in start..=end {
        if skip.contains(&p) || p == prefer {
            continue;
        }
        if free(p) {
            return p;
        }
    }
    prefer.max(start)
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
    // 只建目录，不装服务：launchd plist / systemd user unit / 计划任务都不写。
    // 自启动归安装包与桌面壳（mac .app、Windows NSIS、Linux .deb/AppImage 的桌面项），
    // 服务端在更新时会整体替换，自己注册成常驻服务反而会把旧路径钉死。
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

/// 自举选出来的两个端口。
#[derive(Clone, Copy)]
pub struct Booted {
    /// 主端口：HTTPS（或退化后的明文）监听在 host:port。
    pub port: u16,
    /// 本机明文口：127.0.0.1:local_port，本机浏览器走这个（自签证书会被拦）。
    pub local_port: u16,
}

fn cfg_port(cfg: &Value, key: &str, default: u16) -> u16 {
    cfg.get(key)
        .and_then(|v| v.as_u64())
        .map(|v| v as u16)
        .filter(|p| *p != 0)
        .unwrap_or(default)
}

fn set_port(cfg: &mut Value, key: &str, port: u16) -> bool {
    if cfg.get(key).and_then(|v| v.as_u64()) == Some(port as u64) {
        return false;
    }
    if let Some(o) = cfg.as_object_mut() {
        o.insert(key.into(), Value::from(port));
    }
    true
}

/// 自举并返回实际使用的端口。调用方随后才 `Ctx::load()`（它要读这份刚写好的配置）。
///
/// 两个端口都要保证「不相等且都空闲」：只写 local_port 会让主端口与明文口撞在
/// 一起，TLS 一开本机就没有明文入口了（Windows 上 Safari/Chrome 会直接拦死）。
pub fn prepare() -> Booted {
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

    // 先定本机明文口：launcher 打开的就是它，而且 https 模式下本机只走这条明文。
    let local_port = pick_port_on(
        cfg_port(&cfg, "local_port", PORT_SCAN_START),
        PORT_SCAN_START,
        PORT_SCAN_END,
        &[PLAIN_PORT],
        |p| port_free_on("127.0.0.1", p),
    );
    // 主端口绑 0.0.0.0，必须避开明文口与 plain_port，否则两个监听抢同一个号。
    let port = pick_port_on(
        cfg_port(&cfg, "port", MAIN_PORT),
        MAIN_SCAN_START,
        MAIN_SCAN_END,
        &[local_port, PLAIN_PORT],
        |p| port_free_on("0.0.0.0", p) && port_free_on("127.0.0.1", p),
    );
    changed |= set_port(&mut cfg, "local_port", local_port);
    changed |= set_port(&mut cfg, "port", port);

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
    // 打开的是**本机明文口**：https 主端口上的自签证书会被本机浏览器直接拦死。
    if env_flag("MOSHAN_OPEN") {
        let token = cfg
            .get("token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let first = !path.parent().map(|p| p.join("welcome_done").exists()).unwrap_or(false);
        let url = if first {
            format!("http://127.0.0.1:{local_port}/welcome")
        } else {
            format!("http://127.0.0.1:{local_port}/dash?k={token}")
        };
        open_browser(&url);
    }
    Booted { port, local_port }
}

/// 三平台的「用默认浏览器打开」统一在 `sysopen` 里，这里只保留调用点。
fn open_browser(url: &str) {
    crate::sysopen::open_url(url);
}

/// 把一行状态打到 stderr（桌面壳把它收进日志）。
/// 按 scheme 打：HTTPS 时本机唯一的可用入口是那条明文口，必须说清楚在哪。
pub fn announce(scheme: &str, port: u16, local_port: u16, token: &str) {
    let q = if token.is_empty() {
        String::new()
    } else {
        format!("?k={token}")
    };
    if scheme == "https" {
        eprintln!("[moshan-rs] 就绪 https://127.0.0.1:{port}");
        eprintln!("[moshan-rs] 本机明文 http://127.0.0.1:{local_port}/{q}（无证书警告）");
    } else {
        eprintln!("[moshan-rs] 就绪 http://127.0.0.1:{port}/{q}");
    }
    let _ = std::io::stderr().flush();
}
