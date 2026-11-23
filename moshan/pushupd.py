"""主机推送更新：更新包由「主机」推过来，不从 GitHub 拉。

为什么要这条路
    nucbox 那台办公室电脑连不上 github.com（实测 8 秒超时），它也不该为了
    一次更新去外网拉包。主机（这台 mac / 虫巢中台）把程序包推过来，落盘、
    校验、切换、重启，全程不需要人到机器前 —— 和接入程序的 self_update
    是同一个思路：**更新权在主机，不在被更新的机器**。

协议（官方客户端见 scripts/host_push_update.py）
    POST /api/update/push    {"version","sha256","b64","name","restart"}
        单包上传。程序包约 0.5 MB，base64 后 < 1 MB，一次请求就够。
        大包走分片：额外带 {"index","total","part"}，最后一次收齐后整体校验。
    GET  /api/update/status  {"ok","version","dir","at","history":[…]}

落地布局（全部在状态目录下，更新绝不碰用户数据）
    updates/pkgs/<version>/   解包后的程序目录（server.py、ui.html …）
    updates/current.json      {"version","dir","at"}
    updates/parts.json        分片进度（只在分片时出现）

三条硬约束
    1. sha256 对不上就整包丢弃 —— 不落半成品，不给「更新到一半起不来」留机会；
    2. 解包只认白名单后缀，且拒绝绝对路径与 `..`（更新包不是往用户机器
       随便写文件的通道）；
    3. 先写完再切 current.json（原子替换），切换前旧目录一直可用，可回滚。
"""
from __future__ import annotations

import base64
import hashlib
import io
import json
import os
import re
import shutil
import subprocess
import sys
import time
import zipfile
from pathlib import Path
from typing import Optional

import paths

# 程序文件白名单：多一个后缀就等于多一条「往用户磁盘写任意文件」的口子
ALLOW_SUFFIX = {
    ".py", ".html", ".css", ".js", ".json", ".txt", ".md",
    ".pem", ".crt", ".key", ".svg", ".ico", ".png", ".jpg",
}
SKIP_DIRS = {"__pycache__", ".git", ".idea", ".vscode"}


def _root() -> Path:
    return paths.state_dir() / "updates"


def _ensure() -> Path:
    d = _root()
    (d / "pkgs").mkdir(parents=True, exist_ok=True)
    return d


def _read_json(p: Path, default):
    try:
        return json.loads(p.read_text(encoding="utf-8"))
    except Exception:                                  # noqa: BLE001
        return default


def _write_json(p: Path, obj) -> None:
    """原子写：先写同名 .tmp 再 replace，断电也不会留半截 JSON。"""
    p.parent.mkdir(parents=True, exist_ok=True)
    tmp = p.with_suffix(p.suffix + ".tmp")
    tmp.write_text(json.dumps(obj, ensure_ascii=False, indent=1), encoding="utf-8")
    os.replace(tmp, p)


def _safe_name(name: str) -> Optional[str]:
    """把压缩包里的条目名收成安全的相对路径；非法返回 None。"""
    name = (name or "").replace("\\", "/").strip()
    if not name or name.endswith("/"):
        return None
    parts = [x for x in name.split("/") if x not in ("", ".")]
    if not parts or any(x == ".." for x in parts):
        return None
    if any(x in SKIP_DIRS for x in parts):
        return None
    if Path(parts[-1]).suffix.lower() not in ALLOW_SUFFIX:
        return None
    return "/".join(parts)


def _unpack(raw: bytes, dest: Path) -> dict:
    """解包到 dest；返回 {files, bytes, skipped}。dest 必须已存在且为空。"""
    files = written = 0
    skipped = []
    with zipfile.ZipFile(io.BytesIO(raw)) as zf:
        for info in zf.infolist():
            if info.is_dir():
                continue
            rel = _safe_name(info.filename)
            if rel is None:
                skipped.append(info.filename)
                continue
            out = dest / rel
            out.parent.mkdir(parents=True, exist_ok=True)
            with zf.open(info) as src, open(out, "wb") as fd:
                while True:
                    chunk = src.read(1 << 16)
                    if not chunk:
                        break
                    fd.write(chunk)
                    written += len(chunk)
            files += 1
    return {"files": files, "bytes": written, "skipped": skipped[:5],
            "skipped_n": len(skipped)}


def status() -> dict:
    d = _ensure()
    cur = _read_json(d / "current.json", {})
    pkgs = sorted([p.name for p in (d / "pkgs").iterdir() if p.is_dir()]) if (d / "pkgs").is_dir() else []
    return {
        "ok": True,
        "version": cur.get("version") or "",
        "dir": cur.get("dir") or "",
        "at": cur.get("at") or "",
        "source": cur.get("source") or "",
        "backup": cur.get("backup") or "",
        "packages": pkgs[-10:],
        # 跑的是哪份程序：packages 里的解包目录，还是原来的 app_dir
        "running_from": str(paths.app_dir()),
        "frozen": bool(getattr(sys, "frozen", False)),
    }


