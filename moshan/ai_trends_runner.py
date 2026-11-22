"""AI 技术动向的调度层：每天定点跑 scripts/ai_trends_daily.py，页面能看状态。

抓取 / 去重 / 分级 / 写文件全在 ai_trends_daily.py 里，这里只管
「什么时候跑、跑完怎么展示」。两边分开的理由：脚本要能在终端单独跑
（--date / --no-llm），不依赖口袋服务活着。

不依赖任何外部应用：脚本自己连 127.0.0.1:11434 的 Ollama（qwen3.5-4b），
4B 挂了会降级成规则总览，简报照出，只是总览那段变成「按热度排序的前三项」。

与既有触发器的共存：run 之前先看 runs/<今天>.json，当天已经跑过就跳过。
所以不管是谁先触发（外部定时、手动、本调度），一天只会真跑一次。
"""
from __future__ import annotations

import json
import subprocess
import threading
import time
from datetime import datetime, timedelta
from pathlib import Path

_LOCK = threading.Lock()
STATE: dict = {"running": False, "started": "", "error": "", "last": None}


def _cfg(cfg: dict) -> dict:
    d = cfg.get("ai_trends") or {}
    script = Path(d.get("script")
                 or Path.home() / ".trae-cn/assistant/2026-11-11/local-ai-workspace"
                    / "work/local-ai-workspace/scripts/ai_trends_daily.py")
    return {
        "python": Path(d.get("python") or script.parents[1] / ".venv/bin/python"),
        "script": script,
        "root": Path(d.get("root") or Path.home() / "Documents/AI项目"),
        "hour": int(d.get("hour", 9)),
        "enabled": bool(d.get("enabled", True)),
    }


def day_dir(cfg: dict) -> Path:
    return _cfg(cfg)["root"] / "01-研究" / "普通" / "技术动向"


def runs_dir(cfg: dict) -> Path:
    """运行记录在数据根的系统记录目录（2026-11-18 从代码目录搬出）。"""
    return _cfg(cfg)["root"] / "10-系统记录" / "新闻" / "runs"


def _legacy_runs_dir(cfg: dict) -> Path:
    return _cfg(cfg)["script"].parents[1] / "data" / "ai-trends" / "runs"


def _read_run(cfg: dict, day: str) -> dict:
    p = runs_dir(cfg) / f"{day}.json"
    if not p.exists():
        return {}
    try:
        d = json.loads(p.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}
    if not isinstance(d, dict):
        return {}
    d["_path"] = str(p)
    d["_dir"] = str(day_dir(cfg) / day)
    return d


def latest(cfg: dict) -> dict:
    """最近一次运行记录（不一定是今天）。"""
    rd = runs_dir(cfg)
    if not rd.is_dir():
        rd = _legacy_runs_dir(cfg)      # 迁移前后都能读到历史
    if not rd.is_dir():
        return {}
    files = sorted((p for p in rd.glob("*.json")), reverse=True)
    for p in files[:7]:
        d = _read_run(cfg, p.stem)
        if d:
            return d
    return {}


def history(cfg: dict, limit: int = 14) -> list[dict]:
    """最近若干天的概览，用来判断「是不是停了」。"""
    rd = runs_dir(cfg)
    if not rd.is_dir():
        rd = _legacy_runs_dir(cfg)
    if not rd.is_dir():
        return []
    out: list[dict] = []
    for p in sorted(rd.glob("*.json"), reverse=True)[:limit]:
        d = _read_run(cfg, p.stem)
        if not d:
            continue
        s = d.get("stats") or {}
        out.append({
            "date": d.get("date") or p.stem,
            "status": d.get("status") or "?",
            "items": s.get("day_total", s.get("new", 0)),
            "seen": s.get("seen", 0),
            "finished_at": d.get("finished_at") or "",
            "unavailable": d.get("unavailable") or [],
        })
    return out


def already_done(cfg: dict, day: str | None = None) -> bool:
    """当天是否已经产出过可用简报。用来避免两个触发器重复跑。"""
    day = day or datetime.now().strftime("%Y-%m-%d")
    d = _read_run(cfg, day)
    if not d:
        return False
    return (d.get("status") or "") in ("ok", "partial")


