#!/usr/bin/env python3
"""工作台聚合：把散在四五个目录和服务里的状态收成一份 JSON。

单独成模块的理由：server.py 已经一千三百多行，工作台的读写跟口袋的传输
能力没有交集，混在一起改一次要读两千行。这里只做三件事：

  1. 读 —— 巡检/心跳/新闻/存图/阅读/存文件/去重/OCR/磁盘 + 端口探活
  2. 写 —— 待办 / 行程 / 日记 / 备忘 落到 <数据根>/09-个人工作台/notes.json，
           同时铺一份 markdown 镜像（人直接翻文件夹也能读、能用别的编辑器改）
  3. 页面 —— dash.html 的路径（server 按 no-store 返回）

数据根取 pocket 配置里的 allowed_roots[0]，跟口袋的路径白名单同源。不另
开一个「工作台专用根」：两处配置迟早会漂移，漂了就是「页面空白但没报错」。

所有读取都是「读不到就当空」，只有端口探活带 0.35 秒超时。页面是常驻刷新
的，一个目录暂时不在不该把整块数据变成 500。
"""
from __future__ import annotations

import hashlib
import json
import os
import re
import socket
import sqlite3
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from datetime import date, datetime, timedelta
from pathlib import Path

BASE = Path(__file__).resolve().parent
sys.path.insert(0, str(BASE))

import ai_trends_runner  # noqa: E402
import dedupe_runner  # noqa: E402
import extplugins  # noqa: E402  第三方插件（.app / brew 装的开源程序）
import heartbeat  # noqa: E402
import ocr_runner  # noqa: E402
import paths  # noqa: E402  数据根（别人机器上不是 ~/Documents/AI项目）

_NOTES_LOCK = threading.Lock()
_MAX_NOTES_BYTES = 512 * 1024
_SKIP_DIRS = {"_回收站", "_缩略图", "_备份", "_backup", ".DS_Store"}


# —— 基础读工具 ——

def _load(path: Path, default=None):
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return default


def _human(n: float) -> str:
    n = float(n or 0)
    for unit in ("B", "KB", "MB", "GB", "TB"):
        if n < 1024 or unit == "TB":
            return f"{n:.0f} {unit}" if unit in ("B", "KB") else f"{n:.1f} {unit}"
        n /= 1024
    return f"{n:.1f} TB"


def _pct(a: float, b: float) -> float:
    return round(a * 100.0 / b, 1) if b else 0.0


def _port(port: int, host: str = "127.0.0.1", timeout: float = 0.35) -> bool:
    try:
        with socket.create_connection((host, port), timeout=timeout):
            return True
    except OSError:
        return False


def _http_ok(url: str, timeout: float = 1.2) -> bool:
    try:
        with urllib.request.urlopen(url, timeout=timeout) as r:
            return 200 <= r.status < 500
    except (urllib.error.URLError, OSError, ValueError):
        return False


def _root(cfg: dict) -> Path:
    roots = cfg.get("allowed_roots") or []
    return Path(roots[0]) if roots else Path(cfg.get("inbox_dir", ".")).parent


def page_path(cfg: dict) -> Path:
    return _root(cfg) / "_工作台" / "dash.html"


PERSONAL_DIR = "09-个人工作台"


def personal_dir(cfg: dict) -> Path:
    """个人数据单独成目录，跟 _工作台（页面 + 布局 + 工作记录）分开。

    理由：_工作台里混着 dash.html / launchers.json / worklog.json，那是「程序」；
    待办日记是「我的东西」，要能被别的编辑器打开、能单独备份、能同步。
    """
    return _root(cfg) / PERSONAL_DIR


def notes_path(cfg: dict) -> Path:
    return personal_dir(cfg) / "notes.json"


def _legacy_notes_path(cfg: dict) -> Path:
    """2026-11-18 之前落在 _工作台/notes.json，读不到新的就搬一次。"""
    return _root(cfg) / "_工作台" / "notes.json"


# —— 各板块 ——

# 只有 macOS 上才跑得起来的服务：启动器是 .command、数据是 mac 上的目录，
# 搬到 Windows（EVO）上既点不开也没有数据，只会长成一排红点。
# 2026-11-22 加：EVO 的中台一打开 9 个红点全亮，用户以为「很多服务开不起来」，
# 其实那 9 个压根不是这台机器的。这里按平台过滤掉，不是关掉 —— Mac 上照旧全显示。
POSIX_ONLY = {"gallery", "reader", "media", "inspect", "llmcost", "follow",
              "review", "gateway", "qdrant"}


def _posix_ok(aid: str) -> bool:
    """这台机器上该不该显示这个服务。"""
    return os.name == "posix" or aid not in POSIX_ONLY


def _services(cfg: dict | None = None) -> list[dict]:
    """端口探活。pocket 自己不用探（能渲染这个页面就说明它活着）。

    插件那几行由 _all_specs() 现算 —— 以前这里是手抄的第二份清单，
    加了「记账」忘了同步就会两处对不上。现在只有一份真源：内置清单
    + <数据根>/_工作台/plugins.json（用户自己加的插件）。
    """
    items = []
    for a in (_all_specs(cfg) if cfg else BUILTIN_APPS):
        if not _posix_ok(a["id"]):        # 别的平台上的服务，别在这台机器上报红点
            continue
        port = a.get("svc_port") or a.get("port")
        if not port:                      # scope=view 的东西没有自己的端口
            continue
        href = "" if a.get("self") else str(a.get("url") or "")
        # host：服务不在本机（被控机的 portproxy 端口，如 EVO 的 17788）。
        # 有 host 就探它，没 host 探回环 —— 否则远程插件永远显示「离线」。
        items.append((a["id"], a.get("svc_name") or a["name"], port, href,
                      bool(a.get("self")), str(a.get("host") or "")))
    for sid, name, port in INFRA_SERVICES:
        if not _posix_ok(sid):
            continue
        items.append((sid, name, port, "", False, ""))
    out = []
    for sid, name, port, href, always, host in items:
        out.append({
            "id": sid, "name": name, "port": port, "href": href,
            "up": True if always else _port(port, host or "127.0.0.1"),
            "lazy": sid in LAZY_SERVICES,
        })
    return out


def _gallery(root: Path) -> dict:
    d = _load(root / "04-存图" / "_索引.json", {}) or {}
    total = d.get("total") or {}
    arts = d.get("articles") or []
    live = sum(int(a.get("n_live") or 0) for a in arts)
    video = sum(int(a.get("n_video") or 0) for a in arts)
    gif = sum(int(a.get("n_gif") or 0) for a in arts)
    today = date.today().isoformat()
    today_new = 0
    for a in arts:
        at = str(a.get("archived_at") or "")[:10]
        if at == today:
            today_new += 1
    favs = (_load(root / "04-存图" / "_收藏.json", {}) or {}).get("dirs") or []
    authors = (_load(root / "04-存图" / "_喜欢作者.json", {}) or {}).get("authors") or []
    recent = sorted(arts, key=lambda a: str(a.get("archived_at") or ""), reverse=True)[:5]
    return {
        "articles": int(total.get("articles") or len(arts)),
        "images": int(total.get("images") or 0),
        "bytes_h": _human(total.get("bytes") or 0),
        "live": live, "video": video, "gif": gif,
        "today_new": today_new,
        "favs": len(favs), "authors": len(authors),
        "index_at": str(d.get("generated_at") or ""),
        "recent": [{
            "title": a.get("title") or "", "platform": a.get("platform") or "",
            "author": a.get("author") or "", "count": a.get("count") or 0,
            "live": a.get("n_live") or 0, "at": str(a.get("archived_at") or ""),
            "dir": a.get("dir") or "",
        } for a in recent],
    }


def _reader(root: Path) -> dict:
    """扫文章目录。已读记录里的 rel 可能因为后来改主题而失效，
    所以先按完整 rel 匹配，匹配不上再按目录名（标题）兜底。"""
    base = root / "08-阅读"
    read_state = _load(base / "_已读.json", {}) or {}
    read_rels = set(read_state.get("rels") or [])
    read_titles = {r.split("/")[-1] for r in read_rels}
    favs = (_load(base / "_收藏.json", {}) or {}).get("rels") or []
    arts: list[dict] = []
    if base.exists():
        for topic_dir in sorted(base.iterdir()):
            if not topic_dir.is_dir() or topic_dir.name.startswith("_") or topic_dir.name in _SKIP_DIRS:
                continue
            for art_dir in topic_dir.iterdir():
                if not art_dir.is_dir() or art_dir.name.startswith("_"):
                    continue
                meta = _load(art_dir / "article.json")
                if not isinstance(meta, dict):
                    continue
                rel = f"{topic_dir.name}/{art_dir.name}"
                arts.append({
                    "rel": rel, "title": meta.get("title") or art_dir.name,
                    "topic": meta.get("topic") or topic_dir.name,
                    "platform": meta.get("platform") or "",
                    "author": meta.get("author") or "",
                    "chars": int(meta.get("chars") or 0),
                    "at": str(meta.get("saved_at") or ""),
                    "read": rel in read_rels or art_dir.name in read_titles,
                    "ocr": int(meta.get("ocr_chars") or 0),
                    "need_ocr": bool(meta.get("needs_ocr")),
                })
    total = len(arts)
    read_n = sum(1 for a in arts if a["read"])
    chars = sum(a["chars"] for a in arts)
    today = date.today().isoformat()
    week = (date.today() - timedelta(days=7)).isoformat()
    arts.sort(key=lambda a: a["at"], reverse=True)
    return {
        "total": total, "read": read_n, "unread": total - read_n,
        "pct": _pct(read_n, total), "chars": chars,
        "chars_h": f"{chars / 1000:.1f}k 字" if chars >= 1000 else f"{chars} 字",
        "favs": len(favs),
        "unread_list": [a for a in arts if not a["read"]][:6],
        "recent": arts[:8],
        "today_new": sum(1 for a in arts if str(a["at"])[:10] == today),
        "week_new": sum(1 for a in arts if str(a["at"])[:10] >= week),
        "ocr_done": sum(1 for a in arts if a["ocr"]),
        "need_ocr": sum(1 for a in arts if a["need_ocr"]),
    }


def _media(root: Path) -> dict:
    """媒体墙的只读快照。

    跟看图/阅读一个口径：不调 8773 的 HTTP（双击才起、不常驻），
    直接读 _墙/library.json + watch.json，再用端口探活区分「服务没开」和「库里没片」。
    """
    base = root / "02-媒体"
    lib = _load(base / "_墙" / "library.json", {}) or {}
    state = _load(base / "_墙" / "watch.json", {}) or {}
    items = lib.get("items") or []
    n_ep = 0
    total = 0
    pending = 0
    cont: list[dict] = []
    last_at = ""
    for it in items:
        eps = it.get("episodes") or []
        n_ep += len(eps)
        total += int(it.get("bytes") or 0)
        if not ((it.get("meta") or {}).get("scraped_at")):
            pending += 1
        st = state.get(it.get("id")) or {}
        up = str(st.get("updated") or "")
        if up > last_at:
            last_at = up
        watched = st.get("watched") or {}
        prog = st.get("progress") or {}
        pick = None
        for e in eps:
            k = str(e.get("index"))
            if watched.get(k):
                continue
            p = prog.get(k) or {}
            pos, dur = float(p.get("pos") or 0), float(p.get("dur") or (e.get("duration") or 0))
            if pos > 3 and dur > 0 and pos / dur < 0.95:
                pick = {"ep": e.get("n") or e.get("index"), "pct": round(pos / dur * 100),
                        "pos": pos, "dur": dur}
                break
        if not pick and watched and len(watched) < len(eps):
            nxt = next((e for e in eps if not watched.get(str(e.get("index")))), None)
            if nxt:
                pick = {"ep": nxt.get("n") or nxt.get("index"), "pct": 0,
                        "pos": 0, "dur": nxt.get("duration") or 0}
        if pick:
            pick.update({"title": (it.get("meta") or {}).get("title") or it.get("title") or "",
                         "at": up, "poster": (it.get("meta") or {}).get("poster")})
            cont.append(pick)
    cont.sort(key=lambda x: x.get("at") or "", reverse=True)
    return {
        "up": _port(8773),
        "titles": len(items),
        "episodes": n_ep,
        "bytes_h": _human(total),
        "pending": pending,
        "library_at": str(lib.get("updated_at") or ""),
        "last_at": last_at,
        "continue": cont[:3],
    }


def _llmcost(root: Path) -> dict:
    """Token 价格追踪的只读快照。

    跟看图/阅读/影音一个口径：**不调 8776 的 HTTP**（双击才起、不常驻），
    直接读 13-算价/out/accuracy.json —— 价格页每次刷新都会重写它。
    文件不在或太旧就老实说「还没跑过」，不编数。
    """
    base = root / "13-算价"
    out = base / "out" / "accuracy.json"
    up = _port(8776)
    d = _load(out, {}) or {}
    if not d:
        return {"ready": False, "up": up}
    counts = d.get("counts") or {}
    api = [r for r in (d.get("api") or []) if r.get("output_cny")]
    api.sort(key=lambda r: r["output_cny"])
    std = [r for r in api if (r.get("price_kind") or "standard") == "standard"]
    cheap = (std or api or [None])[0]
    cheap_any = (api or [None])[0]
    conf = []
    for r in d.get("api") or []:
        for c in (r.get("conflicts") or [])[:1]:
            conf.append({"name": r.get("display") or r.get("model") or "",
                         "provider": r.get("provider") or "",
                         "metric": c.get("metric") or "",
                         "rel": round(float(c.get("rel") or 0) * 100)})
    today = date.today().isoformat()
    plans = [p for p in (d.get("plans") or [])
             if p.get("convertible") and p.get("cny_per_1m")
             and not (p.get("promo_until") and str(p["promo_until"])[:10] < today)]
    plans.sort(key=lambda p: p["cny_per_1m"])
    try:
        age = int((time.time() - out.stat().st_mtime) / 86400)
    except OSError:
        age = 999
    return {
        "ready": True, "up": up,
        "at": str(d.get("data_date") or "")[:10], "age": age,
        "generated_at": str(d.get("generated_at") or "")[:19].replace("T", " "),
        "n_api": len(d.get("api") or []), "n_plans": len(d.get("plans") or []),
        "n_rigs": len(d.get("rigs") or []),
        "counts": counts,
        "obs": int(d.get("observations_total") or 0),
        "n_sources": len(d.get("source_coverage") or {}),
        "fx": d.get("fx_usd_cny"),
        "cheapest": ({"name": cheap["display"], "cny": cheap["output_cny"],
                      "status": cheap.get("status")} if cheap else None),
        "cheapest_any": ({"name": cheap_any["display"], "cny": cheap_any["output_cny"],
                          "status": cheap_any.get("status")} if cheap_any else None),
        "best_plan": ({"name": "%s · %s" % (plans[0].get("platform"), plans[0].get("plan")),
                       "cny": plans[0].get("cny_per_1m")} if plans else None),
        "conflicts": conf[:3],
        "changes": len(d.get("changes") or []),
        "history": _llmcost_history(d),
    }


def _llmcost_history(d: dict) -> dict:
    """价格史摘要：覆盖几期、几次调价、变动最大的那个是哪个。

    手工登记的回溯点也在这份数据里，卡片只报「有几次实证调价」，不替它下结论。
    """
    h = d.get("history") or {}
    sm = h.get("summary") or {}
    if not sm.get("n_models"):
        return {}
    moves = sorted(h.get("changes") or [], key=lambda c: -abs(c.get("pct") or 0))
    top = [{"name": c.get("display") or c.get("model") or "",
            "pct": round(float(c.get("pct") or 0) * 100),
            "date": str(c.get("date") or "")[:10]} for c in moves[:2]]
    pano = d.get("panorama") or {}
    psm = pano.get("summary") or {}
    return {"n_dates": sm.get("n_dates") or 0, "n_changed": sm.get("n_changed") or 0,
            "n_models": sm.get("n_models") or 0,
            "n_platforms": psm.get("n_platforms") or 0,
            "spread_max": psm.get("spread_max") or 0,
            "from": str(sm.get("span_from") or "")[:10],
            "to": str(sm.get("span_to") or "")[:10],
            "level_chg": round(float(sm.get("level_chg") or 0) * 100, 1),
            "top": top}


def _files(root: Path) -> dict:
    inbox = root / "07-收件箱"
    today = date.today().isoformat()
    today_dir = inbox / today
    count = today_count = 0
    today_bytes = 0
    recent: list[dict] = []
    if inbox.exists():
        for p in inbox.rglob("*"):
            if not p.is_file() or p.name.startswith(".") or p.name == "index.jsonl":
                continue
            try:
                st = p.stat()
            except OSError:
                continue
            count += 1
            recent.append({
                "name": p.name, "size": _human(st.st_size), "bytes": st.st_size,
                "mtime": st.st_mtime,
                "rel": str(p.relative_to(inbox)),
                "in_today": today_dir in p.parents,
            })
    for r in recent:
        if r["in_today"]:
            today_count += 1
            today_bytes += r["bytes"]
    recent.sort(key=lambda r: r["mtime"], reverse=True)
    return {
        "inbox": count, "today": today_count, "today_h": _human(today_bytes),
        "recent": [{k: v for k, v in r.items() if k not in ("mtime", "in_today")}
                   for r in recent[:5]],
    }


def _health(root: Path) -> dict:
    base = root / "05-采集" / "体检"
    latest = _load(base / "latest.json", {}) or {}
    checks = latest.get("checks") or {}
    lr = checks.get("last_run") or {}
    tp = checks.get("throughput") or {}
    src = checks.get("sources") or {}
    hist = []
    hp = base / "history.jsonl"
    if hp.exists():
        try:
            lines = hp.read_text(encoding="utf-8").strip().splitlines()[-12:]
            for ln in lines:
                try:
                    hist.append(json.loads(ln))
                except ValueError:
                    continue
        except OSError:
            pass
    return {
        "status": latest.get("status") or "unknown",
        "at": latest.get("checked_at") or "",
        "anomaly_count": int(latest.get("anomaly_count") or 0),
        "anomalies": (latest.get("anomalies") or [])[:5],
        "last_run_at": lr.get("last_run_at") or "",
        "last_status": lr.get("last_status") or "",
        "age_minutes": lr.get("age_minutes"),
        "overdue": bool(lr.get("overdue")),
        "new_items": (tp.get("this_round") or {}).get("new_items"),
        "prev_new_items": (tp.get("previous_round") or {}).get("new_items"),
        "qdrant_ok": (tp.get("qdrant") or {}).get("ok"),
        "qdrant_points": (tp.get("qdrant") or {}).get("points_count"),
        "sources": {
            "failed": len(src.get("failed") or []),
            "kept": (src.get("counts") or {}).get("kept"),
            "duplicate": (src.get("counts") or {}).get("duplicate"),
        },
        "history": [{"at": h.get("checked_at") or "", "status": h.get("status") or "",
                     "anomalies": h.get("anomaly_count") or 0} for h in hist[-8:]],
    }


def _heartbeat() -> dict:
    state = dict(heartbeat.STATE or {})
    last = state.get("last") or {}
    checks = []
    for c in (last.get("checks") or []):
        checks.append({
            "id": c.get("id") or "", "name": c.get("name") or "",
            "ok": bool(c.get("ok")), "level": c.get("level") or "info",
            "detail": c.get("detail") or "",
        })
    return {
        "ok": bool(last.get("ok")) if last else None,
        "at": last.get("at") or state.get("last_at") or "",
        "running": bool(state.get("running")),
        "heal": bool(state.get("heal", True)),
        "checks": checks,
        "bad": [c for c in checks if not c["ok"]],
    }


def _trends(cfg: dict) -> dict:
    latest = ai_trends_runner.latest(cfg) or {}
    srcs = []
    for s in (latest.get("sources") or []):
        srcs.append({
            "source": s.get("source") or "", "ok": bool(s.get("ok")),
            "items": s.get("items") or 0, "detail": s.get("detail") or "",
        })
    stats = latest.get("stats") or {}
    return {
        "date": latest.get("date") or "",
        "status": latest.get("status") or ("未跑" if not latest else ""),
        "stats": {k: stats.get(k) for k in ("new", "day_total", "news", "github", "hf", "crowd")},
        "sources": srcs,
        "failed_sources": [s["source"] for s in srcs if not s["ok"]],
        "next_at": ai_trends_runner.next_run_at(cfg),
        "today_done": ai_trends_runner.already_done(cfg),
        "running": bool(ai_trends_runner.STATE.get("running")),
        "output": latest.get("output") or "",
        "llm_ok": (latest.get("llm") or {}).get("ok"),
        "degraded": latest.get("degraded") or [],
        "history": ai_trends_runner.history(cfg, 7),
    }


def _dedupe() -> dict:
    last = (dedupe_runner.STATE or {}).get("last") or {}
    sc = last.get("scanned") or {}
    return {
        "at": str(last.get("ts") or ""),
        "groups": len(last.get("groups") or []),
        "freed_h": _human(last.get("freed") or 0),
        "dirs": sc.get("dirs"), "images": sc.get("images"),
        "running": bool(dedupe_runner.STATE.get("running")),
        "ms": last.get("ms"),
    }


def _ocr() -> dict:
    st = ocr_runner.STATE or {}
    out = st.get("last_output") or {}
    return {
        "at": st.get("last_at") or "",
        "done": st.get("done") or 0, "failed": st.get("failed") or 0,
        "scanned": st.get("scanned") or 0,
        "chars": out.get("chars") or 0,
        "seconds": out.get("seconds") or 0,
        "running": bool(st.get("running")),
        "error": out.get("error") or "",
    }


def _disk(root: Path) -> dict:
    try:
        # statvfs 是 Unix 专有；Windows 上拿 shutil.disk_usage 顶，返回值语义一致
        if hasattr(os, "statvfs"):
            u = os.statvfs(str(root))
            total = u.f_blocks * u.f_frsize
            free = u.f_bavail * u.f_frsize
        else:
            import shutil as _shutil
            _u = _shutil.disk_usage(str(root))
            total, free = _u.total, _u.free
        used = total - free
        return {"used_pct": _pct(used, total), "free_h": _human(free),
                "total_h": _human(total), "used_h": _human(used)}
    except (OSError, AttributeError, ValueError):
        return {"used_pct": 0, "free_h": "", "total_h": "", "used_h": ""}


# --------------------------------------------------------------------------
# 使用报告：只读索引，不在这里生成
#
# 生成是 report_runner 的事（每小时一次、幂等补缺）。工作台每次刷新都重生一份
# 报告没有意义，而且会把「报告什么时候出的」变成「最后一次打开工作台的时间」。
# 这里只回答两件事：有没有报告、最新那份的头一句是什么。

