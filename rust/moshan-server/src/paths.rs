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
    /// 运行数据根（账目/日志/缓存/工作台/虫巢/传输），见 [`runtime_dir`]。
    pub runtime_dir: PathBuf,
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

/// 与 Python 的 `Path(...).expanduser().resolve()` 对齐：macOS 上 `/var` 是
/// `/private/var` 的软链接，解析与不解析会让 `/api/about` 里同一台机器出现两种
/// 路径写法，对拍时误判成差异。
///
/// `expanduser` 不能省：Python 两处 `MOSHAN_HOME` 都先 expanduser 再 resolve，
/// 用户写 `MOSHAN_HOME=~/moshan-data` 时如果不展开，`~` 会被当成字面目录名。
fn resolve(p: PathBuf) -> PathBuf {
    let p = match p.to_str() {
        Some(s) if s == "~" || s.starts_with("~/") => crate::storage::expanduser(s),
        _ => p,
    };
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
        // 与 Python 的 `os.environ.get("APPDATA") or (Path.home()/"AppData"/"Roaming")` 同口径：
        // 变量存在但为空串时也要退回 Roaming，不能拼出相对路径 `Moshan`。
        std::env::var_os("APPDATA")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join("AppData").join("Roaming"))
            .join(APP_NAME)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // Python 是 `Path(os.environ.get("XDG_STATE_HOME", ~/.local/state))`。
        // 空串按 XDG 规范当「未设置」处理（Python 那边会得到相对路径 `./moshan`，
        // 那是它的漏洞，不复刻）。
        std::env::var_os("XDG_STATE_HOME")
            .filter(|v| !v.is_empty())
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
        // Python 是 `Path(v).expanduser()` —— 配置文件里写 `~/Documents/墨山` 要展开。
        return crate::storage::expanduser(&from_cfg);
    }
    home().join("Documents").join(APP_NAME)
}

/// 运行数据根：账目、流水、系统记录、工作台、缓存、巡检、虫巢、传输。
///
/// 与 `library_dir`（媒体与文件）分开，是因为两者的读写特征相反：
/// 媒体是「一次写入、偶尔读、体积大」（几个 G，适合放外挂盘），
/// 运行数据是「高频追加、文件小、必须本地快」（几十 M，放系统盘最稳）。
/// 混在一个根下，换盘时要么把几百 G 图集一起搬，要么把账本留在快满的盘上。
///
/// 优先级：MOSHAN_HOME → config.runtime_dir → `<state_dir>/data`。
/// **没有 `runtime_dir` 的老配置会落到 `<state_dir>/data`**，不是落回媒体根 ——
/// 老用户升级后由 `migrate_legacy_runtime()` 把旧目录搬一次，之后各走各的。
pub fn runtime_dir(cfg: &Value) -> PathBuf {
    if let Some(v) = env_path("MOSHAN_HOME") {
        return resolve(v).join("data");
    }
    let from_cfg = cfg
        .get("runtime_dir")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if !from_cfg.is_empty() {
        return crate::storage::expanduser(&from_cfg);
    }
    state_dir().join("data")
}

/// 数据根下的工作台目录：外观设置、壁纸、页面产物都挂这里。
pub fn workbench_dir(ctx: &Ctx) -> PathBuf {
    ctx.runtime_dir.join("_工作台")
}

/// 同上，但只拿配置对象（自举阶段还没有 `Ctx`）。
pub fn workbench_dir_for(cfg: &Value) -> PathBuf {
    runtime_dir(cfg).join("_工作台")
}

/// 属于「运行数据」的子目录名单 —— 与媒体/文件分属两个根。
///
/// 这张表是**唯一的口径**：欢迎页把两个根分开选之后，凡是这里的名字都拼在
/// `runtime_dir` 下，其余（02-媒体、04-存图、08-阅读、01-研究、11-关注追踪、
/// 新闻摘录、07-收件箱、15-GitHub、17-游戏、18-界面评审）留在 `library_dir`。
///
/// 有几个地方拿不到 `Ctx`，只有「目录名 + 一个根字符串」（启动器判断脚本是否
/// 存在、包内资源清单），那时用 [`is_runtime_sub`] 决定该配哪个根。
pub const RUNTIME_SUBDIRS: &[&str] = &[
    "12-记账",
    "10-系统记录",
    "13-算价",
    "_本机工作",
    "_工作台",
    "_巡检",
    "05-采集",
    "16-虫巢思维",
    "16-多机",
    "14-传输",
    "19-AI工作计划",
];

