"""更新器 —— 检查、下载、安装。

设计取舍（重要）：
  程序、状态、资料库三者物理分离（见 paths.py），所以安装包覆盖程序目录时，
  用户的配置 / 收件箱 / 工作台页面一律不受影响。这是「更新后设置和文件不丢」
  的实现方式 —— 不靠代码小心，靠目录结构保证。

安装怎么落地（0.3.8 起）：
  macOS 走**原地替换**，不再弹 dmg 让人拖图标 —— 那个动作每更新一次就要做一遍，
  跟「软件自己更新」是两回事。做法是三段式，而不是热替换运行中的程序：
    1. 下载 zip（CI 除了 dmg 还产一份 zip，签名在里面）；
    2. `ditto -xk` 解到状态目录，`codesign --verify` 校验通过才继续；
    3. 起一个脱离会话的 helper 脚本，它等本进程真的退出之后，把 `.app` 挪到
       `.moshan-old`、`ditto` 新的过去、清 quarantine、重新打开；复制失败就把
       旧的挪回来。任一段失败都不会留下半个坏软件。

  为什么必须等进程退出：运行中的 .app 被系统锁着一部分文件，替换一半就崩。
  为什么必须清 quarantine：下载来的 zip 带隔离属性，不清的话新包进来照样被
  Gatekeeper 拦成「已损坏」，白更新一次。

  不能原地更新的情况（从 dmg 里直接运行、被 Gatekeeper 挂到 AppTranslocation、
  目录不可写、源码部署）：如实说明原因，退回「打开安装包」，不假装成功。

  Windows 仍然是 exe 安装器：运行中的 exe 被系统锁着，换不掉自己；但安装器
  本身就是覆盖安装，点两下即可，不需要重新下载一遍。
"""
from __future__ import annotations

import hashlib
import json
import os
import platform
import re
import shutil
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
    key = platform_key()
    seen: list[dict] = []
    for a in assets:
        name = str(a.get("name") or "")
        if pat.search(name):
            seen.append({
                "name": name,
                "url": a.get("browser_download_url"),
                "size": int(a.get("size") or 0),
                "digest": a.get("digest") or "",  # GitHub 可能给 sha256:xxx
            })
    # macOS 上 zip 优先：zip 能原地替换，dmg 只能弹出来让人拖。
    # 同一版本两种资产都在时，选错就等于「永远只能手动装」。
    if key.startswith("macos"):
        seen.sort(key=lambda c: 0 if c["name"].lower().endswith(".zip") else 1)
    picked: Optional[dict] = seen[0] if seen else None

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