def _restart_script(new_dir: Path, version: str) -> dict:
    """写一个「等 3 秒、停旧进程、从新目录拉起」的脚本，并 detached 执行。

    必须是独立进程：脚本一执行，当前进程马上就会被 Stop-Process 掉。
    """
    d = _ensure()
    pid = os.getpid()
    py = sys.executable or "python"
    entry = str(new_dir / "server.py")

    if os.name == "nt":
        ps1 = d / "restart.ps1"
        # 环境必须**显式带上**：EVO 的服务不是随手 `python server.py` 起的，
        # 而是 run-evo.cmd 先把 USERPROFILE/APPDATA 指到 pamphile——
        # 少了这几行，新进程会以 SYSTEM 的家目录起，跑进 systemprofile 里的
        # 另一套状态目录：配置、令牌、数据全都「变空」，页面变成首次引导页。
        # 踩过一次，这里不再靠「子进程应该会继承」这种假设。
        env_keys = ("USERPROFILE", "HOMEDRIVE", "HOMEPATH", "APPDATA",
                    "LOCALAPPDATA", "TEMP", "TMP", "HOME", "PATH")
        env_lines = ""
        for k in env_keys:
            v = os.environ.get(k)
            if v:
                env_lines += "$env:%s='%s'\r\n" % (k, v.replace("'", "''"))
        ps1.write_text(
            "$ErrorActionPreference='Continue'\r\n"
            + env_lines +
            "Start-Sleep -Seconds 3\r\n"
            f"Stop-Process -Id {pid} -Force -ErrorAction SilentlyContinue\r\n"
            "Start-Sleep -Seconds 2\r\n"
            f"Start-Process -FilePath '{py}' -ArgumentList '{entry}' "
            f"-WorkingDirectory '{new_dir}' -WindowStyle Hidden\r\n",
            encoding="utf-8",
        )
        # 直接 Popen("powershell.exe ...") 在 SYSTEM 会话里实测**没跑起来**：
        # 文件写了、进程没拉起、旧进程还活着（pid 一直是同一个）。改成落一个
        # cmd 包装再执行它 —— 这条路在同一台机器上用 proc_start 跑过，是验证过的。
        cmd = d / "restart.cmd"
        cmd.write_text(
            "@echo off\r\n"
            f'powershell.exe -NoProfile -ExecutionPolicy Bypass -File "{ps1}" '
            f'>> "{d / "restart.log"}" 2>&1\r\n',
            encoding="ascii",
        )
        try:
            subprocess.Popen([str(cmd)], cwd=str(d),
                             creationflags=0x00000008 | 0x00000200,   # DETACHED | NEW_GROUP
                             close_fds=True)
            how = "cmd"
        except Exception as exc:                       # noqa: BLE001
            return {"ok": False, "error": f"重启脚本没拉起来：{exc}", "script": str(cmd)}
        return {"ok": True, "how": how, "script": str(ps1), "wrapper": str(cmd),
                "log": str(d / "restart.log"), "pid": pid}

    if os.name == "posix":
        sh = d / "restart.sh"
        sh.write_text(
            "#!/bin/sh\n"
            "sleep 3\n"
            f"kill {pid} 2>/dev/null\n"
            "sleep 2\n"
            f"cd '{new_dir}' && exec '{py}' '{entry}' >>/dev/null 2>&1 &\n",
            encoding="utf-8",
        )
        sh.chmod(0o755)
        subprocess.Popen(["/bin/sh", str(sh)], start_new_session=True, close_fds=True)
        return {"ok": True, "how": "sh", "script": str(sh), "pid": pid}

    return {"ok": False, "error": "这个平台不会写重启脚本，包已落盘，手工重启即可"}


def _commit(version: str, dest: Path, *, source: str = "push", restart: bool = True,
            backup: str = "") -> dict:
    d = _ensure()
    prev = _read_json(d / "current.json", {})
    _write_json(d / "current.json", {
        "version": version,
        "dir": str(dest),
        "at": time.strftime("%Y-%m-%d %H:%M:%S"),
        "source": source,
        "backup": backup,
        "prev": prev.get("version") or "",
    })
    out = {"ok": True, "version": version, "dir": str(dest),
           "prev": prev.get("version") or "", "backup": backup}
    if getattr(sys, "frozen", False):
        out.update({"restart": False,
                    "note": "这是安装版（frozen），换程序要走安装包；包已落盘，未重启"})
        return out
    if restart:
        out["restart"] = _restart_script(dest, version)
    else:
        out["restart"] = False
    return out