/// 这个子目录属于运行数据吗？（见 [`RUNTIME_SUBDIRS`]）
pub fn is_runtime_sub(name: &str) -> bool {
    RUNTIME_SUBDIRS.contains(&name)
}

/// 运行数据根下的子目录，如 `rt_join(ctx, "10-系统记录")`。
///
/// 存在的意义是把「这是运行数据还是媒体文件」这个判断收在一处：
/// 代码里原来到处写 `crate::paths::rt_join(ctx, "10-系统记录")`，而 `root` 是媒体根，
/// 分盘后这些账目就会留在装满图片的盘上。逐个改成 `rt_join` 之后，
/// 谁属于哪一侧在调用处一眼可见，也不会再漏。
pub fn rt_join(ctx: &Ctx, name: &str) -> PathBuf {
    ctx.runtime_dir.join(name)
}

/// 自举阶段读配置：读不到就是 `Null`，与 `Ctx::load()` 同口径。
pub fn read_config_value() -> Value {
    std::fs::read_to_string(config_file())
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or(Value::Null)
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

        // `app_dir` 是**只读页面资源目录**（dash.html / ui.html / hostwork_ui.html），
        // 不是数据目录 —— 可写的重定向到 state_dir（见 `frozen`）。
        //
        // 顺序：显式覆盖 → 打包后的资源位置 → 开发态源码目录。
        //
        // 打包后的位置必须靠**运行时可执行文件**推，不能靠 `CARGO_MANIFEST_DIR`：
        // 那是编译机上的绝对路径（/Volumes/AI仓库/Moshan/rust/moshan-server），
        // 装到别人机器上根本不存在；即使在编译机上，它也让 app 去读外挂卷，
        // macOS 会弹「想访问可移除宗卷上的文件」，而这次 `open()` 是同步的，
        // 弹窗没人点就一路卡到端口绑不上。这个坑踩过一次（2026-11-27）。
        let app_dir = env_path("MOSHAN_APP_DIR")
            .filter(|p| p.join("dash.html").is_file())
            .or_else(|| {
                let exe = std::env::current_exe().ok()?;
                let dir = exe.parent()?;
                // mac：Contents/MacOS/moshan → Contents/Resources/moshan
                // win/linux：resources 与可执行文件并排铺开（Tauri 的约定）
                let cands = [
                    dir.join("assets"),
                    dir.join("..").join("Resources").join("assets"),
                    dir.join("moshan"),
                    dir.join("..").join("Resources").join("moshan"),
                ];
                cands.into_iter().find(|p| p.join("dash.html").is_file())
            })
            .unwrap_or_else(|| {
                // 开发态：cargo run 时源码目录就在手边。
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .parent()
                    .and_then(|p| p.parent())
                    .map(|p| p.join("moshan"))
                    .unwrap_or_else(|| PathBuf::from("."))
            });

        let library_dir = library_dir(&cfg);
        let state_dir = state_dir();
        let runtime_dir = runtime_dir(&cfg);

        // 打包态要认出来：`.app` 内部的资源目录是只读的（改它会破坏签名），
        // 所有可写的东西（pocket.jsonl、日报、临时件）都得改落 state_dir。
        // 判据用「资源目录在 .app 包里」而不是「装了包就一定 frozen」——
        // 开发态从 .app 里手跑也能得到正确行为。
        // 必须在 `app_dir` 被移进结构体之前算，否则借用检查不通过。
        let frozen = std::env::var("MOSHAN_FROZEN")
            .map(|v| v == "1" || v == "true")
            .unwrap_or_else(|_| app_dir.to_string_lossy().contains(".app/Contents/"));

        Self {
            cfg,
            token,
            upstream,
            app_dir,
            state_dir,
            library_dir,
            runtime_dir,
            frozen,
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

// ---------------------------------------------------------------- 媒体根看门狗
//
// 为什么需要：macOS 的 TCC 在 `open()` 里**同步**等授权结果。媒体根（`library_dir`）
// 落在「文稿」或外挂卷上时，没授权的进程拿到的不是 `EPERM`，而是整条线程停在内核里
// —— 实测一个没授权的 app 进程里同时挂了 10 条线程，`/api/dash`、`/api/notes`、
// `/api/worklog` 全部无响应，而只读运行数据根的 `/api/ledger`、`/api/brief` 正常
// （2026-11-27）。这种「静默挂死」比报错难查得多，所以：
//
//   1. 启动后由看门线程探一次媒体根，探不通就置 `blocked`；
//   2. 所有「要读媒体根」的入口先问 [`library_ready`]，不通就返回空块 / 跳过写盘；
//   3. 看门线程按退避重试，用户中途授权了也能自己恢复，但重试次数封顶，
//      避免每条卡住的探测线程都永久留在这里。
//
// 口径上 `unknown` 一律当作「可用」：探测还没跑完就先按原样干活，不改变正常路径的行为。

use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};

const LIB_UNKNOWN: u8 = 0;
const LIB_OK: u8 = 1;
const LIB_BLOCKED: u8 = 2;

static LIB_STATE: AtomicU8 = AtomicU8::new(LIB_UNKNOWN);

/// 媒体根的绝对路径。给 [`deny_library_read`] 判「这个路径是不是在媒体根下面」用，
/// 免得每个读盘函数都得先从 `Ctx` 里翻根。
static LIB_ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

pub fn set_library_root(p: &Path) {
    let _ = LIB_ROOT.set(p.to_path_buf());
}

/// 这次读盘要不要直接放弃：媒体根处于 `blocked`，且目标路径就落在媒体根里。
///
/// 放在 `load_json` 这类最底层读写函数里，是因为「哪些接口会碰媒体根」已经数不清了
/// （dash 里就有十几个块），把闸设在读盘那一步才漏不掉。
pub fn deny_library_read(p: &Path) -> bool {
    if !library_blocked() {
        return false;
    }
    match LIB_ROOT.get() {
        Some(root) => p.starts_with(root),
        // 没登记根就只挡「明确的媒体根相对路径」——宁可少挡，也不要误伤运行数据。
        None => false,
    }
}

/// 每次探测最多等这么久；超时就认为 `open()` 被 TCC 卡住了。
const PROBE_WAIT_SECS: u64 = 6;

/// 媒体根现在能不能读。`unknown` 与 `ok` 都是 `true`。
pub fn library_ready() -> bool {
    LIB_STATE.load(Ordering::Relaxed) != LIB_BLOCKED
}

/// 探不通（或正在等授权）时为 `true`。写盘入口据此直接跳过，避免把真数据覆盖成空。
pub fn library_blocked() -> bool {
    LIB_STATE.load(Ordering::Relaxed) == LIB_BLOCKED
}

#[allow(dead_code)] // 同上：诊断要「unknown/ok/blocked」这个三态，别只给 bool
pub fn library_state() -> &'static str {
    match LIB_STATE.load(Ordering::Relaxed) {
        LIB_OK => "ok",
        LIB_BLOCKED => "blocked",
        _ => "unknown",
    }
}

