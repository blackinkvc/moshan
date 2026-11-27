// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：缩略图 / 图片缩放 / heic→jpg 的三平台实现，替代散在路由里的 `sips`。
// 不做：不引 Pillow 之类的解码库，不联网，不改原图。
//
// 分层：
//   macOS               系统自带 `sips`，行为与 Python（`server._make_thumb`、
//                       `ledger.replace_image`、`linkbox`）逐参数一致。
//   Windows / Linux     先探测 `magick` / `convert`（ImageMagick）、`ffmpeg`，
//                       有就用，都没有就返回 false —— 调用方退回原图。
//                       **Python 在这些平台同样没有实现**（`_make_thumb` 开头就是
//                       `if sys.platform != "darwin": return None`），所以这里是
//                       「比 Python 好一点」，而不是必须。
//
// 缩略图缓存目录与键沿用 Python 的 `~/.cache/pocket-thumbs` + sha1(路径|mtime_ns|size|宽)，
// 两边共用同一份磁盘缓存（对拍时 Python 先生成的，Rust 直接命中）。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// 起子进程并等它结束；超时就 kill。返回 `(是否成功, 是否超时)` —— 调用方只关心成功与否。
fn run(argv: &[&str], timeout_secs: u64) -> bool {
    if argv.is_empty() {
        return false;
    }
    let mut cmd = Command::new(argv[0]);
    cmd.args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let Ok(mut child) = cmd.spawn() else { return false };
    let start = Instant::now();
    let limit = Duration::from_secs(timeout_secs);
    loop {
        match child.try_wait() {
            Ok(Some(st)) => return st.success(),
            Ok(None) => {
                if start.elapsed() >= limit {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
                std::thread::sleep(Duration::from_millis(40));
            }
            Err(_) => {
                let _ = child.kill();
                return false;
            }
        }
    }
}

/// PATH 里有没有这个可执行文件（Windows 也认 PATHEXT，由 `Command` 自己处理）。
#[cfg(not(target_os = "macos"))]
fn have(bin: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else { return false };
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".into())
            .split(';')
            .map(|s| s.to_ascii_lowercase())
            .collect()
    } else {
        vec![String::new()]
    };
    for dir in std::env::split_paths(&path) {
        for ext in &exts {
            if dir.join(format!("{bin}{ext}")).is_file() {
                return true;
            }
        }
    }
    false
}

fn nonempty(p: &Path) -> bool {
    std::fs::metadata(p).map(|m| m.is_file() && m.len() > 0).unwrap_or(false)
}

/// 与 Python 的 `THUMB_DIR = Path.home() / ".cache" / "pocket-thumbs"` 同位置。
/// 注意：storage.rs 里那份在非 mac 上退到 `%LOCALAPPDATA%`，那是**它自己的**口径；
/// 这里刻意跟随 Python（三平台都是 ~/.cache），缓存键才对得上。
fn thumb_dir() -> PathBuf {
    if let Some(v) = std::env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(v).join("pocket-thumbs");
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".cache").join("pocket-thumbs")
}

fn thumb_target(p: &Path, md: &std::fs::Metadata, w: u32) -> PathBuf {
    use sha1::{Digest, Sha1};
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let key = format!("{}|{}|{}|{}", p.to_string_lossy(), mtime, md.len(), w);
    let mut h = Sha1::new();
    h.update(key.as_bytes());
    let mut hex = String::with_capacity(40);
    for b in h.finalize() {
        hex.push_str(&format!("{:02x}", b));
    }
    thumb_dir().join(format!("{hex}.jpg"))
}

/// `server._make_thumb` 的等价物：有缓存就命中，没有就缩一张落盘。
/// 返回 None 表示这个平台缩不出来，调用方退回原图（与 Python 非 mac 的行为一致）。
pub fn thumb(p: &Path, w: u32) -> Option<PathBuf> {
    let md = std::fs::metadata(p).ok()?;
    let out = thumb_target(p, &md, w);
    if nonempty(&out) {
        return Some(out);
    }
    std::fs::create_dir_all(thumb_dir()).ok()?;
    // 质量 60 与 Python 的第一条 sips 命令一致；第二条是不带 format 的兜底。
    // `make_thumb_q` 自己先写 `.<名字>.<pid>.tmp` 再改名，失败不留残片。
    if make_thumb_q(p, &out, w, 60) {
        return Some(out);
    }
    None
}

/// 把 `src` 缩到最长边不超过 `max_side`，写到 `dst`（调用方给 .jpg 就用 JPEG）。
/// 成功返回 true；失败时 **不留下半截文件**，`dst` 保持调用前的样子。
pub fn make_thumb(src: &Path, dst: &Path, max_side: u32) -> bool {
    make_thumb_q(src, dst, max_side, 0)
}