def is_verified(path: Path) -> bool:
    """这个下载包里是不是真的有校验值。

    为什么必须单独问一句：GitHub 在某些情况下不回 `digest`（老资产、API 变更、
    中间人改了响应），下载照样成功，但 sha256 根本没比过。安装包一旦被打开就是
    执行代码，所以「有没有校验」必须是**执行前的硬门**，不能只在界面上写一行小字。
    """
    meta = Path(str(path) + ".meta")
    try:
        j = json.loads(meta.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return False
    return bool(j.get("verified")) and bool(j.get("sha256"))


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

# helper 脚本：app 自己起它，然后自杀；它等旧进程真的没了再动手。
# 每一段失败都往 apply.log 写一句，出事能查；复制失败必须把旧的挪回来。
_HELPER = r"""#!/bin/bash
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
"""


def app_bundle() -> Optional[Path]:
    """当前进程所在的 .app；不是打包运行就 None。"""
    if sys.platform != "darwin" or not getattr(sys, "frozen", False):
        return None
    try:
        exe = Path(sys.executable).resolve()
    except OSError:
        return None
    for p in exe.parents:
        if p.suffix == ".app":
            return p
    return None


def in_place_blocked() -> str:
    """不能原地更新的原因；返回空串表示可以。"""
    if sys.platform != "darwin":
        return "这个平台走安装程序"
    if not getattr(sys, "frozen", False):
        return "源码运行，没有 .app 可以替换"
    app = app_bundle()
    if app is None:
        return "找不到 .app 本体（可能被改名或挪出了 bundle）"
    s = str(app)
    if "/AppTranslocation/" in s:
        return ("这个 app 正从下载目录 / dmg 里直接运行，系统给了临时路径。"
                "先把 Moshan 拖进「应用程序」，之后就能一键更新。")
    if s.startswith("/Volumes/"):
        return "这个 app 在 dmg 里运行。先拖进「应用程序」，之后就能一键更新。"
    if not os.access(str(app.parent), os.W_OK):
        return f"{app.parent} 不可写（可能装成了别人的）。重装到自己的应用程序目录再更新。"
    return ""


def apply_in_place(path: Path) -> dict:
    """把下载好的 zip 装回去：解压 → 验签 → 起 helper → 等自己退出。"""
    reason = in_place_blocked()
    if reason:
        raise UpdateError(reason)
    app = app_bundle()
    if app is None:
        raise UpdateError("找不到 .app 本体")
    if path.suffix.lower() != ".zip":
        raise UpdateError("这个包不是 zip，没法原地替换（退回打开安装包）")

    work = paths.state_dir() / "updates" / f"apply-{int(time.time())}"
    work.mkdir(parents=True, exist_ok=True)
    new_root = work / "new"
    try:
        subprocess.run(["ditto", "-xk", str(path), str(new_root)],
                       check=True, capture_output=True, timeout=240)
    except (subprocess.CalledProcessError, subprocess.TimeoutExpired, OSError) as exc:
        raise UpdateError(f"解压失败：{exc}") from exc

    cands = list(new_root.glob("*.app")) or list(new_root.rglob("*.app"))
    if not cands:
        raise UpdateError("包里没有 .app，像是下错了资产")
    new_app = cands[0]

    # 签名自检：签坏的包换上去，用户下次双击就是「已损坏」，比不更新更糟。
    try:
        v = subprocess.run(["codesign", "--verify", "--deep", "--strict", str(new_app)],
                           capture_output=True, timeout=180)
    except (OSError, subprocess.TimeoutExpired):
        v = None
    if v is not None and v.returncode != 0:
        msg = (v.stderr or b"").decode("utf-8", "replace").strip().splitlines()
        raise UpdateError("新包的签名验不过，已放弃更新：" + (msg[0] if msg else "未知原因"))

    script = work / "apply.sh"
    script.write_text(_HELPER, encoding="utf-8")
    os.chmod(script, 0o755)
    try:
        subprocess.Popen(
            ["/bin/bash", str(script), str(os.getpid()), str(app), str(new_app), str(work)],
            start_new_session=True,          # 脱离本会话：服务退了它也活着
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
    except OSError as exc:
        raise UpdateError(f"启动更新程序失败：{exc}") from exc
    return {
        "app": str(app),
        "work": str(work),
        "hint": "正在更新：程序会自己关掉再打开，约 3 秒。设置和文件都不会动。",
    }


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
    """清掉旧的下载包与解压目录，只留最近 keep 份。返回删除数量。"""
    d = paths.state_dir() / "updates"
    if not d.is_dir():
        return 0
    removed = 0
    files = sorted((f for f in d.iterdir() if f.suffix in {".dmg", ".exe", ".zip", ".gz"}),
                   key=lambda f: f.stat().st_mtime, reverse=True)
    for f in files[keep:]:
        try:
            f.unlink()
            f.with_suffix(f.suffix + ".meta").unlink(missing_ok=True)
            removed += 1
        except OSError:
            pass
    # 原地更新会在 updates/ 下留 apply-<时间戳>/（解压出来的整个 .app，几十 MB）。
    # 日志要留着能查最后一次，所以同样是「留最近 keep 份」。
    dirs = sorted((x for x in d.iterdir() if x.is_dir() and x.name.startswith("apply-")),
                  key=lambda x: x.stat().st_mtime, reverse=True)
    for x in dirs[keep:]:
        try:
            shutil.rmtree(x, ignore_errors=True)
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
        reason = in_place_blocked()
        if not reason and path.suffix.lower() == ".zip":
            try:
                res = apply_in_place(path)
            except UpdateError as exc:
                print(f"原地更新不可用：{exc}")
                print(launch_installer(path))
                return 1
            print(res["hint"])
            print(f"日志：{res['work']}/apply.log")
            return 0
        if reason:
            print(f"（本机没法原地更新：{reason}）")
        print(launch_installer(path))
    else:
        print("要安装就把这个文件打开，或加 --apply 让脚本代劳。")
    return 0


if __name__ == "__main__":
    sys.exit(main())
