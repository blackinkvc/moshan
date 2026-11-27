// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：`updater.py` 的安装段 —— `app_bundle` / `in_place_blocked` /
//       `apply_in_place` / `launch_installer` / `cleanup`。
// 不做：检查与下载（update.rs / download.rs），也不做安装以外的业务判断。
//
// 为什么是三段式（见 updater.py 的模块注释）：运行中的 .app 有一部分文件被系统
// 锁着，替换到一半就崩。所以这里只做「解压 → 验签 → 起 helper」，真正的
// `mv` 旧包 / 复制新包在 HELPER 里，由它等**本进程真的退出**之后动手。
//
// 与 Python 的两处实现差异（都写在汇报里）：
//   1. 解压用 `zip` crate，不再调 `ditto -xk` / `/usr/bin/unzip` —— 那两个在
//      Windows 上根本不存在。代价：不还原 Mac 的扩展属性与资源叉（zip 里本来
//      也不带），签名校验用的是包内文件的字节，够用。
//   2. `cleanup` 的 state 目录从 `paths::state_dir()` 现取（Python 也是每次现取），
//      所以签名保持 `cleanup(keep)`。

#![allow(dead_code)]

use crate::paths::Ctx;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

/// `updater.apply_in_place()` 的返回：app / work / hint。
pub struct ApplyInfo {
    pub app: PathBuf,
    pub work: PathBuf,
    pub hint: String,
}

/// 原文，前端直接显示。
pub const APPLY_HINT: &str = "正在更新：程序会自己关掉再打开，约 3 秒。设置和文件都不会动。";

// ---------------------------------------------------------------- app 本体

/// `updater.app_bundle()`：当前进程所在的 .app。
///
/// `MOSHAN_APP_BUNDLE` 只给测试与自动验收用，正常路径读自身位置 —— 测试里绝不能
/// 让 helper 去替换正在运行的服务，所以把它指到一个 /tmp 下的假 bundle。
pub fn app_bundle() -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("MOSHAN_APP_BUNDLE") {
        if !v.is_empty() {
            return Some(PathBuf::from(v));
        }
    }
    let exe = std::env::current_exe().ok()?;
    // Python 侧是 `Path(sys.executable).resolve()`：resolve 失败就当没有。
    let exe = std::fs::canonicalize(&exe).ok()?;
    exe.ancestors()
        .find(|p| p.extension().map(|s| s == "app").unwrap_or(false))
        .map(|p| p.to_path_buf())
}

/// `os.access(dir, os.W_OK)`：按**真实**权限问系统，不看 owner/group 推导。
/// 只读 mode 位（`Permissions::readonly`）在 555 上碰巧也对，但 Windows 的
/// 只读位语义完全不同，所以 unix 走 access、其它平台退回只读位判断。
fn writable(dir: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let Ok(c) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
            return false;
        };
        unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
    }
    #[cfg(not(unix))]
    {
        std::fs::metadata(dir)
            .map(|m| !m.permissions().readonly())
            .unwrap_or(false)
    }
}

/// `updater.in_place_blocked()`：不能原地更新的原因；空串表示可以。
///
/// 五道门一道都不能省：平台 / 是否 frozen / 找不找得到 .app / AppTranslocation /
/// /Volumes/ / 父目录可写。文案是前端直接显示的，一个字都别改。
pub fn in_place_blocked(ctx: &Ctx) -> String {
    if !cfg!(target_os = "macos") {
        return "这个平台走安装程序".into();
    }
    if !ctx.frozen {
        return "源码运行，没有 .app 可以替换".into();
    }
    let Some(app) = app_bundle() else {
        return "找不到 .app 本体（可能被改名或挪出了 bundle）".into();
    };
    let s = app.to_string_lossy().to_string();
    if s.contains("/AppTranslocation/") {
        return "这个 app 正从下载目录 / dmg 里直接运行，系统给了临时路径。先把 Moshan 拖进「应用程序」，之后就能一键更新。".into();
    }
    if s.starts_with("/Volumes/") {
        return "这个 app 在 dmg 里运行。先拖进「应用程序」，之后就能一键更新。".into();
    }
    let parent = app.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    if !writable(&parent) {
        return format!(
            "{} 不可写（可能装成了别人的）。重装到自己的应用程序目录再更新。",
            parent.to_string_lossy()
        );
    }
    String::new()
}

// ---------------------------------------------------------------- helper