def _report(root: Path) -> dict:
    base = root / "10-系统记录" / "使用报告"
    idx = base / "_索引.json"
    out = {"count": 0, "latest": None, "updated_at": "", "recent": []}
    if not idx.exists():
        return out
    try:
        data = json.loads(idx.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return out
    items = data.get("items") or []
    out["count"] = len(items)
    out["updated_at"] = data.get("updated_at") or ""
    days = [i for i in items if i.get("kind") == "day" and i.get("key")]
    if days:
        top = max(days, key=lambda x: x["key"])
        out["latest"] = {"key": top["key"], "file": top.get("file") or "",
                         "head": top.get("head") or ""}
    out["recent"] = [{"kind": i.get("kind"), "key": i.get("key"), "file": i.get("file")}
                     for i in items[:6]]
    return out


# --------------------------------------------------------------------------
# 巡检的账本（2026-11-21：「工作台 ?v=ops 是巡检唯一的脸」）
#
# 巡检原来还有自己的页面（8774 的 inspect_ui.html，六张卡）。用户定了方向：
# 工作台 ?v=ops 才是它唯一的脸，8774 只留 /api/*（页面入口已经重定向回去）。
# 所以这里把三份 jsonl + 报告目录一次读全，页面不再需要第二个请求。
#
#   · 历史.jsonl     早期「只检查」那批（2026-11-19 之前，source=手动）
#   · 修复记录.jsonl  现在每轮一条 kind=round（检查 + 按需自愈）
#   · 宕机.jsonl     中断区间（巡检启动时自己补记的 + 手工补的）
#
# 两套字段名不同（passed/total  vs  checks_passed/checks_total），这里统一
# 成一套再给前端，省得页子里写 if。verdict 也在后端定好 —— 前端只管排版。
# --------------------------------------------------------------------------

def _human_span(sec: float) -> str:
    """秒 → 「2 天 3 小时」这种。只在巡检这块用，所以没放进公共工具。"""
    sec = max(0, int(sec or 0))
    d, r = divmod(sec, 86400)
    h, r = divmod(r, 3600)
    m = r // 60
    if d:
        return "%d 天 %d 小时" % (d, h)
    if h:
        return "%d 小时 %d 分" % (h, m)
    return ("%d 分" % m) if m else "刚刚"


def _short_title(reason: str, limit: int = 12) -> str:
    """从原因里截一句短标题（老记录没写 title 时的兜底）。

    和 syscheck.py 的 down_title() 同一个口径 —— 那边写进去，这边只是兜住
    2026-11-21 之前留下的那几条；两边都不许把整段话放到一行里。
    """
    s = (reason or "").strip().replace("\n", " ")
    for sep in ("。", "；", "：", "（", "(", ";", "，", ","):
        i = s.find(sep)
        if 0 < i <= limit:
            return s[:i]
    s = s.split("。")[0]
    return (s[:limit] + "…") if len(s) > limit else (s or "巡查中断")


def _inspect(root: Path) -> dict:
    base = root / "10-系统记录" / "巡检"

    def _lines(name: str) -> list:
        return _tail_jsonl(base / name, 400)

    rows_at: dict = {}

    for r in _lines("历史.jsonl"):
        at = str(r.get("at") or "")
        if not at:
            continue
        d = rows_at.setdefault(at, {"at": at, "kind": "check"})
        d.update({"passed": r.get("passed"), "total": r.get("total"),
                  "disk": r.get("disk_pct"), "mem": r.get("mem_pct"),
                  "load": r.get("load1"),
                  "up": r.get("services_up"), "svc": r.get("services_total"),
                  "failed": (r.get("failed") or [])[:3],
                  "down_svc": (r.get("services_down") or [])[:4],
                  "ok": bool(r.get("ok")), "src": r.get("source") or ""})

    for r in _lines("修复记录.jsonl"):
        # kind=item 是「某个检查项结论变了」的单条，跟 round 共用 at。
        # 不过滤会把同一时刻那轮的 passed/total 覆盖成 None（列表里就变成只有「通过」没细节）。
        if (r.get("kind") or "round") != "round":
            continue
        at = str(r.get("at") or "")
        if not at:
            continue
        d = rows_at.setdefault(at, {"at": at, "kind": "round"})
        unresolved = [u.get("name") if isinstance(u, dict) else str(u)
                      for u in (r.get("unresolved") or [])][:3]
        d.update({"passed": r.get("checks_passed"), "total": r.get("checks_total"),
                  "up": r.get("services_up"), "svc": r.get("services_total"),
                  "fixed": int(r.get("fixed") or 0),
                  "bad": int(r.get("failed") or 0) + int(r.get("manual") or 0),
                  "unresolved": unresolved,
                  # heal=false 的轮就是「只检查」，别把它写成自愈
                  "src": "巡检 + 自愈" if r.get("heal") else "只检查"})

    # 报告文件名就是那一轮的时刻：20261120-222701.md → 2026-11-20 22:27:01
    reps: dict = {}
    try:
        for p in sorted((base / "报告").glob("2*.md"))[-80:]:
            k = p.stem
            if len(k) == 15 and k[8] == "-":
                reps["%s-%s-%s %s:%s:%s" % (k[0:4], k[4:6], k[6:8],
                                            k[9:11], k[11:13], k[13:15])] = p.name
    except OSError:
        pass

    for at, d in rows_at.items():
        if at in reps:
            d["report"] = reps[at]
        if d["kind"] == "round":
            bad, fixed = d.get("bad") or 0, d.get("fixed") or 0
            if bad:
                d.update(level="bad", verdict="%d 项没修好" % bad)
            elif fixed:
                d.update(level="warn", verdict="修了 %d 项" % fixed)
            else:
                d.update(level="ok", verdict="通过")
        else:
            ok = bool(d.get("ok"))
            d.update(level="ok" if ok else "bad", verdict="通过" if ok else "有异常")
        # 一行里要能看出「检查过了 / 服务在不在」；按需服务本来就没开，
        # 所以这里说的是「常驻 N/M」，别写成「服务 5/11」让人以为掉了 6 个。
        parts = []
        p, t = d.get("passed"), d.get("total")
        if p is not None and t:
            parts.append("%s/%s 检查" % (p, t))
        up, svc = d.get("up"), d.get("svc")
        if up is not None and svc:
            parts.append("常驻 %s/%s" % (up, svc))
        d["brief"] = " · ".join(parts)

    rows = sorted(rows_at.values(), key=lambda x: x["at"], reverse=True)[:40]

    downs = []
    for r in _lines("宕机.jsonl")[-6:]:
        reason = (r.get("reason") or "").strip()
        downs.append({"from": r.get("from") or "", "to": r.get("to") or "",
                      "duration": r.get("duration") or _human_span(r.get("seconds") or 0),
                      "seconds": int(r.get("seconds") or 0),
                      # title 是一行里那句短的，reason 是点开才看的全文；
                      # 老记录没写 title 就先拿 reason 头顶上那句兜着，别在页面上露一整段
                      "title": (r.get("title") or "").strip() or _short_title(reason),
                      "reason": reason,
                      "fix": (r.get("fix") or "").strip(),
                      "source": r.get("source") or ""})

    alive: dict = {}
    try:
        alive = json.loads((base / "在线.json").read_text(encoding="utf-8")) or {}
    except (OSError, ValueError):
        pass
    alive_at = str(alive.get("at") or "")
    up_span, fresh = "", False
    if alive_at:
        try:
            fresh = (datetime.now()
                     - datetime.strptime(alive_at, "%Y-%m-%d %H:%M:%S")).total_seconds() < 180
        except ValueError:
            fresh = False
    if alive.get("started"):
        try:
            up_span = _human_span((datetime.now() - datetime.strptime(
                str(alive["started"]), "%Y-%m-%d %H:%M:%S")).total_seconds())
        except ValueError:
            up_span = ""

    return {
        "rows": rows,
        "downs": downs,
        "reports": len(reps),
        "alive": {"at": alive_at, "pid": alive.get("pid"),
                  "started": alive.get("started") or "",
                  "always": bool(alive.get("always")),
                  "up": up_span, "fresh": fresh},
    }


# --------------------------------------------------------------------------
# 工作记录（worklog）：两张卡回答两个不同的问题
#
#   左卡「口袋工作」＝ 口袋自己干的活。数据全部来自 pocket.jsonl（抓取/文章/
#     上传/OCR/新闻/去重的开始与结束，heartbeat 每轮结果也镜像在里面），
#     每次请求现算、不另存一份。**不收补记** —— 手写的那几条记的是「某个
#     AI 客户端改了 pocket 的代码」，那属于客户端的工作（见 _worklog 注释）。
#
#   右卡「本机工作」＝ 这台电脑上别的活。三个来源：各 AI 客户端（_CLIENTS
#     注册表：WorkBuddy 会话 jsonl、Cherry Studio 的 sqlite）、pocket 一天一条、
#     以及 <数据根>/_工作台/worklog.json 里补记的条目（POST /api/worklog）。
#
#   * 空跑不进列表：巡检正常、OCR 扫到 0 张、调度器报「下次几点」都不算工作，
#     否则两分钟一条心跳把真正的事全埋了。
#   * 每条都带 client（哪个客户端 / pocket / 空＝补记时没标），右卡据此分筛。
#
#   status: ok 成功 / fail 失败 / run 进行中 / skip 跳过（识别成图集之类）
# --------------------------------------------------------------------------

_WL_LIMIT = 90            # 左卡返回给页面的条数（中台四个插件并一份，60 条不够看）
_WL_KEEP = 300            # 补记文件最多留多少条
_WL_RUN_WINDOW = 120      # 「开始了还没结束」多久之内算进行中，过期不再挂

# 有账、但没有页面因而不在 plugins.json 里的那几栏，给个中文名。
# 不写进 plugins.json：那里每一条都会在顶栏长出一颗药丸，点开得有地方去。
_PG_LABEL = {"apps": "App 监工", "media": "影音室"}


def worklog_path(cfg: dict) -> Path:
    return _root(cfg) / "_工作台" / "worklog.json"


def _tail_jsonl(path: Path, limit: int = 1200) -> list[dict]:
    """只读尾部。append-only 的日志越早的行越没用，整读迟早拖慢每分钟一次的刷新。

    从中间切进去第一行可能半个 JSON，解析失败跳过即可 —— 这也是这里不
    用 `read_text().splitlines()` 的原因。
    """
    try:
        with path.open("rb") as f:
            f.seek(0, os.SEEK_END)
            size = f.tell()
            f.seek(max(0, size - 512 * 1024))
            raw = f.read()
    except OSError:
        return []
    out: list[dict] = []
    for ln in raw.decode("utf-8", "replace").splitlines():
        ln = ln.strip()
        if not ln:
            continue
        try:
            d = json.loads(ln)
        except ValueError:
            continue
        if isinstance(d, dict):
            out.append(d)
    return out[-limit:]


def _tail_by_event(path: Path, events: set, days: int = 2) -> list[dict]:
    """按**时间**取尾部，只留指定 event（`_tail_jsonl` 是按行数的）。

    为什么要另一个：pocket.jsonl 里 heartbeat/ocr 两分钟一条，1200 行只够
    回看十几个小时；而「传到手机」这种一天可能就一两条，按行数切必然被
    冲掉 —— 用户问的正是「昨天发的那张图什么时候发的」，账本里明明有。
    从文件尾按块倒读，遇到早于 cutoff 的行就停，代价只跟这几天的量有关。
    """
    cutoff = (datetime.now() - timedelta(days=days)).strftime("%Y-%m-%dT%H:%M")
    out: list[dict] = []
    try:
        with path.open("rb") as f:
            f.seek(0, os.SEEK_END)
            pos = f.tell()
            tail = b""                      # 上一块开头那半行，拼到这一块末尾
            stop = False
            while pos > 0 and not stop:
                step = min(128 * 1024, pos)
                pos -= step
                f.seek(pos)
                lines = (f.read(step) + tail).split(b"\n")
                tail = lines.pop(0) if pos > 0 else b""
                for raw in reversed(lines):
                    raw = raw.strip()
                    if not raw:
                        continue
                    try:
                        d = json.loads(raw.decode("utf-8", "replace"))
                    except ValueError:
                        continue
                    if not isinstance(d, dict):
                        continue
                    ts = str(d.get("ts") or "")
                    if ts and ts[:16] < cutoff:     # 这一块已经比 cutoff 老，收工
                        stop = True
                        break
                    if str(d.get("event") or "") in events:
                        out.append(d)
            if tail.strip():                # 文件第一行（没有换行在它前面）
                try:
                    d = json.loads(tail.decode("utf-8", "replace"))
                    if (isinstance(d, dict)
                            and str(d.get("event") or "") in events
                            and str(d.get("ts") or "")[:16] >= cutoff):
                        out.append(d)
                except ValueError:
                    pass
    except OSError:
        return []
    out.reverse()
    return out


def _iso(v) -> str:
    """'2026-11-18T16:45:09+0800' / '2026-11-18 16:45:09.123' → '2026-11-18 16:45:09'。"""
    s = str(v or "").strip().replace("T", " ")
    return s[:19] if len(s) >= 19 else s


def _secs(a: str, b: str) -> int | None:
    try:
        ta = datetime.strptime(a[:19], "%Y-%m-%d %H:%M:%S")
        tb = datetime.strptime(b[:19], "%Y-%m-%d %H:%M:%S")
    except (ValueError, TypeError):
        return None
    return max(0, int((tb - ta).total_seconds()))


def _code(name: str) -> dict:
    return {"kind": "code", "name": name}


def _ai(name: str) -> dict:
    return {"kind": "ai", "name": name}


def _tool(name: str) -> dict:
    """执行者不止「脚本」和「模型」两种：口袋是服务，WorkBuddy 是客户端。
    混进 code 里会让人以为是个 .py 文件干的。"""
    return {"kind": "tool", "name": name}


def _local_model(cfg: dict) -> str:
    """口袋内嵌的模型：config.model 形如 qwen3.5-4b-gguf:q4km（ollama 本地）。

    取冒号前、去掉 -gguf 后缀 —— 「qwen3.5-4b-gguf」挂在页面上是没必要的噪音，
    gguf 是格式不是型号。
    """
    raw = str(cfg.get("model") or "").split(":")[0].strip()
    return re.sub(r"[-_](gguf|ggml)$", "", raw, flags=re.I) or "本机模型"


def _state_of(state) -> str:
    s = str(state or "").strip().lower()
    if s in ("done", "ok", "success", "fine"):
        return "ok"
    if s in ("gallery", "dup", "read", "skip", "skipped", "unsupported"):
        return "skip"
    if s in ("run", "running", "start", "started", "pending"):
        return "run"
    return "fail"


# —— 行程：一段话 → 结构化（2026-11-18）—————————————
# 4B 模型读中文口语，拿手的是「从哪到哪、几号几点、多少钱」这几件事，
# 但它也会漏字段、把 JSON 说走样。所以：模型先来一遍，字段逐个校验；
# 模型不在线或吐出垃圾就退回正则。这条路必须永远能出结果 ——
# 用户写了一段话点「读一下」，什么都不发生比解析得糙更糟。

_EV_OLLAMA = "http://127.0.0.1:11434/api/generate"

EV_PROMPT = """从下面这段中文里抽出行程信息，只输出一个 JSON 对象，不要解释、不要代码块。

字段（没有的就填空字符串 "" 或空数组 []）：
- title: 一句话概括，不超过 12 字
- date: 出发日期，YYYY-MM-DD
- time: 出发时刻，HH:MM
- endDate: 到达日期，YYYY-MM-DD（当天到达就留空）
- endTime: 到达时刻，HH:MM
- from: 出发地
- stops: 途经地，数组
- to: 到达地
- flight: 航班号或车次
- fee: 路费，只要数字
- delay: 延误分钟数，只要数字

今天是 {today}（{wd}）。文中「明天」「后天」「下周三」都按今天推算。

文字：
{text}

JSON:"""


def _llm_json(prompt: str, model: str, num_predict: int = 420, timeout: float = 75) -> str:
    body = json.dumps({
        "model": model, "prompt": prompt, "stream": False, "think": False,
        "format": "json",                      # 让 ollama 自己兜住 JSON 形状
        "options": {"temperature": 0.1, "num_predict": num_predict, "top_p": 0.9},
    }).encode()
    req = urllib.request.Request(_EV_OLLAMA, data=body,
                                 headers={"Content-Type": "application/json"})
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))   # 本机地址别走代理
    with opener.open(req, timeout=timeout) as r:
        d = json.loads(r.read())
    return str(d.get("response") or "")


def _grab_json(raw: str) -> dict:
    """模型爱在 JSON 外面包 ```json 或者写两句废话，剥出来再解析。"""
    s = (raw or "").strip()
    s = re.sub(r"^```[a-zA-Z]*\s*", "", s)
    s = re.sub(r"\s*```$", "", s).strip()
    i, j = s.find("{"), s.rfind("}")
    if i >= 0 and j > i:
        s = s[i:j + 1]
    try:
        d = json.loads(s)
    except ValueError:
        return {}
    return d if isinstance(d, dict) else {}


def _num(v) -> float | None:
    if isinstance(v, (int, float)):
        return float(v)
    m = re.search(r"-?\d+(?:\.\d+)?", str(v or "").replace(",", ""))
    return float(m.group()) if m else None


def _ok_date(s: str) -> str:
    try:
        return datetime.strptime(str(s)[:10], "%Y-%m-%d").strftime("%Y-%m-%d")
    except (ValueError, TypeError):
        return ""


def _ok_time(s: str) -> str:
    m = re.match(r"^\s*(\d{1,2})[:：.点](\d{1,2})?\s*$", str(s or ""))
    if not m:
        return ""
    h, mi = int(m.group(1)), int(m.group(2) or 0)
    if h > 23 or mi > 59:
        return ""
    return f"{h:02d}:{mi:02d}"


def _cn_date(text: str, today: date) -> tuple[str, str]:
    """把「11月18日」「11/18」「明天」「下周三」认成 YYYY-MM-DD。
    返回 (日期, 匹配到的原文)，认不出就空。"""
    t = text or ""
    m = re.search(r"(20\d{2})\s*[-/.年]\s*(\d{1,2})\s*[-/.月]\s*(\d{1,2})", t)
    if m:
        d = _ok_date(f"{m.group(1)}-{int(m.group(2)):02d}-{int(m.group(3)):02d}")
        return d, m.group()
    m = re.search(r"(\d{1,2})\s*月\s*(\d{1,2})\s*[日号]", t)
    if m:
        d = _ok_date(f"{today.year}-{int(m.group(1)):02d}-{int(m.group(2)):02d}")
        return d, m.group()
    m = re.search(r"(?<!\d)(\d{1,2})[/／](\d{1,2})(?!\d)", t)
    if m:
        d = _ok_date(f"{today.year}-{int(m.group(1)):02d}-{int(m.group(2)):02d}")
        return d, m.group()
    for word, off in (("大后天", 3), ("后天", 2), ("明天", 1), ("今天", 0), ("今晚", 0), ("明早", 1)):
        if word in t:
            return (today + timedelta(days=off)).strftime("%Y-%m-%d"), word
    m = re.search(r"(下周|礼拜|周|星期)([一二三四五六日天])", t)
    if m:
        want = "一二三四五六日".find(m.group(2).replace("天", "日"))
        if want >= 0:
            if m.group(1) == "下周":
                # 下周三 = 下周一 + 2，不是「今天 +7 +2」
                base = today + timedelta(days=7 - today.weekday())
                d = base + timedelta(days=want)
            else:
                d = today + timedelta(days=(want - today.weekday()) % 7)
            return d.strftime("%Y-%m-%d"), m.group()
    return "", ""


def _ev_norm(d: dict, today: date) -> dict:
    """模型给的字段一律过一遍：类型不对就当没给，别把脏数据写进 notes.json。"""
    out = {
        "title": "", "date": "", "time": "", "endDate": "", "endTime": "",
        "from": "", "stops": [], "to": "", "flight": "", "fee": None, "delay": None,
    }
    if not isinstance(d, dict):
        return out
    out["title"] = re.sub(r"\s+", " ", str(d.get("title") or "")).strip()[:24]
    out["date"] = _ok_date(d.get("date"))
    out["time"] = _ok_time(d.get("time"))
    out["endDate"] = _ok_date(d.get("endDate"))
    out["endTime"] = _ok_time(d.get("endTime"))
    for k in ("from", "to"):
        out[k] = _clean_place(d.get(k))[:24]
    stops = d.get("stops")
    if isinstance(stops, str):
        stops = [x for x in re.split(r"[,，、/]", stops)]
    if isinstance(stops, list):
        out["stops"] = [re.sub(r"\s+", " ", str(x)).strip()[:24] for x in stops if str(x).strip()][:6]
    fl = re.sub(r"\s+", "", str(d.get("flight") or "")).upper()
    out["flight"] = fl[:12]
    fee = _num(d.get("fee"))
    out["fee"] = round(fee, 2) if fee is not None and 0 <= fee < 1e6 else None
    dl = _num(d.get("delay"))
    out["delay"] = int(dl) if dl and 0 < dl < 10000 else None
    # 跨天但只给了到达时刻：按「比出发晚」补日期
    if out["date"] and out["endTime"] and not out["endDate"]:
        if out["time"] and out["endTime"] < out["time"]:
            out["endDate"] = (datetime.strptime(out["date"], "%Y-%m-%d") + timedelta(days=1)).strftime("%Y-%m-%d")
    # 只有日期没有到达时刻的「跨天」是模型瞎猜的，清掉
    if out["endDate"] and (not out["endTime"] or out["endDate"] <= out["date"]):
        out["endDate"] = ""
    return out


def _clean_place(s: str) -> str:
    """「下周三从家」「明早去机场」这类把时间词蹭进地点的，剥干净。
    剥空了就当没抓到 —— 宁可让用户自己填，也别把「周五」当地名存下去。"""
    s = str(s or "").strip(" 的，,。")
    s = re.sub(r"^(大后天|后天|明天|今天|今晚|明早|下周[一二三四五六日天]?|(?:周|星期)[一二三四五六日天]"
               r"|凌晨|上午|中午|下午|晚上|早上|半夜|从|到|去|往|在)", "", s).strip()
    s = re.sub(r"^(大后天|后天|明天|今天|今晚|明早|凌晨|上午|中午|下午|晚上|早上|半夜|从|到|去|往|在|的)", "", s).strip()
    return s if len(s) >= 1 else ""


def _ev_rule(text: str, today: date) -> dict:
    """正则兜底，同时也是「硬字段」的裁判：原文里写死的日期/时刻/航班/金额
    一律以这里为准 —— 4B 算日子最不稳（能把 11月20日 数成 19 日），
    但这几样正则是确定性的。"""
    t = text or ""
    out = _ev_norm({}, today)
    out["date"], _ = _cn_date(t, today)

    hms = re.findall(r"(?<!\d)(\d{1,2})[:：](\d{2})(?!\d)", t)
    if hms:
        out["time"] = _ok_time(f"{hms[0][0]}:{hms[0][1]}")
        if len(hms) > 1:
            out["endTime"] = _ok_time(f"{hms[1][0]}:{hms[1][1]}")

    fl = re.search(r"\b([A-Z]{2}\d{3,4}|[GDCKZTL]\d{1,4}|\d{3,4}\s*次)\b", t.upper())
    if fl:
        out["flight"] = re.sub(r"\s*次$", "", fl.group(1))

    m = re.search(r"(?:路费|票价|机票|车费|花了|费用)[^\d]{0,6}(\d+(?:\.\d+)?)", t)
    if m:
        out["fee"] = float(m.group(1))
    m = re.search(r"(?:延误|晚点|delay)[^\d]{0,6}(\d+)\s*(?:分钟|min)?", t)
    if m:
        out["delay"] = int(m.group(1))

    m = re.search(r"从\s*([^\s，,。到]{2,14})\s*(?:飞|坐|乘|开车|出发)?\s*到\s*([^\s，,。]{2,14})", t)
    if not m:
        m = re.search(r"([^\s，,。]{2,12})\s*(?:→|->|到|飞)\s*([^\s，,。]{2,12})", t)
    if m:
        out["from"], out["to"] = _clean_place(m.group(1)), _clean_place(m.group(2))
    return out


def parse_event_text(cfg: dict, text: str) -> dict:
    """一段话 → 行程字段。返回 {ok, source, event, note, ms}。"""
    text = re.sub(r"\s+", " ", str(text or "")).strip()
    if not text:
        return {"ok": False, "source": "", "event": {}, "note": "没有文字", "ms": 0}
    today = date.today()
    wd = "周" + "一二三四五六日"[today.weekday()]
    res = {"ok": True, "source": "rule", "event": {}, "note": "", "ms": 0}
    t0 = time.time()

    try:
        raw = _llm_json(EV_PROMPT.format(today=today.isoformat(), wd=wd, text=text[:1500]),
                        _local_model_cfg(cfg))
        got = _ev_norm(_grab_json(raw), today)
        # 模型给出「出发/到达」或「航班」才算真读懂了；只回一个标题等于没干活
        if got["from"] or got["to"] or got["flight"] or got["endTime"]:
            hard = _ev_rule(text, today)
            # 原文写死的这几样压过模型：日期/时刻/航班/金额/延误都有确定性写法
            for k in ("date", "time", "endTime", "flight", "fee", "delay"):
                if hard.get(k) not in (None, ""):
                    got[k] = hard[k]
            for k in ("from", "to"):
                if not got.get(k) and hard.get(k):
                    got[k] = hard[k]
            if not got.get("stops") and hard.get("stops"):
                got["stops"] = hard["stops"]
            if not got["date"]:
                got["date"] = today.isoformat()
            res.update(source="llm", event=got)
        else:
            res.update(source="rule", event=_ev_rule(text, today), note="模型没抽出关键信息，用了规则解析")
    except Exception as e:                                     # noqa: BLE001
        res.update(source="rule", event=_ev_rule(text, today), note=f"模型不可用（{type(e).__name__}），用了规则解析")
    res["ms"] = int((time.time() - t0) * 1000)
    ev = res["event"]
    if not ev.get("date"):
        ev["date"] = today.isoformat()
    if not ev.get("title"):
        ev["title"] = ("去" + ev["to"]) if ev.get("to") else (text[:16] or "行程")
    return res


def _local_model_cfg(cfg: dict) -> str:
    """config.model 是 ollama 里的完整名（qwen3.5-4b-gguf:q4km），调接口必须用全名。"""
    return str(cfg.get("model") or "").strip() or "qwen3.5-4b-gguf:q4km"


# ---------------------------------------------------------------- 记录 → 去哪儿
# 一条工作记录点一下该落到哪儿：
#   存图 → 那个相册（看图 8771 `?dir=`）、存文章 → 那篇（阅读 8772 `?a=`）、
#   巡检 → 8774；只能落到工具首页的就给首页，工作台自己的卡给 `#card=` 锚点（同页跳）。
# 端口一律走 BUILTIN_APPS，不在这里手抄第二份。
def _app_base(app_id: str) -> str:
    for a in BUILTIN_APPS:
        if a.get("id") == app_id:
            return str(a.get("url") or "")
    return ""


def _go(app_id: str, path: str = "") -> dict:
    """给一条记录配跳转目标。path 形如 `?dir=…`、`?a=…`，或 `#card=trends`。"""
    if path.startswith("#"):
        return {"url": path, "same": True}      # 同页滚到那张卡，不新开标签
    # 巡检 2026-11-21 收敛：它没有自己的页面了（8774 的 / 302 回工作台），
    # 所以「去巡检」直接给工作台那一页的日志卡。不特判的话下面会把
    # `/dash?v=ops` 当成服务根地址拼成 `/dash?v=ops/` —— 一个打不开的 URL。
    if app_id == "inspect":
        return {"url": "/dash?v=ops#card=inspLog", "app": app_id}
    base = _app_base(app_id)
    if not base:
        return {}
    return {"url": base.rstrip("/") + "/" + path.lstrip("/"), "app": app_id}


def _ext(url) -> dict:
    """存失败的那条：目标就是用户当初粘进来的那个链接。

    存不进去也要把链接还给他 —— 页面上点这一行就打开原网页，手动存。
    ext=True 让前端别去查端口/拉服务，直接开。
    """
    u = str(url or "").strip()
    return {"url": u, "ext": True} if u.startswith("http") else {}


