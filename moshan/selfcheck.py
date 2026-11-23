#!/usr/bin/env python3
"""本机巡检 —— 不依赖 cuntu 的那一份。

以前 `/api/inspect/*` 只做转发：真正的检查、报告归档都在 cuntu 的
`syscheck.py`（8774）里。于是「没装 cuntu 的机器」点开巡检就是一片空 ——
交付版全是这种情况，Windows 上尤其明显（8774 根本没起）。

而 syscheck 的检查项是给作者本机那套 launchd / gateway / OpenClaw 写的：
换台机器，/launchd、/锁文件 这些项本身也不成立。所以这里补一份**哪台机器都成立**
的基线检查：

  数据目录可写 / 磁盘剩余 / 内存压力 / Python 可用 / https 证书 /
  本机服务在听 / 采集层命中几个源 / 外网能否连 api.github.com

报告写到和 syscheck 同一个目录（`10-系统记录/巡检/报告`），所以
`/api/inspect/report` 的读法不变、历史也接得上。

8774 活着时 server.py 仍然优先用它 —— 那边还有端口矩阵、一键拉起这些更细的东西；
这里只保证「没有 8774 也有一页能看的巡检」，不是要取代它。

文件名叫 selfcheck 不叫 inspect：`moshan/` 会进 sys.path，叫 inspect.py 会顶掉
标准库的 inspect，pydantic / typing_extensions 一 import 就炸
（实测 `module 'inspect' has no attribute 'signature'`，整个服务起不来）。
"""
from __future__ import annotations

import importlib
import json
import os
import shutil
import socket
import ssl
import sys
import threading
import time
from datetime import datetime
from pathlib import Path

import paths
import pyexe

_LOCK = threading.Lock()
STATE: dict = {"running": False, "started": "", "phase": "", "done": 0, "total": 0,
               "checks": [], "last": None, "report": "", "error": ""}

_STATUS_CN = {"ok": "通过", "warn": "注意", "fail": "没过", "skip": "跳过"}


# ---------------------------------------------------------------- 路径
def _root(cfg: dict | None = None) -> Path:
    """数据根。优先用配置里的 allowed_roots[0]，和 dash / 巡检报告保持一致。"""
    try:
        roots = (cfg or {}).get("allowed_roots") or []
        if roots:
            return Path(roots[0])
    except Exception:                                   # noqa: BLE001
        pass
    return paths.data_root()


def report_dir(cfg: dict | None = None) -> Path:
    return _root(cfg) / "10-系统记录" / "巡检" / "报告"


def history_file(cfg: dict | None = None) -> Path:
    return _root(cfg) / "10-系统记录" / "巡检" / "历史.jsonl"


# ---------------------------------------------------------------- 单项检查
#   每项返回 (status, detail)。status 只认 ok / warn / fail / skip。
#   单项抛异常不算整轮失败 —— 包一层，异常本身就当成这一项没过。
def _check_dir(cfg) -> tuple[str, str]:
    d = _root(cfg)
    if not d.is_dir():
        return "fail", f"数据根不存在：{d}"
    probe = d / ".moshan-probe"
    try:
        probe.write_text(str(time.time()), encoding="utf-8")
        probe.unlink()
    except OSError as e:
        return "fail", f"数据根不可写：{e}"
    return "ok", str(d)


def _check_disk(cfg) -> tuple[str, str]:
    d = _root(cfg)
    try:
        u = shutil.disk_usage(str(d if d.exists() else d.anchor or "."))
    except OSError as e:
        return "skip", f"读不到磁盘用量：{e}"
    free = u.free / 1073741824.0
    total = u.total / 1073741824.0
    st = "fail" if free < 0.5 else ("warn" if free < 5 else "ok")
    return st, f"剩余 {free:.1f} GB / 共 {total:.1f} GB"


def _check_mem(cfg) -> tuple[str, str]:
    try:
        import sysinfo
        m = sysinfo.sample().get("mem") or {}
    except Exception as e:                              # noqa: BLE001
        return "skip", f"读不到内存：{type(e).__name__}: {e}"
    pct = m.get("pct")
    used = m.get("used_gb")
    total = m.get("total_gb")
    if pct is None:
        return "skip", json.dumps(m, ensure_ascii=False)[:80]
    st = "warn" if float(pct) >= 90 else "ok"
    return st, f"已用 {used} GB / 共 {total} GB（{pct}%）"