/// `updater._HELPER` 逐字照抄（含注释里的中文与 `printf '%s %s\n'`）。
/// 改一个字都会让「回滚」这条路径上的日志对不上。
const HELPER: &str = r#"#!/bin/bash
set -u
PID="$1"; APP="$2"; NEW="$3"; WORK="$4"
log() { printf '%s %s\n' "$(date '+%F %T')" "$*" >>"$WORK/apply.log"; }
log "helper 起：pid=$PID app=$APP new=$NEW"
for _ in $(seq 1 240); do
  kill -0 "$PID" 2>/dev/null || break
  sleep 0.25
done
sleep 1
if kill -0 "$PID" 2>/dev/null; then
  log "旧进程 60 秒仍未退出，放弃替换（不冒险做半个）"
  exit 2
fi
BAK="$APP.moshan-old"
rm -rf "$BAK"
if ! mv "$APP" "$BAK" 2>>"$WORK/apply.log"; then
  log "挪走旧 app 失败，放弃"
  exit 3
fi
if ! ditto "$NEW" "$APP" 2>>"$WORK/apply.log"; then
  log "复制新 app 失败，回滚"
  rm -rf "$APP"
  mv "$BAK" "$APP"
  open "$APP"
  exit 4
fi
xattr -dr com.apple.quarantine "$APP" 2>/dev/null
rm -rf "$BAK"
if open "$APP" 2>>"$WORK/apply.log"; then
  log "替换完成，已重新打开"
else
  log "替换完成，但 open 没成功 —— 让用户自己双击一次"
fi
exit 0
"#;

/// 解压到 `dest`。`ditto -xk` 的替身：保留 unix mode（新 app 里的可执行位丢了，
/// `open` 就起不来），挡掉 zip-slip（`enclosed_name` 会拒绝绝对路径与 `..`）。
fn extract_zip(pkg: &Path, dest: &Path) -> Result<(), String> {
    let f = std::fs::File::open(pkg).map_err(|e| format!("解压失败：{e}"))?;
    let mut ar = zip::ZipArchive::new(f).map_err(|e| format!("解压失败：{e}"))?;
    std::fs::create_dir_all(dest).map_err(|e| format!("解压失败：{e}"))?;
    for i in 0..ar.len() {
        let mut zf = ar.by_index(i).map_err(|e| format!("解压失败：{e}"))?;
        let Some(rel) = zf.enclosed_name() else {
            // 越界条目：直接跳过，别让它写到 work 之外。
            continue;
        };
        let out = dest.join(&rel);
        let mode = zf.unix_mode();
        let is_dir = zf.is_dir();
        if is_dir {
            drop(zf);
            if let Err(e) = std::fs::create_dir_all(&out) {
                return Err(format!("解压失败：{e}"));
            }
        } else {
            if let Some(parent) = out.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    return Err(format!("解压失败：{e}"));
                }
            }
            let mut dst = match std::fs::File::create(&out) {
                Ok(v) => v,
                Err(e) => return Err(format!("解压失败：{e}")),
            };
            if let Err(e) = std::io::copy(&mut zf, &mut dst) {
                return Err(format!("解压失败：{e}"));
            }
            drop(dst);
            drop(zf);
        }
        #[cfg(unix)]
        {
            if let Some(m) = mode {
                use std::os::unix::fs::PermissionsExt;
                // 有些打包工具给目录条目的 mode 是 0：照抄会得到一个 000 的目录，
                // 后面的文件全写不进去。0 当「没给」，目录再补上属主 rwx。
                if m & 0o777 != 0 {
                    let m = if is_dir { (m & 0o7777) | 0o700 } else { m & 0o7777 };
                    let _ = std::fs::set_permissions(&out, std::fs::Permissions::from_mode(m));
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (mode, is_dir);
        }
    }
    Ok(())
}

/// 先看 `new/` 直下（对应 `glob("*.app")`），再递归（对应 `rglob("*.app")`）。
fn find_app(root: &Path) -> Option<PathBuf> {
    let entries = |d: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(d)
            .map(|it| it.flatten().map(|e| e.path()).collect())
            .unwrap_or_default()
    };
    let is_app = |p: &Path| p.extension().map(|s| s == "app").unwrap_or(false);
    let direct = entries(root);
    if let Some(p) = direct.iter().find(|p| is_app(p)) {
        return Some(p.clone());
    }
    // 广度/深度顺序与 pathlib 的 rglob 不完全一致时只影响「多个候选选哪个」，
    // codeload 与 CI 产的包里只有一个 .app。
    fn walk(d: &Path, is_app: &dyn Fn(&Path) -> bool) -> Option<PathBuf> {
        let items: Vec<PathBuf> = std::fs::read_dir(d)
            .map(|it| it.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        for p in &items {
            if is_app(p) {
                return Some(p.clone());
            }
        }
        for p in &items {
            if p.is_dir() {
                if let Some(hit) = walk(p, is_app) {
                    return Some(hit);
                }
            }
        }
        None
    }
    walk(root, &is_app)
}