def _auto_worklog(cfg: dict) -> list[dict]:
    rows = _tail_jsonl(BASE / "pocket.jsonl")
    model = _local_model(cfg)
    now = datetime.now()
    now_s = now.strftime("%Y-%m-%d %H:%M:%S")
    items: list[dict] = []
    open_start: dict[str, str] = {}
    dd_last = (dedupe_runner.STATE or {}).get("last") or {}

    def push(task: str, status: str, start: str, end: str, detail: str,
             sources: list[dict], rid: str = "", link: dict | None = None) -> None:
        it = {
            "id": rid or f"a{len(items)}-{start}",
            "task": task, "detail": detail, "status": status,
            "start": start, "end": end,
            "secs": _secs(start, end) if (start and end) else None,
            "sources": sources,
            "plugin": "pocket",     # 中台卡按插件分栏用（见 _plugin_items）
        }
        if link:
            it["link"] = link       # 空目标不写这个键，前端就不会把它画成能点
        items.append(it)

    # —— 传输助手 → 手机：一次点选 N 张会按张记 N 条（同秒同 kind），先聚成一批 ——
    # 2026-11-22：这 50+ 条 tf_push 之前一条都没进工作台 —— 下面那条 if 链里
    # 没有它的分支，就安安静静被丢掉了。用户想找「这张图什么时候发的」
    # 只能翻手机，账本里明明有。归类的 if 链是白名单，加 event 必须同步加分支。
    tf_batch: dict = {}
    for d in _tail_by_event(BASE / "pocket.jsonl", {"tf_push"}, days=2):
        k = (str(d.get("ts") or ""), str(d.get("kind") or ""))
        b = tf_batch.setdefault(k, {"total": 0, "ok": 0, "attempt": 0,
                                    "names": [], "error": ""})
        b["total"] += 1
        if d.get("ok"):
            b["ok"] += 1
        elif not b["error"]:
            b["error"] = str(d.get("error") or "")
        b["attempt"] = max(b["attempt"], int(d.get("attempt") or 0))
        nm = str(d.get("name") or "")
        if nm and nm not in b["names"]:
            b["names"].append(nm)
    # 3 张图 × 3 次重试 = 9 条 = 3 个 batch。落成 3 行「3 张没到」太吵：
    # 同一组文件名在 30 分钟内反复出现，就是同一次投递的多次尝试，只留最后一次。
    def _ts_epoch(s: str) -> float:
        try:
            return datetime.strptime(s[:19], "%Y-%m-%dT%H:%M:%S").timestamp()
        except ValueError:
            return 0.0

    tf_alias: dict = {}             # 被合并掉的旧 key → 保留的那批
    tf_keep: dict = {}              # 文件名签名 → 当前保留的 key
    for k in sorted(tf_batch):
        b = tf_batch[k]
        sig = (k[1], tuple(sorted(b.get("names") or [])) or (k[0],))
        prev = tf_keep.get(sig)
        if prev and _ts_epoch(k[0]) - _ts_epoch(prev[0]) <= 1800:
            # 结果以**后一批**为准：前两次 3 张没到、第三次 3 张到了，
            # 就记「3 张到手机（试了 3 次）」，不能把前一回的报错继承下来。
            b["attempt"] = max(int(tf_batch[prev].get("attempt") or 0),
                               int(b.get("attempt") or 1))
            tf_alias[prev] = k
            del tf_batch[prev]
        tf_keep[sig] = k

    tf_seen: set = set()            # 同一批只落一行

    def tf_push_item(k: tuple, b: dict) -> None:
        at = _iso(k[0])
        total = int(b.get("total") or 1)
        got = int(b.get("ok") or 0)
        kind = {"image": "图片", "file": "文件", "video": "视频",
                "text": "文字"}.get(k[1], "媒体")
        unit = {"image": "张", "text": "条"}.get(k[1], "个")
        if got >= total:
            detail = f"{total} {unit}到手机"
        elif got:
            detail = f"{got}/{total} {unit}到手机"     # 部分没到也要看得见
        else:
            detail = f"{total} {unit}没到"
        if b.get("error"):
            detail += " · " + str(b["error"])[:60]
        if int(b.get("attempt") or 0) > 1:
            detail += f"（试了 {int(b['attempt'])} 次）"
        push(f"传到手机 · {kind}", "ok" if got >= total else "fail", at, at,
             detail, [_code("transfer.py")], "tf" + at,
             _go("", "#card=transfer"))    # 点这行滚回传输卡，东西就在那儿

    for d in rows:
        ev = str(d.get("event") or "")
        at = _iso(d.get("ts"))
        rid = str(d.get("id") or "")

        # —— 抓取：start/done 同 id 配对，配不上就用 done 的时间顶开始时间 ——
        if ev in ("link_start", "article_start"):
            if rid:
                open_start[("link" if ev.startswith("link") else "article") + ":" + rid] = at
            continue

        if ev == "link_done":
            state = _state_of(d.get("state"))
            plat = str(d.get("platform") or "链接")[:16]
            n = int(d.get("images") or 0)
            start = open_start.pop("link:" + rid, at)
            if state == "ok":
                # `dir` 是相对 04-存图 的相册路径（小红书/不哭了…）：能钻进那个相册
                _dir = str(d.get("dir") or "")
                push(f"存图 · {plat}", "ok", start, at,
                     (_dir + (f"（{n} 张）" if n else "")).strip(),
                     [_code("cuntu.py")], "l" + rid,
                     _go("gallery", "?dir=" + urllib.parse.quote(_dir)) if _dir else _go("gallery"))
            elif state == "skip":
                push(f"存图 · {plat}", "skip", start, at,
                     f"不支持或重复：{d.get('state')}", [_code("cuntu.py")], "l" + rid,
                     _ext(d.get("url")))
            else:
                why = {"error": "抓取出错", "unsupported": "不支持的链接"}.get(
                    str(d.get("state") or ""), str(d.get("state") or "抓取失败"))
                # 优先用抓取器自报的原因（「短链解析不出 noteId」这种），
                # 没有才退回笼统的「抓取出错」。
                push(f"存图 · {plat}", "fail", start, at,
                     str(d.get("error") or d.get("message") or why)[:160],
                     [_code("cuntu.py")], "l" + rid,
                     _ext(d.get("url")))      # 失败也能点开原链接，手动存
            continue

        if ev == "article_done":
            kind = str(d.get("kind") or "fetch")
            state = _state_of(d.get("state"))
            chars = int(d.get("chars") or 0)
            rel = str(d.get("rel") or "")
            start = open_start.pop("article:" + rid, at)
            src = [_code("article.py")]
            if kind == "classify":
                src.append(_ai(model))
            if kind == "gallery":
                # 判成图集后已经改存到看图那边了，跳过去就是它的新家
                push("存文章", "skip", start, at, "识别成图集，已分流到存图",
                     [_code("article.py")], "a" + rid, _go("gallery"))
            elif kind == "classify":
                push("文章分类", state, start, at,
                     rel + (f"（{d.get('topic')}）" if d.get("topic") and state == "ok" else ""),
                     src, "a" + rid,
                     _go("reader", "?a=" + urllib.parse.quote(rel)) if (rel and state == "ok") else None)
            elif state == "ok":
                push("存文章", "ok", start, at,
                     (f"{rel}（{chars} 字）" if rel else f"{chars} 字"),
                     src, "a" + rid,
                     _go("reader", "?a=" + urllib.parse.quote(rel)) if rel else _go("reader"))
            else:
                push("存文章", "fail", start, at,
                     str(d.get("error") or "抓取失败")[:160], src, "a" + rid,
                     _ext(d.get("url")))
            continue

        if ev == "tf_push":
            k = (str(d.get("ts") or ""), str(d.get("kind") or ""))
            if k in tf_seen or k in tf_alias:   # 同批只落一行；被合并的旧批直接跳过
                continue
            tf_seen.add(k)
            tf_push_item(k, tf_batch.get(k) or {"total": 1})
            continue

        if ev == "upload":
            saved = int(d.get("saved") or 0)
            failed = int(d.get("failed") or 0)
            names = d.get("names") or []
            one = str(names[0])[:40] if len(names) == 1 else ""
            push("收件箱上传", "fail" if (failed and not saved) else "ok", at, at,
                 (one or f"{saved} 个文件") + (f"，{failed} 个失败" if failed else "")
                 + (f" · {int(d.get('ms') or 0)}ms" if d.get("ms") else ""),
                 [_code("server.py")], "u" + at)
            continue

        if ev == "ocr_sweep":
            done = int(d.get("done") or 0)
            failed = int(d.get("failed") or 0)
            if not (done or failed):        # 扫到 0 张 = 没干活，不进列表
                continue
            push("长图 OCR", "fail" if failed and not done else "ok", at, at,
                 f"{done} 张转文字" + (f"，{failed} 张失败" if failed else "")
                 + (f" · {str(d.get('error'))[:60]}" if d.get("error") else ""),
                 [_code("ocr.py")] + ([_ai(model)] if done else []), "o" + at,
                 _go("gallery"))       # 转的是存图里的长图
            continue

        if ev == "trends_done":
            ok = bool(d.get("ok"))
            partial = str(d.get("status") or "") == "partial"
            push("AI 新闻抓取", "ok" if ok else "fail", at, at,
                 f"{int(d.get('items') or 0)} 条"
                 + ("（部分源失败）" if partial else "")
                 + ("，模型已摘要" if d.get("llm") else "")
                 + (f" · {str(d.get('error'))[:80]}" if d.get("error") else ""),
                 [_code("ai_trends_runner.py")] + ([_ai(model)] if d.get("llm") else []),
                 "t" + at, _go("", "#card=trends"))   # 新闻就在本页那张卡上，同页跳
            continue

        if ev == "dedupe_manual":
            apply_ = bool(d.get("apply"))
            alive = str(dd_last.get("ts") or "")[:10] == at[:10]
            groups = len(dd_last.get("groups") or []) if alive else 0
            freed = _human(dd_last.get("freed") or 0) if alive else ""
            bits = ["已清理" if apply_ else "试跑"]
            if groups:
                bits[0] += f" {groups} 组"
            if freed and freed not in ("0 B", "0 KB"):
                bits.append(f"省 {freed}")
            push("图片去重", "ok", at, at, " · ".join(bits),
                 [_code("dedupe_runner.py")], "d" + at, _go("gallery"))
            continue

        if ev == "heartbeat_manual":
            push("系统巡检（手动）", "ok", at, at, "点了一次「巡检并自愈」",
                 [_code("heartbeat.py")], "hm" + at, _go("inspect"))
            continue

        if ev == "chat":
            text = str(d.get("text") or "")[:40]
            ms = int(d.get("ms") or 0)
            push("微信助手对话", "ok", at, at,
                 f"「{text}」" + (f" · {ms / 1000:.1f}s" if ms else ""),
                 [_ai(model)], "c" + at)
            continue

        if ev == "move":
            n = int(d.get("n") or 0)
            dest = str(d.get("dest") or "").rstrip("/").split("/")[-1]
            push("收件箱整理", "ok" if int(d.get("ok") or 0) == n else "fail", at, at,
                 f"{n} 个文件 → {dest}", [_code("server.py")], "m" + at)
            continue

        if ev == "rename":
            push("重命名", "ok", at, at,
                 str(d.get("to") or "").rstrip("/").split("/")[-1][:60],
                 [_code("server.py")], "rn" + at)
            continue

        if ev == "heartbeat_done":
            healed = d.get("healed") or []
            if d.get("ok") and not healed:
                continue                     # 正常心跳不记账，否则两分钟一条
            push("系统巡检", "ok" if d.get("ok") else "fail", at, at,
                 ("自愈 " + "、".join(str(x) for x in healed[:3])) if healed
                 else str(d.get("error") or "有检查项没过")[:140],
                 [_code("heartbeat.py")], "h" + at, _go("inspect"))
            continue

    # —— 传输记录补漏：1200 行窗口之外、但还在两日内的，从 tf_batch 补进来 ——
    # 顺序不用管，下面 _collapse 之后会统一按 start 倒排。
    for k, b in tf_batch.items():
        if k not in tf_seen:
            tf_push_item(k, b)

    # —— 传输助手的另两头：收到的（手机→网页）与被删的。同样按时间捞 ——
    # 网页自己加的那条不记 tf_in（推送链已经记 tf_push），只有微信来的才算「收到」，
    # 否则同一次操作在账上会出现两行。
    for d in _tail_by_event(BASE / "pocket.jsonl", {"tf_in", "tf_del"}, days=2):
        ev2 = str(d.get("event") or "")
        at2 = _iso(d.get("ts"))
        kind2 = {"image": "图片", "file": "文件", "video": "视频",
                 "text": "文字", "link": "链接"}.get(
                     str(d.get("kind") or ""), "消息")
        if ev2 == "tf_in":
            nm = str(d.get("name") or "")
            if nm:                          # 文件/图片：名字 + 大小
                detail = nm[:60] + (f" · {_human(int(d.get('size') or 0))}"
                                    if d.get("size") else "")
            else:                           # 文字/链接：给开头
                detail = str(d.get("text") or "")[:60] or "（空）"
            push(f"收到 · {kind2}", "ok", at2, at2, detail,
                 [_code("transfer.py")], "in" + at2, _go("", "#card=transfer"))
        else:
            detail = str(d.get("name") or d.get("text") or "一条消息")[:60]
            push(f"传输助手删除 · {kind2}", "ok", at2, at2, detail,
                 [_code("transfer.py")], "dl" + at2, _go("", "#card=transfer"))

    # —— 进行中：日志里只有 start 没 done，且还在窗口内 ——
    for key, st in open_start.items():
        if not st:
            continue
        age = _secs(st, now_s)
        if age is None or age > _WL_RUN_WINDOW:
            continue
        kind, _, rid = key.partition(":")
        link = kind == "link"
        push("正在抓取存图" if link else "正在抓取文章", "run", st, "",
             "开始了，还没收尾", [_code("cuntu.py" if link else "article.py")],
             "run-" + rid)

    # —— 进行中：各 runner 自报 ——
    for label, mod, script in (
        ("长图 OCR", ocr_runner, "ocr.py"),
        ("图片去重", dedupe_runner, "dedupe_runner.py"),
        ("AI 新闻抓取", ai_trends_runner, "ai_trends_runner.py"),
    ):
        st = getattr(mod, "STATE", {}) or {}
        if not st.get("running"):
            continue
        push(label, "run", _iso(st.get("started")), "", "正在跑",
             [_code(script)] + ([_ai(model)] if label == "AI 新闻抓取" else []),
             "run-" + script)
    if bool((heartbeat.STATE or {}).get("running")):
        push("系统巡检", "run", "", "", "正在跑", [_code("heartbeat.py")], "run-hb")

    return items


# --------------------------------------------------------------------------
# 中台在做的事：口袋以外，中台里别的插件也在干活（2026-11-21）
#
# 这张卡原来叫「口袋在做的事」，账只从 pocket.jsonl 来。可中台不止口袋：
# 虫巢能远程锁屏 / 卸软件 / 换 agent，设备能开关灯，游戏能记时长 —— 它们各自
# 在自己的目录里记账（16-多机/fleet.db 的 oplog、13-设备/使用记录.jsonl、
# 17-游戏/记录.jsonl），以前只有口袋那份被读进来。现在这几本账都翻成和口袋
# 一样的 item，多带一个 plugin 字段，卡片按插件分栏。
#
# 跟口袋那边一个口径：
#   * 一次操作一行：虫巢同一个 ref_id 的 queued→running→done 是三次快照，
#     折成一条，最后一态定成败，开始时间取第一次出现那条。
#   * 读不到就当空：插件没装、库被别的进程独占、文件里半行 JSON，都只是
#     这一条不出现 —— 不能让整块 worklog 变 500。
#   * 插件名单以 <数据根>/_工作台/plugins.json 为准：加插件、改名、换端口，
#     卡片跟着变，这里不用改代码。没日志的插件（GitHub）先不接，chip 上写 0。
# --------------------------------------------------------------------------

_PG_LIMIT = 300              # 每个插件最多回看多少条原始记录
_PG_RUN_STALE = 3600         # 虫巢的 queued/running 超过一小时没下文，就算没成


def plugin_manifest(cfg: dict) -> list[dict]:
    """中台里在跑的插件 + 口袋自己。soon 的是占位，还没干活，不上筛选条。"""
    out = [{"id": "pocket", "name": "口袋", "glyph": "", "url": "", "n": 0, "fail": 0}]
    d = _load(_root(cfg) / "_工作台" / "plugins.json", {}) or {}
    for p in (d.get("plugins") or []):
        if not isinstance(p, dict):
            continue
        pid = str(p.get("id") or "").strip()
        if not pid or p.get("soon"):
            continue
        out.append({"id": pid, "name": str(p.get("name") or pid),
                    "glyph": str(p.get("glyph") or ""),
                    "url": str(p.get("url") or ""), "n": 0, "fail": 0})
    return out


def _pg_item(plugin: str, task: str, status: str, start: str, end: str,
             detail: str, sources: list[dict], rid: str,
             link: dict | None = None) -> dict:
    """插件的一条活，形状跟 _auto_worklog 的 push 完全一致，只多 plugin。"""
    it = {
        "id": f"{plugin}-{rid}", "plugin": plugin,
        "task": task, "detail": str(detail or "")[:160], "status": status,
        "start": start, "end": end,
        "secs": _secs(start, end) if (start and end) else None,
        "sources": sources,
    }
    if link and link.get("url"):
        it["link"] = link
    return it


# 动作译名照抄 16-多机/fleet.py 的 CAP_NAME（那边是英文 key → 中文按钮名），
# 补上 kind=config 那几种。不 import fleet：那个模块一进来就连库、起线程。
_NEST_ACT = {
    "lock": "锁定屏幕", "logout": "注销", "reboot": "重启",
    "refresh": "采集清单", "install": "安装软件", "uninstall": "卸载软件",
    "upgrade": "升级软件", "self_update": "换 agent 版本",
    "access": "改设备权限", "rename": "改显示名",
}


def _nest_items(cfg: dict, urls: dict) -> list[dict]:
    """虫巢思维：fleet.db 的 oplog 是一本只追加的账 —— 谁在什么时候想把什么
    操作发到哪台机器、成没成、花了多久。"""
    db = _root(cfg) / "16-多机" / "fleet.db"
    if not db.exists():
        return []
    con = None
    rows: list[dict] = []
    try:
        con = sqlite3.connect(f"file:{db}?mode=ro", uri=True, timeout=0.8)
        con.row_factory = sqlite3.Row
        rows = [dict(r) for r in con.execute(
            "SELECT id, ts, device_name, kind, action, target, state, detail,"
            " dur_ms, ref_id FROM oplog ORDER BY ts DESC LIMIT ?", (_PG_LIMIT,))]
    except sqlite3.Error:
        return []                     # 库被独占 / 还没建表：这一栏先空着
    finally:
        if con is not None:
            try:
                con.close()
            except sqlite3.Error:
                pass
    if not rows:
        return []
    link = {"url": urls.get("nest") or "http://127.0.0.1:8782/fleet", "pg": True}
    groups: dict[str, list[dict]] = {}
    for r in rows:                    # 已按时间倒序，先到的是最新一态
        key = str(r.get("ref_id") or "") or "#%s" % r.get("id")
        groups.setdefault(key, []).append(r)
    now = datetime.now()
    out: list[dict] = []
    for key, g in groups.items():
        g.sort(key=lambda x: str(x.get("ts") or ""))      # 升序：第一次在前
        last = g[-1]
        name = _NEST_ACT.get(str(last.get("action") or ""),
                             str(last.get("action") or "操作"))
        dev = str(last.get("device_name") or "").strip()
        det = str(last.get("detail") or "").strip()
        tgt = str(last.get("target") or "").strip()
        bits = [dev] if dev else []
        if tgt and tgt not in det:
            bits.append(tgt)
        if det:
            bits.append(det)
        st = str(last.get("state") or "")
        if st in ("done", "ok", "success"):
            # fleet 有时候把「跑完了，但里面报错」也记成 done（例如换 agent 那步
            # detail 写「查版本失败：基础连接已经关闭」）。状态是 done 就刷成绿点，
            # 旁边却写着「失败」，读的人会以为卡坏了 —— 所以留一道文字兜底。
            status = "fail" if re.search(r"失败|出错|超时|错误", det) else "ok"
        elif st in ("queued", "running", "pending", "taken"):
            # 排队/执行中超过一小时还没下文＝那台机器关机了，别在页面上永远转圈
            age = None
            try:
                age = (now - datetime.strptime(_iso(last.get("ts")), "%Y-%m-%d %H:%M:%S")).total_seconds()
            except ValueError:
                pass
            status = "run" if (age is not None and age < _PG_RUN_STALE) else "fail"
            if status == "fail" and not det:
                bits.append("没等到结果")
        else:                          # failed / expired / denied
            status = "fail"
        out.append(_pg_item("nest", name, status,
                            _iso(g[0].get("ts")), _iso(last.get("ts")),
                            " · ".join(bits), [_tool("虫巢思维")], key, link))
    return out


_DEV_ACT = {"add": "接入设备", "delete": "移除设备", "on": "打开设备",
            "off": "关闭设备", "set": "改设备设置", "use": "记录使用"}


def _device_items(cfg: dict, urls: dict) -> list[dict]:
    """智能设备：使用记录.jsonl 开关一次记一行（人点的、发现的、扫进来的都算）。"""
    rows = _tail_jsonl(_root(cfg) / "13-设备" / "使用记录.jsonl", _PG_LIMIT)
    if not rows:
        return []
    link = {"url": urls.get("devices") or "http://127.0.0.1:8778/", "pg": True}
    out: list[dict] = []
    for i, r in enumerate(rows):
        act = str(r.get("action") or "")
        name = str(r.get("name") or "（没名字）")
        det = str(r.get("detail") or "").strip()
        at = _iso(r.get("at"))
        if act == "add":
            # detail 本来就是「空调 · 小米 KFR-35GW/N1A1」，加上名字就成了三条 · 拼一起
            d = f"{name}（{det}）" if det else name
        else:
            d = f"{name} · {det}" if det else name
        out.append(_pg_item("devices", _DEV_ACT.get(act, act or "操作"), "ok",
                            at, at, d, [_tool("智能设备")],
                            "%s-%s" % (r.get("id") or i, at), link))
    return out


_GAME_ACT = {"play": "玩游戏", "milestone": "游戏里程碑", "match": "游戏战绩",
             "buy": "游戏花费", "note": "游戏笔记"}


def _game_items(cfg: dict, urls: dict) -> list[dict]:
    """游戏记录：记录.jsonl 里 play / milestone / match / buy / note 五种事件。
    事件只写 gid，名字得回 游戏.json 查 —— 那个文件是台账，事件是流水。"""
    rows = _tail_jsonl(_root(cfg) / "17-游戏" / "记录.jsonl", _PG_LIMIT)
    if not rows:
        return []
    lib = _load(_root(cfg) / "17-游戏" / "游戏.json", {}) or {}
    names = {str(g.get("id") or ""): str(g.get("name") or "") for g in (lib.get("games") or [])}
    link = {"url": urls.get("games") or "http://127.0.0.1:8779/", "pg": True}
    out: list[dict] = []
    for i, r in enumerate(rows):
        kind = str(r.get("kind") or "play")
        gname = names.get(str(r.get("gid") or ""), "") or "某个游戏"
        at = _iso(r.get("ts"))
        note = str(r.get("note") or "").strip()
        title = str(r.get("title") or "").strip()
        if kind == "play":
            d = f"{gname} · {int(r.get('min') or 0)} 分钟"
        elif kind == "milestone":
            d = f"{gname} · {title or '里程碑'}（{int(r.get('progress') or 0)}%）"
        elif kind == "match":
            d = (f"{gname} · {title or '一局'} · " + ("胜" if r.get("win") else "负")
                 + (f" · {r.get('kda')}" if r.get("kda") else "")
                 + (f" · {r.get('rank')}" if r.get("rank") else ""))
        elif kind == "buy":
            d = f"{gname} · {title or '购入'} · ¥{r.get('cost') or 0}"
        else:
            d = f"{gname} · {title or note or '笔记'}"
        if note and kind != "note" and kind != "play":
            d += f"：{note}"
        elif note and kind == "play":
            d += f" · {note}"
        out.append(_pg_item("games", _GAME_ACT.get(kind, kind), "ok",
                            at, at, d, [_tool("游戏记录")],
                            "%s-%s" % (r.get("gid") or i, at), link))
    return out


_MEDIA_ACT = {
    "refresh": "重扫库", "scrape": "刮削海报", "move": "移动文件",
    "watch": "标记已看", "delete": "删除", "undo": "撤销删除",
    "boot_timeout": "启动超时",     # media.py 启动等满 90s 还没通就记一笔（不误报，只留痕）
    "launch_timeout": "拉起超时",   # 口袋侧等不到端口时兜底补记（media.py 没机会写的那种）
}

# 各服务「愿意等多久端口通」。默认 14 秒够绝大多数，影音室例外：
# 它启动要先 walk 每个库（231+341 部实测 60~90 秒），14 秒必超 —— 那是误报不是失败。
_LAUNCH_PATIENCE = {"media": 100}


def _media_ts_epoch(s: str) -> float:
    for fmt in ("%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S"):
        try:
            return datetime.strptime(str(s)[:19], fmt).timestamp()
        except ValueError:
            continue
    return 0.0


def _media_boot_note(base, kind: str, error: str, secs: int = 0) -> bool:
    """往影音室活账补一笔启动记录。账本该由 media.py 写，这里只在它
    根本没机会写（进程秒崩 / 压根没跑起来）时兜底，格式跟 media_log 对齐。"""
    p = base / "_墙" / "记录.jsonl"
    rec = {"ts": datetime.now().strftime("%Y-%m-%d %H:%M:%S"), "kind": kind,
           "ok": False, "secs": int(secs), "done": 0, "total": 0, "error": error}
    try:
        p.parent.mkdir(parents=True, exist_ok=True)
        with p.open("a", encoding="utf-8") as f:
            f.write(json.dumps(rec, ensure_ascii=False) + "\n")
        return True
    except OSError:
        return False


def _media_boot_recent(base, secs: int = 300) -> bool:
    """最近几分钟里是不是已经有一条启动超时了 —— 有就别重复记。"""
    rows = _tail_jsonl(base / "_墙" / "记录.jsonl", 60)
    now = time.time()
    for r in reversed(rows):
        if str(r.get("kind") or "") not in ("boot_timeout", "launch_timeout"):
            continue
        t = _media_ts_epoch(r.get("ts") or "")
        return bool(t) and (now - t) <= secs
    return False


def _media_items(cfg: dict, urls: dict) -> list[dict]:
    """影音室：重扫 / 刮削 / 移动 / 标记已看 / 删除。

    账在 `02-媒体/_墙/记录.jsonl`，由 media.py 的 media_log 写。
    2026-11-22 之前这里一条记录都没有 —— 扫库、刮削跑完就没了影子，
    工作台上连「影音」这一栏都不存在，用户只能翻硬盘猜它有没有干活。
    **播放进度不记**（十秒一次，会把整本账刷成进度流水）。
    """
    base = _root(cfg) / "02-媒体"
    rows = _tail_jsonl(base / "_墙" / "记录.jsonl", 400)
    if not rows:
        return []
    reg = _load(base / "_墙" / "库.json", {}) or {}
    names = {str(l.get("id") or ""): str(l.get("name") or "")
             for l in (reg.get("libraries") or []) if isinstance(l, dict)}
    link = {"url": urls.get("media") or "http://127.0.0.1:8773/", "pg": True}
    out: list[dict] = []
    for i, r in enumerate(rows):
        kind = str(r.get("kind") or "refresh")
        at = _iso(r.get("ts"))
        ok = bool(r.get("ok", True))
        bits = [x for x in (names.get(str(r.get("lib") or ""), ""),
                            str(r.get("item") or ""),
                            str(r.get("note") or "")) if x]
        detail = " · ".join(bits)
        if kind in ("boot_timeout", "launch_timeout"):
            # 两类超时文案不同：boot_timeout 是 media.py 自述（只给秒数+原因），
            # launch_timeout 是口袋兜底写的，error 本身就是一句完整话，别再套壳。
            secs = int(r.get("secs") or 0)
            if kind == "boot_timeout":
                detail = f"等了 {secs}s 还没通" if secs else "等待超时"
                if r.get("error"):
                    detail += " · " + str(r["error"])[:80]
            else:
                detail = str(r.get("error") or "端口一直没通")[:120]
        elif kind in ("refresh", "scrape", "move"):
            dn, tt = int(r.get("done") or 0), int(r.get("total") or 0)
            if tt:
                detail += (" · " if detail else "") + f"{dn}/{tt}"
            secs = int(r.get("secs") or 0)
            if secs:
                detail += (" · " if detail else "") + f"{secs}s"
            if not ok and r.get("error"):
                detail = str(r["error"])[:120]
        # 启动类超时用「注意」而不是「失败」：服务确实还没通，但它通常还在扫盘、
        # 一两分钟后就自己好了。标成 fail 会让人以为它崩了（用户口径：不要误报）。
        status = ("warn" if kind in ("boot_timeout", "launch_timeout")
                  else ("ok" if ok else "fail"))
        out.append(_pg_item("media", _MEDIA_ACT.get(kind, kind),
                            status, at, at, detail,
                            [_tool("影音室")],
                            f"{r.get('ts') or i}-{kind}", link))
    return out


def _app_items(cfg: dict, urls: dict) -> list[dict]:
    """本机 App：谁打开了、谁关了、谁闪退、谁卡住。

    账在 `10-系统记录/App事件.jsonl`，由 `cuntu/appwatch.py` 这个常驻监工写
    （它不在 pocket 里：App 的开关跟哪个 web 服务活不活没关系，口袋心跳
    120 秒一轮也太粗，开两分钟就关掉的窗口会整段漏掉）。

    账上「打开」「退出」是两行，卡上合成一行 —— 否则每开一个窗口就是两条流水，
    一天下来能把别的活全顶出去。配对靠 pid（appwatch 写的时候就带着）。
    闪退 / 卡住 / 卡住后被强制退出 各占一行：它们在账上本来就是独立的事故，
    而且正是最该被看见的那几行。
    """
    rows = _tail_jsonl(_root(cfg) / "10-系统记录" / "App事件.jsonl", 400)
    if not rows:
        return []
    # 后来退掉了的 pid：它们的「打开」不必再单列（有始有终，看「退出」那行）
    ended = {str(r.get("pid") or "") for r in rows
             if str(r.get("event") or "") in ("app_close", "app_crash", "app_kill")}
    out: list[dict] = []
    for i, r in enumerate(rows):
        ev = str(r.get("event") or "")
        if ev == "app_rec":            # 恢复只是把「卡了多久」补全，不单独占一行
            continue
        app = str(r.get("app") or "（不知道名字）")
        at = _iso(r.get("ts"))
        pid = str(r.get("pid") or "")
        status = "ok"
        detail = str(r.get("note") or "")
        if ev == "app_open":
            if pid and pid in ended:
                continue
            task = f"打开 {app}"
            detail = detail or "这次还没退出"
        elif ev == "app_close":
            task = f"退出 {app}"
        elif ev == "app_crash":
            task, status = f"{app} 闪退", "fail"
        elif ev == "app_hang":
            task, status = f"{app} 卡住", "warn"
        elif ev == "app_kill":
            task, status = f"{app} 卡住后强制退出", "fail"
        else:
            task = f"{app} {ev}"
        out.append(_pg_item("apps", task, status, at, at, detail,
                            [_tool("App 监工")], f"{r.get('ts') or i}-{ev}-{pid}"))
        # 「打开/退出」一天几十条很正常，只回看最近一批；事故行不设上限（少，且重要）
        if len(out) >= 60 and ev in ("app_open", "app_close"):
            break
    return out


def _plugin_items(cfg: dict) -> list[dict]:
    """把各插件的账并成一份。一个插件嗝住只影响它自己那一栏。"""
    urls = {p["id"]: p["url"] for p in plugin_manifest(cfg) if p.get("url")}
    # 多机（虫巢）那本账默认不并进来（2026-11-22 按用户口径）：被控机干的活是
    # 那台机器自己的账，不该跟本机的事排在同一张「中台在做的事」里。
    # 数据一条不少 —— 全在 16-多机/fleet.db，看的地方是虫巢思维自己的页面。
    # 以后要搬进中台：config.json 里 dash.nest_in_flow 改 true，这里自动并回来。
    fns = [_device_items, _game_items, _media_items, _app_items]
    if bool((cfg.get("dash") or {}).get("nest_in_flow")):
        fns.insert(0, _nest_items)
    out: list[dict] = []
    for fn in fns:
        try:
            out.extend(fn(cfg, urls))
        except Exception:      # noqa: BLE001
            continue
    # 进行中的不合并（_collapse 的规矩：正在跑的一律单列），但同插件同任务连点
    # 三次也只留最新那条 ——「换 agent 版本」点三下不该在卡上排三行进行中。
    seen: set[tuple] = set()
    keep: list[dict] = []
    for it in sorted(out, key=lambda x: x.get("start") or "", reverse=True):
        if it.get("status") == "run":
            k = (it.get("plugin"), it.get("task"))
            if k in seen:
                continue
            seen.add(k)
        keep.append(it)
    return keep


def read_worklog(cfg: dict) -> list[dict]:
    d = _load(worklog_path(cfg), {}) or {}
    raw = d.get("items") if isinstance(d.get("items"), list) else []
    out: list[dict] = []
    for it in raw:
        if not isinstance(it, dict) or not str(it.get("task") or "").strip():
            continue
        src = []
        if it.get("code"):
            src.append(_code(str(it["code"])[:40]))
        if it.get("ai"):
            src.append(_ai(str(it["ai"])[:40]))
        st, en = _iso(it.get("start")), _iso(it.get("end"))
        status = it.get("status") if it.get("status") in ("ok", "fail", "run", "skip") else "ok"
        out.append({
            "id": str(it.get("id") or "") or ("w" + st),
            "task": str(it.get("task"))[:80],
            "detail": str(it.get("detail") or "")[:200],
            "status": status, "start": st, "end": en,
            "secs": _secs(st, en) if (st and en) else None,
            "sources": src, "manual": True,
            # 哪个客户端干的（WorkBuddy / Trae / Cherry Studio …）。
            # 没填就写「手记」—— 这一栏只有两种东西：机器读出来的客户端名，
            # 和「人自己记的」。写「未标来源」会让人误以为是识别失败。
            "client": str(it.get("client") or "手记")[:40],
        })
    return out