def _check_python(cfg) -> tuple[str, str]:
    venv = Path(os.environ.get("MOSHAN_CUNTU") or (Path.home() / "Documents" / "cuntu")) / ".venv"
    py, tried = pyexe.resolve(None, venv if venv.is_dir() else None)
    if not py:
        return "warn", pyexe.not_found_hint(tried)
    try:
        import subprocess
        r = subprocess.run([str(py), "--version"], capture_output=True, text=True, timeout=10)
        ver = (r.stdout or r.stderr or "").strip().splitlines()[0]
    except Exception:                                   # noqa: BLE001
        ver = "版本未知"
    return "ok", f"{ver}（{py}）"


def _check_tls(cfg) -> tuple[str, str]:
    try:
        import certifi
    except ImportError:
        return "fail", "缺 certifi：https 请求会 CERTIFICATE_VERIFY_FAILED"
    ca = Path(certifi.where())
    if not ca.is_file():
        return "fail", f"证书文件不存在：{ca}"
    try:
        ctx = ssl.create_default_context()
        n = len(ctx.get_ca_certs())
    except Exception as e:                              # noqa: BLE001
        return "warn", f"建不了默认上下文：{e}"
    st = "ok" if n > 0 else "fail"
    return st, f"根证书 {n} 张（{ca}）"


def _check_port(port: int | None) -> tuple[str, str]:
    if not port:
        return "skip", "不知道自己的端口"
    try:
        with socket.create_connection(("127.0.0.1", int(port)), timeout=3):
            return "ok", f"127.0.0.1:{port} 在听"
    except OSError as e:
        return "fail", f"127.0.0.1:{port} 连不上：{e}"


