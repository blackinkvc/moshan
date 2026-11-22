"""更新器 —— 检查、下载、引导安装。

设计取舍（重要）：
  程序、状态、资料库三者物理分离（见 paths.py），所以安装包覆盖程序目录时，
  用户的配置 / 收件箱 / 工作台页面一律不受影响。这是「更新后设置和文件不丢」
  的实现方式 —— 不靠代码小心，靠目录结构保证。

为什么不尝试热替换运行中的程序：
  macOS 上 .app 正在运行时无法安全替换自身，Windows 上运行中的 exe 被锁。
  强行做需要「拉起新进程→旧进程自杀→回滚」三段式，任一段失败就是半个坏软件。
  首版采用「下载 → 校验 → 打开安装包」，由系统安装流程负责替换，失败也不会破坏
  现有安装；等程序拆成 onedir + current 符号链接后再上原子切换。
"""
from __future__ import annotations

import hashlib
import json
import os
import platform
import re
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Callable, Optional

import paths
import version

API_LATEST = "https://api.github.com/repos/{repo}/releases/latest"
USER_AGENT = "Moshan-Updater/0.1 (+https://github.com/{repo})"
CHUNK = 256 * 1024


class UpdateError(Exception):
    """更新流程中可预期的失败（网络、校验、磁盘）。"""


# ---------------------------------------------------------------- 平台标识

def platform_key() -> str:
    """与 CI 产出的资产命名保持一致：macos-arm64 / macos-x64 / windows-x64。"""
    if sys.platform == "darwin":
        return "macos-arm64" if platform.machine() in ("arm64", "aarch64") else "macos-x64"
    if os.name == "nt":
        return "windows-x64" if platform.machine().endswith("64") else "windows-x86"
    return "linux-x64"


def _asset_pattern(key: str) -> re.Pattern:
    """在 Release 资产名里找当前平台对应的那个。

    忽略大小写必须用 re.IGNORECASE 标志，不能写内联 (?i)：
    Python 只允许 (?i) 出现在表达式最开头，`a|(?i)b` 这种写法会直接抛
    re.error: global flags not at the start of the expression。
    这个坑真实踩过 —— 检查更新对所有人都是 500，直到有人在断网环境里试才发现。
    """
    if key.startswith("macos"):
        arch = "arm64" if key.endswith("arm64") else "x64"
        return re.compile(
            rf"macos.*{arch}.*\.(dmg|zip)$|{arch}.*macos.*\.(dmg|zip)$", re.IGNORECASE)
    if key.startswith("windows"):
        return re.compile(r"windows.*(setup|installer).*\.exe$|setup.*\.exe$", re.IGNORECASE)
    return re.compile(r"linux.*\.(tar\.gz|zip)$", re.IGNORECASE)


# ---------------------------------------------------------------- HTTP

def _request(url: str, *, repo: str, timeout: float = 10) -> urllib.request.Request:
    return urllib.request.Request(
        url,
        headers={
            "User-Agent": USER_AGENT.format(repo=repo or version.DEFAULT_REPO),
            "Accept": "application/vnd.github+json",
        },
    )


def _read_json(url: str, *, repo: str, timeout: float = 10) -> dict:
    try:
        with urllib.request.urlopen(_request(url, repo=repo, timeout=timeout), timeout=timeout) as resp:
            return json.loads(resp.read().decode("utf-8"))
    except urllib.error.HTTPError as exc:
        if exc.code == 404:
            raise UpdateError("仓库或 Release 不存在（可能还没发布版本）") from exc
        if exc.code == 403:
            raise UpdateError("GitHub API 限流，请稍后再试") from exc
        raise UpdateError(f"检查更新失败：HTTP {exc.code}") from exc
    except (urllib.error.URLError, TimeoutError, OSError) as exc:
        raise UpdateError(f"网络不可用：{exc}") from exc


# ---------------------------------------------------------------- 检查

