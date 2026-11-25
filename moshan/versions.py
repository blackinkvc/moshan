"""版本足迹 —— 这台机器什么时候开始用 Moshan、之后跑过哪些版本。

用户的原话（2026-11-25）：「新增开始使用 moshan 的日期时间和之后用了哪些版本」。
跟「检查更新」不是一回事：那个说「有没有新的」，这个说「我这条线怎么走过来的」。
查更新要向 GitHub 问，这个完全本地，断网也能看。

落在 <状态目录>/versions.json：

  first_at        最早能证实的启用时间。第一次写这份文件时**推**出来：
                  配置文件的诞生时间（装完第一次启动就会建）／.app 的诞生时间，
                  取更早的那个。推的就标 first_guess=1，页面写「不晚于」，
                  不冒充「我亲眼看见你哪天装的」。
  first_version   记账开始时的版本（更早的版本没有记录，不编）
  seen[]          每个跑过的版本一条：首次、最后一次、启动次数
  last_at         最后一次启动

为什么不写进 config.json：那份是用户可见的配置（端口、令牌、数据根），
每次启动都往里塞一行会让 diff 变脏、也会让「配置」和「流水」混成一锅。
为什么不写 sqlite：这点数据一年也就几 KB，文本文件人可以直接打开看。
"""
from __future__ import annotations

import json
import threading
import time
from datetime import datetime
from pathlib import Path
from typing import Any

import paths
import version

_LOCK = threading.Lock()
_MAX_SEEN = 40                  # 版本再多也只留最近 40 条，别让文件无限长


def _file() -> Path:
    return paths.state_dir() / "versions.json"


def _iso(ts: float | None = None) -> str:
    return datetime.fromtimestamp(ts or time.time()).isoformat(timespec="seconds")


def _birth(p: Path) -> float:
    """文件/目录的诞生时间（macOS 上是真的创建时间，别的地方退回 mtime）。"""
    try:
        st = p.stat()
        return float(getattr(st, "st_birthtime", 0) or st.st_mtime or 0)
    except OSError:
        return 0.0


def _guess_first() -> tuple[float, str]:
    """第一次记账时，尽量往前推一个「最早可能的启用时间」。

    按可信度排：配置文件诞生时间 → 状态目录 → .app 包。都拿不到就现在。
    返回 (时间戳, 依据)，依据会显示在页面上，别让人以为这是精确值。
    """
    cands: list[tuple[float, str]] = []
    cfg = _file().parent / "config.json"
    t = _birth(cfg)
    if t:
        cands.append((t, "配置文件的创建时间"))
    t = _birth(_file().parent)
    if t:
        cands.append((t, "状态目录的创建时间"))
    import sys
    exe = Path(sys.executable)
    for up in (exe, *exe.parents):
        if up.suffix == ".app" or (up / "Contents" / "Info.plist").exists():
            t = _birth(up)
            if t:
                cands.append((t, "应用包的创建时间"))
            break
    if not cands:
        return time.time(), ""
    cands.sort()
    return cands[0]


def _load() -> dict:
    try:
        d = json.loads(_file().read_text(encoding="utf-8"))
        if isinstance(d, dict):
            return d
    except (OSError, ValueError):
        pass
    return {}


def _save(d: dict) -> None:
    p = _file()
    try:
        p.parent.mkdir(parents=True, exist_ok=True)
        tmp = p.with_suffix(".json.tmp")
        tmp.write_text(json.dumps(d, ensure_ascii=False, indent=1), encoding="utf-8")
        tmp.replace(p)
    except OSError:
        pass


def read() -> dict:
    """给页面看的整份足迹（附带几句人话摘要）。"""
    d = _load()
    seen = [x for x in (d.get("seen") or []) if isinstance(x, dict) and x.get("v")]
    cur = version.VERSION
    return {
        "first_at": d.get("first_at") or "",
        "first_guess": bool(d.get("first_guess")),
        "first_basis": str(d.get("first_basis") or ""),
        "first_version": str(d.get("first_version") or ""),
        "last_at": d.get("last_at") or "",
        "last_version": str(d.get("last_version") or ""),
        "current": cur,
        "n_versions": len(seen),
        "versions": seen,
    }


def note_launch(v: str | None = None) -> dict:
    """启动时记一笔。同一进程只记一次（server 只会调一次）。

    别的进程/别的用户调不到这里 —— 结果只落本机状态目录。
    """
    v = (v or version.VERSION).strip() or "0"
    now = time.time()
    with _LOCK:
        d = _load()
        fresh = not d
        if fresh:
            t, basis = _guess_first()
            d = {"first_at": _iso(t), "first_guess": True, "first_basis": basis,
                 "first_version": v, "seen": []}
        seen = [x for x in (d.get("seen") or []) if isinstance(x, dict)]
        hit = None
        for x in seen:
            if str(x.get("v")) == v:
                hit = x
                break
        if hit is None:
            hit = {"v": v, "first": _iso(now), "last": _iso(now), "n": 0}
            seen.append(hit)
        hit["last"] = _iso(now)
        hit["n"] = int(hit.get("n") or 0) + 1
        seen = seen[-_MAX_SEEN:]
        d.update(seen=seen, last_at=_iso(now), last_version=v)
        _save(d)
        return {"ok": True, "fresh": fresh, **read()}
