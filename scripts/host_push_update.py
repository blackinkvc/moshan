#!/usr/bin/env python3
"""主机把 Moshan 程序包推给一台被控机（不走 GitHub）。

两台机器的两种处境，两种送法：

  ① 同局域网、能直连（EVO 这种）
       python3 scripts/host_push_update.py --http http://192.168.0.103:8321

  ② 只能被它主动连出来（nucbox 那种：没有 SMB/RDP/WinRM，入站全封）
       python3 scripts/host_push_update.py --fleet evox2 --port 8321
     走虫巢：分片 put 到目标机 → 目标机上跑一段小程序在**本机**回环 POST 给中台。
     这样「入站一条路都没有」的机器也能被主机更新。

为什么不用 GitHub
     办公室电脑连不上 github.com（实测 8 秒超时）。更新权在主机：谁有包谁推送，
     被更新的机器不需要外网，也不需要知道 GitHub 是什么。

包内容
     只打 `moshan/` 目录（程序源码 + 页面 + 证书），**不打用户数据**：
     数据在状态目录与资料库里，更新和它们无关。
"""
from __future__ import annotations

import argparse
import base64
import glob
import hashlib
import json
import os
import re
import ssl
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SRC = ROOT / "moshan"
CHUNK = 1500 * 1024          # 虫巢 put 单请求 4 MB 上限，base64 后约 2 MB，留足余量
SKIP = {"__pycache__", ".git", ".idea", ".vscode"}

# 白名单跟着服务端走（pushupd.ALLOW_SUFFIX），两边各写一份迟早漂移：
# 客户端打进来的东西服务端拒收，看起来像「推送失败」其实是口径不一致。
sys.path.insert(0, str(SRC))
import pushupd  # noqa: E402

ALLOW = pushupd.ALLOW_SUFFIX


def version() -> str:
    m = re.search(r'VERSION\s*=\s*"([^"]+)"', (SRC / "version.py").read_text(encoding="utf-8"))
    return m.group(1) if m else "0.0.0"


def make_package(version_hint: str = "") -> tuple[Path, str, str]:
    """把 moshan/ 打成 zip，返回 (路径, sha256, 版本)。"""
    ver = version_hint or version()
    fd, tmp = tempfile.mkstemp(suffix=f"-moshan-{ver}.zip")
    os.close(fd)
    out = Path(tmp)
    n = 0
    with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as zf:
        for p in sorted(SRC.rglob("*")):
            if p.is_dir() or p.name.startswith("."):
                continue
            if any(x in SKIP for x in p.relative_to(SRC).parts):
                continue
            # 只打程序文件：moshan/ 里偶尔会混进采集产生的 *.jsonl，那不是程序
            if p.suffix.lower() not in ALLOW:
                continue
            zf.write(p, p.relative_to(SRC).as_posix())
            n += 1
    raw = out.read_bytes()
    return out, hashlib.sha256(raw).hexdigest(), ver


def _request(url: str, body: dict | None = None, token: str = "", timeout: float = 600,
             insecure: bool = False):
    data = json.dumps(body).encode() if body is not None else None
    headers = {"Content-Type": "application/json"}
    if token:
        headers["X-Token"] = token
    req = urllib.request.Request(url, data=data, headers=headers,
                                 method="POST" if data else "GET")
    ctx = ssl._create_unverified_context() if insecure else None
    with urllib.request.urlopen(req, context=ctx, timeout=timeout) as r:
        return json.load(r)


def push_http(zip_path: Path, sha: str, ver: str, url: str, token: str,
              restart: bool, insecure: bool) -> dict:
    raw = zip_path.read_bytes()
    body = {"version": ver, "sha256": sha, "name": zip_path.name,
            "b64": base64.b64encode(raw).decode(), "restart": restart}
    return _request(url.rstrip("/") + "/api/update/push", body, token,
                    timeout=900, insecure=insecure)


# --------------------------------------------------------------------------- 虫巢