def push(body: dict) -> dict:
    """处理 /api/update/push 的 body。返回给调用方的 dict（ok 字段定成败）。"""
    version = str(body.get("version") or "").strip().lstrip("vV")
    if not version:
        return {"ok": False, "error": "version 必填"}
    want = str(body.get("sha256") or "").strip().lower()
    if len(want) != 64:
        return {"ok": False, "error": "sha256 必填（64 位十六进制）"}
    restart = bool(body.get("restart", True))
    name = str(body.get("name") or f"moshan-{version}.zip")
    total = int(body.get("total") or 1)
    index = int(body.get("index") or 0)
    d = _ensure()

    # ---- 分片：先攒齐再校验 ----
    if total > 1:
        st = _read_json(d / "parts.json", {})
        if st.get("version") != version:
            st = {"version": version, "total": total, "parts": {}}
        part = body.get("part") or body.get("b64")
        if not part:
            return {"ok": False, "error": "分片缺 part"}
        st["parts"][str(index)] = part
        _write_json(d / "parts.json", st)
        if len(st["parts"]) < total:
            return {"ok": True, "partial": True, "got": len(st["parts"]), "total": total}
        try:
            raw = b"".join(base64.b64decode(st["parts"][str(i)]) for i in range(total))
        except Exception as exc:                       # noqa: BLE001
            return {"ok": False, "error": f"分片解码失败：{exc}"}
    else:
        b64 = body.get("b64")
        if not b64:
            return {"ok": False, "error": "b64 必填"}
        try:
            raw = base64.b64decode(b64)
        except Exception as exc:                       # noqa: BLE001
            return {"ok": False, "error": f"base64 解码失败：{exc}"}

    got = hashlib.sha256(raw).hexdigest()
    if got != want:
        return {"ok": False, "error": "sha256 不匹配，整包丢弃",
                "want": want, "got": got, "bytes": len(raw)}

    # ---- 解包到版本目录（先清再解，避免半新半旧）----
    frozen = bool(getattr(sys, "frozen", False))
    if frozen:
        # 安装版：程序在 _internal 里，运行中换不掉，只能落盘等人跑安装包
        dest = d / "pkgs" / version
        if dest.exists():
            shutil.rmtree(dest, ignore_errors=True)
        dest.mkdir(parents=True, exist_ok=True)
    else:
        # 源码部署：**原地替换**。为什么要原地 —— 开机自启的那条命令指向
        # app_dir（EVO 是 run-evo.cmd → C:\moshan-024\moshan），换成别的目录
        # 就等于「这次更新有效、重启又回旧版」。所以先备份，再覆盖同名文件。
        dest = paths.app_dir()
    backup = ""
    if not frozen:
        cur = _read_json(d / "current.json", {})
        curver = str(cur.get("version") or "") or _app_version(dest)
        bak = d / "pkgs" / (curver + "-bak-" + time.strftime("%Y%m%d-%H%M%S"))
        try:
            shutil.copytree(dest, bak, dirs_exist_ok=True,
                            ignore=shutil.ignore_patterns("__pycache__", "*.pyc", "*.log"))
            backup = str(bak)
        except Exception as exc:                       # noqa: BLE001
            return {"ok": False,
                    "error": f"备份失败，为免更新到一半退不回去，已中止：{exc}"}

    try:
        info = _unpack(raw, dest)
    except Exception as exc:                           # noqa: BLE001
        return {"ok": False, "error": f"解包失败：{exc}"}
    if not (dest / "server.py").exists():
        # 拒收时把空壳收掉，别在 packages 里留个「看起来收过」的目录
        if frozen:
            shutil.rmtree(dest, ignore_errors=True)
        return {"ok": False, "error": "包里没有 server.py，拒收", "unpacked": info}
    if (d / "parts.json").exists():
        try:
            (d / "parts.json").unlink()
        except OSError:
            pass

    out = _commit(version, dest, source=f"push:{name}", restart=restart, backup=backup)
    out["unpacked"] = info
    out["sha256"] = got
    out["mode"] = "frozen" if frozen else "in-place"
    return out


def _app_version(app: Path) -> str:
    """读 app_dir 里 version.py 的版本号（备份目录命名用）。"""
    try:
        m = re.search(r'VERSION\s*=\s*"([^"]+)"', (app / "version.py").read_text(encoding="utf-8"))
        return m.group(1) if m else "unknown"
    except OSError:
        return "unknown"


def rollback(restart: bool = True) -> dict:
    """回到更新前那份（current.json 里记着 backup / 上一版目录）。"""
    d = _ensure()
    cur = _read_json(d / "current.json", {})
    bak = str(cur.get("backup") or "")
    if bak and Path(bak).is_dir():
        dest = paths.app_dir()
        try:
            shutil.copytree(bak, dest, dirs_exist_ok=True,
                            ignore=shutil.ignore_patterns("__pycache__", "*.pyc"))
        except Exception as exc:                       # noqa: BLE001
            return {"ok": False, "error": f"回滚复制失败：{exc}"}
        return _commit(_app_version(dest), dest, source="rollback", restart=restart,
                       backup="")
    prev = str(cur.get("prev") or "")
    dest = d / "pkgs" / prev if prev else None
    if not dest or not dest.is_dir():
        return {"ok": False, "error": "没有可回滚的备份", "prev": prev}
    return _commit(prev, dest, source="rollback", restart=restart, backup="")
