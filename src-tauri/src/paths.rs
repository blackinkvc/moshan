// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：解析状态目录与 config.json，规则与 moshan/paths.py 保持一致。
// 不做：不建目录、不写配置——写入永远归 Python 侧，Rust 只读。

use std::path::PathBuf;

const APP_NAME: &str = "Moshan";

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// 状态目录。优先级与 `paths.state_dir()` 完全相同：
/// MOSHAN_HOME/state → macOS 的 Application Support → Windows 的 APPDATA → Linux 的 XDG。
pub fn state_dir() -> PathBuf {
    if let Some(v) = env_nonempty("MOSHAN_HOME") {
        return PathBuf::from(v).join("state");
    }
    platform_state_dir()
}

#[cfg(target_os = "macos")]
fn platform_state_dir() -> PathBuf {
    home()
        .join("Library")
        .join("Application Support")
        .join(APP_NAME)
}

#[cfg(windows)]
fn platform_state_dir() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join("AppData").join("Roaming"))
        .join(APP_NAME)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_state_dir() -> PathBuf {
    // paths.py 在 Linux 上用 APP_NAME.lower()，这里必须一致，否则壳读不到服务写的配置。
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local").join("state"))
        .join(APP_NAME.to_lowercase())
}

pub fn config_file() -> PathBuf {
    state_dir().join("config.json")
}

/// 首启标记：`bootstrap.marker_file()` 写的就是这个文件。
pub fn welcome_done() -> PathBuf {
    state_dir().join("welcome.done")
}

pub fn log_file() -> PathBuf {
    state_dir().join("logs").join("moshan.log")
}

/// config.json 的最小视图。Python 会写入更多字段，这里只取壳需要的四个，
/// 其余字段靠 serde 忽略——不要在 Rust 侧做「完整配置模型」，那会变成第二份真相。
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct Config {
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub local_port: Option<u16>,
    /// HTTPS 主端口。壳不走它（自签证书在 WebView 里会被拒），读进来只为在
    /// 排查日志里能对上号，所以允许它未被使用。
    #[serde(default)]
    #[allow(dead_code)]
    pub port: Option<u16>,
    #[serde(default)]
    pub plain_port: Option<u16>,
    /// 收件箱绝对路径。壳用它判断「这个端口上应答的到底是不是这台机器该用的那份数据」：
    /// 只看 HTTP 200 会把残留的测试实例当成自己（实测 8325 被 `dbg-rl-*` 临时目录的实例占着，
    /// 壳就一路复用它，自己那份服务压根没起，页面读的是临时目录）。
    #[serde(default)]
    pub inbox_dir: String,
}

impl Config {
    /// 本机明文端口：launcher 打开浏览器用的就是它，壳也用它。
    pub fn plain(&self) -> Option<u16> {
        self.local_port.or(self.plain_port)
    }
}

pub fn read_config() -> Option<Config> {
    let raw = std::fs::read_to_string(config_file()).ok()?;
    serde_json::from_str(&raw).ok()
}