def check(repo: str = "", *, timeout: float = 10) -> dict:
    """查询最新发布。

    返回 dict：
      has_update  bool
      current     当前版本
      latest      远端版本
      notes       更新说明（截断）
      asset       {name, url, size} 或 None（当前平台没有产物）
      page        Release 页面地址
    """
    repo = repo or version.DEFAULT_REPO
    data = _read_json(API_LATEST.format(repo=repo), repo=repo, timeout=timeout)

    tag = str(data.get("tag_name") or "").lstrip("vV").strip()
    latest = tag or "0.0.0"
    assets = data.get("assets") or []

    pat = _asset_pattern(platform_key())
    picked: Optional[dict] = None
    for a in assets:
        name = str(a.get("name") or "")
        if pat.search(name):
            picked = {
                "name": name,
                "url": a.get("browser_download_url"),
                "size": int(a.get("size") or 0),
                "digest": a.get("digest") or "",  # GitHub 可能给 sha256:xxx
            }
            break

    return {
        "has_update": version.is_newer(latest),
        "current": version.VERSION,
        "latest": latest,
        "notes": (data.get("body") or "")[:4000],
        "asset": picked,
        "page": data.get("html_url") or f"https://github.com/{repo}/releases",
        "platform": platform_key(),
    }


# ---------------------------------------------------------------- 下载

def _sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as fh:
        for block in iter(lambda: fh.read(CHUNK), b""):
            h.update(block)
    return h.hexdigest()


def download(asset: dict, *, on_progress: Optional[Callable[[int, int], None]] = None,
             timeout: float = 30) -> Path:
    """下载资产到 state/updates/，边下边算 sha256，完成后校验。"""
    url = asset.get("url")
    name = asset.get("name") or "moshan-update.bin"
    if not url:
        raise UpdateError("该版本没有可下载的安装包")

    dest_dir = paths.state_dir() / "updates"
    dest_dir.mkdir(parents=True, exist_ok=True)
    target = dest_dir / name
    partial = target.with_suffix(target.suffix + ".part")

    expected = (asset.get("digest") or "").replace("sha256:", "").strip().lower()
    total = int(asset.get("size") or 0)
    started = time.time()
    hasher = hashlib.sha256()

    try:
        req = urllib.request.Request(url, headers={"User-Agent": USER_AGENT.format(repo=version.DEFAULT_REPO)})
        with urllib.request.urlopen(req, timeout=timeout) as resp, partial.open("wb") as out:
            declared = int(resp.headers.get("Content-Length") or total or 0)
            done = 0
            while True:
                block = resp.read(CHUNK)
                if not block:
                    break
                out.write(block)
                hasher.update(block)
                done += len(block)
                if on_progress:
                    on_progress(done, declared)
    except (urllib.error.URLError, TimeoutError, OSError) as exc:
        partial.unlink(missing_ok=True)
        raise UpdateError(f"下载中断：{exc}") from exc

    got = hasher.hexdigest()
    verified = bool(expected)
    if expected and got != expected:
        partial.unlink(missing_ok=True)
        raise UpdateError(f"校验失败（期望 {expected[:12]}… 实际 {got[:12]}…），已丢弃")

    elapsed = max(time.time() - started, 0.001)
    size = partial.stat().st_size
    partial.replace(target)
    target.with_suffix(target.suffix + ".meta").write_text(
        json.dumps({"name": name, "size": size, "sha256": got,
                    "verified": verified,
                    "seconds": round(elapsed, 1),
                    "rate_mb_s": round(size / elapsed / 1048576, 2)},
                   ensure_ascii=False, indent=2),
        encoding="utf-8",
    )
    return target


# ---------------------------------------------------------------- 安装

