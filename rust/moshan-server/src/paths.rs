// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：目录解析 + 配置读取，规则逐条对齐 `moshan/paths.py`。
// 不做：不建目录、不写配置。目录骨架与 config.json 的写入权仍归 Python，
//       Rust 侧在迁移期只读——两边同时写配置就一定会分叉。

use serde_json::Value;
use std::path::PathBuf;

pub const APP_NAME: &str = "Moshan";
pub const VERSION: &str = "0.3.13";
pub const DEFAULT_REPO: &str = "blackinkvc/moshan";

pub struct Ctx {
    pub cfg: Value,
    pub token: String,
    /// 未迁路由的上游（迁移期指向 Python 服务，全部迁完后为空）。
    pub upstream: Option<String>,
    pub app_dir: PathBuf,
    pub state_dir: PathBuf,
    pub library_dir: PathBuf,
    pub frozen: bool,
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// 与 Python 的 `Path(...).resolve()` 对齐：macOS 上 `/var` 是 `/private/var` 的软链接，
/// 解析与不解析会让 `/api/about` 里同一台机器出现两种路径写法，对拍时误判成差异。
fn resolve(p: PathBuf) -> PathBuf {
    std::fs::canonicalize(&p).unwrap_or(p)
}

/// 与 `paths.state_dir()` 一致：MOSHAN_HOME/state → Application Support → APPDATA → XDG。
pub fn state_dir() -> PathBuf {
    if let Some(v) = env_path("MOSHAN_HOME") {
        return resolve(v).join("state");
    }
    #[cfg(target_os = "macos")]
    {
        home().join("Library").join("Application Support").join(APP_NAME)
    }
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join("AppData").join("Roaming"))
            .join(APP_NAME)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".local").join("state"))
            .join(APP_NAME.to_lowercase())
    }
}

pub fn config_file() -> PathBuf {
    state_dir().join("config.json")
}

/// 与 `paths.library_dir()` 一致：MOSHAN_HOME/library → config.library_dir → ~/Documents/Moshan。
pub fn library_dir(cfg: &Value) -> PathBuf {
    if let Some(v) = env_path("MOSHAN_HOME") {
        return resolve(v).join("library");
    }
    let from_cfg = cfg
        .get("library_dir")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if !from_cfg.is_empty() {
        return PathBuf::from(from_cfg);
    }
    home().join("Documents").join(APP_NAME)
}

/// 数据根下的工作台目录：外观设置、壁纸、页面产物都挂这里。
pub fn workbench_dir(ctx: &Ctx) -> PathBuf {
    ctx.library_dir.join("_工作台")
}

impl Ctx {
    pub fn load() -> Self {
        let cfg: Value = std::fs::read_to_string(config_file())
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or(Value::Null);

        let token = cfg
            .get("token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // 上游缺省指向 Python 服务：迁移期把没迁的路由原样转过去，
        // 这样任意一波做完，产品都是完整可用的（不存在"迁到一半用不了"）。
        let upstream = std::env::var("MOSHAN_PY_UPSTREAM").ok().filter(|s| !s.is_empty()).or_else(|| {
            let port = cfg
                .get("local_port")
                .or_else(|| cfg.get("plain_port"))
                .and_then(|v| v.as_u64())?;
            Some(format!("http://127.0.0.1:{port}"))
        });

        // 打包后 app_dir 是 .app 内部资源目录；这里允许覆盖，方便与 Python 侧对拍。
        let app_dir = env_path("MOSHAN_APP_DIR").unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .and_then(|p| p.parent())
                .map(|p| p.join("moshan"))
                .unwrap_or_else(|| PathBuf::from("."))
        });

        let library_dir = library_dir(&cfg);
        let state_dir = state_dir();

        Self {
            cfg,
            token,
            upstream,
            app_dir,
            state_dir,
            library_dir,
            frozen: std::env::var("MOSHAN_FROZEN").map(|v| v == "1").unwrap_or(false),
        }
    }

    pub fn inbox(&self) -> String {
        self.cfg
            .get("inbox_dir")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    }

    /// `INBOX_DIR`：配置里的 inbox_dir，缺省为 数据根/收件箱。
    pub fn inbox_dir(&self) -> PathBuf {
        let raw = self.inbox();
        if raw.is_empty() {
            return self.library_dir.join("收件箱");
        }
        PathBuf::from(raw)
    }

    /// `ALLOWED_ROOTS`：配置为空时退回数据根本身。
    /// Python 侧此时会得到一个空列表（任何路径都是 403），那是坏配置的表现；
    /// 这里选择「退回根目录」，宁可少一道闸也不要让所有文件接口静默全 403。
    pub fn allowed_roots(&self) -> Vec<PathBuf> {
        let list: Vec<PathBuf> = self
            .cfg
            .get("allowed_roots")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .map(PathBuf::from)
                    .collect()
            })
            .unwrap_or_default();
        if list.is_empty() {
            vec![self.library_dir.clone()]
        } else {
            list
        }
    }

    /// `TRASH_DIR`：配置里的 trash_dir（支持 `~`），缺省为 数据根/.回收站。
    pub fn trash_dir(&self) -> PathBuf {
        match self.cfg.get("trash_dir").and_then(|v| v.as_str()) {
            Some(s) if !s.trim().is_empty() => crate::storage::expanduser(s),
            _ => self.library_dir.join(".回收站"),
        }
    }

    pub fn shares_file(&self) -> PathBuf {
        self.state_dir.join("shares.json")
    }

    /// `max_upload_mb`：单文件上限（默认 500MB）。整批上限是它的两倍。
    pub fn max_upload_mb(&self) -> f64 {
        self.cfg
            .get("max_upload_mb")
            .and_then(|v| v.as_f64())
            .unwrap_or(500.0)
    }

    pub fn trash_days(&self) -> i64 {
        self.cfg.get("trash_days").and_then(|v| v.as_i64()).unwrap_or(30)
    }
}