def append_worklog(cfg: dict, body: dict) -> dict:
    """补记一条。AI 干完活可以 POST 过来（也方便用户手动记）。

    kind: code（谁写的脚本）/ ai（哪个模型）/ both（一起，who 写「脚本 + 模型」）。
    """
    task = str(body.get("task") or "").strip()[:80]
    if not task:
        return {"ok": False, "error": "先说清楚做了什么"}
    status = str(body.get("status") or "ok")
    if status not in ("ok", "fail", "run", "skip"):
        status = "ok"
    kind = str(body.get("kind") or "ai")
    who = str(body.get("who") or "").strip()[:60]
    # 客户端单独一栏：模型名（DeepSeek-V4.1-Flash）说明不了是哪个软件在跑，
    # 换成 Trae / Cherry 之后同一句话要能看出换过工具。
    client = str(body.get("client") or "").strip()[:40]
    parts = [p.strip() for p in re.split(r"[+＋,，/、]", who) if p.strip()]
    code = ai = ""
    if kind == "both":
        if len(parts) >= 2:
            code, ai = parts[0], parts[1]
        elif parts:
            code = ai = parts[0]
    elif kind == "code":
        code = who or "脚本"
    else:
        ai = who or "AI"

    now = datetime.now().strftime("%Y-%m-%d %H:%M:%S")
    start = _iso(body.get("start")) or now
    end = "" if status == "run" else (_iso(body.get("end")) or now)
    item = {
        "id": "w" + uuid.uuid4().hex[:10],
        "task": task, "detail": str(body.get("detail") or "")[:200],
        "status": status, "start": start, "end": end,
        "code": code, "ai": ai, "client": client,
        "created": now,
    }
    p = worklog_path(cfg)
    with _NOTES_LOCK:
        d = _load(p, {}) or {}
        items = d.get("items") if isinstance(d.get("items"), list) else []
        items.append(item)
        items = items[-_WL_KEEP:]
        p.parent.mkdir(parents=True, exist_ok=True)
        tmp = p.with_suffix(".json.tmp")
        tmp.write_text(json.dumps({"items": items, "updated_at": now},
                                  ensure_ascii=False, indent=1), encoding="utf-8")
        tmp.replace(p)
    return {"ok": True, "item": item}


def _collapse(items: list[dict]) -> list[dict]:
    """同一个任务、同一个结果、同一天 → 合成一条「×N」。

    抓图一天能跑四十次，原样铺开会把别的记录全顶下去；而「存图 ×40，
    15:40→16:45」才是一个人想看的那种账。明细不丢：塞进 children，
    页面上点那一行才展开。进行中的不合并 —— 它本来就只有一条，
    而且「现在在跑什么」必须一直可见。
    """
    buckets: dict[tuple, list[dict]] = {}
    order: list[tuple] = []
    for it in items:
        if it["status"] == "run":
            continue
        # 插件也进 key：虫巢的「卸载软件」和设备的「接入设备」就算赶上同名，
        # 也是两个插件两件事，不能折成一条 ×2（2026-11-21 合卡后加的）
        key = ((it.get("start") or "")[:10], it["task"], it["status"],
               str(it.get("plugin") or ""))
        if key not in buckets:
            buckets[key] = []
            order.append(key)
        buckets[key].append(it)

    out: list[dict] = [it for it in items if it["status"] == "run"]
    for key in order:
        g = sorted(buckets[key], key=lambda x: x.get("start") or "")
        head = dict(g[-1])                       # 拿最近那条当代表（detail 最新鲜）
        # 合并条的时间＝**最近一次**，不是最早那次。
        # 之前取 g[0] 会让「存图 · 小红书 ×74」显示成 00:05 —— 用户晚上八点
        # 丢的链接全被折进凌晨那一条里，看起来就像「八点以后没记账」。
        # 最早一次单独放 first，页面用它写「最早 00:05」。
        head["start"] = g[-1].get("start") or ""
        head["end"] = g[-1].get("end") or g[-1].get("start") or ""
        head["first"] = g[0].get("start") or ""
        head["secs"] = _secs(head["start"], head["end"]) if (head["start"] and head["end"]) else None
        if len(g) > 1:
            head["count"] = len(g)
            # token 是消耗量，合并行必须求和；拿最后一行代表会把前面几轮烧的 token 抹掉。
            if any(isinstance(x.get("tokens"), dict) for x in g):
                tk = {"in": 0, "out": 0, "cached": 0, "total": 0}
                for x in g:
                    t = x.get("tokens") or {}
                    for k in tk:
                        tk[k] += int(t.get(k) or 0)
                head["tokens"] = tk
            d0 = str(head.get("detail") or "")
            head["detail"] = (f"最近：{d0}" if d0 else "")[:120]
            head["children"] = [{
                "start": x.get("start") or "", "end": x.get("end") or "",
                "detail": str(x.get("detail") or "")[:120],
                "status": x["status"],
                "link": x.get("link") or None,      # 展开的每一次也能各自点进去
            } for x in reversed(g[-12:])]
        out.append(head)
    return out


# --------------------------------------------------------------------------
# 本机工作：口袋以外这台电脑上还干了什么
#
# 一个客户端一个取数函数，登记在 _CLIENTS 里（见下方），目前两个：
#   WorkBuddy     —— ~/.workbuddy/projects/<项目>/<会话>.jsonl。每轮把
#                    function_call（Edit/Write 带 file_path）连同用户原话
#                    追加进去，是本机唯一一份「谁在什么时候动了哪些文件」的账。
#   Cherry Studio —— <AppSupport>/CherryStudio/Data/cherrystudio.sqlite 的
#                    message 表（topic_id / role / data / model_id / created_at）。
#
# （~/.workbuddy/changes-index/ 看着更像，其实只索引工作区里的记忆文件，
#  改代码根本不在里面 —— 2026-11-18 踩过一次，别回头再去用它。）
#
# 会话 jsonl 几十 MB，但它是 append-only 的：记住读到第几个字节，下次接着读。
# 页面每分钟刷一次，从头解一遍是不能接受的。
# --------------------------------------------------------------------------

_WB_HOME = Path.home() / ".workbuddy"
_WB_SESSIONS = _WB_HOME / "projects"
# Cherry Studio 用 SQLite 存对话，比 jsonl 还好读：一张 message 表就是一本账。
# 库在哪按平台找 —— macOS 在 Application Support，Windows 在 %APPDATA%，
# Linux 在 ~/.config。以前只写了 macOS 那一条，别的系统上这格永远是空的。
_CHERRY_REL = (
    ("Library", "Application Support", "CherryStudio", "Data", "cherrystudio.sqlite"),
    ("AppData", "Roaming", "CherryStudio", "Data", "cherrystudio.sqlite"),
    (".config", "CherryStudio", "Data", "cherrystudio.sqlite"),
)
_WL_HOST_DAYS = 2            # 回溯几天：面板只关心最近，整本旧账翻出来没意义
_WL_HOST_LIMIT = 80          # 本机那格给页面的条数（一天四十轮会话很正常，别卡在 40）
_WL_SESSION_TAIL = 64 * 1024 * 1024   # 单个会话文件的读取上限（实测 55MB 全解 0.2s）
# 每轮请求的「具体步骤」：卡片上点开一行看的就是它。内存里每轮留 80 条（够回看，
# 110 步那种也不会把几十 MB 的会话撑爆），发给页面的只留最近 18 条 —— 再多就是
# 把整本流水塞进一张卡片，payload 也要跟着涨。
_WL_STEPS_MEM = 80
_WL_STEPS_SEND = 18

_HOST_LOCK = threading.Lock()
_HOST_CACHE: dict[str, dict] = {}


def _ms(ts) -> str:
    try:
        return datetime.fromtimestamp(float(ts) / 1000).strftime("%Y-%m-%d %H:%M:%S")
    except (ValueError, TypeError, OSError, OverflowError):
        return ""


def _clip(s: str, n: int) -> str:
    """截断时留个省号。以前是硬切，句子停在半个词上，看着就像「记录丢了」——
    真截了就得让人看得出来是截了。"""
    return s if len(s) <= n else s[:n - 1] + "…"


def _brief(s: str, n: int) -> str:
    """收起态的短名：只截长名字，短名一个字不动。跟 _clip 的区别是它专门
    用在「上下文标签」上 —— 全名还在 facets / title 里，不怕丢。"""
    s = str(s or "")
    return s if len(s) <= n else s[:n] + "…"


def _ask_of(text: str, plain_ok: bool = False) -> str:
    """用户消息里真正的那句话。前面挂着一大坨 system-reminder，
    不剥就是满屏英文说明。

    `plain_ok` 给别家客户端用：Claude Code 之类的用户消息没有 <user_query>
    标签，整段就是原话。WorkBuddy 这边不能开 —— 没标签的多是工具回执，
    当成提问会顶掉真正的标题。

    截断上限给到 320：以前是 70，一句「我发好几条链接的时候，先存起来，进入一个列表，
    前期工作全部用代码实现……」正好卡在 70 字，页面上一行句子硬生生停在半截，
    看着像聊天记录丢了。折叠状态本来就只显示一行（CSS 省略号），
    点开才需要全文 —— 所以这里要把话留全，让「点开」真有东西看。
    """
    m = re.search(r"<user_query>(.*?)</user_query>", text or "", re.S)
    if m:
        src = m.group(1)
    elif plain_ok:
        src = re.sub(r"<system-reminder>[\s\S]*?</system-reminder>", " ", text or "")
    else:
        return ""
    q = re.sub(r"@image#\d+:\S+", "", src)            # 贴图占位不占字数
    q = re.sub(r"<[^>]+>", " ", q)
    return _clip(re.sub(r"\s+", " ", q).strip(), 320)


def _call_args(d: dict) -> dict:
    raw = d.get("arguments")
    if isinstance(raw, dict):
        return raw
    if not isinstance(raw, str) or not raw.strip().startswith("{"):
        return {}
    try:
        v = json.loads(raw)
    except ValueError:
        return {}
    return v if isinstance(v, dict) else {}


_FILE_TOOLS = ("Write", "Edit", "MultiEdit", "NotebookEdit")


def _step_text(name: str, args: dict) -> str:
    """一条 function_call → 一行「Edit dash.html」。

    卡片上那张表只答「这轮干了什么大事」，点开一行才该看到「具体动了哪几处」。
    只留认得出对象的那半句：改文件取文件名、跑命令取命令、搜索取关键词、
    开网页取域名；其余工具（ToolSearch / TodoWrite 之类）只写工具名，
    抄参数进去反而没法读。
    """
    tgt = ""
    if name in ("Bash", "PowerShell", "execute_command"):
        cmd = re.sub(r"\s+", " ", str(args.get("command") or "")).strip()
        # 「cd 某个目录 && 真命令」占了大半条，留着它后面那半句就被 48 字截没了。
        # 目录信息本来也不重要 —— 看的是「跑了什么」。
        cmd = re.sub(r"^cd\s+\S+\s*&&\s*", "", cmd)
        return ("$ " + cmd if cmd else name)[:64]
    if name in _FILE_TOOLS or name == "Read":
        fp = str(args.get("file_path") or args.get("path") or "")
        tgt = fp.rstrip("/").split("/")[-1]
    elif name in ("Grep", "search_data"):
        pat = str(args.get("pattern") or args.get("query") or "")
        tgt = ("/" + pat + "/") if pat else ""
    elif name == "Glob":
        tgt = str(args.get("pattern") or "")
    elif name == "WebSearch":
        tgt = str(args.get("query") or "")
    elif name == "WebFetch":
        tgt = re.sub(r"^https?://", "", str(args.get("url") or "")).split("/")[0]
    elif name == "Skill":
        tgt = str(args.get("skill") or args.get("name") or "")
    elif name in ("Agent", "Task"):
        tgt = str(args.get("subagent_type") or args.get("description") or "")
    elif name.startswith("mcp__"):
        parts = [x for x in name.split("__") if x]
        # mcp__server__tool → server·tool：省掉那个永远一样的前缀
        name = "·".join(parts[1:]) if len(parts) > 1 else name
    if not name:
        return ""
    out = re.sub(r"\s+", " ", (name + " " + tgt).strip() if tgt else name)
    return out[:64]


# 模型名与请求 id：只在 reasoning / function_call 条目上有。这两条正则让
# reasoning 那种几 MB 的行不必整行 json.loads，跑一遍字符串就够了。
_MODEL_RE = re.compile(r'"requestModelName"\s*:\s*"([^"]{1,80})"')
_RID_RE = re.compile(r'"conversationRequestId"\s*:\s*"([A-Za-z0-9-]{8,64})"')

# 模式 / 专家：WorkBuddy 把这两样拼在每轮用户消息前的 system-reminder 里
# （<craft_mode>…You are now in Agent mode…</craft_mode>、
#  <current-expert>当前会话已选择 Expert「ex_xxx」…</current-expert>）。
# 「这轮是在什么模式下、跟谁一起干的」跟「改了什么文件」是同一层的事实，
# 不记下来，事后就只剩一个光秃秃的客户端名。
_MODE_RE = re.compile(r"You are now in ([A-Za-z][A-Za-z ]{2,14}?) mode")
_EXPERT_RE = re.compile(r"<current-expert>[\s\S]{0,200}?(ex_[A-Za-z0-9]{6,})")

# 专家清单只存在当前登录用户的目录下（storage/<user-id>/global/expert.json），
# 用户 id 是随机的，所以 glob 一遍取最新的那份，别把 id 写死在代码里。
_EXPERT_GLOB = "storage/*/global/expert.json"
_EXPERT_CACHE: dict[str, object] = {"mtime": -1.0, "map": {}}


def _expert_name(eid: str) -> str:
    """ex_hXSIP3lgdCnU → 全栈自动化脚本工程师。

    清单里只有「最近用过的几位」，查不到就退回 id 尾 6 位 —— 至少还能看出
    这轮换过专家，而不是当成没设置。
    """
    if not eid:
        return ""
    try:
        files = sorted((Path.home() / ".workbuddy").glob(_EXPERT_GLOB),
                       key=lambda p: p.stat().st_mtime, reverse=True)
    except OSError:
        files = []
    if files:
        p = files[0]
        try:
            mt = p.stat().st_mtime
        except OSError:
            mt = -1.0
        if mt != _EXPERT_CACHE["mtime"]:
            amap: dict[str, str] = {}
            try:
                raw = json.loads(p.read_text(encoding="utf-8"))
            except (OSError, ValueError):
                raw = {}
            for row in (raw.get("recents") if isinstance(raw, dict) else None) or []:
                if not isinstance(row, dict):
                    continue
                rid, nm = str(row.get("id") or ""), str(row.get("name") or "")
                if rid and nm:
                    amap[rid] = nm
            # 二级兜底：recents 只留最近几位，老专家在这儿只剩 slug
            # （ex_xxx → ui-designer）。slug 也比 id 尾巴能认。
            try:
                bmap = json.loads((Path.home() / ".workbuddy" / "app" / "cache"
                                   / "experts" / "expert-bundle-map.json")
                                  .read_text(encoding="utf-8"))
            except (OSError, ValueError):
                bmap = {}
            if isinstance(bmap, dict):
                for rid, slug in bmap.items():
                    if str(rid).startswith("ex_") and slug:
                        amap.setdefault(str(rid), str(slug))
            _EXPERT_CACHE["map"] = amap
            _EXPERT_CACHE["mtime"] = mt
    return str((_EXPERT_CACHE["map"] or {}).get(eid) or eid[-6:])


def _new_req(ts: float, cwd: str) -> dict:
    """一轮请求的空白账页。字段名就是前端认的那几个，加一家客户端不用改前端。"""
    return {"t0": ts, "t1": ts, "ask": "", "model": "", "cwd": cwd, "files": [],
            "calls": 0, "steps": [], "steps_n": 0, "mcp": {},
            "mode": "", "expert": "", "skills": [], "note": "",
            "tin": 0, "tout": 0, "cached": 0}