/// `codesign --verify --deep --strict`。签坏的包换上去，用户下次双击就是
/// 「已损坏」，比不更新更糟，所以这道自检是硬门。
///
/// `codesign` 不存在（非 mac、精简系统）→ 跳过，照 Python 的 `v is None`；
/// 180 秒超时同样按「没验成」跳过（Python 把 TimeoutExpired 归进 v=None）。
fn verify_signature(new_app: &Path) -> Result<(), String> {
    let mut child = match Command::new("codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(new_app)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return Ok(()),
    };
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return Ok(()),
        }
    }
    let out = match child.wait_with_output() {
        Ok(o) => o,
        Err(_) => return Ok(()),
    };
    if out.status.success() {
        return Ok(());
    }
    let text = String::from_utf8_lossy(&out.stderr);
    let first = text.trim().lines().next().unwrap_or("").trim().to_string();
    Err(format!(
        "新包的签名验不过，已放弃更新：{}",
        if first.is_empty() { "未知原因".to_string() } else { first }
    ))
}

/// 起 helper 并**脱离会话**：`/bin/bash script pid app new work`。
///
/// 必须 setsid（`Popen(start_new_session=True)` 的等价物）：本进程 1.5 秒后会
/// `exit(0)`，helper 落在同一个会话/进程组里就会被一起收走，替换永远做不成。
fn spawn_helper(script: &Path, app: &Path, new_app: &Path, work: &Path) -> Result<(), String> {
    let mut cmd = Command::new("/bin/bash");
    cmd.arg(script)
        .arg(std::process::id().to_string())
        .arg(app)
        .arg(new_app)
        .arg(work)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    let child = cmd.spawn().map_err(|e| format!("启动更新程序失败：{e}"))?;
    // 不等它（Popen 的语义）。drop 不会杀子进程；本进程退出后它由 launchd/init 收。
    drop(child);
    Ok(())
}

fn set_mode_755(p: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("启动更新程序失败：{e}"))
    }
    #[cfg(not(unix))]
    {
        let _ = p;
        Ok(())
    }
}

/// `updater.apply_in_place()`：解压 → 验签 → 起 helper → 等自己退出。
///
/// 五道门（`in_place_blocked`）由调用方在进来之前查过 —— 与 `about.update_apply`
/// 的顺序一致：先 reason，再 `suffix == .zip`，然后才进来。
pub fn apply_in_place(pkg: &Path, state_dir: &Path) -> Result<ApplyInfo, String> {
    let app = app_bundle().ok_or_else(|| "找不到 .app 本体".to_string())?;
    let is_zip = pkg
        .extension()
        .map(|s| s.to_string_lossy().to_lowercase() == "zip")
        .unwrap_or(false);
    if !is_zip {
        return Err("这个包不是 zip，没法原地替换（退回打开安装包）".into());
    }

    let work = state_dir
        .join("updates")
        .join(format!("apply-{}", super::util::now_epoch() as i64));
    std::fs::create_dir_all(&work).map_err(|e| format!("解压失败：{e}"))?;
    let new_root = work.join("new");
    extract_zip(pkg, &new_root)?;

    let new_app = find_app(&new_root).ok_or_else(|| "包里没有 .app，像是下错了资产".to_string())?;

    verify_signature(&new_app)?;

    let script = work.join("apply.sh");
    std::fs::write(&script, HELPER).map_err(|e| format!("启动更新程序失败：{e}"))?;
    set_mode_755(&script)?;
    spawn_helper(&script, &app, &new_app, &work)?;

    Ok(ApplyInfo {
        app,
        work,
        hint: APPLY_HINT.to_string(),
    })
}

// ---------------------------------------------------------------- 打开安装包

/// `updater.launch_installer()`：用系统默认方式打开安装包，返回一句给用户看的说明。
pub fn launch_installer(pkg: &Path) -> Result<String, String> {
    let p = pkg.to_string_lossy().to_string();
    let spawned = if cfg!(target_os = "macos") {
        Command::new("open").arg(&p).spawn().map(|_| ())
    } else if cfg!(windows) {
        // `start` 的第一个参数是窗口标题，给个空串才不会把包名当成标题吃掉。
        Command::new("cmd")
            .args(["/C", "start", "", &p])
            .spawn()
            .map(|_| ())
    } else {
        Command::new("xdg-open").arg(&p).spawn().map(|_| ())
    };
    spawned.map_err(|e| format!("无法打开安装包：{e}（文件在 {p}）"))?;
    Ok(if cfg!(target_os = "macos") {
        "已打开安装包，把 Moshan 拖进「应用程序」即可，原有设置与文件不会丢失。"
    } else if cfg!(windows) {
        "安装程序已启动，按提示覆盖安装即可，原有设置与文件不会丢失。"
    } else {
        "已用系统默认程序打开安装包。"
    }
    .to_string())
}