/// `quality = 0` 表示不传 `formatOptions`（sips 用默认质量 / ImageMagick 用 85）。
pub fn make_thumb_q(src: &Path, dst: &Path, max_side: u32, quality: u8) -> bool {
    if max_side == 0 {
        return false;
    }
    let tmp = tmp_beside(dst);
    let ok = scale_into(src, &tmp, max_side, quality);
    if ok && nonempty(&tmp) {
        // Windows 的 rename 不覆盖已存在的目标；先删再改名（Python 用 os.replace）。
        let _ = std::fs::remove_file(dst);
        if std::fs::rename(&tmp, dst).is_ok() {
            return true;
        }
    }
    let _ = std::fs::remove_file(&tmp);
    false
}

fn tmp_beside(dst: &Path) -> PathBuf {
    let name = dst
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "img".into());
    let dir = dst.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from("."));
    dir.join(format!(".{name}.{}.tmp", std::process::id()))
}

#[cfg(target_os = "macos")]
fn scale_into(src: &Path, dst: &Path, max_side: u32, quality: u8) -> bool {
    let ws = max_side.to_string();
    let qs = quality.to_string();
    let src_s = src.to_string_lossy().to_string();
    let dst_s = dst.to_string_lossy().to_string();
    // 与 Python / 原 ledger 代码同序：先试着强制 JPEG，某些格式不吃这个参数再退回纯 -Z。
    let with_q = [
        "sips", "-Z", &ws, "-s", "format", "jpeg", "-s", "formatOptions", &qs, &src_s, "--out", &dst_s,
    ];
    let no_q = ["sips", "-Z", &ws, "-s", "format", "jpeg", &src_s, "--out", &dst_s];
    let plain = ["sips", "-Z", &ws, &src_s, "--out", &dst_s];
    let variants: Vec<&[&str]> = if quality > 0 {
        vec![&with_q, &plain]
    } else {
        vec![&no_q, &plain]
    };
    for v in variants {
        if run(v, 30) && nonempty(dst) {
            return true;
        }
        let _ = std::fs::remove_file(dst);
    }
    false
}

#[cfg(not(target_os = "macos"))]
fn scale_into(src: &Path, dst: &Path, max_side: u32, quality: u8) -> bool {
    let src_s = src.to_string_lossy().to_string();
    let dst_s = dst.to_string_lossy().to_string();
    let box_s = format!("{max_side}x{max_side}");
    let q = if quality == 0 { 85 } else { quality }.to_string();
    let scale = format!("scale='min({max_side},iw)':-2");
    // 顺序：ImageMagick 7 → ImageMagick 6 → ffmpeg。都没有就 false（Python 同样没有实现）。
    let attempts: Vec<Vec<String>> = vec![
        vec!["magick".into(), src_s.clone(), "-auto-orient".into(), "-resize".into(), box_s.clone(), "-quality".into(), q.clone(), dst_s.clone()],
        vec!["convert".into(), src_s.clone(), "-auto-orient".into(), "-resize".into(), box_s, "-quality".into(), q, dst_s.clone()],
        vec!["ffmpeg".into(), "-y".into(), "-loglevel".into(), "error".into(), "-i".into(), src_s, "-vf".into(), scale, "-frames:v".into(), "1".into(), dst_s],
    ];
    for a in attempts {
        if !have(&a[0]) {
            continue;
        }
        let argv: Vec<&str> = a.iter().map(|s| s.as_str()).collect();
        if run(&argv, 30) && nonempty(dst) {
            return true;
        }
        let _ = std::fs::remove_file(dst);
    }
    false
}

/// heic/heif → jpg（`linkbox._move_images` 那一步）。
/// mac 用 sips 转；其它平台返回 false —— 调用方本来就把「转不出来」当跳过处理。
/// Python 也是只认 `/usr/bin/sips`（`linkbox.SIPS`），没有第二套实现。
pub fn heic_to_jpg(src: &Path, dst: &Path) -> bool {
    #[cfg(target_os = "macos")]
    {
        let src_s = src.to_string_lossy().to_string();
        let dst_s = dst.to_string_lossy().to_string();
        let argv = [
            "/usr/bin/sips",
            "-s",
            "format",
            "jpeg",
            "-s",
            "formatOptions",
            "85",
            &src_s,
            "--out",
            &dst_s,
        ];
        return run(&argv, 60) && nonempty(dst);
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (src, dst);
        false
    }
}