def _note_call(b: dict, nm: str, args: dict, ts: float) -> None:
    """一次工具调用记进这一轮：动了哪个文件 / 碰了哪个 MCP / 装过哪本技能 + 一行明细。

    WorkBuddy 的 function_call、Claude Code 的 tool_use 都走这一份 —— 加一家
    客户端只要把它的条目翻译成 (name, args, ts) 喂进来，记账逻辑不用抄第二遍。
    """
    b["calls"] += 1
    b.setdefault("files", [])
    # 技能是「加载」进来的，不是调用去干活的：Skill 工具的 skill 参数就是这轮
    # 装了哪本手册。同一条会话里重复加载要合并，不能长出五行。
    if nm == "Skill":
        sk = str(args.get("skill") or "").strip()
        if sk:
            lst = b.setdefault("skills", [])
            if sk not in lst:
                lst.append(sk)
    # MCP 调用的名字形如 mcp__<server>__<tool>。表上要答的是「这轮动没动外部
    # 服务、动的哪个」，具体哪个 tool 明细里已经有一行，所以只记 server 和次数。
    if nm.startswith("mcp__"):
        seg = [x for x in nm.split("__") if x]
        srv = seg[1] if len(seg) > 1 else nm[5:]
        if srv:
            mm = b.setdefault("mcp", {})
            mm[srv] = mm.get(srv, 0) + 1
    if nm in _FILE_TOOLS:
        fp = str(args.get("file_path") or args.get("path") or "")
        base = fp.rstrip("/").split("/")[-1]
        if base and base not in b["files"]:
            b["files"].append(base)
    txt = _step_text(nm, args)
    if txt:
        b.setdefault("steps", []).append((ts, txt))
        b["steps_n"] = int(b.get("steps_n") or 0) + 1
        if len(b["steps"]) > _WL_STEPS_MEM:
            del b["steps"][:_WL_STEPS_MEM // 4]


def _session_reqs(path: Path, since_ms: float) -> dict:
    """一个会话文件里「每轮请求干了什么」。

    jsonl 是 append-only 的，所以缓存里记「已经读到第几个字节」，下次从那儿
    接着读就行 —— 页面每分钟刷一次，每次都把 50MB 从头解一遍是不能接受的。
    首次打开只吃尾部，半截行直接丢掉（那次从那行开头算起，不重不漏）。

    返回 {requestId: {t0, t1, ask, model, cwd, files, calls}}。
    """
    try:
        st = path.stat()
    except OSError:
        return {}
    key = str(path)
    with _HOST_LOCK:
        c = _HOST_CACHE.get(key)
    if c is None or c["size"] > st.st_size:          # 头一回，或文件被截断重写
        c = {"size": 0, "reqs": {}}
    if c["size"] >= st.st_size:
        with _HOST_LOCK:
            _HOST_CACHE[key] = c
        return c["reqs"]

    start = c["size"] if c["size"] else max(0, st.st_size - _WL_SESSION_TAIL)
    try:
        with path.open("rb") as f:
            f.seek(start)
            raw = f.read()
    except OSError:
        return c["reqs"]
    consumed = start + len(raw)
    if start:                                        # 从中间切进去，第一行多半是半截
        i = raw.find(b"\n")
        raw = raw[i + 1:] if i >= 0 else b""

    reqs = c["reqs"]
    for ln in raw.decode("utf-8", "replace").splitlines():
        # 预筛：reasoning 行带着整段思考文本，占了体积的九成，整行 json 解纯亏。
        # 但模型名只挂在 reasoning 和 function_call 上 —— assistant 的 message
        # 条目里没有（只有 agent/cli）。所以这类行只花两个正则抠 rid + 模型名，
        # 不解析整行：不然「这轮用的什么模型」就永远是空的。
        if '"function_call"' not in ln and '"message"' not in ln:
            m = _MODEL_RE.search(ln)
            r = _RID_RE.search(ln) if m else None
            if r:
                b0 = reqs.get(r.group(1))
                if b0 is not None and not b0.get("model"):
                    b0["model"] = m.group(1)
            continue
        try:
            d = json.loads(ln)
        except ValueError:
            continue
        if not isinstance(d, dict):
            continue
        ts = d.get("timestamp")
        if not isinstance(ts, (int, float)):
            continue
        pd = d.get("providerData") or {}
        rid = str(pd.get("conversationRequestId") or "")
        if not rid:
            continue
        b = reqs.get(rid)
        if b is None:
            b = reqs[rid] = _new_req(ts, str(d.get("cwd") or ""))
        b["t0"] = min(b["t0"], ts)
        b["t1"] = max(b["t1"], ts)
        # 模型：任何一条带 requestModelName 的都算数，不再只认 assistant message
        if not b["model"]:
            mv = str(pd.get("requestModelName") or pd.get("requestModelId") or "")
            if mv:
                b["model"] = mv
        # 同一轮里每条 assistant 消息各带一份 usage，累加才是这轮真烧掉的 token。
        # cached 已含在 inputTokens 里，只做展示，不再加到 total 上重复计。
        u = pd.get("usage")
        if isinstance(u, dict):
            b["tin"] += int(u.get("inputTokens") or u.get("input_tokens") or 0)
            b["tout"] += int(u.get("outputTokens") or u.get("output_tokens") or 0)
            det = u.get("inputTokensDetails")
            if isinstance(det, list) and det and isinstance(det[0], dict):
                b["cached"] += int(det[0].get("cached_tokens") or 0)
        if d.get("type") == "message":
            if d.get("role") == "user":
                parts = d.get("content") or []
                txt = "".join(x.get("text") or "" for x in parts if isinstance(x, dict))
                # 模式和专家就藏在这段 system-reminder 里，跟用户那句话一起进来的。
                # 每轮的头一条消息里就有，拿到就够，后面的补充消息不必再扫。
                if txt and not b.get("mode"):
                    m = _MODE_RE.search(txt)
                    if m:
                        b["mode"] = m.group(1).strip().title()
                if txt and not b.get("expert"):
                    e = _EXPERT_RE.search(txt)
                    if e:
                        b["expert"] = e.group(1)
                if not b["ask"]:
                    q = _ask_of(txt)
                    # 只取第一句当标题。别想着把「后面补的话」也收进来 —— 客户端把
                    # 下一轮那句用户消息记在了上一轮的 conversationRequestId 上，
                    # 收进来的 17 条里 14 条就是下一行的标题，页面上等于同一句话写两遍。
                    if q:
                        b["ask"] = q
        else:
            _note_call(b, str(d.get("name") or "").strip(), _call_args(d), ts)

    c["size"] = consumed
    with _HOST_LOCK:
        _HOST_CACHE[key] = c
        if len(_HOST_CACHE) > 16:
            for k in list(_HOST_CACHE)[:-16]:
                _HOST_CACHE.pop(k, None)
    return reqs


# 会话目录里那种「只有时间戳」的一层：2026-11-11-02-32-32 / 2026_11_20_01-49-36。
# WorkBuddy 的 space 目录用**下划线**连日期（spaces/2026_11_20_01-49-36），以前只认
# 连字符，这串就原样当项目名发给前端了 —— 跟左边的时间列并排显示，同一个日期
# 写了两遍。分隔符统一用 [-_] 容错。
_DATE_LAYER = re.compile(r"\d{4}[-_]\d{1,2}[-_]\d{1,2}(?:[-_ T]\d{1,2}[-:]\d{2}(?:[-:]\d{2})?)?")
# 时间戳上面还有一层纯容器名（spaces / projects / sessions），它本身不是项目名，
# 剥完才能落回「WorkBuddy」这种真名字。
_CONTAINER_LAYERS = {"spaces", "space", "projects", "project", "sessions",
                     "conversations", "chats", "workspaces", "workspace"}


def _proj_of(cwd: str) -> str:
    """cwd → 一个能认出来的项目名。

    WorkBuddy 的会话目录是 <...>/WorkBuddy/2026-11-11-02-32-32，space 是
    <...>/WorkBuddy/spaces/2026_11_20_01-49-36。直接取最后一段会把日期当项目名，
    剥掉「时间戳层」和「容器层」才落回「WorkBuddy」这种名字；剥完什么都不剩就
    返回空串，前端那边只显示 token 数，不重复第二遍日期。
    """
    parts = [p for p in str(cwd or "").rstrip("/").split("/") if p]
    # 最多剥三层：再往上就是用户名/盘符了，剥成 "pamphile" 还不如空着
    for _ in range(3):
        if not parts:
            break
        last = parts[-1]
        if _DATE_LAYER.fullmatch(last) or last.lower() in _CONTAINER_LAYERS:
            parts.pop()
        else:
            break
    return parts[-1] if parts else ""


# 上下文维度预设：模式 / 专家是 WorkBuddy 的说法，不是所有 agent 都有。
# TRAE 讲的是「代理类型」（solo_agent / general_purpose_task），Claude Code
# 是「权限模式 / 子代理」，Cherry 只有技能。facets 的内部 key 保持不变（搜索、
# 过滤、合并都靠它），这里只决定「展开时这几个维度在界面上叫什么」。
# 新接一个 agent：给它加一行，再在取数函数里把证据翻译成对应 key 即可。
_CTX_PRESETS: dict[str, dict[str, str]] = {
    "WorkBuddy":     {"mode": "模式", "expert": "专家", "skills": "技能", "connectors": "连接器"},
    "Claude Code":   {"mode": "权限模式", "agent": "子代理", "skills": "技能", "connectors": "MCP"},
    "Cherry Studio": {"skills": "技能", "connectors": "MCP"},
    "TRAE":          {"agent": "代理", "skills": "技能", "connectors": "MCP"},
}
_CTX_FALLBACK = {"mode": "模式", "expert": "专家", "skills": "技能", "connectors": "连接器"}


def ctx_labels(client: str) -> dict[str, str]:
    """这轮展开时各上下文维度的显示名。前端拿不到就退回 WorkBuddy 那套。"""
    return dict(_CTX_PRESETS.get(client, _CTX_FALLBACK))


def _req_item(b: dict, rid: str, client: str, tag: str,
              status: str = "ok") -> dict | None:
    """一轮账页 → 卡片上的一行。

    所有客户端共用这一份：谁读出来的账页都长同一个样（ask / model / files /
    mcp / mode / expert / skills），组装规则就不该抄四遍。加一家客户端只要写
    一个把它的日志翻译成 `_new_req()` + `_note_call()` 的取数函数。

    `status` 由调用方判定（见 `_live_rid`）：还在跑的那一轮传 "run"，
    交付时 end 留空、secs 用「到现在」——已经干了多久，而不是干完了多久。
    """
    if not b.get("calls") and not b.get("ask"):
        return None                      # 只列了个计划的请求不算干过活
    start, end = _ms(b["t0"]), _ms(b["t1"])
    if not start:
        return None
    run = status == "run"
    if run:
        end = ""
        secs = max(0, int((time.time() * 1000 - b["t0"]) / 1000))
    else:
        secs = _secs(start, end)
    bits = []
    # 「这一轮干完是什么结果」排最前：Trae 的会话记忆自带 outcome 一句话，
    # 比「几步、动了哪些文件」更像人话。没有这一项（别的客户端）就跳过。
    note = " ".join(str(b.get("note") or "").split())
    if note:
        bits.append(_clip(note, 60))
    # 「用什么方式干的」排最前：同样是改一个文件，Agent 模式下一步步试错、
    # Plan 模式下先给方案、Ask 模式压根没动手 —— 事后看账，这比文件名更能
    # 解释「为什么花了 20 步」。专家同理：同一条任务换了人做，结论不同。
    mode = str(b.get("mode") or "")
    if mode:
        bits.append(mode)
    ename = _expert_name(str(b.get("expert") or ""))
    if ename:
        # 全名留给 facets 的 chip（展开后能看全），收起时这行只有半宽，
        # 「专家 全栈自动化脚本工程师」会把文件名和步数全挤出屏幕。
        bits.append("专家 " + _brief(ename, 4))
    proj = _proj_of(b["cwd"])
    if proj and proj not in (client, "WorkBuddy"):   # 执行者那列已经写着客户端了
        bits.append(proj)
    # MCP 排在文件名前：25 步里混着 Bash/Edit 看不出动了外部服务（表格、
    # 邮箱、别人的知识库）。放尾巴上会被 160 字上限连同文件名一起截掉。
    mcp = b.get("mcp") or {}
    if mcp:
        top = sorted(mcp.items(), key=lambda kv: (-kv[1], kv[0]))[:2]
        bits.append("MCP " + "、".join(f"{k}×{v}" if v > 1 else k for k, v in top)
                    + ("…" if len(mcp) > 2 else ""))
    skills = [s for s in (b.get("skills") or []) if s]
    if skills:
        bits.append("技能 " + "、".join(skills[:2]) + ("…" if len(skills) > 2 else ""))
    # 后面这两段单独留一份：展开时上下文那半截已经在 chips 里了，详情行只剩
    # 「改了哪些文件 · 几步」，同一句话不写两遍（上一轮为「信息重复」返工过）。
    rest: list[str] = []
    # 步数在前：detail 有 160 字上限，把步数摆末尾会被文件名截掉半个数字
    if b["calls"]:
        rest.append(f"{b['calls']} 步")
    if b["files"]:
        # 三个文件名连起来比模式+专家还长，半宽卡里等于什么都没说。
        # 两个足够认出「这轮动了哪块」，余下的报个数，点开步骤里都还在。
        rest.append("、".join(b["files"][:2])
                    + (f" 等{len(b['files'])}个" if len(b["files"]) > 2 else ""))
    task = b["ask"] or (("改 " + b["files"][0]) if b["files"] else "会话")
    src = [_tool(client)]
    if b["model"]:
        src.append(_ai(str(b["model"])))
    for srv in list(mcp)[:3]:            # 供搜索/过滤，执行者那列不显示它
        src.append({"kind": "mcp", "name": srv})
    # 模式 / 专家 / 技能也进 sources：卡片上不占地方，但「哪几轮是 Plan 起手的」
    # 「哪位专家做过什么」这类筛选取的是这里 —— 右卡目前还没搜索框，先备着。
    if mode:
        src.append({"kind": "mode", "name": mode})
    if ename:
        src.append({"kind": "expert", "name": ename})
    for sk in skills[:3]:
        src.append({"kind": "skill", "name": sk})
    # 展开后那段「这轮是在什么上下文里跑的」：执行者那格只有两行高（客户端
    # / 模型），塞不下四样，所以四样一律走 facets，前端在明细顶部排一行。
    facets: dict[str, object] = {}
    if mode:
        facets["mode"] = mode
    if ename:
        facets["expert"] = ename
    if skills:
        facets["skills"] = skills[:6]
    if mcp:
        facets["connectors"] = [{"name": k, "n": v} for k, v in
                                sorted(mcp.items(), key=lambda kv: (-kv[1], kv[0]))[:6]]
    # 具体干了哪几步：卡片上点开这一行看到的就是它（时间 + 工具 + 对象）。
    # 只发最近 18 条，剩下的报个「还有 N 步」，别让卡片点开变成一整本流水。
    tail = (b.get("steps") or [])[-_WL_STEPS_SEND:]
    # 只发两列（时间 / 一行字）：这条每 60 秒随整份快照传一次，
    # 键名和状态字段看着不多，80 条 × 18 步乘起来就是几十 KB。
    kids = [{"start": _ms(t), "detail": txt[:48]} for t, txt in tail]
    tin, tout = int(b.get("tin") or 0), int(b.get("tout") or 0)
    return {
        "id": tag + str(rid)[-6:],
        # 会话 id（jsonl 文件名）：页面靠它去 spaces 映射里查「这轮属于哪个空间」。
        # 上面那个 id 是「标签+轮次末 6 位」，本来就只为 DOM key 服务，join 不上会话库。
        "sid": str(b.get("sid") or ""),
        "task": _clip(task, 320),        # 跟着 _ask_of 一起放宽，别再二次截回 80
        "detail": " · ".join(bits + rest)[:160],
        "rest": " · ".join(rest)[:160],  # 展开时用这句：上下文已由 chips 交代
        "status": status,
        "start": start, "end": end,
        "secs": secs,
        # 毫秒起始点：页面上「进行中」那几行的秒表要自己走，不能等一分钟一次的轮询
        "t0": int(b["t0"]),
        "sources": src,
        "facets": facets,
        "ctx_labels": ctx_labels(client),  # 各 agent 自己定义维度名（模式/代理/权限模式…）
        "client": client,
        "children": kids,
        "more_steps": max(0, int(b.get("steps_n") or 0) - len(kids)),
        "project": proj,
        "tokens": {"in": tin, "out": tout,
                   "cached": int(b.get("cached") or 0), "total": tin + tout},
    }


# —— 实时采样：这一轮到底还在跑，还是已经答完 ——————————————
# 日志是 append-only 的，**最后一条记录的类型**就是最诚实的答案：
#   assistant 的 message      → 这一轮答完了（模型把话说完了）
#   reasoning / function_call / function_call_result / user 消息
#                             → 还在想、工具还在跑、或刚发出指令还没回
# 不按「多久没写」猜：长任务中间本来就会停几十秒（跑测试、等编译）。
# 只有超过 5 分钟一条新记录都没有，才当成收尾了 —— 那多半是窗口关了、
# 进程被杀了，不能让它永远挂着一根绿条。用户 2026-11-21 要求「正在做的
# 和做好的一直没区分开」，要的就是这个口径。
_LIVE_TAIL = 48 * 1024              # 只读文件尾巴：够装下最后一轮的几步
_LIVE_STALE_MS = 300 * 1000         # 5 分钟没有任何新记录 = 不装「进行中」了
_LIVE_FRESH_S = 7200                # 2 小时内动过的会话才值得读尾巴
# 这些是「记账用的旁注」，不是一轮里的动作，跳过它们去找真正的最后一条
_LIVE_SKIP = {"session-meta", "ai-title", "file-history-snapshot",
              "token_count", "usage", "compact", "checkpoint"}


def _tail_entry(path: Path) -> dict | None:
    """文件最后一条**有业务含义**的记录。认不出结构就返回 None。"""
    try:
        size = path.stat().st_size
        with path.open("rb") as f:
            f.seek(max(0, size - _LIVE_TAIL))
            buf = f.read()
    except OSError:
        return None
    for raw in reversed(buf.split(b"\n")):
        s = raw.strip()
        if not s.startswith(b"{"):
            continue                      # 半截行（正在写）直接跳过
        try:
            d = json.loads(s)
        except ValueError:
            continue
        if not isinstance(d, dict):
            continue
        t = str(d.get("type") or "")
        if not t or t in _LIVE_SKIP:
            continue
        pd = d.get("providerData") or {}
        return {"type": t, "role": str(d.get("role") or ""),
                "rid": str(pd.get("conversationRequestId") or ""),
                "ts": int(d.get("timestamp") or 0)}
    return None


def _live_rid(path: Path) -> dict:
    """这个会话现在跑的是哪一轮：{rid, run, ts}。

    `rid` 为空表示尾部认不出轮次（老格式/纯工具会话），调用方退回
    「最后一轮算进行中」。判完成的三个条件写在一起，免得三处口径漂移。
    """
    e = _tail_entry(path)
    if not e:
        return {"rid": "", "run": False, "ts": 0}
    if e["type"] == "message" and e["role"] == "assistant":
        done = True
    elif e["ts"] and (time.time() * 1000 - e["ts"]) > _LIVE_STALE_MS:
        done = True
    else:
        done = False
    # 刚写完这一条的一两秒里可能还在补收尾，先按「在跑」显示，下一轮采样归位
    if done and e["ts"] and (time.time() * 1000 - e["ts"]) < 2000:
        done = False
    return {"rid": e["rid"], "run": not done, "ts": e["ts"]}


def _wb_items(since_ms: float) -> list[dict]:
    """WorkBuddy 每轮会话一条：用户那句话当任务名，写过哪些文件当明细。

    数据源不是 changes-index —— 那份只索引工作区内的记忆文件，改代码根本不
    在里面。真正靠得住的是会话 jsonl 里的 function_call（Edit/Write 带
    file_path）和 message（用户原话 + 模型名 + 模式 + 专家）。
    """
    try:
        paths = [p for p in _WB_SESSIONS.glob("*/*.jsonl")
                 if p.stat().st_mtime * 1000 >= since_ms]
    except OSError:
        return []
    out: list[dict] = []
    now_s = time.time()
    for p in paths:
        try:
            mt = p.stat().st_mtime
        except OSError:
            continue
        reqs = _session_reqs(p, since_ms)
        if not reqs:
            continue
        # 采样：这个会话现在是不是还在跑，跑的是哪一轮
        live = _live_rid(p) if (now_s - mt) < _LIVE_FRESH_S else {"rid": "", "run": False}
        # 尾部认不出轮次时，退一步把「最后一轮」当进行中（本会话的最后一轮＝眼前这轮）
        last_rid = max(reqs, key=lambda r: reqs[r]["t1"])
        target = live["rid"] or last_rid
        for rid, b in reqs.items():
            if b["t1"] < since_ms:
                continue
            b["sid"] = p.stem           # 会话 id，留给「项目＝空间」join 用
            st = "run" if (live["run"] and rid == target) else "ok"
            it = _req_item(b, rid, "WorkBuddy", "h" + p.stem[:8], st)
            if it:
                out.append(it)
    return out


# —— Cherry Studio：SQLite 里的对话 ——————————————————————————
# message 表一行一条消息，字段：topic_id / role / data(JSON) / model_id /
# created_at(毫秒) / status。同一 topic 的一组消息算一轮「活」。

def _cherry_text(raw) -> str:
    """message.data → 纯文本。形如 {"parts":[{"type":"text","text":"…"}]}。"""
    try:
        d = json.loads(raw or "{}")
    except (ValueError, TypeError):
        return ""
    parts = d.get("parts") if isinstance(d, dict) else None
    if not isinstance(parts, list):
        return ""
    txt = " ".join(str(p.get("text") or "") for p in parts
                   if isinstance(p, dict) and p.get("type") == "text")
    txt = re.sub(r"@\S+?:\S*", " ", txt)          # 附件占位
    return re.sub(r"\s+", " ", txt).strip()


def _cherry_model(mid) -> str:
    """model_id 形如 <user_model_id>::<provider>/<model>:<量化>，只留模型名。

    留着 `72a4aecc-4dc3…::kiwi_kiwi/qwen3.8-abliterated-9b:q4k` 挂在页面上
    一行都放不下，而且那串 uuid 对人没有任何信息量。
    """
    s = str(mid or "")
    if "::" in s:
        s = s.split("::", 1)[1]
    s = s.split("/")[-1].split(":")[0]
    return s.strip()


def _cherry_items(since_ms: float) -> list[dict]:
    """Cherry Studio 这两天的对话，一个话题一条。

    `?mode=ro` 只读打开：Cherry 可能正开着，绝不能写它的库。
    话题先按「窗口内有消息」筛出来，再把整段消息读全 —— 否则一条
    聊了三天的长对话，任务名会变成中间某句话。
    """
    dbs: list[Path] = []
    for h in _home_dirs():
        for rel in _CHERRY_REL:
            p = h.joinpath(*rel)
            if p.exists() and p not in dbs:
                dbs.append(p)
    if not dbs:
        return []
    con = None
    try:
        con = sqlite3.connect(dbs[0].as_uri() + "?mode=ro", uri=True, timeout=1.5)
        con.row_factory = sqlite3.Row
        rows = con.execute(
            "select topic_id, role, data, model_id, created_at, status from message "
            "where deleted_at is null and topic_id in ("
            "  select topic_id from message"
            "  where deleted_at is null and created_at >= ? group by topic_id"
            ") order by created_at", (int(since_ms),)).fetchall()
    except sqlite3.Error:
        return []
    finally:
        if con is not None:
            con.close()

    by_topic: dict[str, dict] = {}
    for r in rows:
        tid = str(r["topic_id"] or "")
        if not tid:
            continue
        b = by_topic.get(tid)
        if b is None:
            b = by_topic[tid] = {"t0": r["created_at"], "t1": r["created_at"],
                                 "ask": "", "model": "", "n": 0, "fail": False}
        b["t0"] = min(b["t0"], r["created_at"])
        b["t1"] = max(b["t1"], r["created_at"])
        role = str(r["role"] or "")
        if role == "user" and not b["ask"]:
            b["ask"] = _clip(_cherry_text(r["data"]), 320)
        elif role == "assistant" and not b["model"]:
            b["model"] = _cherry_model(r["model_id"])
        # 只统计「窗口内」的消息：一个话题能挂几个月（问完就放着），
        # 用全量算时长会写出「1 个月 22 天」，用全量算状态会把两个月前的
        # 一次报错挂到今天那一行上。
        if r["created_at"] >= since_ms:
            b["n"] += 1
            if role == "assistant" and str(r["status"] or "") == "error":
                b["fail"] = True

    out: list[dict] = []
    for tid, b in by_topic.items():
        if not b["n"]:
            continue
        start = _ms(max(b["t0"], since_ms))      # 起点不早于窗口，否则时长没意义
        end = _ms(b["t1"])
        if not start or not end:
            continue
        src = [_tool("Cherry Studio")]
        if b["model"]:
            src.append(_ai(b["model"]))
        out.append({
            "id": "c" + tid.replace("-", "")[:10],
            "task": _clip(b["ask"] or "Cherry 对话", 320),
            "detail": f"{b['n']} 条消息"[:160],
            "status": "fail" if b["fail"] else "ok",
            "start": start, "end": end,
            "secs": _secs(start, end),
            "sources": src,
            "client": "Cherry Studio",
        })
    return out


# —— 通用 JSONL：Claude Code 这类「一行一个事件」的 agent ————————————
# Claude Code 落在 ~/.claude/projects/<目录 slug>/<会话>.jsonl，一行一个事件：
#   {"type":"user","message":{"role":"user","content":[{"type":"text","text":"…"}]},
#    "timestamp":"2026-11-19T08:06:10.879Z","cwd":"/x","uuid":"…"}
#   {"type":"assistant","message":{"role":"assistant","model":"claude-…",
#    "content":[{"type":"tool_use","name":"Bash","input":{…}}]}, …}
# 轮次靠用户消息切：content 全是 tool_result 的 user 行是上一步的回执，不是新
# 的一轮。模式和专家这两家没有，就留空 —— 面板上宁可不写，也不编一个出来。
_CLAUDE_SESSIONS = Path.home() / ".claude" / "projects"
_CLI_CACHE: dict[str, dict] = {}


def _iso_ms(v) -> float:
    """ISO 时间串（2026-11-19T08:06:10.879Z）→ 毫秒；已经是数字就原样用。"""
    if isinstance(v, (int, float)):
        return float(v)
    s = str(v or "").strip()
    if not s:
        return 0.0
    try:
        return datetime.fromisoformat(s.replace("Z", "+00:00")).timestamp() * 1000
    except ValueError:
        return 0.0


def _cli_reqs(path: Path) -> dict:
    """一个通用 JSONL 会话 → {轮 key: 账页}。增量读，道理同 _session_reqs。

    轮 key 用那条用户消息的 uuid；没有 uuid 就退化成它的时间戳。同一份文件
    追加读时 cur 一并存进缓存，所以半路接上也能续到正确的那一轮。
    """
    try:
        st = path.stat()
    except OSError:
        return {}
    key = "cli:" + str(path)
    with _HOST_LOCK:
        c = _CLI_CACHE.get(key)
    if c is None or c["size"] > st.st_size:
        c = {"size": 0, "cur": "", "reqs": {}}
    if c["size"] >= st.st_size:
        with _HOST_LOCK:
            _CLI_CACHE[key] = c
        return c["reqs"]

    start = c["size"] if c["size"] else max(0, st.st_size - _WL_SESSION_TAIL)
    try:
        with path.open("rb") as f:
            f.seek(start)
            raw = f.read()
    except OSError:
        return c["reqs"]
    consumed = start + len(raw)
    if start:
        i = raw.find(b"\n")
        raw = raw[i + 1:] if i >= 0 else b""

    reqs = c["reqs"]
    cur = str(c.get("cur") or "")
    for ln in raw.decode("utf-8", "replace").splitlines():
        if '"message"' not in ln:        # 摘要 / 快照 / 状态行一概跳过
            continue
        try:
            d = json.loads(ln)
        except ValueError:
            continue
        msg = d.get("message") if isinstance(d, dict) else None
        if not isinstance(msg, dict):
            continue
        blocks = msg.get("content")
        blocks = blocks if isinstance(blocks, list) else []
        ts = _iso_ms(d.get("timestamp"))
        role = str(msg.get("role") or d.get("type") or "")
        if role == "user":
            if blocks and all(isinstance(x, dict) and x.get("type") == "tool_result"
                              for x in blocks):
                continue                 # 工具回执，不是新的一轮
            txt = "".join(str(x.get("text") or "") for x in blocks
                          if isinstance(x, dict) and x.get("type") == "text")
            if not txt.strip():
                continue
            cur = str(d.get("uuid") or "") or ("%d" % int(ts or 0))
            b = reqs.get(cur)
            if b is None:
                b = reqs[cur] = _new_req(ts, str(d.get("cwd") or ""))
            b["t0"] = min(b["t0"], ts)
            b["t1"] = max(b["t1"], ts)
            if not b["ask"]:
                b["ask"] = _ask_of(txt, plain_ok=True)
            continue
        b = reqs.get(cur)
        if b is None:
            continue                     # 会话开头没用户消息（续读从半路起）
        b["t1"] = max(b["t1"], ts)
        if not b["model"]:
            b["model"] = str(msg.get("model") or "")
        for x in blocks:
            if isinstance(x, dict) and x.get("type") == "tool_use":
                args = x.get("input") if isinstance(x.get("input"), dict) else {}
                _note_call(b, str(x.get("name") or ""), args, ts)

    c["size"] = consumed
    c["cur"] = cur
    with _HOST_LOCK:
        _CLI_CACHE[key] = c
        if len(_CLI_CACHE) > 16:
            for k in list(_CLI_CACHE)[:-16]:
                _CLI_CACHE.pop(k, None)
    return reqs


def _claude_items(since_ms: float) -> list[dict]:
    """Claude Code 每轮一条。没装就是空列表，不编数据。"""
    if not _CLAUDE_SESSIONS.exists():
        return []
    try:
        paths = [p for p in _CLAUDE_SESSIONS.glob("*/*.jsonl")
                 if p.stat().st_mtime * 1000 >= since_ms]
    except OSError:
        return []
    out: list[dict] = []
    for p in paths:
        for rid, b in _cli_reqs(p).items():
            if b["t1"] < since_ms:
                continue
            b["sid"] = p.stem
            it = _req_item(b, rid, "Claude Code", "a" + p.stem[:8])
            if it:
                out.append(it)
    return out


# —— Trae：memory/projects 下的会话记忆 ————————————————————————
# 这里原先写着「Trae 读不了」：对话在云端，database.db 是私有格式。
# 那句话只说对了一半 —— 对话正文确实在加密库里，但 Trae 自己每轮会往
#   <家目录>/.trae-cn/memory/projects/<项目>/<yyyymmdd>/session_memory_*.jsonl
# 写一行明文记忆：intent / actions / outcome / message_summary_time。
# 「这一轮想干什么、做了哪几步、结果如何」都在里面，比事后猜改了哪些文件准。
#
# 两个目录都看：.trae-cn 是国内版，.trae 是国际版。Windows 上还得扫
# C:\Users\* —— 中台常以服务身份跑，Path.home() 会指到 systemprofile，
# 那里当然没有任何人的 Trae 数据。
_TRAE_DIRS = (".trae-cn", ".trae")


def _home_dirs() -> list[Path]:
    """这台机器的家目录候选。Windows 服务账号下 Path.home() 是错的，
    得按 C:\\Users\\* 扫一遍 —— 和 agent_link 那份同源，别各写一套。

    两个名字都认：`homes` 是新公开名，`_homes` 是老名字。机器上跑着的
    agent_link 不一定是同一个版本（升级只换了 dash.py 的时候就会错位），
    认死一个名字的结果是静默回落 Path.home()，在 Windows 上等于读不到数据。
    """
    try:
        import agent_link
        fn = getattr(agent_link, "homes", None) or getattr(agent_link, "_homes", None)
        if fn is not None:
            dirs = [p for p in fn() if p]
            if dirs:
                return dirs
    except Exception:                    # noqa: BLE001
        pass
    return [Path.home()]


def _trae_roots() -> list[Path]:
    out: list[Path] = []
    for h in _home_dirs():
        for d in _TRAE_DIRS:
            p = h / d / "memory" / "projects"
            if p.is_dir():
                out.append(p)
    return out


def _trae_ts(v) -> float:
    """'2026-11-23 02:12:06' → 毫秒。按本地时间算，和卡片上显示的口径一致。"""
    try:
        return datetime.strptime(str(v or "")[:19],
                                 "%Y-%m-%d %H:%M:%S").timestamp() * 1000
    except (ValueError, TypeError, OSError):
        return 0.0


def _trae_items(since_ms: float) -> list[dict]:
    """Trae 这些天的会话记忆，一轮一条。只读，不动它的目录。

    单个文件坏掉就跳过那一份：卡片不该因为一行半截的 json 整块空掉。
    记忆行里没有文件级信息，所以 steps 用 actions 顶（点开一行看到的就是
    「它当时自己记下的几步」），files 留空 —— 不编。
    """
    out: list[dict] = []
    for root in _trae_roots():
        try:
            files = list(root.glob("*/*/session_memory_*.jsonl"))
        except OSError:
            continue
        for p in files:
            try:
                if p.stat().st_mtime * 1000 < since_ms:
                    continue
                text = p.read_text(encoding="utf-8", errors="replace")
            except OSError:
                continue
            for line in text.splitlines():
                line = line.strip()
                if not line:
                    continue
                try:
                    o = json.loads(line)
                except (ValueError, TypeError):
                    continue
                if not isinstance(o, dict):
                    continue
                ts = _trae_ts(o.get("message_summary_time"))
                if not ts or ts < since_ms:
                    continue
                intent = " ".join(str(o.get("intent") or "").split())
                acts = [" ".join(str(a).split()) for a in (o.get("actions") or []) if a]
                if not intent and not acts:
                    continue
                mid = str(o.get("message_id") or p.stem)
                b = _new_req(ts, "")
                b["t1"] = ts
                b["ask"] = _clip(intent or acts[0], 320)
                b["note"] = " ".join(str(o.get("outcome") or "").split())
                b["calls"] = len(acts) or 1
                b["steps"] = [(ts, a) for a in acts][:_WL_STEPS_MEM]
                b["steps_n"] = len(acts)
                b["sid"] = p.stem
                it = _req_item(b, mid, "TRAE", "tr" + mid[:8])
                if it:
                    out.append(it)
    out.extend(_trae_self_items(since_ms))
    return out


def _trae_self_items(since_ms: float) -> list[dict]:
    """TRAE 自己报回来的账（`_本机工作/TRAE自述.jsonl`），一轮一条。

    为什么还要这条路：会话记忆（session_memory_*.jsonl）只有「想干什么、结论是什么」，
    **没有用量**，而且新版 Trae 一换存放位置就读不到；自述这条是 Trae 主动 POST 来的，
    带 token、换了机器和版本都还在。两条并存，按 id 天然不撞（一个带 message_id，
    一个带时间戳+任务名）。

    字段口径与 /api/trae/note 一致：task / summary / project / model / tin / tout /
    cached / calls。估不出来就写 0 —— 宁可显示 0，也不编一个像样的数字。
    """
    p = paths.data_root() / "_本机工作" / "TRAE自述.jsonl"
    out: list[dict] = []
    try:
        if not p.exists() or p.stat().st_mtime * 1000 < since_ms:
            return out
        lines = p.read_text(encoding="utf-8", errors="replace").splitlines()[-400:]
    except OSError:
        return out
    for ln in lines:
        ln = ln.strip()
        if not ln:
            continue
        try:
            o = json.loads(ln)
        except (ValueError, TypeError):
            continue
        if not isinstance(o, dict):
            continue
        try:
            ts = float(o.get("at") or 0) * 1000
        except (TypeError, ValueError):
            continue
        if not ts or ts < since_ms:
            continue
        task = " ".join(str(o.get("task") or "").split())
        if not task:
            continue
        b = _new_req(ts, str(o.get("project") or ""))
        b["t1"] = ts
        b["ask"] = _clip(task, 320)
        b["note"] = " ".join(str(o.get("summary") or "").split())
        b["model"] = str(o.get("model") or "")
        for k, src in (("tin", "tin"), ("tout", "tout"), ("cached", "cached")):
            try:
                b[k] = max(0, int(o.get(src) or 0))
            except (TypeError, ValueError):
                b[k] = 0
        try:
            b["calls"] = max(0, int(o.get("calls") or 0))
        except (TypeError, ValueError):
            b["calls"] = 0
        # 自述里没有分步，calls 只用来让卡片显示「× 步」；一条都没有就按 1 算，
        # 否则 _req_item 会因为「没调用也没提问」把它整条丢掉。
        b["calls"] = b["calls"] or 1
        rid = "trself" + hashlib.sha1(f"{o.get('at')}|{task}".encode()).hexdigest()[:10]
        it = _req_item(b, rid, "TRAE", rid)
        if it:
            it["source"] = "自述"
            out.append(it)
    return out


# 客户端注册表：加一个新客户端＝加一行 + 一个取数函数。
# 不在 _worklog 里写 if 分支 —— 那样「以后换成 Trae / Cursor」就得改核心逻辑。
#
# 没登记的客户端及原因（别为了凑数编数据）：
#   Codex / Cursor —— 本机还没装。Codex 的 rollout jsonl 塞在
#              ~/.codex/sessions/年/月/日/ 下，结构不同，装上后照 _cli_reqs
#              再写一个翻译层即可（_req_item 那一份不用动）。
_CLIENTS = (
    ("WorkBuddy", _wb_items),
    ("Cherry Studio", _cherry_items),
    ("Claude Code", _claude_items),
    ("TRAE", _trae_items),
)


# 本机工作的采集层（原来跑在 8775 那个独立服务里，2026-11-20 并进口袋）。
# 同进程直接调模块，不走 HTTP：少一个进程、少一次跨端口，手机上也一样。
_CUNTU_DIR = Path.home() / "Documents" / "cuntu"
if _CUNTU_DIR.is_dir() and str(_CUNTU_DIR) not in sys.path:
    sys.path.insert(0, str(_CUNTU_DIR))


def _hostwork_module():
    try:
        import hostwork
        return hostwork
    except Exception:                              # noqa: BLE001
        return None


def _hostwork_rounds(days: int) -> list[dict]:
    """右卡的逐轮记录问本机工作的采集层要 —— 它才是解析会话流水的那一方。

    工作台这张卡是**调用方**，不自己再解析一遍 WorkBuddy / Cherry / Claude
    的 jsonl：两边各读一次，字段名和截断规则迟早漂移。模块不在时退回本地
    解析，卡片不至于空着（资源视图那档也是这个待遇）。
    """
    key = "today" if days <= 1 else ("7d" if days <= 7 else "30d")
    hw = _hostwork_module()
    if hw is not None:
        try:
            d = hw.rounds_items(key)
            if d.get("ok") and isinstance(d.get("items"), list):
                return d["items"][:_WL_HOST_LIMIT]
        except Exception:                          # noqa: BLE001
            pass
    return []


def _stamp_credible(items: list[dict]) -> bool:
    """这批账的时间戳有区分度吗。

    采集层（hostwork）读不到证据链时会拿「当前时刻」兜底，几十条于是挤在
    同一秒里：卡片上看着有 60 条活，点开全是同一个时间 —— 比空着更误导
    （EVO 上实测 40 条卡在 22:21:00、14 条卡在 22:22:17）。
    真账里同一秒最多也就几轮同时起，绝不会占到两成。所以看「最大的那一秒
    占了多少」：超过两成就是在兜底，这批不算数。混进几条真记录也蒙不过去。
    """
    if len(items) < 5:                 # 太少，少到没有统计意义
        return True
    tally: dict[str, int] = {}
    for i in items:
        s = str(i.get("start") or "")[:19]
        if s:
            tally[s] = tally.get(s, 0) + 1
    if not tally:
        return False
    return max(tally.values()) * 5 <= len(items)


def _agent_items(days: int = _WL_HOST_DAYS) -> list[dict]:
    """本机上所有 AI 客户端的活，合成一份。一个客户端读挂了不拖垮整张卡。"""
    remote = _hostwork_rounds(days)
    if remote and _stamp_credible(remote):
        return remote
    if remote:
        # 不静默吞掉：以后有人翻日志，得能看出「为什么卡片没用采集层那份」。
        print(f"[worklog] hostwork 交来 {len(remote)} 条，时间戳重复过多"
              "（判定为兜底值），改走本地读取", flush=True)
    since_ms = (time.time() - days * 86400) * 1000
    out: list[dict] = []
    for _name, fn in _CLIENTS:
        try:
            out.extend(fn(since_ms))
        except Exception:            # noqa: BLE001
            continue
    return out


def _pocket_pulse(items: list[dict]) -> list[dict]:
    """口袋的一堆零碎活折成「一天一条」。

    本机那格要回答的是「这台电脑今天在忙什么」，pocket 在里面应该是一个名字，
    不是几十行抓取日志 —— 细节在左边那格拉。
    """
    by_day: dict[str, list[dict]] = {}
    for it in items:
        by_day.setdefault((it.get("start") or "")[:10], []).append(it)
    out: list[dict] = []
    for day, its in by_day.items():
        if not day:
            continue
        okn = sum(1 for x in its if x["status"] == "ok")
        failn = sum(1 for x in its if x["status"] == "fail")
        runn = sum(1 for x in its if x["status"] == "run")
        kinds: dict[str, int] = {}
        for x in its:
            k = str(x["task"]).split(" · ")[0]
            kinds[k] = kinds.get(k, 0) + 1
        top = sorted(kinds.items(), key=lambda kv: -kv[1])[:3]
        starts = sorted(x["start"] for x in its if x.get("start"))
        ends = sorted(x["end"] for x in its if x.get("end"))
        bits = [f"{len(its)} 条"] + [f"{k} {n}" for k, n in top]
        if failn:
            bits.append(f"失败 {failn}")
        out.append({
            "id": "pp" + day,
            "task": "pocket",
            "detail": " · ".join(bits)[:160],
            # 有失败也还是「干完了」：整条标红是口袋挂了的意思，不是几条抓取没成
            "status": "run" if runn else "ok",
            "start": starts[0] if starts else "",
            "end": ends[-1] if ends else "",
            "secs": None,
            "sources": [_tool("pocket")],
            "client": "pocket",
        })
    return out


def _manual_host_items(cfg: dict) -> list[dict]:
    """补记的那几条：能手写「本机干了什么」的只有人，日志里没有的活靠它。

    2026-11-18 起它们进右卡，不进左卡 —— 补记的内容基本都是「WorkBuddy 改了
    pocket 的代码」，那是 WorkBuddy 的活，不是 pocket 的活。左卡只认日志。
    """
    out: list[dict] = []
    for it in read_worklog(cfg):
        it = dict(it)
        client = str(it.get("client") or "")
        # 客户端放最前面：一行里先认出「谁」，再看它用哪个模型、动了哪个文件
        it["sources"] = ([_tool(client)] if client else []) + list(it.get("sources") or [])
        out.append(it)
    return out


def _worklog(cfg: dict) -> dict:
    """两张卡，两份账，不互相掺。

    左卡 `items` = **中台在做的事**（2026-11-21 改名，原来叫「口袋在做的事」）：
        口袋 + 虫巢 + 设备 + 游戏，各插件自己的日志翻成同一种 item，多带
        `plugin` 字段，页面按插件分栏。口袋那半由 pocket.jsonl 现算，
        **不接受补记** ——「谁改了 pocket 的代码」是那个客户端干的。
    右卡 `host` = 这台电脑上别的活：各 AI 客户端（WorkBuddy / Cherry …）
        每轮一条、pocket 折一天一条、补记的几条，三类并一份。
        pocket 那半只吃口袋自己的账，不吃别的插件 —— 那格回答的是
        「这台机器在忙什么」，插件细账在左卡拉。
    """
    auto = _auto_worklog(cfg)               # 口袋自己的活
    items = _collapse(auto + _plugin_items(cfg))
    # 进行中的永远顶在最上面（「现在在跑什么」比五分钟前跑完什么更该先看到）
    items.sort(key=lambda x: (x.get("status") == "run", x.get("start") or ""), reverse=True)

    # 左卡的插件筛选条：名单来自 plugins.json（含还没出过活的），条数按合并后的账现数
    # 虫巢先从筛选条上拿掉：账不并进来，留一个「虫巢 0」只会让人以为它坏了。
    # 它自己的操作记录在 8782 那个页面上，一条不少。哪天 nest_in_flow 打开，
    # 下面 by_id 的兜底会自动把这一栏加回来。
    plugins = [p for p in plugin_manifest(cfg) if p["id"] != "nest"]
    by_id = {p["id"]: p for p in plugins}
    for it in items:
        pid = str(it.get("plugin") or "pocket")
        p = by_id.get(pid)
        if p is None:                   # plugins.json 里删了、日志里还留着的插件
            p = by_id.setdefault(pid, {"id": pid, "name": _PG_LABEL.get(pid, pid),
                                       "glyph": "", "url": "", "n": 0, "fail": 0})
            plugins.append(p)
        p["n"] += 1
        if it.get("status") == "fail":
            p["fail"] += 1

    # 右卡：pocket 折成一天一条 + 各客户端 + 补记，按「最近还在动」排
    host = _collapse(_pocket_pulse(auto) + _agent_items() + _manual_host_items(cfg))
    for it in host:
        if it.get("client") == "pocket":
            it["secs"] = None      # 一天的跨度不是工时，别在页面上写「17h 27m」
    # 进行中的永远顶在最上面：卡上第一行回答的必须是「现在在跑什么」，
    # 而不是「三分钟前跑完的那个」。第二键仍按结束/开始时间倒序。
    host.sort(key=lambda x: (x.get("status") == "run",
                             x.get("end") or x.get("start") or ""), reverse=True)
    # 导入/补记的账要占得住位置：它们的时间戳可能是几天前，纯按时间倒序会被今天的
    # 几十轮会话挤出 80 条之外 —— 用户点了「导入」却什么都看不见，等于没导。
    # 给它们留最多 24 格（补记和导入的抢这几个位置），剩下按原样截断。
    keep = [x for x in host if x.get("manual")][:24]
    if keep:
        ids = {id(x) for x in keep}
        rest = [x for x in host if id(x) not in ids][:_WL_HOST_LIMIT - len(keep)]
        host = keep + rest
        host.sort(key=lambda x: (x.get("status") == "run",
                                 x.get("end") or x.get("start") or ""), reverse=True)
    else:
        host = host[:_WL_HOST_LIMIT]       # 先截断再数，否则筛选条的数字比列表里多

    # 右卡的筛选条由这份清单生成：客户端名 + 条数。空名＝补记时没标客户端，
    # 页面上显示成「未标来源」，不硬塞给某个客户端。
    tally: dict[str, int] = {}
    for it in host:
        c = str(it.get("client") or "")
        tally[c] = tally.get(c, 0) + 1
    clients = [{"name": k, "n": v}
               for k, v in sorted(tally.items(), key=lambda kv: -kv[1])]

    return {
        "items": items[:_WL_LIMIT],
        "host": host,
        "clients": clients,
        "plugins": plugins,
        "total": len(items),
        "runs": sum(1 for x in items if x["status"] == "run"),
        "ok": sum(1 for x in items if x["status"] == "ok"),
        "fail": sum(1 for x in items if x["status"] == "fail"),
        "skip": sum(1 for x in items if x["status"] == "skip"),
        "ai": sum(1 for x in items if any(s.get("kind") == "ai" for s in x["sources"])),
        "at": datetime.now().strftime("%Y-%m-%d %H:%M:%S"),
    }


# --------------------------------------------------------------------------
# 应用入口：顶栏按钮 + 「我的应用」卡
#
#   内置五个（口袋/看图/阅读/影音/巡检）是写死的：它们知道自己的端口和双击
#   脚本，点一下就能「没开就先拉起，开好了再开页面」。
#   自定义入口是用户黏贴进来的东西（应用名、.app 路径、bundle id、网址），
#   落盘在 <数据根>/_工作台/launchers.json，不走 notes.json —— 那份是白名单，
#   塞不进来；而且入口是「配置」不是「记录」，单独一个文件更好备份和手改。
#
#   安全边界：服务端只做三件事 —— 校验目标存在、用 open 打开、返回结果。
#   不接受任意 shell 命令，也不把用户输入拼进命令行（参数一律走 list 形式）。
# --------------------------------------------------------------------------

# 口袋内置的插件（新插件别往这儿加 —— 写 <数据根>/_工作台/plugins.json 就行，
# 那份改完刷新页面就生效，不用重启进程）。这里留着的是「出厂就有的那几颗」：
# 顶栏药丸、＋插件弹窗的「插件清单」、服务清单卡三处都从这份合并清单生成。
#   scope="service"  自己的进程 + 端口（没开就先拉 .command，等端口通）
#   scope="view"     没有独立进程，只是口袋自己的一个页面（url 指过去）
#   cmd              双击启动脚本（相对数据根），只有 service 才有
#   svc_port         服务清单探的端口（pocket 自己跟自己同进程，故单独给）
BUILTIN_APPS = [
    {"id": "pocket", "name": "口袋", "scope": "service", "port": None, "svc_port": 8321,
     "svc_name": "口袋（传输/抓取）", "self": True, "url": "/",
     "note": "抓取 / 传输 / 工作台数据源", "glyph": "pocket", "group": "plugin"},
    {"id": "gallery", "name": "看图", "scope": "service", "port": 8771,
     "url": "http://127.0.0.1:8771/",
     "note": "存图库", "cmd": ("04-存图", "_看图.command"), "glyph": "gallery", "group": "tool"},
    {"id": "reader", "name": "阅读", "scope": "service", "port": 8772,
     "url": "http://127.0.0.1:8772/",
     "note": "长文阅读 / 标注", "cmd": ("08-阅读", "_阅读.command"), "glyph": "reader", "group": "tool"},
    {"id": "media", "name": "影音", "scope": "service", "port": 8773, "svc_name": "媒体墙",
     "url": "http://127.0.0.1:8773/",
     "note": "媒体墙 / 播放", "cmd": ("02-媒体", "_媒体墙.command"), "glyph": "media", "group": "tool"},
    # 2026-11-21 收敛：巡检不再有自己的页面（8774 的 / 现在 302 回工作台 ?v=ops）。
    # url 指着工作台内部那一页；port 留着是为了服务方阵里还能看见 8774 活没活。
    # 写绝对路径 /dash?v=ops —— 相对地址喂给 window.open() 会在 about:blank 里解析失败。
    {"id": "inspect", "name": "巡检", "scope": "service", "port": 8774, "svc_name": "系统巡检",
     "url": "/dash?v=ops",
     "note": "系统巡检", "cmd": ("_巡检", "_系统巡检.command"), "glyph": "inspect", "group": "plugin"},
    # 本机工作：2026-11-20 起没有独立端口了（采集线程并进口袋），这一行只剩
    # 「顶栏一颗药丸」的作用 —— port 留空就不探活、不标离线，点一下由 PAGE_OF
    # 送到 ?v=host 那一页（原来点它会新开 8775 的标签页）。
    {"id": "hostwork", "name": "本机工作", "scope": "view", "port": None,
     "url": "/dash?v=host", "note": "进程 / 磁盘内存 / token 折算",
     "glyph": "hostwork", "group": "plugin"},
    # Token 价格追踪：API / 套餐 / 本地部署 三维度成本 + 多源交叉校验。数据在 13-算价/。
    {"id": "llmcost", "name": "Token 价格追踪", "scope": "service", "port": 8776,
     "svc_name": "Token 价格追踪", "url": "http://127.0.0.1:8776/",
     "note": "模型 API / 套餐 / 本地部署成本", "cmd": ("13-算价", "_算价.command"),
     "glyph": "llmcost", "group": "plugin"},
    {"id": "follow", "name": "关注", "scope": "service", "port": 8780, "svc_name": "关注追踪",
     "url": "http://127.0.0.1:8780/",
     "note": "微博/B站/小红书/抖音更新", "cmd": ("11-关注追踪", "_关注追踪.command"),
     "glyph": "follow", "group": "plugin"},
    # 界面评审（2026-11-22）：以前每轮体检都是我手搓一个静态 HTML（ui-review/*.html），
    # 条目、截图、做完没做完全写死在页面里。改成插件后数据落在 18-界面评审/评审.json，
    # 页面按状态折叠 —— 做完的收起来，一打开只剩「还没定的」。
    {"id": "review", "name": "界面评审", "scope": "service", "port": 8785,
     "svc_name": "界面评审", "url": "http://127.0.0.1:8785/",
     "note": "UI 体检 / 待决定 / 已落地", "cmd": ("18-界面评审", "_界面评审.command"),
     "glyph": "review", "group": "tool"},
    # 手机接入（2026-11-21 改）：它**不是插件**，是「智能设备」那个插件里的一张功能卡
    # （8778 的 data-card="pair"，见 pgkit.CARDS["devices"]）。所以这里不挂号丸 ——
    # 顶栏只放真正的插件，功能卡去设备页看，或在工作台「＋ 插件卡片」把它钉到主页。
    # 口袋自己的 /pair 页留着，当「把链接发到微信」的那双手（从卡片里的链接进）。
    # 记账没有独立进程（后端在 pocket，界面是工作台里那张卡），所以**不写在这里** ——
    # 页面上那张卡自己带 data-tool="记账"，服务端扫出来就是一个插件。见 _auto_view_apps()。
]

# 工作台里「只是一张卡」的工具，靠卡片自己声明（2026-11-18 加）：
#     <section class="card" data-card="ledger" data-tool="记账"
#              data-tool-glyph="ledger" data-tool-note="截图记账 / 卖出盈亏">
# 有了这三个属性，顶栏药丸、＋入口弹窗、深链 #card=ledger 自动就有，
# 不用再到 BUILTIN_APPS 里手抄一行 —— 抄漏了（记账就是这么漏的）页面上看不出来。
_TAG_RE = re.compile(r"<[a-zA-Z][^>]*\bdata-card=\"([A-Za-z0-9_-]+)\"[^>]*>")
_ATTR_RE = re.compile(r'data-tool(-glyph|-note)?="([^"]*)"')


def _auto_view_apps(cfg: dict) -> list[dict]:
    """扫页面，把 data-tool="名字" 的卡片变成入口（读不到页面就当没有）。"""
    try:
        html = page_path(cfg).read_text(encoding="utf-8")
    except OSError:
        return []
    out: list[dict] = []
    for m in _TAG_RE.finditer(html):
        cid = m.group(1)
        attrs = {k or "": v for k, v in _ATTR_RE.findall(m.group(0))}
        name = (attrs.get("") or "").strip()
        if not name:
            continue
        out.append({
            "id": cid, "name": name, "kind": "builtin",
            "glyph": (attrs.get("-glyph") or "pocket").strip(),
            "note": (attrs.get("-note") or "").strip(),
            # 卡片型工具没有独立进程，落点是「内容页」（?v=more）并滚到这张卡 ——
            # 2026-11-19 起纯内容卡片合到那一页，首页只留摘要。
            "url": "/dash?v=more#card=" + cid, "port": None, "scope": "view", "up": True,
            "group": "tool",          # 卡片型的就是「小工具」：打开来用，不是运维面板
        })
    return out


# ---------------------------------------------------------------- 插件清单（2026-11-19）
# 顶栏那一排（口袋 / 看图 / 阅读 / 影音 / 巡检 / 本机工作 / 记账 / Token 价格追踪 …）统一叫「插件」。
# 加一个插件有两条路，**都不用改这个 .py**：
#
#   甲、它只是工作台里的一张卡 → 在卡片上写 data-tool（见上），刷新页面就有；
#   乙、它有自己的端口 / 启动脚本 → 往 <数据根>/_工作台/plugins.json 加一段：
#       {"id":"backup","name":"备份","port":8781,
#        "cmd":"14-备份/_备份.command","glyph":"plugin","note":"一句话说明"}
#
# 为什么单独立一份 json：以前加服务型插件只能改 BUILTIN_APPS，改完得
# kickstart 整个 pocket —— 加一个插件搞得像改代码。这份文件每次请求都读，
# 写完刷新页面就生效；同 id 会覆盖内置那一行（改端口/换启动脚本不用先删）。
#
# 字段：id(唯一,小写) / name / url / port / cmd(目录/脚本，或 [目录,脚本]) /
#       glyph(前端图标名，认不出就用通用图标) / note / svc_name(服务清单里的名字)
PLUGINS_FILE = "plugins.json"
_PLUGIN_ID = re.compile(r"^[a-z][a-z0-9_-]{0,31}$")
_PLUGIN_LOCK = threading.Lock()
_PLUGIN_TEMPLATE = ("加插件就往下写一条：{\"id\":\"backup\",\"name\":\"备份\",\"port\":8781,"
                    "\"cmd\":\"14-备份/_备份.command\",\"note\":\"一句话说明\",\"group\":\"plugin\"}。"
                    "group 决定弹窗里归哪一档（tool=小工具 / plugin=内置插件，不写按有没有端口猜）；"
                    "还没做的加 \"soon\":true 先占位。写完刷新页面即可，不用重启口袋；"
                    "id 跟内置重名会覆盖内置那一行。")


def plugins_path(cfg: dict) -> Path:
    return _root(cfg) / "_工作台" / PLUGINS_FILE


def _slug(text: str) -> str:
    """没写 id 时按名字兜一个（中文名会兜成空，那就 p+短哈希）。"""
    s = re.sub(r"[^a-z0-9]+", "-", str(text or "").lower()).strip("-")
    return s[:24]


def _norm_plugin(raw: dict, idx: int = 0) -> dict | None:
    """一行 json → 一份插件规格。缺名字或落点的整行丢掉，不让一条坏行拖垮全部。

    两个可选字段（2026-11-20 加）：

      group  「小工具」(tool) / 「内置插件」(plugin)。只决定弹窗里归哪一档，
             不影响顶栏顺序，写错顶多归错档、不会丢插件。不写就按有没有端口猜。
      soon   占位。还没做的东西先占一格：允许不填 url / 端口，点它是「还没做」，
             而不是弹一个空白页或报「没这个入口」。

    第三方插件（2026-11-22 加）：
      ext    第三方程序的配方 id（见 extplugins.py）。有这一条就走 ext 通道 ——
             装没装、在不在跑由配方去探，点药丸是「启动它」而不是「等端口」。
             这类东西不一定有网页地址（LocalSend 就没有），所以 url 允许为空。
    """
    if not isinstance(raw, dict):
        return None
    name = str(raw.get("name") or "").strip()[:24]
    if not name:
        return None
    soon = bool(raw.get("soon"))
    ext = str(raw.get("ext") or "").strip()
    port = raw.get("port")
    try:
        port = int(port) if port not in (None, "", 0, "0") else None
    except (TypeError, ValueError):
        port = None
    url = str(raw.get("url") or "").strip()
    if not url and port and not ext:
        url = f"http://127.0.0.1:{port}/"
    if not url and not soon and not ext:   # 占位 / 第三方程序可以没有网页地址
        return None
    pid = str(raw.get("id") or "").strip().lower()
    if not _PLUGIN_ID.match(pid):
        # 中文名 slug 出来是空的：退到端口（p8899），手翻 plugins.json 时至少对得上。
        pid = _slug(raw.get("id") or name) or (f"p{port}" if port else "") or ("p" + uuid.uuid4().hex[:6])
        if not _PLUGIN_ID.match(pid):
            pid = "p" + uuid.uuid4().hex[:6]
    cmd = raw.get("cmd")
    cmd_t: tuple[str, str] | None = None
    if isinstance(cmd, (list, tuple)) and len(cmd) == 2 and all(str(x).strip() for x in cmd):
        cmd_t = (str(cmd[0]).strip(), str(cmd[1]).strip())
    elif isinstance(cmd, str) and "/" in cmd:
        d, f = cmd.strip().rsplit("/", 1)
        if d.strip() and f.strip():
            cmd_t = (d.strip(), f.strip())
    group = str(raw.get("group") or "").strip().lower()
    if group not in ("tool", "plugin"):
        group = "plugin" if (port or cmd_t) else "tool"
    return {
        "id": pid, "name": name,
        "glyph": str(raw.get("glyph") or "plugin").strip() or "plugin",
        "note": str(raw.get("note") or "").strip()[:60],
        "url": url, "port": port,
        # host：这个插件的服务不在本机（被控机的 portproxy 端口）。
        # 探活要探它，点开也直接连它 —— 少了这个字段，远程插件会被当成
        # 「本机端口没开」，于是永远显示离线，还去本机找启动脚本。
        "host": str(raw.get("host") or "").strip()[:64],
        "scope": "ext" if ext else ("soon" if soon else ("service" if (port or cmd_t) else "view")),
        "cmd": cmd_t, "svc_name": str(raw.get("svc_name") or name).strip(),
        "source": "plugin", "group": group, "soon": soon, "ext": ext,
    }


def read_plugins(cfg: dict) -> list[dict]:
    """读用户插件清单。文件不在/坏了都当空 —— 顶栏塌一半比少两个插件难查。"""
    d = _load(plugins_path(cfg), {}) or {}
    rows = d.get("plugins") if isinstance(d, dict) else d
    if not isinstance(rows, list):
        return []
    out, seen = [], set()
    for i, raw in enumerate(rows):
        p = _norm_plugin(raw, i)
        if p and p["id"] not in seen:
            seen.add(p["id"])
            out.append(p)
    return out


def write_plugins(cfg: dict, rows: list[dict]) -> None:
    """原子写。只落用户该看的字段，手改的时候一行看得清。"""
    p = plugins_path(cfg)
    p.parent.mkdir(parents=True, exist_ok=True)
    items = []
    for r in rows:
        it = {"id": r["id"], "name": r["name"], "url": r.get("url") or ""}
        if r.get("port"):
            it["port"] = int(r["port"])
        # 服务在别的机器上时要把地址一起存下来，否则下次写回插件清单就丢了，
        # 那个插件会变成「本机端口没开」的黑洞。
        if r.get("host"):
            it["host"] = r["host"]
        if r.get("cmd"):
            it["cmd"] = "/".join(r["cmd"])
        if r.get("glyph") and r["glyph"] != "plugin":
            it["glyph"] = r["glyph"]
        if r.get("note"):
            it["note"] = r["note"]
        if r.get("svc_name") and r["svc_name"] != r["name"]:
            it["svc_name"] = r["svc_name"]
        if r.get("group") in ("tool", "plugin"):
            it["group"] = r["group"]
        if r.get("soon"):
            it["soon"] = True
        if r.get("ext"):
            it["ext"] = r["ext"]
        items.append(it)
    payload = {"_说明": _PLUGIN_TEMPLATE, "plugins": items,
               "updated_at": datetime.now().isoformat(timespec="seconds")}
    tmp = p.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(payload, ensure_ascii=False, indent=1), encoding="utf-8")
    tmp.replace(p)