// ---------------------------------------------------------------- 源码包削壳

/// `about._strip_top`：codeload 的 zip 顶层多一层 `moshan-0.3.1/` 壳。
///
/// 语义逐条对齐（对拍要一致）：顶层集合 = 所有**含 `/`** 的条目名第一段，
/// 不是恰好 1 个就原样返回；输出只留 `top/` 开头的**文件**（目录条目丢弃），
/// 顺序与输入一致；名字去掉 `top/` 前缀。原来靠 `/usr/bin/unzip` +
/// `/usr/bin/zip`，Windows 上根本没有这两条路径。
pub(crate) fn strip_top(raw: &[u8]) -> Result<Vec<u8>, String> {
    use std::io::{Cursor, Read, Write};
    let mut src =
        zip::ZipArchive::new(Cursor::new(raw)).map_err(|_| "不是有效的 zip".to_string())?;
    let names: Vec<String> = {
        let mut v = Vec::with_capacity(src.len());
        for i in 0..src.len() {
            let f = src.by_index_raw(i).map_err(|_| "不是有效的 zip".to_string())?;
            v.push(f.name().to_string());
        }
        v
    };
    let tops: std::collections::BTreeSet<&str> = names
        .iter()
        .filter(|n| n.contains('/'))
        .map(|n| n.split('/').next().unwrap_or(""))
        .collect();
    if tops.len() != 1 {
        return Ok(raw.to_vec());
    }
    let top = format!("{}/", tops.iter().next().unwrap_or(&""));
    let mut out = zip::ZipWriter::new(Cursor::new(Vec::<u8>::new()));
    let opts =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (i, name) in names.iter().enumerate() {
        if !name.starts_with(&top) || name.ends_with('/') {
            continue;
        }
        let mut buf = Vec::new();
        {
            let mut f = src.by_index(i).map_err(|_| "不是有效的 zip".to_string())?;
            f.read_to_end(&mut buf).map_err(|_| "重打包失败".to_string())?;
        }
        out.start_file(&name[top.len()..], opts)
            .map_err(|_| "重打包失败".to_string())?;
        out.write_all(&buf).map_err(|_| "重打包失败".to_string())?;
    }
    let cur = out.finish().map_err(|_| "重打包失败".to_string())?;
    Ok(cur.into_inner())
}

// ---------------------------------------------------------------- 清理

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).ok()?.modified().ok()
}

/// `updater.cleanup(keep)`：`updates/` 下旧包与 `apply-*` 目录各留最近 keep 份。
/// 返回删除数量；删不动的静默跳过（Python 里也是 `except OSError: pass`）。
pub fn cleanup(keep: usize) -> usize {
    cleanup_in(&crate::paths::state_dir().join("updates"), keep)
}

/// 同 `cleanup`，但目录由调用方给 —— 测试要在一个临时目录上跑，
/// 不去碰真实的 state 目录（那里面有正在下载的包）。
pub(crate) fn cleanup_in(d: &Path, keep: usize) -> usize {
    if !d.is_dir() {
        return 0;
    }
    let mut removed = 0usize;
    let mut files: Vec<(PathBuf, SystemTime)> = Vec::new();
    let mut dirs: Vec<(PathBuf, SystemTime)> = Vec::new();
    if let Ok(it) = std::fs::read_dir(d) {
        for e in it.flatten() {
            let p = e.path();
            let Some(mt) = mtime(&p) else { continue };
            if p.is_dir() {
                if p.file_name()
                    .map(|n| n.to_string_lossy().starts_with("apply-"))
                    .unwrap_or(false)
                {
                    dirs.push((p, mt));
                }
            } else {
                // Python 比的是 `f.suffix`，大小写敏感（"A.ZIP" 不算）。
                let ext = p.extension().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                if matches!(ext.as_str(), "dmg" | "exe" | "zip" | "gz") {
                    files.push((p, mt));
                }
            }
        }
    }
    files.sort_by(|a, b| b.1.cmp(&a.1));
    dirs.sort_by(|a, b| b.1.cmp(&a.1));
    for (p, _) in files.iter().skip(keep) {
        if std::fs::remove_file(p).is_ok() {
            let _ = std::fs::remove_file(super::util::append_suffix(p, ".meta"));
            removed += 1;
        }
    }
    for (p, _) in dirs.iter().skip(keep) {
        if std::fs::remove_dir_all(p).is_ok() {
            removed += 1;
        }
    }
    removed
}