/// 真正去碰文件系统的那一下。被 TCC 卡住时它不会返回，所以只能放在独立线程里等。
///
/// **必须真的 `open()` 一个文件**：TCC 对「列目录」和「读文件内容」是分开判的，
/// 实测 `read_dir(媒体根)` 秒过、`open(媒体根/某个 json)` 却卡死。只探目录会得出
/// 「一切正常」的假结论，然后第一个 `/api/dash` 照样挂死（2026-11-27 踩过）。
fn probe_now(root: &Path) -> bool {
    use std::io::Read;

    fn touchable(p: &Path) -> bool {
        match std::fs::File::open(p) {
            Ok(mut f) => {
                let mut b = [0u8; 1];
                let _ = f.read(&mut b);
                true
            }
            Err(_) => false,
        }
    }

    // 先探「真会用到的两个文件」：存图索引与工作台底稿。这两份读不了，
    // 首页的图集/工作台就是空的 —— 这才是要判的东西。
    let mut tried = 0usize;
    for rel in ["04-存图/_索引.json", "09-个人工作台/notes.json"] {
        let p = root.join(rel);
        if !p.is_file() {
            continue;
        }
        tried += 1;
        if touchable(&p) {
            return true;
        }
        // 点名要读的文件打不开就**当场判死**，不再往下捞别的文件：
        // 媒体根里 `04-存图` 常是软链到外挂卷，内网盘上的文件读得动、外挂卷上的读不动，
        // 「捞到一个能读的」就会把不可用判成可用，接着 `/api/dash` 照样挂死（2026-11-27 踩过）。
        return false;
    }

    // 再退一步：随便翻几个文件试。
    // **不能「第一个打不开就判死」**：目录里有个权限 000 的残留文件很常见，
    // 那会把整块媒体判成不可用（2026-11-27 踩过）。这里逐个试，有一个能读就算通。
    let Ok(rd) = std::fs::read_dir(root) else {
        return false;
    };
    for e in rd.flatten().take(16) {
        let p = e.path();
        if p.is_file() {
            tried += 1;
            if touchable(&p) {
                return true;
            }
            continue;
        }
        let Ok(rd2) = std::fs::read_dir(&p) else { continue };
        for e2 in rd2.flatten().take(4) {
            let q = e2.path();
            if q.is_file() {
                tried += 1;
                if touchable(&q) {
                    return true;
                }
            }
        }
    }
    // 一个文件都没找到：目录能列就算通。
    tried == 0
}