_BUILTIN_IDS = {a["id"] for a in BUILTIN_APPS}


def _all_specs(cfg: dict) -> list[dict]:
    """顶栏上的全部插件：plugins.json > BUILTIN_APPS > 卡片自报的 data-tool。

    同 id 时 plugins.json 覆盖内置那一行（只覆盖它写了的字段），这样「改个端口 /
    换个启动脚本」不用先删内置再抄一遍。卡片自报的排在最后（它们是纯卡片，
    没有进程，顺序不影响探活）。
    """
    specs = [dict(a) for a in BUILTIN_APPS]
    for p in read_plugins(cfg):
        for i, b in enumerate(specs):
            if b["id"] == p["id"]:
                specs[i] = {**b, **{k: v for k, v in p.items() if v not in (None, "")}}
                break
        else:
            specs.append(p)
    have = {a["id"] for a in specs}
    for a in _auto_view_apps(cfg):
        if a["id"] not in have:
            specs.append(a)
            have.add(a["id"])
    # 顶栏药丸、＋插件弹窗、服务方阵都从这一份出去，所以过滤放在这里一次做完，
    # 免得三处各写一遍、漏一处就在 Windows 上又冒出点不开的入口。
    return [a for a in specs if _posix_ok(a["id"])]


def _builtin_spec(cfg: dict, aid: str) -> dict | None:
    """插件 id → 规格。三份来源（清单/内置/卡片）在 _all_specs 里已经合过。"""
    for a in _all_specs(cfg):
        if a["id"] == aid:
            return a
    return None

# 按需服务（2026-11-19 七改）：只在「打开它的页面」时该活着，页面一关自己退
# （机制在 cuntu/lease.py：页面每 15 秒续租，关标签 bye，静默几秒就 exit 0）。
#
# 判据是「除了看页面，它还干别的吗」：
#   · 看图 / 阅读 / 媒体墙 / 巡检 / 价格页 → 纯看，关掉就该退
#   · 本机工作（原 8775）→ 已并进口袋进程，不在这个名单里了
#   · 关注追踪 8780 → 采集循环就在它进程里，退了就不采了，留着
#   · 口袋 / gateway / Ollama / Qdrant → 常驻
# 巡检里这些按需服务离线**不是故障**，标「按需」而不是报红。
# 巡检 2026-11-20 起常驻（launchd + ALWAYS=1，页面关了也不退），不再是按需服务 ——
# 留在集合里会让主页把它显示成「按需待起」，巡检真掉线时也就没人当回事。
# 游戏记录 8779（2026-11-21 补）：cuntu/syscheck.py 里一直是 lazy=True，这里漏了，
# 于是它没开就被工作台算成「掉线」，顶栏还为此亮红点。用户原话：「按需的没开没事」。
LAZY_SERVICES = {"gallery", "reader", "media", "llmcost", "ghrepos", "devices", "games", "review"}

# 不是「口袋自带的工具」，但服务清单要看着它们：别的程序/容器。
INFRA_SERVICES = [
    ("gateway", "微信助手 gateway", 18789),
    ("ollama", "本机模型 Ollama", 11434),
    ("qdrant", "向量库 Qdrant", 6333),
]

_APP_LOCK = threading.Lock()
_APP_MAX = 30
_APP_DIRS = ["/Applications", "/System/Applications", "/Applications/Utilities",
             "/System/Applications/Utilities", str(Path.home() / "Applications")]
_URL_LIKE = re.compile(r"^(https?|ftp)://", re.I)
_SCHEME_LIKE = re.compile(r"^[a-z][a-z0-9+.-]*://", re.I)
_BUNDLE_ID = re.compile(r"^[A-Za-z0-9-]+(\.[A-Za-z0-9-]+){2,}$")


def launchers_path(cfg: dict) -> Path:
    return _root(cfg) / "_工作台" / "launchers.json"


def icon_dir(cfg: dict) -> Path:
    return _root(cfg) / "_工作台" / "_图标"


def read_launchers(cfg: dict) -> list[dict]:
    d = _load(launchers_path(cfg), {}) or {}
    items = d.get("items") if isinstance(d.get("items"), list) else []
    out = []
    for it in items:
        if not isinstance(it, dict) or not it.get("id"):
            continue
        out.append({
            "id": str(it.get("id")),
            "name": str(it.get("name") or "入口"),
            "kind": it.get("kind") or "app",
            "target": str(it.get("target") or ""),
            "url": str(it.get("url") or ""),
            "hint": str(it.get("hint") or ""),
            "icon": str(it.get("icon") or ""),
            "card": it.get("card") is not False,
            "created": it.get("created") or "",
        })
    return out


def write_launchers(cfg: dict, items: list[dict]) -> None:
    p = launchers_path(cfg)
    p.parent.mkdir(parents=True, exist_ok=True)
    payload = {"items": items, "updated_at": datetime.now().isoformat(timespec="seconds")}
    tmp = p.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(payload, ensure_ascii=False, indent=1), encoding="utf-8")
    tmp.replace(p)


def _app_exists(path: str) -> bool:
    try:
        return Path(path).exists()
    except OSError:
        return False


def _find_app(query: str) -> Path | None:
    """把「粘贴进来的东西」找成 .app：完整路径 → 各 Applications 目录 → 模糊。"""
    q = (query or "").strip().rstrip("/")
    if not q:
        return None
    p = Path(q).expanduser()
    if q.lower().endswith(".app") and p.is_dir():
        return p
    name = p.name[:-4] if p.name.lower().endswith(".app") else p.name
    for d in _APP_DIRS:
        base = Path(d)
        if not base.is_dir():
            continue
        for cand in (base / (name + ".app"), base / name):
            if cand.is_dir():
                return cand
    ql = name.lower()
    hits: list[Path] = []
    for d in _APP_DIRS:
        base = Path(d)
        if not base.is_dir():
            continue
        try:
            kids = list(base.glob("*.app"))
        except OSError:
            continue
        for c in kids:
            nl = c.stem.lower()
            if nl == ql:
                return c
            if ql in nl:
                hits.append(c)
    if hits:
        return sorted(hits, key=lambda x: len(x.stem))[0]
    # 有些 app 装在子文件夹里（/Applications/开发/xxx.app），只多扫一层
    for d in _APP_DIRS:
        base = Path(d)
        if not base.is_dir():
            continue
        try:
            for c in base.glob("*/*.app"):
                if ql in c.stem.lower():
                    return c
        except OSError:
            continue
    return None