def launch_installer(path: Path) -> str:
    """用系统默认方式打开安装包，返回一句给用户看的说明。"""
    p = str(path)
    try:
        if sys.platform == "darwin":
            subprocess.Popen(["open", p])
            return "已打开安装包，把 Moshan 拖进「应用程序」即可，原有设置与文件不会丢失。"
        if os.name == "nt":
            os.startfile(p)  # type: ignore[attr-defined]
            return "安装程序已启动，按提示覆盖安装即可，原有设置与文件不会丢失。"
        subprocess.Popen(["xdg-open", p])
        return "已用系统默认程序打开安装包。"
    except OSError as exc:
        raise UpdateError(f"无法打开安装包：{exc}（文件在 {p}）") from exc


def cleanup(keep: int = 2) -> int:
    """清掉旧的下载包，只留最近 keep 个。返回删除数量。"""
    d = paths.state_dir() / "updates"
    if not d.is_dir():
        return 0
    files = sorted((f for f in d.iterdir() if f.suffix in {".dmg", ".exe", ".zip", ".gz"}),
                   key=lambda f: f.stat().st_mtime, reverse=True)
    removed = 0
    for f in files[keep:]:
        try:
            f.unlink()
            f.with_suffix(f.suffix + ".meta").unlink(missing_ok=True)
            removed += 1
        except OSError:
            pass
    return removed


# ---------------------------------------------------------------- 命令行
#
# 界面里点「检查更新」走的是同一套函数；这里只是给它一个能在终端跑的壳，
# 方便发版后自测、或在没有图形界面的机器上确认更新源是否可达。
#
#   python moshan/updater.py --check       只看有没有新版
#   python moshan/updater.py --download    下载并校验，不安装
#   python moshan/updater.py --apply       下载、校验、唤起安装包

def main(argv: Optional[list] = None) -> int:
    import argparse

    ap = argparse.ArgumentParser(
        prog="updater", description="检查 / 下载 / 安装 Moshan 更新")
    ap.add_argument("--check", action="store_true", help="只检查是否有新版本（默认行为）")
    ap.add_argument("--download", action="store_true", help="下载并校验，不安装")
    ap.add_argument("--apply", action="store_true", help="下载、校验并打开安装包")
    ap.add_argument("--repo", default="", help="覆盖更新源，形如 owner/name")
    ap.add_argument("--timeout", type=float, default=10.0, help="网络超时秒数")
    args = ap.parse_args(argv)

    print(f"当前版本 {version.VERSION}   平台 {platform_key()}")
    try:
        info = check(args.repo, timeout=args.timeout)
    except UpdateError as exc:
        print(f"检查失败：{exc}")
        return 1

    if not info.get("has_update"):
        print(f"已是最新（远端 {info.get('latest') or '未知'}）")
        return 0

    asset = info.get("asset") or {}
    print(f"发现新版本 {info['latest']}（当前 {version.VERSION}）")
    if asset:
        size = int(asset.get("size") or 0)
        print(f"  安装包 {asset.get('name')}  {size / 1048576:.1f} MB")
    if info.get("notes"):
        print("\n更新说明：")
        print("\n".join(f"  {ln}" for ln in str(info["notes"]).splitlines()[:20]))

    if not (args.download or args.apply):
        print("\n加 --download 下载，加 --apply 直接下载并安装。")
        return 0

    if not asset:
        print("这个版本没有当前平台的安装包。")
        return 1

    def on_progress(done: int, total: int) -> None:
        if total <= 0:
            return
        pct = done * 100 // total
        bar = "#" * (pct // 4)
        sys.stdout.write(f"\r  [{bar:<25}] {pct:3d}%  {done / 1048576:.1f}/{total / 1048576:.1f} MB")
        sys.stdout.flush()

    try:
        path = download(asset, on_progress=on_progress)
    except UpdateError as exc:
        print(f"\n下载失败：{exc}")
        return 1
    print(f"\n已下载并校验通过：{path}")

    if args.apply:
        print(launch_installer(path))
    else:
        print("要安装就把这个文件打开，或加 --apply 让脚本代劳。")
    return 0


if __name__ == "__main__":
    sys.exit(main())
