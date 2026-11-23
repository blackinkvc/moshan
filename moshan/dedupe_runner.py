"""存图去重的调度层：定时跑、手动跑、读最新清单、还原。

扫描判定逻辑全在 ~/Documents/cuntu/dedupe.py，这里只负责「什么时候跑、跑完怎么给页面看」。
分开的理由：dedupe.py 要能在终端里单独跑（--root / --apply / --restore），
不依赖口袋服务活着。
"""
from __future__ import annotations

import json
import subprocess
import threading
import time
from datetime import datetime, timedelta
from pathlib import Path

import pyexe

_LOCK = threading.Lock()          # 同一时刻只允许一次扫描
STATE: dict = {"running": False, "started": "", "error": "", "last": None}


def _cfg(cfg: dict) -> dict:
    d = cfg.get("dedupe") or {}
    script = Path(d.get("script") or Path.home() / "Documents/cuntu/dedupe.py")
    # 同 ai_trends：解释器不写死 `.venv/bin/python`，交给 pyexe 按平台找
    venv = Path.home() / "Documents/cuntu/.venv"
    py, py_tried = pyexe.resolve(d.get("python"), venv)
    return {
        "python": py or pyexe.venv_python(venv),
        "py_found": py is not None,
        "py_tried": py_tried,
        "script": script,
        "root": Path(d.get("root") or Path.home() / "Documents" / "AI项目" / "04-存图"),
        "hour": int(d.get("hour", 3)),
        "apply": bool(d.get("apply", True)),
        "limit": int(d.get("limit", 100)),
    }


def report_dir(cfg: dict) -> Path:
    return _cfg(cfg)["root"] / "_去重报告"


def latest(cfg: dict) -> dict:
    p = report_dir(cfg) / "latest.json"
    if not p.exists():
        return {}
    try:
        return json.loads(p.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}


def history(cfg: dict, limit: int = 20) -> list[dict]:
    idx = report_dir(cfg) / "index.jsonl"
    if not idx.exists():
        return []
    rows: list[dict] = []
    try:
        for line in idx.read_text(encoding="utf-8").splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                rows.append(json.loads(line))
            except ValueError:
                continue
    except OSError:
        return []
    return list(reversed(rows))[:limit]


def run(cfg: dict, apply: bool | None = None, limit: int | None = None,
        timeout: int = 1800) -> dict:
    """跑一次扫描。返回 dedupe.py --json 的结果。已在跑时直接拒绝。"""
    c = _cfg(cfg)
    if not c["py_found"]:
        return {"ok": False, "error": pyexe.not_found_hint(c["py_tried"])}
    if not c["script"].exists():
        return {"ok": False, "error": f"找不到脚本：{c['script']}"}
    if not _LOCK.acquire(blocking=False):
        return {"ok": False, "error": "正在扫，等这次结束"}
    try:
        STATE.update(running=True, started=datetime.now().strftime("%Y-%m-%d %H:%M:%S"),
                     error="")
        cmd = [str(c["python"]), str(c["script"]), "--root", str(c["root"]), "--json"]
        if apply if apply is not None else c["apply"]:
            cmd += ["--apply", "--limit", str(limit or c["limit"])]
        t0 = time.time()
        try:
            r = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
        except subprocess.TimeoutExpired:
            STATE["error"] = "超时"
            return {"ok": False, "error": f"跑了 {timeout}s 还没结束，已中止"}
        line = ""
        for ln in reversed((r.stdout or "").strip().splitlines()):
            if ln.startswith("{"):
                line = ln
                break
        if not line:
            STATE["error"] = (r.stderr or "没有输出")[:300]
            return {"ok": False, "error": STATE["error"]}
        try:
            out = json.loads(line)
        except ValueError:
            return {"ok": False, "error": "输出不是合法 JSON"}
        out["ok"] = True
        out["seconds"] = round(time.time() - t0, 1)
        STATE["last"] = out
        return out
    finally:
        STATE["running"] = False
        _LOCK.release()


def restore(cfg: dict, batch: str) -> dict:
    c = _cfg(cfg)
    batch = batch.strip().strip("/")
    if not batch.startswith("去重_"):
        return {"ok": False, "error": "批次名要以 去重_ 开头"}
    r = subprocess.run([str(c["python"]), str(c["script"]), "--root", str(c["root"]),
                        "--restore", batch], capture_output=True, text=True, timeout=300)
    ok = r.returncode == 0
    return {"ok": ok, "output": (r.stdout or r.stderr or "").strip()[:500]}


def batches(cfg: dict) -> list[dict]:
    """回收站里还有哪些批次可还原。"""
    trash = _cfg(cfg)["root"] / "_回收站"
    if not trash.is_dir():
        return []
    out: list[dict] = []
    for d in sorted(trash.iterdir(), reverse=True):
        if not d.is_dir() or not d.name.startswith("去重_"):
            continue
        man = d / "_manifest.json"
        try:
            items = json.loads(man.read_text(encoding="utf-8")) if man.exists() else []
        except (OSError, ValueError):
            items = []
        out.append({
            "name": d.name,
            "dirs": len(items),
            "bytes": sum(i.get("bytes", 0) for i in items),
        })
    return out[:30]


def next_run_at(cfg: dict) -> str:
    c = _cfg(cfg)
    now = datetime.now()
    nxt = now.replace(hour=c["hour"], minute=0, second=0, microsecond=0)
    if nxt <= now:
        nxt += timedelta(days=1)
    return nxt.strftime("%Y-%m-%d %H:%M")


def scheduler_loop(cfg: dict, on_done=None) -> None:
    """每天 dedupe.hour 点跑一次。由 server.py 在 startup 起一个 daemon 线程。

    不用 sleep 到下一个整点，而是每分钟看一次表：Mac 睡眠时 sleep 不走，
    醒来已经过了 3 点就永远等不到；按分钟检查才有「错过就补跑」的效果。
    启动当天不补跑，免得一重启就扫一遍。
    """
    c = _cfg(cfg)
    done_day = datetime.now().strftime("%Y-%m-%d")
    while True:
        try:
            now = datetime.now()
            today = now.strftime("%Y-%m-%d")
            if now.hour >= c["hour"] and done_day != today:
                done_day = today          # 先占位，避免扫描失败后每分钟重试
                out = run(cfg)
                if on_done:
                    on_done(out)
        except Exception as e:            # 定时任务不能因为一次异常就死掉
            if on_done:
                on_done({"ok": False, "error": str(e)})
        time.sleep(60)