/// 带超时的探测：返回 `Ok(true)` 表示通了，`Ok(false)` 表示明确读不了，
/// `Err(())` 表示超时（也就是卡在 `open()` 里）。
fn probe_with_timeout(root: PathBuf, wait_secs: u64) -> Result<bool, ()> {
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("lib-probe".into())
        .spawn(move || {
            let ok = probe_now(&root);
            let _ = tx.send(ok);
        });
    if spawned.is_err() {
        return Ok(false);
    }
    match rx.recv_timeout(std::time::Duration::from_secs(wait_secs.max(1))) {
        Ok(ok) => Ok(ok),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(()),
        // 探测线程自己崩了：按「读不了」处理，别一直挂在 unknown。
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Ok(false),
    }
}

/// 绑定端口**之前**先探一次，让第一批请求就拿到确定结论。
///
/// 不这么做的话，冷启动那几秒里 `state` 还是 `unknown`（按「可用」处理），
/// 第一个 `/api/dash` 会直接撞进被 TCC 卡住的 `open()` 里，一挂就是永久 ——
/// 实测新装包首次打开时正是这样（2026-11-27）。等一次最多三秒，比挂死划算。
pub fn probe_early(root: &Path) {
    match probe_with_timeout(root.to_path_buf(), 3) {
        Ok(true) => LIB_STATE.store(LIB_OK, Ordering::Relaxed),
        Ok(false) => LIB_STATE.store(LIB_BLOCKED, Ordering::Relaxed),
        Err(()) => {
            LIB_STATE.store(LIB_BLOCKED, Ordering::Relaxed);
            eprintln!(
                "[moshan-rs] {}",
                serde_json::json!({"event": "library_blocked_at_boot",
                                   "dir": root.to_string_lossy(),
                                   "hint": "首次 open() 卡在系统隐私授权上；先按不可用服务，后台继续重试"})
            );
        }
    }
}

/// 启动看门线程。幂等：重复调用只有第一次生效。
pub fn spawn_library_probe(root: PathBuf) {
    use std::sync::OnceLock;
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    std::thread::Builder::new()
        .name("lib-watch".into())
        .spawn(move || {
            // 退避表：前几次密一点 —— 冷启动时那一次 `open()` 卡住往往只是授权框
            // 还没被处理掉，常常十几秒就通了；退到 5 分钟只为不无限重试。
            // 「密」这一段很要紧：探通之前 `/api/dash` 的媒体几块是空的，
            // 用户会以为坏了（实测冷启动空窗持续了两分多钟）。
            let mut wait = 2u64;
            for attempt in 0..20 {
                if attempt > 0 {
                    std::thread::sleep(std::time::Duration::from_secs(wait));
                    wait = ((wait * 3) / 2).min(300);
                }
                // 已经是 ok 就不再探了：运行期的权限问题由各模块自己扛。
                if LIB_STATE.load(Ordering::Relaxed) == LIB_OK {
                    return;
                }
                match probe_with_timeout(root.clone(), PROBE_WAIT_SECS) {
                    Ok(true) => {
                        LIB_STATE.store(LIB_OK, Ordering::Relaxed);
                        eprintln!(
                            "[moshan-rs] {}",
                            serde_json::json!({"event": "library_ok", "dir": root.to_string_lossy()})
                        );
                        return;
                    }
                    Ok(false) => {
                        LIB_STATE.store(LIB_BLOCKED, Ordering::Relaxed);
                        eprintln!(
                            "[moshan-rs] {}",
                            serde_json::json!({"event": "library_unreadable",
                                               "dir": root.to_string_lossy(),
                                               "hint": "目录读不了；检查外挂卷是否挂载"})
                        );
                    }
                    Err(()) => {
                        LIB_STATE.store(LIB_BLOCKED, Ordering::Relaxed);
                        eprintln!(
                            "[moshan-rs] {}",
                            serde_json::json!({"event": "library_blocked",
                                               "dir": root.to_string_lossy(),
                                               "hint": "open() 被系统隐私授权卡住了；\
                                                        到 系统设置 → 隐私与安全性 → 完全磁盘访问权限 \
                                                        里勾上 Moshan.app 后重启 app"})
                        );
                    }
                }
            }
            eprintln!(
                "[moshan-rs] {}",
                serde_json::json!({"event": "library_probe_giveup",
                                   "hint": "已停止重试；授权后重启 app"})
            );
        })
        .ok();
}