def _check_sources(cfg) -> tuple[str, str]:
    try:
        import hostwork
        r = hostwork.sources_report(force=True)
    except Exception as e:                              # noqa: BLE001
        return "skip", f"采集层不可用：{type(e).__name__}: {e}"
    items = r.get("items") or []
    miss = [x["name"] for x in items if not x.get("found")]
    found = r.get("found") or 0
    st = "ok" if found >= max(1, len(items) // 2) else "warn"
    detail = f"{found}/{len(items)} 个源有数据"
    if miss:
        detail += "；缺：" + "、".join(miss[:4]) + (f" 等 {len(miss)} 个" if len(miss) > 4 else "")
    return st, detail


def _check_deps(cfg) -> tuple[str, str]:
    need = ["fastapi", "uvicorn", "certifi"]
    bad = []
    for m in need:
        try:
            importlib.import_module(m)
        except Exception:                               # noqa: BLE001
            bad.append(m)
    if bad:
        return "fail", "缺依赖：" + "、".join(bad)
    return "ok", "fastapi / uvicorn / certifi 都在"


def _check_net(cfg) -> tuple[str, str]:
    """能不能连上 GitHub。这台机器要不要「只认主机推送」，就看这一项。"""
    try:
        t0 = time.time()
        with socket.create_connection(("api.github.com", 443), timeout=4):
            ms = int((time.time() - t0) * 1000)
        return "ok", f"api.github.com:443 可达（{ms} ms）"
    except OSError as e:
        return "warn", f"连不上 api.github.com：{e}（这台机器适合 update.mode=push）"


def _check_uptime(cfg) -> tuple[str, str]:
    try:
        import sysinfo
        h = sysinfo.sample().get("uptime_h")
    except Exception:                                   # noqa: BLE001
        h = None
    if h is None:
        return "skip", "读不到运行时长"
    st = "warn" if float(h) > 24 * 14 else "ok"
    return st, f"开机 {float(h):.1f} 小时" + ("（两周多没重启，建议重启一次）" if st == "warn" else "")


# ---------------------------------------------------------------- 跑一轮
def _checks(cfg: dict, port: int | None) -> list[tuple[str, tuple]]:
    return [
        ("数据目录可写", lambda: _check_dir(cfg)),
        ("磁盘剩余", lambda: _check_disk(cfg)),
        ("内存压力", lambda: _check_mem(cfg)),
        ("运行时长", lambda: _check_uptime(cfg)),
        ("Python 可用", lambda: _check_python(cfg)),
        ("https 证书", lambda: _check_tls(cfg)),
        ("本机服务", lambda: _check_port(port)),
        ("采集层", lambda: _check_sources(cfg)),
        ("运行依赖", lambda: _check_deps(cfg)),
        ("外网 GitHub", lambda: _check_net(cfg)),
    ]


def _one(label: str, fn) -> dict:
    try:
        st, detail = fn()
    except Exception as e:                              # noqa: BLE001
        st, detail = "fail", f"{type(e).__name__}: {e}"[:180]
    return {"name": label, "status": st, "detail": detail,
            "cn": _STATUS_CN.get(st, st)}


def _render(rows: list[dict], port: int | None, root: Path) -> str:
    now = datetime.now().strftime("%Y-%m-%d %H:%M:%S")
    ok = sum(1 for r in rows if r["status"] == "ok")
    warn = sum(1 for r in rows if r["status"] == "warn")
    fail = sum(1 for r in rows if r["status"] == "fail")
    plat = "Windows" if os.name == "nt" else ("macOS" if sys.platform == "darwin" else "POSIX")
    lines = [f"# 巡检报告 {now}", "",
             f"- 平台：{plat}",
             f"- 数据根：{root}",
             f"- 端口：{port or '未知'}",
             f"- 结果：{ok} 项通过 / {warn} 项注意 / {fail} 项没过", "",
             "| 检查项 | 结果 | 说明 |", "|---|---|---|"]
    for r in rows:
        lines.append(f"| {r['name']} | {r['cn']} | {r['detail']} |")
    worse = [r for r in rows if r["status"] in ("fail", "warn")]
    if worse:
        lines += ["", "## 要看一眼的", ""]
        for r in worse:
            lines.append(f"- **{r['name']}**（{r['cn']}）：{r['detail']}")
    return "\n".join(lines) + "\n"


def run(cfg: dict | None = None, port: int | None = None, heal: bool = False) -> dict:
    """跑一轮巡检。后台线程执行，返回 started；结果看 state()。"""
    cfg = dict(cfg or {})
    root = _root(cfg)
    if not _LOCK.acquire(blocking=False):
        return {"ok": True, "started": False, "note": "上一轮还在跑"}
    STATE.update(running=True, started=datetime.now().strftime("%H:%M:%S"),
                 phase="检查中", done=0, total=10, checks=[], error="")

    def work() -> None:
        rows: list[dict] = []
        try:
            if heal:                                     # 能自愈的只有目录骨架
                for sub in ("10-系统记录/巡检/报告", "10-系统记录/巡检", "_本机工作"):
                    try:
                        paths.ensure(root / sub)
                    except Exception:                    # noqa: BLE001
                        pass
            for label, fn in _checks(cfg, port):
                rows.append(_one(label, fn))
                STATE.update(done=len(rows), checks=list(rows))
            text = _render(rows, port, root)
            d = report_dir(cfg)
            paths.ensure(d)
            p = d / (datetime.now().strftime("%Y-%m-%d_%H%M%S") + ".md")
            p.write_text(text, encoding="utf-8")
            hist = history_file(cfg)
            with hist.open("a", encoding="utf-8") as f:
                f.write(json.dumps({"at": datetime.now().strftime("%Y-%m-%d %H:%M:%S"),
                                    "ok": sum(1 for r in rows if r["status"] == "ok"),
                                    "warn": sum(1 for r in rows if r["status"] == "warn"),
                                    "fail": sum(1 for r in rows if r["status"] == "fail"),
                                    "report": p.name}, ensure_ascii=False) + "\n")
            STATE.update(last={"at": datetime.now().strftime("%Y-%m-%d %H:%M:%S"),
                               "report": p.name,
                               "fail": [r["name"] for r in rows if r["status"] == "fail"],
                               "warn": [r["name"] for r in rows if r["status"] == "warn"]},
                         report=p.name)
        except Exception as e:                           # noqa: BLE001
            STATE.update(error=f"{type(e).__name__}: {e}"[:200])
        finally:
            STATE.update(running=False, phase="")
            try:
                _LOCK.release()
            except RuntimeError:
                pass

    threading.Thread(target=work, name="inspect-round", daemon=True).start()
    return {"ok": True, "started": True, "local": True}


def state() -> dict:
    # 不抢锁：这里只是读一份快照，最坏看到上一帧，比把轮询卡住好
    return {"ok": True, "local": True, **{k: STATE[k] for k in
            ("running", "started", "phase", "done", "total", "checks",
             "last", "report", "error")}}