def run(cfg: dict, force: bool = False, timeout: int = 900) -> dict:
    """跑一次。力的来源：手动 / 定时。已在跑时直接拒绝。"""
    c = _cfg(cfg)
    if not c["python"].exists():
        return {"ok": False, "error": f"找不到 Python：{c['python']}"}
    if not c["script"].exists():
        return {"ok": False, "error": f"找不到脚本：{c['script']}"}
    day = datetime.now().strftime("%Y-%m-%d")
    if not force and already_done(cfg, day):
        reason = f"{day} 已经出过简报，本次跳过"
        STATE["last"] = {"ok": True, "skipped": True, "reason": reason,
                         "date": day, "status": (_read_run(cfg, day).get("status") or "")}
        return {"ok": True, "skipped": True, "reason": reason, "run": _read_run(cfg, day)}
    if not _LOCK.acquire(blocking=False):
        return {"ok": False, "error": "正在跑，等这次结束"}
    try:
        STATE.update(running=True, started=datetime.now().strftime("%Y-%m-%d %H:%M:%S"),
                     error="")
        t0 = time.time()
        try:
            r = subprocess.run([str(c["python"]), str(c["script"])],
                               cwd=str(c["script"].parents[1]),
                               capture_output=True, text=True, timeout=timeout)
        except subprocess.TimeoutExpired:
            STATE["error"] = "超时"
            return {"ok": False, "error": f"跑了 {timeout}s 还没结束，已中止"}
        # 退出码：0 全部来源正常｜1 有来源不可用但简报已产出｜2 致命错误
        rec = _read_run(cfg, day)
        if not rec:
            STATE["error"] = (r.stderr or r.stdout or "没有产出")[-300:]
            return {"ok": False, "error": STATE["error"], "returncode": r.returncode}
        out = {
            "ok": r.returncode in (0, 1),
            "returncode": r.returncode,
            "seconds": round(time.time() - t0, 1),
            "date": rec.get("date"),
            "status": rec.get("status"),
            "stats": rec.get("stats") or {},
            "llm": rec.get("llm") or {},
            "unavailable": rec.get("unavailable") or [],
            "dir": rec.get("_dir"),
        }
        STATE["last"] = out
        return out
    finally:
        STATE["running"] = False
        _LOCK.release()


def next_run_at(cfg: dict) -> str:
    c = _cfg(cfg)
    now = datetime.now()
    nxt = now.replace(hour=c["hour"], minute=0, second=0, microsecond=0)
    if nxt <= now:
        nxt += timedelta(days=1)
    return nxt.strftime("%Y-%m-%d %H:%M")


def scheduler_loop(cfg: dict, on_done=None) -> None:
    """每天 ai_trends.hour 点跑一次。由 server.py 在 startup 起一个 daemon 线程。

    每分钟看一次表而不是 sleep 到整点：Mac 睡眠时 sleep 不走，醒来已经过点
    就永远等不到。按分钟检查才有「错过补跑」的效果。
    启动当天如果已经跑过（外部触发器先跑了），already_done 会让它跳过。

    失败不无限重试：同一天最多 3 次，两次之间至少隔 30 分钟。
    否则网络断的那天会每分钟抓一次 GitHub、调一次 4B。
    """
    c = _cfg(cfg)
    tried: dict[str, list] = {}       # day -> [次数, 上次时间戳]
    while True:
        try:
            now = datetime.now()
            today = now.strftime("%Y-%m-%d")
            if c["enabled"] and now.hour >= c["hour"] and not already_done(cfg):
                st = tried.setdefault(today, [0, 0.0])
                if st[0] < 3 and (st[0] == 0 or time.time() - st[1] >= 1800):
                    st[0] += 1
                    st[1] = time.time()
                    out = run(cfg)
                    if on_done:
                        on_done(out)
            if len(tried) > 7:        # 只留最近一周的记录
                for k in sorted(tried)[:-7]:
                    tried.pop(k, None)
        except Exception as e:        # 定时任务不能因为一次异常就死掉
            if on_done:
                on_done({"ok": False, "error": str(e)})
        time.sleep(60)