def _fleet(args):
    """返回 (post, wait) 两个函数；没有虫巢库依赖，直接走它的 HTTP + sqlite。"""
    base = args.fleet_base
    token = args.fleet_token or os.environ.get("NEST_TOKEN", "")
    db = os.path.expanduser(args.fleet_db)
    files = os.path.expanduser(args.fleet_files)
    ctx = ssl._create_unverified_context()

    def post(path: str, body: dict) -> dict:
        req = urllib.request.Request(
            base + path + "?k=" + token,
            data=json.dumps(body).encode(),
            headers={"Content-Type": "application/json"}, method="POST")
        with urllib.request.urlopen(req, context=ctx, timeout=60) as r:
            return json.load(r)

    def wait(tid: str, timeout: float = 120) -> str:
        import sqlite3
        t0 = time.time()
        while time.time() - t0 < timeout:
            c = sqlite3.connect(db)
            row = c.execute("select state, log from tasks where id=?", (tid,)).fetchone()
            c.close()
            if row and row[0] in ("done", "failed", "expired"):
                return f"{row[0]} {row[1] or ''}".strip()
            time.sleep(1.5)
        return "timeout"

    return post, wait, files


# 驱动脚本用 PowerShell，不用 Python：目标机是 SYSTEM 身份跑的，PATH 里
# **没有 python**（EVO 实测 where python 空、py 启动器也找不到已装版本）。
# 第一版用 .py 驱动，proc_start 起来什么都没发生，result.json 永远不出现 ——
# 而 PowerShell 在 Windows 上一定在。
DRIVER_PS = r'''
$ErrorActionPreference = "Continue"
$pdir = "__PDIR__"
$res = @{ parts = 0; bytes = 0 }
$parts = @(Get-ChildItem (Join-Path $pdir "part-*.bin") -ErrorAction SilentlyContinue | Sort-Object Name)
$res.parts = $parts.Count
$ms = New-Object System.IO.MemoryStream
foreach ($p in $parts) {
    $b = [System.IO.File]::ReadAllBytes($p.FullName)
    $ms.Write($b, 0, $b.Length)
}
$raw = $ms.ToArray()
$res.bytes = $raw.Length
$b64 = [Convert]::ToBase64String($raw)
$body = @{ version = "__VER__"; sha256 = "__SHA__"; name = "__NAME__"; b64 = $b64; restart = __RESTART__ } | ConvertTo-Json -Compress
try {
    $r = Invoke-RestMethod -Uri "http://127.0.0.1:__PORT__/api/update/push" -Method Post -Body $body -ContentType "application/json" -TimeoutSec 600
    $res.ok = $true
    $res.body = $r
} catch {
    $res.ok = $false
    $res.error = $_.Exception.Message
}
# 先落盘再等服务回来：更新会把自己重启掉，POST 很可能拿不到响应，
# 那不是失败 —— 真正的判据是「重启后 /api/about 报的版本」。
($res | ConvertTo-Json -Compress -Depth 6) | Out-File (Join-Path $pdir "result.json") -Encoding utf8

for ($i = 0; $i -lt 30; $i++) {
    Start-Sleep -Seconds 2
    try {
        $a = Invoke-RestMethod -Uri "http://127.0.0.1:__PORT__/api/about" -TimeoutSec 4
        $res.after_version = $a.version
        $res.after_app_dir = $a.app_dir
        ($res | ConvertTo-Json -Compress -Depth 6) | Out-File (Join-Path $pdir "result.json") -Encoding utf8
        break
    } catch { }
}
'''

# 驱动脚本带中文注释，必须写 BOM：PS 5.1 把无 BOM 的 UTF-8 当 GBK 读，
# 末尾汉字会吞掉换行（check.sh 第 8 节说的就是这条）。日志重定向到 driver.log，
# 出问题时能取回来看，不用猜。
DRIVER_CMD = '''@echo off
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "__PS1__" >> "__LOG__" 2>&1
'''