def parse_launcher(text: str) -> dict:
    """把黏贴内容解析成 {kind, name, target, hint, error}。

    认得这几种写法，其它一律报错让人看清楚，而不是猜一个开出来：
      /Applications/微信.app · ~/Applications/Foo.app
      open -a "Safari" · open -b com.apple.Safari · open /Applications/IINA.app
      Safari · 微信（按名字在 Applications 里找）
      https://example.com · www.example.com
    """
    raw = (text or "").strip()
    if not raw:
        return {"error": "先粘贴点东西进来"}
    if len(raw) > 400:
        return {"error": "太长了，粘个路径或名字就行"}

    s = raw
    # 允许整行是「open ...」命令：把它们拆成参数，而不是拒绝
    m = re.match(r"^open\s+((-a|-b)\s+)?(.*)$", s, re.I | re.S)
    if m:
        flag, rest = (m.group(2) or "").lower(), (m.group(3) or "").strip()
        rest = rest.strip().strip('"').strip("'").strip()
        if flag == "-b" and rest:
            return {"kind": "bundle", "name": rest.split(".")[-1].title(),
                    "target": rest, "hint": rest}
        s = rest
    s = s.strip().strip('"').strip("'").strip()
    if not s:
        return {"error": "只粘了个 open，后面还差应用名"}

    if _URL_LIKE.match(s):
        host = re.sub(r"^[a-z]+://", "", s, flags=re.I).split("/")[0]
        return {"kind": "url", "name": host or s, "target": s, "url": s, "hint": s}
    if s.lower().startswith("www."):
        return {"kind": "url", "name": s.split("/")[0], "target": "https://" + s,
                "url": "https://" + s, "hint": s}
    if _SCHEME_LIKE.match(s):
        return {"error": "这个链接不是 http(s)，浏览器打不开；换成网址或应用名吧"}

    app = _find_app(s)
    if app is not None:
        return {"kind": "app", "name": app.stem, "target": str(app), "hint": s}

    p = Path(s).expanduser()
    if p.exists():
        if p.is_dir():
            return {"kind": "path", "name": p.name, "target": str(p), "hint": s}
        return {"kind": "path", "name": p.name, "target": str(p), "hint": s}

    if _BUNDLE_ID.match(s):
        return {"kind": "bundle", "name": s.split(".")[-1].title(),
                "target": s, "hint": s}

    # 最后一道：让系统自己认这个名字（本地化名、别名都行；文件扫描认不出中文名）
    try:
        r = subprocess.run(["/usr/bin/open", "-Ra", s], capture_output=True, text=True, timeout=6)
        if r.returncode == 0:
            return {"kind": "appname", "name": s, "target": s, "hint": s}
    except (OSError, subprocess.SubprocessError):
        pass

    return {"error": f"没找到「{s}」这个应用。可以粘贴 /Applications 里的 .app 全路径，"
                     f"或者直接写应用名（如 Safari、Music）"}


def _extract_icon(app_path: Path, dest: Path) -> bool:
    """从 .app 里抠出 icns 转 128px png。单个入口只做一次，失败就算了。"""
    res = app_path / "Contents" / "Resources"
    plist = app_path / "Contents" / "Info.plist"
    names: list[str] = []
    try:
        r = subprocess.run(["/usr/bin/plutil", "-extract", "CFBundleIconFile", "raw", str(plist)],
                           capture_output=True, text=True, timeout=6)
        v = (r.stdout or "").strip()
        if v:
            names.append(v)
    except (OSError, subprocess.SubprocessError):
        pass
    names.append("AppIcon")
    dest.parent.mkdir(parents=True, exist_ok=True)
    for n in names:
        cands = [res / (n if n.lower().endswith(".icns") else n + ".icns"), res / n]
        for f in cands:
            if not f.is_file():
                continue
            try:
                subprocess.run(["/usr/bin/sips", "-Z", "128", "-s", "format", "png",
                                str(f), "--out", str(dest)],
                               capture_output=True, timeout=12)
            except (OSError, subprocess.SubprocessError):
                continue
            try:
                if dest.is_file() and dest.stat().st_size > 200:
                    return True
            except OSError:
                continue
    return False


def apps_payload(cfg: dict) -> dict:
    builtin = []
    for a in _all_specs(cfg):
        src = a.get("source") or ("builtin" if a["id"] in _BUILTIN_IDS else "card")
        ext_id = str(a.get("ext") or "")
        rec = extplugins.recipe(cfg, ext_id) if ext_id else None
        if a.get("soon"):
            up = False
        elif rec:                       # 第三方程序：跑没跑要看进程/端口，不是看我们自己的端口
            up = extplugins.running(rec)
        elif not a.get("port"):
            up = True
        else:
            # 插件可以跑在别的机器上（host 写了局域网地址）：探那台，不探本机
            up = _port(a["port"], str(a.get("host") or "127.0.0.1"))
        builtin.append({
            "id": a["id"], "name": a["name"], "kind": "builtin", "glyph": a["glyph"],
            "note": a["note"], "url": a["url"], "port": a["port"],
            # 服务在别的机器上（被控机映射出来的端口）：前端要据此显示「EVO 上的」
            "host": a.get("host") or "",
            "scope": a.get("scope") or "service",
            # 弹窗里分两档用：小工具 / 内置插件。占位的（soon）恒当没在线，别假装能用。
            "group": a.get("group") or "plugin",
            "soon": bool(a.get("soon")),
            # ext：第三方程序（LocalSend 这种）。前端据此点药丸＝启动它，不是等端口。
            "ext": ext_id, "installed": bool(rec and extplugins.installed(rec)),
            # source：plugin=用户清单里的（可以在这里删）/ builtin=内置 / card=卡片自报
            "source": src, "editable": src == "plugin",
            "up": up,
        })
    custom = []
    for it in read_launchers(cfg):
        alive = it["kind"] in ("url", "bundle", "appname") or _app_exists(it["target"])
        custom.append({**it, "alive": alive})
    return {"builtin": builtin, "custom": custom}


def _spawn(cmd: list[str], log_name: str) -> None:
    """后台起进程：脱离当前进程组，输出进 ~/Library/Logs。"""
    logdir = Path.home() / "Library" / "Logs"
    try:
        logdir.mkdir(parents=True, exist_ok=True)
    except OSError:
        logdir = Path("/tmp")
    out = open(logdir / log_name, "a", encoding="utf-8")
    # CUNTU_NO_OPEN：工作台自己会开标签页（wlGo 先开空页再跳），启动脚本别再弹一个 ——
    # 两个标签页的话，用户关掉看见的那个，另一个还在续租，进程就一直留着。
    env = dict(os.environ, CUNTU_NO_OPEN="1")
    subprocess.Popen(cmd, stdin=subprocess.DEVNULL, stdout=out, stderr=subprocess.STDOUT,
                     start_new_session=True, close_fds=True, env=env)


def launch_app(cfg: dict, aid: str, builtin: bool = False) -> dict:
    """点入口：内置的没开就先拉起（等端口通），自定义的直接 open。"""
    if builtin:
        spec = _builtin_spec(cfg, aid)
        if not spec:
            return {"ok": False, "error": "没有这个入口"}
        if spec.get("soon"):
            return {"ok": False, "error": f"{spec['name']} 还是占位，还没做"}
        # 第三方程序：不等我们自己的端口，先看装没装、跑没跑，再由配方去启动
        if spec.get("ext"):
            r = extplugins.recipe(cfg, spec["ext"])
            if not r:
                return {"ok": False, "error": "这条第三方配方已经不在了"}
            if not extplugins.installed(r):
                return {"ok": False,
                        "error": f"{spec['name']} 还没装：加插件 → 第三方插件 → 装它"}
            if extplugins.running(r):
                return {"ok": True, "up": True, "started": False, "url": spec.get("url") or ""}
            got = extplugins.start(cfg, spec["ext"])
            got["url"] = spec.get("url") or ""
            return got
        if not spec["port"]:
            return {"ok": True, "up": True, "url": spec["url"], "started": False}
        remote = str(spec.get("host") or "").strip()
        if _port(spec["port"], remote or "127.0.0.1"):
            return {"ok": True, "up": True, "url": spec["url"], "started": False}
        # 服务在别人机器上：这台 Mac 上没有它的启动脚本，也不该去拉。
        # 说清楚「那边没开」，比报「没配启动脚本」有用。
        if remote:
            return {"ok": False, "up": False,
                    "error": f"{spec['name']}：{remote}:{spec['port']} 连不上 —— "
                             f"那台机器上的服务没开，或者端口映射（portproxy）没生效"}
        # 插件可以只填端口不填启动脚本（脚本在别处被人起着）：这时别乱拉，
        # 直接说清楚，比抛 KeyError 好查。
        if not spec.get("cmd"):
            return {"ok": False, "error": f"{spec['name']} 没配启动脚本，"
                                          f"先手动把它起来（端口 {spec['port']}）"}
        cmd_p = _root(cfg) / spec["cmd"][0] / spec["cmd"][1]
        if not cmd_p.exists():
            return {"ok": False, "error": f"启动脚本不在：{cmd_p}"}
        try:
            _spawn(["/bin/bash", str(cmd_p)], f"cuntu-{aid}.log")
        except OSError as e:
            return {"ok": False, "error": str(e)[:200]}
        patience = _LAUNCH_PATIENCE.get(aid, 14)
        t0 = time.time()
        while time.time() - t0 < patience:
            if _port(spec["port"], timeout=0.25):
                return {"ok": True, "up": True, "url": spec["url"], "started": True,
                        "seconds": round(time.time() - t0, 1)}
            time.sleep(0.35)
        waited = int(time.time() - t0)
        # 慢启动的（耐心 >20 秒）：还在起就说在起，别喊「没起来」—— 原来那句
        # 「14 秒内没起来」对影音室是纯误报，它 90 秒才好。但要留一笔账。
        if patience > 20:
            base = _root(cfg) / "02-媒体"
            if not _media_boot_recent(base, 300):
                _media_boot_note(base, "launch_timeout",
                                 f"{spec['name']}：口袋等了 {waited}s 端口还没通（多半还在扫盘）",
                                 secs=waited)
            return {"ok": True, "up": False, "started": True, "pending": True,
                    "url": spec["url"], "seconds": waited}
        return {"ok": False, "up": False, "started": True,
                "error": f"{spec['name']} {int(patience)} 秒内没起来，看 ~/Library/Logs/cuntu-{aid}.log"}

    it = next((x for x in read_launchers(cfg) if x["id"] == aid), None)
    if not it:
        return {"ok": False, "error": "入口不存在，可能被删了"}
    if it["kind"] == "url":
        return {"ok": True, "url": it["target"] or it["url"], "started": False}
    target = it["target"]
    if it["kind"] in ("app", "path") and not _app_exists(target):
        return {"ok": False, "error": f"目标不在了：{target}"}
    cmd = ["/usr/bin/open"]
    if it["kind"] == "bundle":
        cmd += ["-b", target]
    elif it["kind"] in ("app", "appname"):
        cmd += ["-a", target]
    else:
        cmd += [target]
    try:
        _spawn(cmd, "dash-open.log")
    except OSError as e:
        return {"ok": False, "error": str(e)[:200]}
    return {"ok": True, "name": it["name"], "started": True}


def preview_launcher(body: dict) -> dict:
    """弹窗里边粘边认：只解析不落盘，让用户看清「识别成了什么」再决定加不加。"""
    parsed = parse_launcher(str(body.get("text") or ""))
    if parsed.get("error"):
        return {"ok": False, "error": parsed["error"]}
    return {"ok": True, **parsed}


def add_plugin(cfg: dict, body: dict) -> dict:
    """加一个插件：只写 plugins.json，不改代码、不重启（刷新页面就出现）。"""
    p = _norm_plugin({
        "id": body.get("id"), "name": body.get("name"), "glyph": body.get("glyph"),
        "port": body.get("port"), "url": body.get("url"), "cmd": body.get("cmd"),
        "note": body.get("note"), "svc_name": body.get("svc_name"),
        "group": body.get("group"), "ext": body.get("ext"),
    })
    if not p:
        return {"ok": False, "error": "名字必填，地址和端口至少给一个"}
    with _PLUGIN_LOCK:
        rows = read_plugins(cfg)
        dup = next((x for x in rows if x["id"] == p["id"]), None)
        if dup:
            return {"ok": False, "error": f"清单里已经有 id={p['id']} 的插件了（「{dup['name']}」）"}
        if p["id"] in _BUILTIN_IDS:
            # 撞内置不拦，等价于「改内置那一行」——但要说清楚，不然用户以为新加了一个。
            p["overrode"] = True
        rows.append(p)
        write_plugins(cfg, rows)
    return {"ok": True, "plugin": p}


def add_ext_plugin(cfg: dict, rid: str) -> dict:
    """把一款第三方程序加进中台：只写 plugins.json，装法/探活都交给配方。

    跟 add_plugin 的区别只在「谁来描述它」：这里不填端口地址，配方里已经有了。
    重复添加是幂等的（已经加过就回 already），前端那张表刷新一下不会冒出两个。
    """
    r = extplugins.recipe(cfg, rid)
    if not r:
        return {"ok": False, "error": "没有这条第三方配方"}
    row = extplugins.plugin_row(r)
    with _PLUGIN_LOCK:
        rows = read_plugins(cfg)
        if any(x["id"] == row["id"] for x in rows):
            p = next(x for x in rows if x["id"] == row["id"])
            return {"ok": True, "already": True, "plugin": p, "state": extplugins.probe(r)}
        p = _norm_plugin(row)
        if not p:
            return {"ok": False, "error": "配方不完整，加不进来"}
        rows.append(p)
        write_plugins(cfg, rows)
    return {"ok": True, "plugin": p, "state": extplugins.probe(r)}


def remove_plugin(cfg: dict, pid: str) -> dict:
    """从清单里删。内置的不在清单里，删不到 —— 返回说明，别静默成功。"""
    with _PLUGIN_LOCK:
        rows = read_plugins(cfg)
        keep = [x for x in rows if x["id"] != pid]
        if len(keep) == len(rows):
            return {"ok": False, "error": "清单里没有这个插件（内置插件改不了，只能覆盖）"}
        write_plugins(cfg, keep)
    return {"ok": True}


def add_launcher(cfg: dict, body: dict) -> dict:
    raw = str(body.get("text") or "")
    parsed = parse_launcher(raw)
    if parsed.get("error"):
        return {"ok": False, "error": parsed["error"]}
    items = read_launchers(cfg)
    if len(items) >= _APP_MAX:
        return {"ok": False, "error": f"入口最多 {_APP_MAX} 个，先删几个"}
    name = (str(body.get("name") or "").strip() or parsed["name"])[:40]
    lid = "l" + uuid.uuid4().hex[:10]
    icon = ""
    if parsed["kind"] in ("app", "path"):
        dest = icon_dir(cfg) / f"{lid}.png"
        src = Path(parsed["target"])
        app = src if src.suffix == ".app" else None
        if app is None and src.is_dir():
            subs = list(src.glob("*.app"))[:1]
            app = subs[0] if subs else None
        if app and _extract_icon(app, dest):
            icon = dest.name
    item = {
        "id": lid, "name": name, "kind": parsed["kind"], "target": parsed["target"],
        "url": parsed.get("url") or "", "hint": raw.strip()[:200], "icon": icon,
        "card": bool(body.get("card", True)),
        "created": datetime.now().isoformat(timespec="seconds"),
    }
    with _APP_LOCK:
        items.append(item)
        write_launchers(cfg, items)
    return {"ok": True, "item": {**item, "alive": True}}


def update_launcher(cfg: dict, body: dict) -> dict:
    lid = str(body.get("id") or "")
    with _APP_LOCK:
        items = read_launchers(cfg)
        it = next((x for x in items if x["id"] == lid), None)
        if not it:
            return {"ok": False, "error": "没有这个入口"}
        if "card" in body:
            it["card"] = bool(body.get("card"))
        if str(body.get("name") or "").strip():
            it["name"] = str(body["name"]).strip()[:40]
        write_launchers(cfg, items)
    return {"ok": True, "item": it}


def remove_launcher(cfg: dict, lid: str) -> dict:
    with _APP_LOCK:
        items = read_launchers(cfg)
        keep = [x for x in items if x["id"] != lid]
        if len(keep) == len(items):
            return {"ok": False, "error": "没有这个入口"}
        write_launchers(cfg, keep)
    p = icon_dir(cfg) / f"{lid}.png"
    try:
        if p.is_file():
            p.unlink()
    except OSError:
        pass
    return {"ok": True}


def launcher_icon(cfg: dict, lid: str) -> Path | None:
    p = icon_dir(cfg) / f"{re.sub(r'[^A-Za-z0-9_-]', '', lid)}.png"
    return p if p.is_file() else None


# --------------------------------------------------------------------------
# 个人数据：notes.json 是唯一真源，markdown 是给人看的镜像（2026-11-18）
#
#   为什么两份：json 给页面读写（结构化、整份覆盖、原子落盘）；markdown 给人
#   看 —— 扔进 Obsidian / 备忘录 / git 都能读，手机上翻也不用开工作台。
#   markdown 一律「整份重建」：数据就几十条，比维护增量 diff 可靠得多。
#   内容没变就不落笔（mtime 不动），否则页面每次自动保存都把全部 md 的修改
#   时间刷一遍，同步工具会当成「全都改了」。
#
#   目录（都在 <数据根>/09-个人工作台/）：
#     notes.json          真源
#     待办.md              未完成 + 已完成
#     行程/YYYY-MM-DD.md   一天一文件，跨天的写在出发那天
#     日记/YYYY-MM-DD.md   一天一文件
#     备忘/YYYY-MM.md      一月一文件
# --------------------------------------------------------------------------

_MD_LOCK = threading.Lock()
_MIGRATED: set[str] = set()
_WD = "一二三四五六日"
_DAY_RE = re.compile(r"^\d{4}-\d{2}-\d{2}$")
_MON_RE = re.compile(r"^\d{4}-\d{2}$")
# 清理时匹配的是文件名，带 .md —— 跟校验日期字段的正则不是一回事
_MD_DAY_RE = re.compile(r"^\d{4}-\d{2}-\d{2}\.md$")
_MD_MON_RE = re.compile(r"^\d{4}-\d{2}\.md$")
_MD_HEAD = "<!-- 工作台自动生成，改这里会被覆盖；真源是同目录的 notes.json -->"


def _ensure_notes(cfg: dict) -> None:
    """旧位置 _工作台/notes.json 搬一次；搬完把旧文件改名留底，不删。"""
    new = notes_path(cfg)
    key = str(new)
    if key in _MIGRATED or new.exists():
        _MIGRATED.add(key)
        return
    _MIGRATED.add(key)
    old = _legacy_notes_path(cfg)
    if not old.is_file():
        return
    try:
        new.parent.mkdir(parents=True, exist_ok=True)
        tmp = new.with_suffix(".json.tmp")
        tmp.write_bytes(old.read_bytes())
        tmp.replace(new)
        old.replace(old.with_name(f"notes.json.migrated-{date.today():%Y%m%d}"))
    except OSError:
        _MIGRATED.discard(key)


def _s(v) -> str:
    return str(v).strip() if isinstance(v, (str, int, float)) else ""


def _md_write(p: Path, text: str) -> bool:
    """内容一样就不落笔，保住 mtime。"""
    try:
        if p.is_file() and p.read_text(encoding="utf-8") == text:
            return False
        p.parent.mkdir(parents=True, exist_ok=True)
        tmp = p.with_suffix(".md.tmp")
        tmp.write_text(text, encoding="utf-8")
        tmp.replace(p)
        return True
    except OSError:
        return False


def _md_prune(d: Path, keep: set[str], pat: re.Pattern) -> int:
    """删掉不该再存在的镜像。只动认得出的日期文件名，用户自己放的文件不碰。"""
    n = 0
    if not d.is_dir():
        return 0
    for p in d.iterdir():
        if p.is_file() and p.name not in keep and pat.match(p.name):
            try:
                p.unlink()
                n += 1
            except OSError:
                pass
    return n


def _md_doc(body: str) -> str:
    return body.rstrip() + "\n\n" + _MD_HEAD + "\n"


def _due_hint(due: str) -> str:
    m = re.match(r"(\d{4})-(\d{2})-(\d{2})", due or "")
    if not m:
        return ""
    try:
        d = date(int(m.group(1)), int(m.group(2)), int(m.group(3)))
    except ValueError:
        return ""
    delta = (d - date.today()).days
    if delta == 0:
        return "今天"
    if delta == 1:
        return "明天"
    if delta == -1:
        return "昨天"
    if delta < 0:
        return f"逾期 {-delta} 天"
    if delta <= 7:
        return f"{delta} 天后"
    return ""


def _money(v) -> str:
    s = re.sub(r"^[¥￥]\s*", "", _s(v))
    return f"¥{s}" if s else ""


def _md_todos(todos: list) -> str:
    open_, done = [], []
    for t in todos:
        if not isinstance(t, dict):
            continue
        text = _s(t.get("text"))
        if not text:
            continue
        due = _s(t.get("due"))[:10]
        hint = _due_hint(due)
        meta = f" — {due}（{hint}）" if due and hint else (f" — {due}" if due else "")
        line = f"- [{'x' if t.get('done') else ' '}] {text}{meta}"
        (done if t.get("done") else open_).append((due or "\uffff", line))
    open_.sort(key=lambda x: x[0])
    done.sort(key=lambda x: x[0])
    out = ["# 待办", "", f"未完成 {len(open_)} / 已完成 {len(done)}", "", "## 未完成", ""]
    out += [line for _, line in open_] or ["（没有待办）"]
    if done:
        out += ["", "## 已完成", ""] + [line for _, line in done]
    return _md_doc("\n".join(out))


def _ev_lines(e: dict) -> list[str]:
    day = _s(e.get("date"))[:10]
    t = _s(e.get("time"))
    title = _s(e.get("title"))
    frm, to = _s(e.get("from")), _s(e.get("to"))
    end_d, end_t = _s(e.get("endDate"))[:10], _s(e.get("endTime"))
    stops = [x.strip() for x in (e.get("stops") or []) if isinstance(x, str) and x.strip()]
    if not title:
        title = " → ".join(x for x in (frm, to) if x) or "未命名行程"
    out = [f"## {t} {title}".strip(), ""]
    if frm:
        out.append(f"- 出发：{frm}" + (f" {t}" if t else ""))
    if stops:
        out.append("- 途经：" + " → ".join(stops))
    if to:
        arrive = f"- 到达：{to}"
        if end_d and end_d != day:
            arrive += " · " + " ".join(x for x in (end_d[5:], end_t) if x)
        elif end_t:
            arrive += f" {end_t}"
        out.append(arrive)
    elif end_d and end_d != day:
        out.append("- 结束：" + " ".join(x for x in (end_d[5:], end_t) if x))
    if _s(e.get("flight")):
        out.append(f"- 航班：{_s(e.get('flight'))}")
    fee = _money(e.get("fee"))
    if fee:
        out.append(f"- 路费：{fee}")
    note = _s(e.get("delayNote"))
    try:
        dm = int(float(_s(e.get("delay")))) if _s(e.get("delay")) else 0
    except (TypeError, ValueError):
        dm = 0
    if dm > 0:
        out.append(f"- 延误：{dm} 分钟" + (f"（{note}）" if note else ""))
    elif note:
        out.append(f"- 备注：{note}")
    if e.get("done"):
        out.append("- 状态：已完成")
    out.append("")
    return out


def _md_trip(day: str, evs: list[dict]) -> str:
    try:
        wd = _WD[date.fromisoformat(day).weekday()]
    except ValueError:
        wd = ""
    out = [f"# 行程 · {day}" + (f" 周{wd}" if wd else ""), ""]
    for e in sorted(evs, key=lambda x: (_s(x.get("time")), _s(x.get("created")))):
        out += _ev_lines(e)
    return _md_doc("\n".join(out))


def _md_diary(day: str, text: str) -> str:
    try:
        wd = _WD[date.fromisoformat(day).weekday()]
    except ValueError:
        wd = ""
    head = f"# 日记 · {day}" + (f" 周{wd}" if wd else "")
    return _md_doc(f"{head}\n\n{_s(text)}")


def _md_memo(month: str, items: list[dict]) -> str:
    out = [f"# 备忘 · {month}", ""]
    for m in sorted(items, key=lambda x: (_s(x.get("at")), _s(x.get("id"))), reverse=True):
        at = _s(m.get("at"))
        stamp = at[5:10] if len(at) >= 10 else at
        text = _s(m.get("text")).replace("\n", "\n  ")
        out.append(f"- **{stamp}** {text}")
    return _md_doc("\n".join(out))


_README = """# 09-个人工作台

工作台页面里的待办 / 行程 / 日记 / 备忘，全收在这一个目录。

| 文件 | 是什么 |
| --- | --- |
| `notes.json` | 唯一真源。工作台读写的就是它，整份覆盖、原子落盘 |
| `待办.md` | 未完成 + 已完成的清单 |
| `行程/YYYY-MM-DD.md` | 一天一个文件；跨天的行程写在出发那天 |
| `日记/YYYY-MM-DD.md` | 一天一个文件，只有写过日记的日子才有 |
| `备忘/YYYY-MM.md` | 一个月一个文件，速记按日期倒序 |

`.md` 全是自动生成的镜像：工作台里改完会立刻重写对应文件。
直接手改 `.md` **不会**回流到 `notes.json`，下次保存就被覆盖；
要批量导入就改 `notes.json`，再 `python dash.py --export` 重铺一遍。
"""


def _ensure_readme(base: Path) -> None:
    p = base / "README.md"
    if p.exists():
        return
    try:
        base.mkdir(parents=True, exist_ok=True)
        p.write_text(_README, encoding="utf-8")
    except OSError:
        pass


def export_markdown(cfg: dict, data: dict | None = None) -> dict:
    """notes → markdown 镜像。整份重建（几十个小文件，代价可忽略）。"""
    d = data if isinstance(data, dict) else read_notes(cfg)
    base = personal_dir(cfg)
    written = pruned = 0

    if _md_write(base / "待办.md", _md_todos(d.get("todos") or [])):
        written += 1

    trips: dict[str, list] = {}
    for e in d.get("events") or []:
        if not isinstance(e, dict):
            continue
        day = _s(e.get("date"))[:10]
        if not _DAY_RE.match(day):
            day = _s(e.get("created"))[:10]
        if not _DAY_RE.match(day):
            continue
        trips.setdefault(day, []).append(e)
    for day, evs in trips.items():
        if _md_write(base / "行程" / f"{day}.md", _md_trip(day, evs)):
            written += 1
    pruned += _md_prune(base / "行程", {f"{k}.md" for k in trips}, _MD_DAY_RE)

    keep: set[str] = set()
    diary = d.get("diary") if isinstance(d.get("diary"), dict) else {}
    for day, text in diary.items():
        day = str(day)[:10]
        if not _DAY_RE.match(day) or not _s(text):
            continue
        keep.add(f"{day}.md")
        if _md_write(base / "日记" / f"{day}.md", _md_diary(day, _s(text))):
            written += 1
    pruned += _md_prune(base / "日记", keep, _MD_DAY_RE)

    months: dict[str, list] = {}
    for m in d.get("memo") or []:
        if not isinstance(m, dict) or not _s(m.get("text")):
            continue
        month = _s(m.get("at"))[:7]
        if not _MON_RE.match(month):
            month = datetime.now().strftime("%Y-%m")
        months.setdefault(month, []).append(m)
    for month, items in months.items():
        if _md_write(base / "备忘" / f"{month}.md", _md_memo(month, items)):
            written += 1
    pruned += _md_prune(base / "备忘", {f"{k}.md" for k in months}, _MD_MON_RE)

    _ensure_readme(base)
    return {"ok": True, "dir": str(base), "written": written, "pruned": pruned}


