// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：把「用系统默认程序打开某个路径 / 网址」收敛成三平台一份实现。
// 不做：不判断路径是否存在、不创建目录、不等待子进程退出 —— 调用方各自保留
//      原来的容错形状（有的吞错，有的回 JSON）。
//
// 三平台的分工：
//   macOS   `open <p>` / `open -R <p>`（Finder 里选中）/ `open <url>`
//   Windows `cmd /C start "" <p>`（`start` 的第一个引号参数是窗口标题，必须给空串，
//           否则带空格的路径会被当成标题吞掉）/ `explorer /select,<p>` / 同上 start
//   Linux   `xdg-open <p>`；没有 `-R` 的等价物，定位退回打开父目录
//
// Python 那边（`moshan/server.py`）有三处直接写死 `["open", ...]`，在 Windows 上
// 本来就是坏的。这里做到比 Python 好，但**返回值/错误形状仍与调用方原来的口径一致**。

use std::path::Path;
use std::process::{Command, Stdio};

/// 起一个不占终端的子进程。只关心「起没起来」，不等它退出。
fn spawn(mut cmd: Command) -> std::io::Result<()> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
}

/// 用默认程序打开路径（目录即文件管理器）。
pub fn open_path(p: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let mut c = Command::new("open");
        c.arg(p);
        return spawn(c);
    }
    #[cfg(windows)]
    {
        let mut c = Command::new("cmd");
        // 空串是 start 的「窗口标题」占位，缺了它第一个带引号的参数会被当标题。
        c.args(["/C", "start", ""]);
        c.arg(p);
        return spawn(c);
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let mut c = Command::new("xdg-open");
        c.arg(p);
        return spawn(c);
    }
}

/// 在文件管理器里「定位并选中」这个路径。
/// Linux 的 xdg-open 没有 `-R`，只能打开父目录 —— 这是明确的降级。
///
/// 目前仓库里没有「定位到某个文件」的路由（Python 也没有，`/api/reveal` 只是
/// 打开数据根/状态目录），所以这里暂时没有调用点。前端要做「在文件夹中显示」
/// 时直接接它，别再各写一遍平台分支。
#[allow(dead_code)]
pub fn reveal_path(p: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let mut c = Command::new("open");
        c.arg("-R").arg(p);
        return spawn(c);
    }
    #[cfg(windows)]
    {
        let mut c = Command::new("explorer");
        // explorer 的 `/select,<path>` 必须是**一个**参数，逗号后不能有空格。
        c.arg(format!("/select,{}", p.display()));
        return spawn(c);
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let dir = p.parent().unwrap_or(p);
        let mut c = Command::new("xdg-open");
        c.arg(dir);
        return spawn(c);
    }
}

/// 用默认浏览器打开网址。启用了 `MOSHAN_OPEN` 时由 `boot.rs` 调用。
pub fn open_url(url: &str) {
    #[cfg(target_os = "macos")]
    {
        let mut c = Command::new("open");
        c.arg(url);
        let _ = spawn(c);
        return;
    }
    #[cfg(windows)]
    {
        let mut c = Command::new("cmd");
        c.args(["/C", "start", "", url]);
        let _ = spawn(c);
        return;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        let _ = spawn(c);
        return;
    }
}