def push_fleet(zip_path: Path, sha: str, ver: str, args, restart: bool) -> dict:
    post, wait, files = _fleet(args)
    pdir = args.remote_dir
    raw = zip_path.read_bytes()
    parts = [raw[i:i + CHUNK] for i in range(0, len(raw), CHUNK)]
    print(f"包 {len(raw)} B → {len(parts)} 片 → {args.device}:{pdir}")

    for i, ch in enumerate(parts):
        r = post("/api/fleet/task", {"device_id": args.device, "kind": "put",
                                     "args": {"path": f"{pdir}\\part-{i:02d}.bin",
                                              "b64": base64.b64encode(ch).decode()}})
        tid = r.get("id")
        print(f"  put {i + 1}/{len(parts)}:", wait(tid) if tid else r)
        if not tid:
            return {"ok": False, "error": f"put 失败：{r}"}

    ps1_path = pdir + "\\apply_push.ps1"
    cmd_path = pdir + "\\apply_push.cmd"
    drv = (DRIVER_PS.replace("__PDIR__", pdir).replace("__VER__", ver).replace("__SHA__", sha)
           .replace("__NAME__", zip_path.name).replace("__PORT__", str(args.port))
           .replace("__RESTART__", "$true" if restart else "$false"))
    r = post("/api/fleet/task", {"device_id": args.device, "kind": "put",
                                 "args": {"path": ps1_path,
                                          "b64": base64.b64encode(drv.encode("utf-8-sig")).decode()}})
    tid = r.get("id")
    print("  put driver.ps1:", wait(tid) if tid else r)

    cmd = DRIVER_CMD.replace("__PS1__", ps1_path).replace("__LOG__", pdir + "\\driver.log")
    r = post("/api/fleet/task", {"device_id": args.device, "kind": "put",
                                 "args": {"path": cmd_path,
                                          "b64": base64.b64encode(cmd.encode("utf-8")).decode()}})
    tid = r.get("id")
    print("  put driver.cmd:", wait(tid) if tid else r)

    r = post("/api/fleet/task", {"device_id": args.device, "kind": "proc_start",
                                 "args": {"path": cmd_path}})
    tid = r.get("id")
    print("  run driver:", wait(tid) if tid else r)

    # 取结果（更新会重启进程，POST 已在结果写出后才生效）
    result = {}
    for _ in range(30):
        time.sleep(4)
        r = post("/api/fleet/task", {"device_id": args.device, "kind": "get",
                                     "args": {"path": pdir + "\\result.json"}})
        tid = r.get("id")
        if not tid:
            continue
        st = wait(tid, 40)
        p = os.path.join(files, tid + ".bin")
        if os.path.exists(p):
            try:
                # PowerShell 的 Out-File -Encoding utf8 会带 BOM，
                # 用 utf-8 读会让 json.loads 报 Unexpected UTF-8 BOM
                result = json.loads(Path(p).read_text(encoding="utf-8-sig", errors="replace"))
                break
            except ValueError:
                pass
        print("  waiting:", st)
    return result or {"ok": False, "error": "拿不到 result.json"}


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description="主机推送 Moshan 更新（不走 GitHub）")
    ap.add_argument("--http", help="直连中台地址，如 http://192.168.0.103:8321")
    ap.add_argument("--token", default=os.environ.get("MOSHAN_TOKEN", ""), help="中台令牌")
    ap.add_argument("--insecure", action="store_true", help="忽略 https 证书")
    ap.add_argument("--fleet", dest="device", help="走虫巢：目标机 device_id")
    ap.add_argument("--fleet-base", default="https://127.0.0.1:8781")
    ap.add_argument("--fleet-token", default="")
    ap.add_argument("--fleet-db", default="~/Documents/AI项目/16-多机/fleet.db")
    ap.add_argument("--fleet-files", default="~/Documents/AI项目/16-多机/files")
    ap.add_argument("--remote-dir", default="C:\\moshan-push", help="目标机上的中转目录")
    ap.add_argument("--port", type=int, default=8321, help="目标机中台端口（fleet 模式用）")
    ap.add_argument("--no-restart", action="store_true", help="只落盘，不重启")
    ap.add_argument("--keep", action="store_true", help="保留本地打好的包")
    ap.add_argument("--dry-run", action="store_true", help="只打包报大小与 sha，不发送")
    args = ap.parse_args(argv)

    zip_path, sha, ver = make_package()
    print(f"版本 {ver}  包 {zip_path.name}  {zip_path.stat().st_size} B")
    print(f"sha256 {sha}")
    try:
        if args.dry_run:
            print("dry-run：不发")
            return 0
        restart = not args.no_restart
        if args.device:
            res = push_fleet(zip_path, sha, ver, args, restart)
        elif args.http:
            res = push_http(zip_path, sha, ver, args.http, args.token, restart, args.insecure)
        else:
            print("要给一个目标：--http URL 或 --fleet device", file=sys.stderr)
            return 3
        print(json.dumps(res, ensure_ascii=False, indent=2))
        return 0 if res.get("ok") else 1
    finally:
        if not args.keep:
            try:
                zip_path.unlink()
            except OSError:
                pass


if __name__ == "__main__":
    raise SystemExit(main())