def read_notes(cfg: dict) -> dict:
    _ensure_notes(cfg)
    d = _load(notes_path(cfg), None)
    if not isinstance(d, dict):
        d = {}
    return {
        "todos": d.get("todos") if isinstance(d.get("todos"), list) else [],
        "events": d.get("events") if isinstance(d.get("events"), list) else [],
        "memo": d.get("memo") if isinstance(d.get("memo"), list) else [],
        "diary": d.get("diary") if isinstance(d.get("diary"), dict) else {},
        "updated_at": d.get("updated_at") or "",
    }


def write_notes(cfg: dict, body: dict) -> dict:
    """整份覆盖写。原子落盘（tmp + replace），页面自动保存时不会读到半个文件。"""
    p = notes_path(cfg)
    payload = {
        "todos": body.get("todos") if isinstance(body.get("todos"), list) else [],
        # 行程：{id, date, time, title, place, done, created}。白名单写法意味着漏一个键
        # 就会在保存时被静默丢掉，新增字段记得同步 read_notes / write_notes 两处。
        "events": body.get("events") if isinstance(body.get("events"), list) else [],
        "memo": body.get("memo") if isinstance(body.get("memo"), list) else [],
        "diary": body.get("diary") if isinstance(body.get("diary"), dict) else {},
        "updated_at": datetime.now().isoformat(timespec="seconds"),
    }
    raw = json.dumps(payload, ensure_ascii=False, indent=1)
    if len(raw.encode("utf-8")) > _MAX_NOTES_BYTES:
        return {"ok": False, "error": "内容太大（超过 512KB），先清理旧备忘"}
    with _NOTES_LOCK:
        p.parent.mkdir(parents=True, exist_ok=True)
        tmp = p.with_suffix(".json.tmp")
        tmp.write_text(raw, encoding="utf-8")
        tmp.replace(p)
    # markdown 镜像：失败只当没铺，不能让一次保存因为写 md 报错而整体失败
    with _MD_LOCK:
        try:
            export_markdown(cfg, payload)
        except OSError:
            pass
    return {"ok": True, "bytes": len(raw.encode("utf-8")), "updated_at": payload["updated_at"],
            "dir": str(p.parent)}


# --------------------------------------------------------------------------
# 工作流水：09-个人工作台/流水/（2026-11-18）
#
#   半年的手写流水（2026-04 ~ 2026-11，166 天近千条）不进 notes.json：
#   那份是「今天」，整份覆盖写、512KB 上限，把它撑爆了会连带待办一起丢。
#   这里单独一份 流水.json，卡片只做两件事 —— 读某个月、勾一条。
#   不做整份覆盖写，两个标签页同时开着也不会互相冲掉。
#
#   真源链路：原始流水.md（手写底稿）→ _build.py → 流水.json → 本模块。
#   勾选改的是 流水.json，同时把该月的 markdown 镜像重铺一遍，别让两边对不上。
# --------------------------------------------------------------------------

_FLOW_DIR = "流水"
_FLOW_LOCK = threading.Lock()
_FLOW_CACHE: dict = {"key": None, "data": None}
_WD = "一二三四五六日"

# 2026-11-20：用户真正在写的那份在「生活区」，json 旁边那份 原始流水.md 是旧副本。
# 这里直接盯着外面那份，改一行、刷一次页面就是新内容。
_FLOW_MD_DIR = Path.home() / "Documents" / "生活区" / "工作流水"
_FLOW_MD_NAME = "工作流水202604-.md"
_FLOW_SRC_FALLBACK = "原始流水.md"
_FLOW_BUILD: dict = {"mod": None}


def flow_dir(cfg: dict) -> Path:
    return personal_dir(cfg) / _FLOW_DIR


def flow_path(cfg: dict) -> Path:
    return flow_dir(cfg) / "流水.json"


def flow_src_path(cfg: dict) -> Path:
    """被监控的手写底稿：优先生活区那份，没有才退回 json 旁边的旧副本。"""
    live = _FLOW_MD_DIR / _FLOW_MD_NAME
    if live.exists():
        return live
    return flow_dir(cfg) / _FLOW_SRC_FALLBACK


def _flow_builder(cfg: dict):
    """复用 _build.py 的解析器 —— 脚本与卡片必须同一套口径，
    否则会出现「脚本说 966 条、卡片只显示 737 条」这种对不上的事。"""
    mod = _FLOW_BUILD.get("mod")
    if mod is not None:
        return mod
    p = flow_dir(cfg) / "_build.py"
    if not p.exists():
        return None
    try:
        import importlib.util
        spec = importlib.util.spec_from_file_location("cuntu_flow_build", p)
        mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(mod)
    except Exception:
        return None
    _FLOW_BUILD["mod"] = mod
    return mod


def _flow_key(t) -> str:
    s = re.sub(r"\s+", "", str(t or ""))
    return s.strip("，,、：:;；。. ")


def flow_marks_path(cfg: dict) -> Path:
    """卡片里勾的勾，跟条目本身分开放。

    md 只回答「有哪些事」；勾没勾由这里说了算。否则 md 里那些「1. xxx」
    编号条目（没有 checkbox）在卡片上永远勾不上，新写的日期更是连条目都
    还没进 json —— 勾一下后端只能回一句「没这条流水」。
    """
    return flow_dir(cfg) / "勾选.json"


def _flow_marks(cfg: dict) -> dict:
    return _load(flow_marks_path(cfg), {}) or {}


def _flow_mark_key(date, text) -> str:
    return f"{str(date or '')}|{_flow_key(text)}"


def _flow_merge_done(cfg: dict, days: list) -> None:
    """勾选状态叠加上来，优先级：

      勾选.json（卡片里的操作，最高） > md 的 [x] > 流水.json 里的历史勾选。

    这样「在卡片上勾一条 md 里没 checkbox 的事」才存得住，反过来取消也生效。
    """
    marks = _flow_marks(cfg)
    hist: dict = {}
    d = _load(flow_path(cfg), {}) or {}
    for day in d.get("days") or []:
        for it in day.get("items") or []:
            if it.get("done") is True:
                hist[_flow_mark_key(day.get("date"), it.get("text"))] = it.get("done_at") or True
    for day in days:
        for it in day.get("items") or []:
            k = _flow_mark_key(day.get("date"), it.get("text"))
            m = marks.get(k)
            if isinstance(m, dict):
                it["done"] = bool(m.get("done"))
                if it["done"] and m.get("at"):
                    it["done_at"] = str(m.get("at"))[:10]
                continue
            if it.get("done") is True:
                continue
            if k in hist:
                it["done"] = True
                v = hist[k]
                if isinstance(v, str):
                    it.setdefault("done_at", v)


def read_flow(cfg: dict) -> dict:
    """整份读，按 md 的 mtime 缓存 —— 文件没动就不重解析。

    真源换成手写 md 后，卡片的「刷新」等价于「文件一存盘就看得见」。
    """
    src = flow_src_path(cfg)
    try:
        st = src.stat()
        key = (str(src), st.st_mtime_ns, st.st_size)
    except OSError:
        key = None
    if _FLOW_CACHE["data"] is not None and _FLOW_CACHE["key"] == key:
        return _FLOW_CACHE["data"]

    days: list = []
    if key is not None:
        b = _flow_builder(cfg)
        try:
            days = b.parse_file(src) if b else []
        except Exception:
            days = []
    if days:
        _flow_merge_done(cfg, days)
        note = "md"
    else:
        # 底稿解析不出来就退回 json，别让整张卡空着
        d = _load(flow_path(cfg), {}) or {}
        days = d.get("days") if isinstance(d.get("days"), list) else []
        note = "json"
    for day in days:
        _flow_kinds(day.get("items") or [])
    out = {"days": days, "missing": not days, "src": note}
    _FLOW_CACHE["key"], _FLOW_CACHE["data"] = key, out
    return out


def _flow_kinds(items: list) -> None:
    """给每条标 kind：group（板块/子板块）/ trip（行程）/ task（真任务）。

    底稿里层级只记了缩进，没记「这一行是标题」；靠两句就能判准：
      · 后面紧跟着更深一层 —— 它是分组，不是事（墨禽、红驼都算，红的只是子板块）；
      · 文字和它自己的 cat 完全相同 —— 纯粹是个板块名，不是事；
      · cat 是「行程」或文字以「行程」开头 —— 那是日程，不是待办。
    标在服务端，工作台与工具页才可能口径一致。
    """
    for i, it in enumerate(items):
        lv = int(it.get("level") or 0)
        nxt = items[i + 1] if i + 1 < len(items) else None
        has_child = bool(nxt) and int(nxt.get("level") or 0) > lv
        text = str(it.get("text") or "").strip()
        cat = str(it.get("cat") or "").strip()
        if has_child or (cat and text == cat):
            it["kind"] = "group"
        elif cat == "行程" or text.startswith("行程"):
            it["kind"] = "trip"
        else:
            it["kind"] = "task"


def _flow_counts(days: list) -> dict:
    """只数任务。板块名、行程都不是待办 —— 之前把「行程：leo住院」算进未完成，
    卡片顶部就一直挂着一笔还不掉的欠账。"""
    items = [it for d in days for it in (d.get("items") or [])]
    tasks = [it for it in items if (it.get("kind") or "task") == "task"]
    return {
        "days": len(days),
        "items": len(tasks),
        "done": sum(1 for it in tasks if it.get("done") is True),
        "open": sum(1 for it in tasks if it.get("done") is False),
        "trips": sum(1 for it in items if it.get("kind") == "trip"),
        "groups": sum(1 for it in items if it.get("kind") == "group"),
    }


def flow_summary(cfg: dict) -> dict:
    """首屏要的：按月统计 + 未完成数 + 最近两天。整月条目按需另拉。"""
    data = read_flow(cfg)
    all_days = data["days"]
    # 没日期的小节（AI项目 / 重要待办事项）不参与「哪天干了什么」的统计，
    # 也不该算进某个月份 —— 单独一份给前端放在列表最后。
    days = [d for d in all_days if d.get("date")]
    undated = [{"title": str(d.get("title") or ""),
                "items": d.get("items") or [],
                "count": sum(1 for it in (d.get("items") or [])
                             if (it.get("kind") or "task") == "task")}
               for d in all_days if not d.get("date")]
    months: dict[str, list] = {}
    for d in days:
        months.setdefault(str(d.get("month") or ""), []).append(d)
    out_months = []
    for m in sorted(months, reverse=True):
        c = _flow_counts(months[m])
        out_months.append({"month": m, **c})
    recent = [{"date": d["date"],
               "items": [it for it in (d.get("items") or [])
                         if it.get("kind") != "group"][:12],
               "count": sum(1 for it in (d.get("items") or [])
                            if (it.get("kind") or "task") == "task")}
              for d in days if d.get("items")][-2:][::-1]
    cats: dict[str, int] = {}
    for d in days:
        for it in d.get("items") or []:
            # 板块名本身不计数，但它下面的事仍然算在这个板块头上
            if it.get("kind") == "group":
                continue
            c = it.get("cat") or "其他"
            cats[c] = cats.get(c, 0) + 1
    total = _flow_counts(days)
    # 首屏「今日」要的两个数：今天记了几条、近 7 天每天几条（画小柱子）。
    # 在服务端算，前端就只认一个 week 数组，不用自己去 recent 里翻今天。
    by_date = {str(d.get("date") or ""): sum(
        1 for it in (d.get("items") or []) if (it.get("kind") or "task") == "task")
        for d in days}
    today = datetime.now().strftime("%Y-%m-%d")
    week = []
    for i in range(6, -1, -1):
        day = (datetime.now() - timedelta(days=i)).strftime("%Y-%m-%d")
        week.append({"date": day, "count": by_date.get(day, 0)})
    try:
        src_mtime = flow_src_path(cfg).stat().st_mtime
    except OSError:
        src_mtime = 0
    return {
        "missing": data.get("missing", False),
        "src_mtime": src_mtime,
        "months": out_months,
        "recent": recent,
        "cats": sorted(cats.items(), key=lambda x: -x[1]),
        "dir": str(flow_dir(cfg)),
        "src": data.get("src") or "",
        "src_path": str(flow_src_path(cfg)),
        "undated": undated,
        "today": {"date": today, "count": by_date.get(today, 0)},
        "week": week,
        **total,
    }


def flow_month(cfg: dict, month: str) -> dict:
    """某个月的完整天数。month 形如 2026-09；空给最近一个月，'all' 给全部。

    'all' 是给「跨月看未完成」用的：238 条散在八个月里，逐月翻太累。
    近千条一次给出去约 200KB，本机/局域网都是毫秒级，不值得再分页。
    """
    all_days = read_flow(cfg)["days"]
    days = [d for d in all_days if d.get("date")]
    undated = [{"title": str(d.get("title") or ""), "items": d.get("items") or [],
                "count": sum(1 for it in (d.get("items") or [])
                             if (it.get("kind") or "task") == "task")}
               for d in all_days if not d.get("date")]
    months = sorted({str(d.get("month") or "") for d in days if d.get("month")}, reverse=True)
    if month == "all":
        return {"month": "all", "months": months, "days": days, "undated": undated,
                "counts": _flow_counts(days)}
    if month not in months:
        month = months[0] if months else ""
    picked = [d for d in days if str(d.get("month") or "") == month]
    return {"month": month, "months": months, "days": picked, "undated": undated,
            "counts": _flow_counts(picked)}


def _flow_md(cfg: dict, month: str, days: list) -> None:
    """重铺一个月的 markdown 镜像。跟 _build.py 的格式保持一致。"""
    out = [f"# {month} 工作流水", "",
           "> 由 `流水.json` 自动生成，手改会被覆盖。底稿是 `原始流水.md`。", ""]
    for d in days:
        try:
            wd = _WD[datetime.strptime(d["date"], "%Y-%m-%d").weekday()]
        except (ValueError, KeyError):
            wd = ""
        out.append(f"## {str(d['date'])[5:]} 周{wd}" if wd else f"## {d['date']}")
        out.append("")
        if not d.get("items"):
            out.append("- （无记录）")
        for it in d.get("items") or []:
            done = it.get("done")
            box = "-" if done is None else ("- [x]" if done else "- [ ]")
            pad = "  " * int(it.get("level") or 0)
            tail = f"  `{it['cat']}`" if it.get("cat") and not it.get("level") else ""
            out.append(f"{pad}{box} {it.get('text') or ''}{tail}")
        out.append("")
    p = flow_dir(cfg) / f"{month}.md"
    tmp = p.with_suffix(".md.tmp")
    tmp.write_text("\n".join(out).rstrip() + "\n", encoding="utf-8")
    tmp.replace(p)


def toggle_flow(cfg: dict, iid: str, done: bool) -> dict:
    """勾一条：只记「这个 id 现在是什么状态」，回写 勾选.json。

    不去改 md —— 手写稿是用户的地盘，编号列表里也没有地方放 checkbox。
    """
    with _FLOW_LOCK:
        data = read_flow(cfg)
        target = None
        for day in data["days"]:
            for it in day.get("items") or []:
                if it.get("id") == iid:
                    target = (day.get("date") or "", it.get("text") or "")
                    break
            if target:
                break
        if not target:
            return {"ok": False, "error": "没这条流水"}
        marks = _flow_marks(cfg)
        marks[_flow_mark_key(*target)] = {
            "done": bool(done),
            "at": datetime.now().strftime("%Y-%m-%d %H:%M"),
        }
        p = flow_marks_path(cfg)
        try:
            tmp = p.with_suffix(".json.tmp")
            tmp.write_text(json.dumps(marks, ensure_ascii=False, indent=1), encoding="utf-8")
            tmp.replace(p)
        except OSError as e:
            return {"ok": False, "error": f"写不进去：{e}"}
        _FLOW_CACHE["key"] = None      # 缓存作废，下读重来
    return {"ok": True, "id": iid, "done": bool(done), "month": target[0][:7]}


_FLOW_DAY_RE = re.compile(r"^##\s+.*?(\d{4}-\d{2}-\d{2}|\d{1,2}-\d{1,2}|\d{4})\s*")


def _flow_src_append(cfg: dict, day: str, text: str) -> bool:
    """往 原始流水.md 的「那一天」末尾补一行。

    为什么不只写 json：底稿才是真源，`_build.py` 重建时按底稿走，
    只落在 json 里的补记会在下一次重建时无声消失。
    找不到那天的标题就整个文件末尾补一个 —— 手写稿里「今天」往往还没起头。
    """
    p = flow_src_path(cfg)
    if not p.exists():
        return False
    try:
        lines = p.read_text(encoding="utf-8").split("\n")
    except OSError:
        return False
    y, mo, d = day.split("-")
    pats = (day, f"{int(mo)}-{int(d)}", f"{int(mo)}{int(d)}")

    def is_that_day(line: str) -> bool:
        if not line.startswith("## "):
            return False
        body = line[3:].strip()
        return any(body.startswith(x) for x in pats) or body.replace(" ", "").startswith(day)

    head = next((i for i, ln in enumerate(lines) if is_that_day(ln)), None)
    row = f"- {text}"
    if head is None:
        while lines and not lines[-1].strip():
            lines.pop()
        wd = "一二三四五六日"[datetime.strptime(day, "%Y-%m-%d").weekday()]
        lines += ["", f"## {day}-周{wd}", "", row, ""]
    else:
        # 这一天的最后一行内容之前（下一个 ## / # 标题处截止）
        stop = len(lines)
        for i in range(head + 1, len(lines)):
            if lines[i].startswith("## ") or lines[i].startswith("# "):
                stop = i
                break
        last = head
        for i in range(head + 1, stop):
            if lines[i].strip():
                last = i
        if last == head:                      # 这一天还空着：先空一行再写，跟手写稿的版式一致
            lines[head + 1:head + 1] = ["", row]
        else:
            lines.insert(last + 1, row)
    try:
        tmp = p.with_suffix(".md.tmp")
        tmp.write_text("\n".join(lines), encoding="utf-8")
        tmp.replace(p)
    except OSError:
        return False
    return True


def flow_add(cfg: dict, text: str, day: str = "") -> dict:
    """首屏速记：在今天的流水里补一条（json + 底稿 + 当月镜像，三处对齐）。"""
    text = " ".join((text or "").split())
    if not text:
        return {"ok": False, "error": "空的一行"}
    text = text[:200]
    day = day or datetime.now().strftime("%Y-%m-%d")
    try:
        datetime.strptime(day, "%Y-%m-%d")
    except ValueError:
        return {"ok": False, "error": "日期不对"}
    with _FLOW_LOCK:
        p = flow_path(cfg)
        d = _load(p, {}) or {}
        days = d.get("days") if isinstance(d.get("days"), list) else []
        hit = next((x for x in days if x.get("date") == day), None)
        if hit is None:
            hit = {"date": day, "month": day[:7], "items": []}
            days.append(hit)
            days.sort(key=lambda x: str(x.get("date") or ""))
        items = hit.setdefault("items", [])
        seq = len(items) + 1
        used = {it.get("id") for it in items}
        while f"{day.replace('-', '')}-{seq}" in used:
            seq += 1
        iid = f"{day.replace('-', '')}-{seq}"
        items.append({"id": iid, "text": text, "done": None, "level": 0, "cat": "速记"})
        d["days"] = days
        try:
            tmp = p.with_suffix(".json.tmp")
            tmp.write_text(json.dumps(d, ensure_ascii=False, indent=1), encoding="utf-8")
            tmp.replace(p)
        except OSError as e:
            return {"ok": False, "error": f"写不进去：{e}"}
        _FLOW_CACHE["key"] = None
        month = str(hit.get("month") or day[:7])
    src = _flow_src_append(cfg, day, text)
    try:
        _flow_md(cfg, month, [x for x in days if str(x.get("month") or "") == month])
    except OSError:
        pass
    return {"ok": True, "id": iid, "day": day, "text": text, "src": src,
            "count": len(items)}


# --------------------------------------------------------------------------
# 系统记录：10-系统记录/总览.md（2026-11-18）
#
#   心跳、磁盘、巡检、新闻各自写自己的 jsonl（真源，追加式）；这份 markdown 是
#   给人看的汇总，由 /api/dash 每次请求时重算 —— 数据都在手边，不必另存一份。
#   内容没变就不落笔，页面每分钟拉一次也不会把 mtime 刷成「一直在改」。
# --------------------------------------------------------------------------

_SYS_DIR = "10-系统记录"


def sys_dir(cfg: dict) -> Path:
    return _root(cfg) / _SYS_DIR


def _overview_md(cfg: dict, services: list, trends: dict) -> str:
    base = sys_dir(cfg)
    now = datetime.now()
    hb = _tail_jsonl(base / "心跳" / "流水.jsonl", 400)
    hb_runs = [r for r in hb if (r.get("event") or "") == "heartbeat"]
    disk = _tail_jsonl(base / "磁盘" / "磁盘.jsonl", 400)
    insp = _tail_jsonl(base / "巡检" / "历史.jsonl", 30)

    # 抬头只写日期：写分钟的话每分钟内容都变，"内容不变不落笔"就失效了，
    # 而精确到分钟的时间下面每段都有（最近一轮心跳/采样）。
    out = ["# 系统记录总览", "",
           f"> {now:%Y-%m-%d} 自动生成。真源是同目录的 jsonl / json，这份只是给人读的汇总。", ""]

    # 心跳
    cutoff = now - timedelta(hours=24)
    day_runs = [r for r in hb_runs if str(r.get("at") or "") >= cutoff.strftime("%Y-%m-%d %H:%M")]
    bad = [r for r in day_runs if not r.get("ok")]
    healed = sum(len(r.get("healed") or []) for r in day_runs)
    out += ["## 心跳", ""]
    if hb_runs:
        last = hb_runs[-1]
        out.append(f"- 最近一次：{last.get('at') or '—'} · {'正常' if last.get('ok') else '异常'}"
                   f" · gateway {'在线' if last.get('online') else '离线'}"
                   f" · 陈旧锁 {last.get('stale') or 0}")
        out.append(f"- 最近 24 小时：{len(day_runs)} 轮，异常 {len(bad)} 轮，自愈动作 {healed} 次")
        out += ["", "| 时间 | 结果 | 说明 |", "|---|---|---|"]
        for r in hb_runs[::-1][:8]:
            note = "、".join(str(x) for x in (r.get("healed") or [])) or "—"
            out.append(f"| {str(r.get('at') or '')[5:16]} | {'正常' if r.get('ok') else '异常'} | {note} |")
    else:
        out.append("（还没有心跳记录）")
    out.append("")

    # 磁盘
    out += ["## 磁盘", ""]
    if disk:
        last = disk[-1]
        out.append(f"- 当前：已用 {last.get('used_pct')}%，可用 {last.get('free_gb')} GB"
                   f" / 共 {last.get('total_gb')} GB")
        recent = [d for d in disk if str(d.get("at") or "") >= cutoff.strftime("%Y-%m-%d %H:%M")] or disk
        pcts = [float(d.get("used_pct") or 0) for d in recent]
        out.append(f"- 最近 24 小时：{recent[0].get('used_pct')}% → {recent[-1].get('used_pct')}%，"
                   f"峰值 {max(pcts):.1f}%，采样 {len(recent)} 次")
        out += ["", "| 时间 | 已用 | 可用 |", "|---|---|---|"]
        for d in disk[::-1][:8]:
            out.append(f"| {str(d.get('at') or '')[5:16]} | {d.get('used_pct')}% | {d.get('free_gb')} GB |")
    else:
        out.append("（每轮心跳会记一条，还没有采样）")
    out.append("")

    # 巡检（手动）
    out += ["## 巡检（手动跑的完整一轮）", ""]
    if insp:
        last = insp[-1]
        down = last.get("services_down") or []
        out.append(f"- 最近一次：{last.get('at')} · {'通过' if last.get('ok') else '有问题'}"
                   f" · 检查 {last.get('passed')}/{last.get('total')}"
                   f" · 服务 {last.get('services_up')}/{last.get('services_total')} 在线"
                   f"（{'、'.join(down) + ' 离线' if down else '全部在线'}）")
        out.append(f"- 机器：磁盘 {last.get('disk_pct')}% · 内存 {last.get('mem_pct')}% · 负载 {last.get('load1')}")
        if last.get("healed"):
            out.append("- 自愈：" + "；".join(str(x) for x in last["healed"]))
        out += ["", "| 时间 | 结论 | 失败项 |", "|---|---|---|"]
        for r in insp[::-1][:6]:
            fails = "、".join(r.get("failed") or []) or "—"
            out.append(f"| {r.get('at')} | {'通过' if r.get('ok') else '异常'} | {fails} |")
    else:
        out.append("（还没手动跑过；平时的自动检查看上面的心跳）")
    out.append("")

    # 服务端口（此刻）
    out += ["## 服务端口（此刻探活）", ""]
    down = [x for x in services if not x.get("up")]
    out.append(f"- {len(services) - len(down)}/{len(services)} 在线"
               + ("；离线：" + "、".join(x.get("name") or x.get("id") or "" for x in down) if down else ""))
    out.append("")

    # 新闻
    hist = (trends or {}).get("history") or []
    out += ["## 新闻（技术动向）", ""]
    if hist:
        out += ["| 日期 | 状态 | 条目 | 完成时间 |", "|---|---|---|---|"]
        for h in hist[:7]:
            fa = str(h.get("finished_at") or "")
            try:
                # 运行记录里是 UTC，本机看要换成本地时区
                fa = datetime.fromisoformat(fa.replace("Z", "+00:00")).astimezone().strftime("%m-%d %H:%M")
            except ValueError:
                fa = fa[:16] or "—"
            status = {"ok": "正常", "partial": "部分来源失败", "fail": "失败"}.get(
                str(h.get("status") or ""), h.get("status") or "—")
            out.append(f"| {h.get('date')} | {status} | {h.get('items')} | {fa} |")
    else:
        out.append("（没有运行记录）")
    out.append("")
    return "\n".join(out)


def write_overview(cfg: dict, services: list, trends: dict) -> bool:
    """重写总览.md。内容一样就不落笔。"""
    try:
        p = sys_dir(cfg) / "总览.md"
        text = _overview_md(cfg, services, trends)
        if p.is_file() and p.read_text(encoding="utf-8") == text:
            return False
        p.parent.mkdir(parents=True, exist_ok=True)
        tmp = p.with_suffix(".md.tmp")
        tmp.write_text(text, encoding="utf-8")
        tmp.replace(p)
        return True
    except OSError:
        return False


def payload(cfg: dict) -> dict:
    root = _root(cfg)
    now = datetime.now()
    gallery = _gallery(root)
    reader = _reader(root)
    services = _services(cfg)
    trends = _trends(cfg)
    # 系统记录总览跟着这次请求重算（数据都在手边；内容没变不落笔）
    try:
        write_overview(cfg, services, trends)
    except Exception:
        pass
    return {
        "now": now.strftime("%Y-%m-%d %H:%M:%S"),
        "today": now.strftime("%Y-%m-%d"),
        "weekday": "周" + "一二三四五六日"[now.weekday()],
        "root": str(root),
        "services": services,
        "disk": _disk(root),
        "gallery": gallery,
        "reader": reader,
        "media": _media(root),
        "llmcost": _llmcost(root),
        "files": _files(root),
        "health": _health(root),
        "heartbeat": _heartbeat(),
        "trends": trends,
        "dedupe": _dedupe(),
        "ocr": _ocr(),
        "worklog": _worklog(cfg),
        "flow": flow_summary(cfg),
        "report": _report(root),
        "inspect": _inspect(root),
        "apps": apps_payload(cfg),
        "notes": read_notes(cfg),
    }


if __name__ == "__main__":  # 终端排错用：python dash.py [--json|--export]
    _cfg = {"allowed_roots": [str(Path.home() / "Documents/AI项目")],
            "inbox_dir": str(Path.home() / "Documents/AI项目/07-收件箱")}
    _cfg.update(_load(BASE / "config.json", {}) or {})
    if "--export" in sys.argv:  # 只重铺 markdown 镜像，不动页面数据
        print(json.dumps(export_markdown(_cfg), ensure_ascii=False))
        sys.exit(0)
    data = payload(_cfg)
    if "--json" in sys.argv:
        print(json.dumps(data, ensure_ascii=False, indent=1))
    else:
        for k in ("gallery", "reader", "media", "files", "health", "trends", "dedupe", "ocr", "disk"):
            print(k, "=", json.dumps(data[k], ensure_ascii=False)[:180])
