#!/usr/bin/env python3
"""每日简报：把散在五处的账收成五段，一段一条消息。

为什么分条发（2026-11-21 用户要求）：微信那头跑的是 4B
（qwen3.5-4b-gguf:q4km），一次塞五段长文进去必爆上下文 —— 它连工具都不一定
调得对。拆成五条，每条独立自包含（带日期 + 「1/5」编号），模型（和人）每次
只需要读一段，漏看一条也不影响其它四条。

为什么不让模型写：结论都是**算出来的**（服务探活、token 累加、存图计数），
代码算完直接发最准；模型只负责那半句人话概括，有就用（关注追踪的 4B 摘要），
没有就不硬编。

数据一律现取，不新建第二本账：
  中台    dash.payload() 现算 —— 服务探活 / 磁盘 / 心跳 / 存图，跟工作台同一份
  Token   hostwork 的会话聚合 —— 跟「本机工作」那几张卡同一份，单价走 13-算价
  工作    工作流水的补记 + 各 AI 客户端昨天干完的任务 + 待办/行程里的今天
  新闻    10-系统记录/新闻/days/<日期>.json（每天 9 点那一轮的产物）
  关注    11-关注追踪/feed.json + 简报.json（4B 的一句话概括）

落盘两份，放 <数据根>/10-系统记录/简报/：
  <日期>.json  页面读这份（一段一个 section，带推送原文）
  <日期>.md    人翻的，跟 10-系统记录 下别的目录一个规矩

缓存规矩：一天只算一次。早上 8 点推完落盘，白天页面读同一份；要重算就
带 force（页面上的「重建」按钮 / POST /api/brief/run）。
"""
from __future__ import annotations

import json
import os
import sys
import time
from datetime import date, datetime, timedelta
from pathlib import Path

BASE = Path(__file__).resolve().parent
if str(BASE) not in sys.path:
    sys.path.insert(0, str(BASE))

# hostwork（本机工作的采集层）在 ~/Documents/cuntu，跟 dash.py 里同一个走法。
_CUNTU = Path.home() / "Documents" / "cuntu"
if _CUNTU.is_dir() and str(_CUNTU) not in sys.path:
    sys.path.insert(0, str(_CUNTU))

# 内存缓存：同一个进程里页面连点几次刷新，不必每次都把 payload 重算一遍。
# 落盘的 JSON 才是跨进程的缓存（8 点推送那个进程和页面服务不是同一个）。
_MEM = {"day": "", "at": 0.0, "data": None}
_MEM_TTL = 120.0

# 一条微信上限 1500 字（wechat_push 自己会截）。这里再收一道：宁可少说两句，
# 也别让 4B 读一坨 —— 它读不完的部分等于没发。
TEXT_MAX = 900


# ---------------------------------------------------------------- 小工具

def _load(path: Path, default):
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return default


def _human(n) -> str:
    n = float(n or 0)
    for unit in ("B", "KB", "MB", "GB", "TB"):
        if n < 1024 or unit == "TB":
            return (f"{n:.0f} {unit}" if unit in ("B", "KB") else f"{n:.1f} {unit}")
        n /= 1024.0
    return f"{n:.1f} TB"


def _wan(n) -> str:
    """token 用「万」说话：128.4 万比 1284321 好读，也不占字数。"""
    n = int(n or 0)
    if n >= 10000:
        return f"{n / 10000:.1f} 万"
    return str(n)


def _root(cfg: dict) -> Path:
    roots = (cfg or {}).get("allowed_roots") or []
    if roots:
        return Path(roots[0])
    return Path.home() / "Documents" / "AI项目"


def _yest_of(day: str) -> str:
    d = date.fromisoformat(day) if day else date.today()
    return (d - timedelta(days=1)).isoformat()


def _weekday_cn(day: str) -> str:
    d = date.fromisoformat(day)
    return "周" + "一二三四五六日"[d.weekday()]


def _cut(s, n: int) -> str:
    s = " ".join(str(s or "").split())
    return s if len(s) <= n else s[: n - 1] + "…"


def _hostwork():
    try:
        import hostwork
        return hostwork
    except Exception:                      # noqa: BLE001
        return None


# ---------------------------------------------------------------- 各段取数

def _middle(root: Path, cfg: dict) -> dict:
    """中台：服务 / 磁盘 / 存图 / 心跳。全是现算，不读第二份缓存。"""
    import dash as dashmod
    p = dashmod.payload(cfg) or {}
    svcs = [s for s in (p.get("services") or []) if isinstance(s, dict)]
    up = [s for s in svcs if s.get("up")]
    down = [s for s in svcs if not s.get("up") and not s.get("lazy")]
    lazy = [s for s in svcs if not s.get("up") and s.get("lazy")]
    disk = p.get("disk") or {}
    gal = p.get("gallery") or {}
    # 心跳优先读文件：STATE 只活在常驻的那个进程里，脚本（早上 8 点那条 launchd）
    # 拿到的 STATE 是空的，读文件才看得到「最近一次几点、几项待修」。
    hb = _load(root / "10-系统记录" / "心跳" / "最新.json", {}) or (p.get("heartbeat") or {})
    checks = hb.get("checks") or []
    hb = {"ok": hb.get("ok"), "at": str(hb.get("at") or ""),
          "bad": [c for c in checks if not c.get("ok")]}
    # 整份 payload 挂回来：worklog 那半（各客户端昨天干完的活）就在里面，
    # 再单独调一次 _worklog() 等于把会话流水扫两遍。
    return {"svcs": svcs, "up": up, "down": down, "lazy": lazy,
            "disk": disk, "gallery": gal, "heartbeat": hb, "payload": p}


def _gallery_day(root: Path, day: str) -> tuple[int, int]:
    """那一天存了几组、几张。索引里 archived_at 是落盘时刻，按日期前缀比。"""
    idx = _load(root / "04-存图" / "_索引.json", {}) or {}
    arts = idx.get("articles") or []
    g = n = 0
    for a in arts:
        if str(a.get("archived_at") or "")[:10] != day:
            continue
        g += 1
        n += int(a.get("count") or 0)
    return g, n


def _tokens(day: str) -> dict:
    """那一天的 token 与折算。

    口径跟「本机工作」的卡完全一致 —— 同一份 sessions_snapshot，同一份
    price_config，同一个 _cost_of。**不在简报里另立一套算法**：两处对不上时
    人会先怀疑简报，然后就不看它了。
    """
    empty = {"in": 0, "out": 0, "cached": 0, "calls": 0, "total": 0,
             "models": [], "sessions": [], "cost": None, "points": None,
             "currency": "¥", "unpriced": [], "cache_hit": None, "ok": False}
    hw = _hostwork()
    if hw is None:
        return empty
    try:
        hw.scan_if_stale()
        price_cfg = hw.price_config()
        by_model: dict[str, dict] = {}
        calls = 0
        sess: list[dict] = []
        for s in hw.sessions_snapshot():
            dm = (s.get("days") or {}).get(day)
            if not dm:
                continue
            calls += int(dm.get("calls") or 0)
            for m, v in (dm.get("models") or {}).items():
                cur = by_model.setdefault(m, {"in": 0, "out": 0, "cached": 0, "n": 0})
                for k in ("in", "out", "cached", "n"):
                    cur[k] += int(v.get(k) or 0)
            # 那一天还在烧 token 的会话 = 那天在干的活。标题比「客户端名」有用得多，
            # 也不像 worklog.host 那样只剩最近 40 条（今天一忙就把昨天挤没了）。
            title = str(s.get("title") or "").strip()
            utt = s.get("utt") or []
            if not title and utt:
                title = str(utt[-1][1] if isinstance(utt[-1], (list, tuple)) else "") or ""
            if title:
                sess.append({"title": title,
                             "calls": int(dm.get("calls") or 0),
                             "tokens": int(dm.get("in") or 0) + int(dm.get("out") or 0),
                             "proj": str(s.get("proj") or "")})
    except Exception:                      # noqa: BLE001  采集层坏了不能拖垮简报
        return empty
    sess.sort(key=lambda x: -x["tokens"])
    base = {**empty, "sessions": sess[:10], "ok": True}
    if not by_model:
        return base
    tin = sum(v["in"] for v in by_model.values())
    tout = sum(v["out"] for v in by_model.values())
    cached = sum(v["cached"] for v in by_model.values())
    cost = 0.0
    points = 0.0
    any_cost = any_pts = False
    unpriced: list[str] = []
    rows = []
    for m, v in sorted(by_model.items(), key=lambda kv: -(kv[1]["in"] + kv[1]["out"])):
        pt = ct = None
        try:
            pt, ct = hw._cost_of(v, hw._price_for(m, price_cfg))
        except Exception:                  # noqa: BLE001
            pass
        if ct is not None:
            cost += ct
            any_cost = True
        if pt is not None:
            points += pt
            any_pts = True
        if ct is None and pt is None:
            unpriced.append(m)
        rows.append({"model": m, "in": v["in"], "out": v["out"],
                     "cached": v["cached"], "n": v["n"],
                     "cost": round(ct, 2) if ct is not None else None,
                     "points": round(pt, 1) if pt is not None else None})
    return {"in": tin, "out": tout, "cached": cached, "calls": calls,
            "total": tin + tout, "models": rows[:6], "sessions": sess[:10],
            "cost": round(cost, 2) if any_cost else None,
            "points": round(points, 1) if any_pts else None,
            "currency": price_cfg.get("currency") or "¥",
            "unpriced": unpriced[:4],
            "cache_hit": round(cached / tin, 3) if tin else None,
            "ok": True}


def _works(root: Path, yest: str, today: str, t: dict, payload: dict) -> dict:
    """昨天干了什么 + 今天要做什么。

    昨日主要活的主源是**会话标题**（昨天还在烧 token 的那些会话）：worklog.host
    只留最近 40 条，今天上午一忙就能把昨天整段挤没，靠它做日报会经常空着。
    流水补记是第二源 —— 手写的那些事模型看不见，只有那儿有。

    今天要做的事按「逾期 → 今天到期 → 没写日期」排，行程单列在后面。
    都没有就如实说没有，不做无中生有的总结：空白比瞎编有用。
    """
    done: list[str] = []
    seen: set[str] = set()

    def add(txt: str) -> None:
        txt = _cut(txt, 52)
        if txt and txt not in seen:
            seen.add(txt)
            done.append(txt)

    # 「继续」「好的」这类是回话不是任务，标题里出现就等于没标题
    _FILLER = {"继续", "好的", "可以", "对", "嗯", "是的", "行", "OK", "ok"}
    for s in (t.get("sessions") or []):
        title = str(s.get("title") or "").strip()
        if not title or title in _FILLER or len(title) < 4:
            continue
        add(title)
        if len(done) >= 5:
            break

    flow = _load(root / "09-个人工作台" / "流水" / "流水.json", {}) or {}
    for d in (flow.get("days") or []):
        if d.get("date") != yest:
            continue
        for it in (d.get("items") or []):
            if int(it.get("level") or 0) > 1:
                continue
            add(it.get("text") or "")

    notes = _load(root / "09-个人工作台" / "notes.json", {}) or {}
    todo_rows, ev_rows = [], []
    for t2 in (notes.get("todos") or []):
        if t2.get("done"):
            continue
        due = str(t2.get("due") or "")
        if due and due > today:
            continue
        tag = "（逾期）" if due and due < today else ("（今天）" if due == today else "")
        todo_rows.append((0 if due and due < today else (1 if due == today else 2),
                          _cut(str(t2.get("text") or ""), 40) + tag))
    for ev in (notes.get("events") or []):
        if str(ev.get("date") or "") != today:
            continue
        extra = " ".join(x for x in (ev.get("time"), ev.get("to")) if x)
        ev_rows.append(_cut((ev.get("title") or "行程") + (" · " + extra if extra else ""), 40))
    todo_rows.sort(key=lambda x: x[0])
    plan = [x for _, x in todo_rows[:5]] + ev_rows[:2]
    return {"done": done[:6], "plan": plan[:7]}


def _news(root: Path, day: str) -> dict:
    """那一天的新闻抓取产物，按 final_score 取前几条。

    优先取「昨天」那份：早上 8 点推的时候，今天 9 点那轮还没跑，
    只有昨天那份是最新的完整清单。没有就退回今天（有就有）。
    """
    base = root / "10-系统记录" / "新闻" / "days"
    items, used = [], ""
    for cand in (day, date.today().isoformat()):
        arr = _load(base / f"{cand}.json", None)
        if isinstance(arr, list) and arr:
            items, used = arr, cand
            break
    if not items:
        return {"items": [], "n": 0, "ai": 0, "day": ""}
    arr = sorted(items, key=lambda x: -(x.get("final_score") or 0))
    # 按来源限量：纯按分数排，前五条会全是 GitHub 上当天涨星的项目 —— 那是
    # 「今天又冒了什么仓库」，不是「新闻」。同一来源最多两条，剩下留给媒体。
    picked, per_src = [], {}
    for x in arr:
        src = str(x.get("source") or "?")
        if per_src.get(src, 0) >= 2:
            continue
        per_src[src] = per_src.get(src, 0) + 1
        picked.append(x)
        if len(picked) >= 5:
            break
    top = []
    for x in picked:
        top.append({
            "name": _cut(x.get("name") or x.get("title") or "", 60),
            "what": _cut(x.get("what") or x.get("why") or "", 68),
            "source": x.get("source") or "",
            "heat": _cut(x.get("heat") or "", 24),
            "tier": ((x.get("grade") or {}).get("tier") or ""),
            "url": x.get("url") or "",
        })
    ai = sum(1 for x in items if x.get("ai_related"))
    return {"items": top, "n": len(items), "ai": ai, "day": used}


def _follow(root: Path, day: str) -> dict:
    """关注的人昨天更新了什么。条目自带 4B 写的 summary，直接用，不再重算。

    两个源，缺一不可：feed.json 只留**最新一批**（每小时覆盖），早上 8 点推的
    时候它早就被今天凌晨那一轮换掉了 —— 所以「昨天读过什么」要看 11-关注追踪/
    简报.json：那份**按天存**，4B 的一句话概括和三个要点都在里面。
    """
    feed = _load(root / "11-关注追踪" / "feed.json", {}) or {}
    items = feed.get("items") or []
    day_items = [x for x in items if str(x.get("fetched") or "")[:10] == day]
    digest = _load(root / "11-关注追踪" / "简报.json", {}) or {}
    d = digest.get(day) or {}

    by: dict[str, int] = {}
    for x in day_items:
        k = str(x.get("platform") or "?")
        by[k] = by.get(k, 0) + 1

    # 条目挑「昨天的」；昨天那份已经被覆盖了就退回最新一批，并如实标明是「最新」
    pool = day_items or (items[:6] if d else [])
    picks = []
    seen_pl: dict[str, int] = {}
    for x in pool:
        plat = str(x.get("platform") or "?")
        # 每个平台最多两条：46 条微博全列出来，4B 读到第三条就顾不上了
        if seen_pl.get(plat, 0) >= 2:
            continue
        seen_pl[plat] = seen_pl.get(plat, 0) + 1
        picks.append({"platform": plat, "author": _cut(x.get("author") or "", 16),
                      "text": _cut(x.get("text") or "", 48),
                      "summary": _cut(x.get("summary") or "", 60),
                      "url": x.get("url") or ""})
        if len(picks) >= 4:
            break
    return {"n": len(day_items), "by": by, "picks": picks,
            "stale": bool(picks and not day_items),
            "text": _cut(d.get("text") or "", 160),
            "points": [_cut(p, 40) for p in (d.get("points") or [])[:3]],
            "at": d.get("at") or "", "model": d.get("model") or ""}


# ---------------------------------------------------------------- 组装成段

def _sec_middle(m: dict, yest: str) -> dict:
    svcs, up, down, lazy = m["svcs"], m["up"], m["down"], m["lazy"]
    disk = m["disk"]
    gal = m["gallery"]
    hb = m["heartbeat"]
    g_day, img_day = m["gallery_day"]

    pct = disk.get("pct") or disk.get("percent") or disk.get("used_pct")
    free = disk.get("free_h") or disk.get("free") or ""
    lines = []
    lines.append(f"服务 {len(up)}/{len(svcs)} 在线"
                 + (f"，离线：{'、'.join(s.get('name','') for s in down)}" if down else ""))
    if pct is not None:
        lines.append(f"磁盘 {pct}%" + (f"，剩 {free}" if free else ""))
    lines.append(f"存图 {gal.get('articles', 0)} 组 / {gal.get('images', 0)} 张"
                 f"（{gal.get('bytes_h') or '—'}）· 昨日 +{g_day} 组 {img_day} 张")
    if hb:
        at = hb.get("at") or ""
        # ok 是 None = 心跳线程还没跑过第一轮，别把「没跑」说成「异常」
        if hb.get("ok") is None:
            lines.append("心跳 还没有跑过第一轮")
        else:
            bad = hb.get("bad") or []
            when = at[11:16] if len(at) > 10 else at
            lines.append(f"心跳 {when or '—'} " + ("正常" if hb["ok"] else "异常")
                         + (f"，待修 {len(bad)} 项：" + "、".join(_cut(c.get('name'), 10) for c in bad[:3])
                            if bad else ""))
    if lazy:
        lines.append(f"按需服务没开 {len(lazy)} 个：{'、'.join(s.get('name','') for s in lazy[:5])}")
    head = f"{len(up)}/{len(svcs)} 服务在线 · 存图 {gal.get('images', 0)} 张"
    badge = "warn" if (down or (pct and float(pct) >= 92)) else "ok"
    return {"id": "middle", "title": "中台运行", "glyph": "server",
            "head": head, "badge": badge, "lines": lines}


def _sec_token(t: dict, yest: str) -> dict:
    if not t.get("ok"):
        return {"id": "token", "title": "昨日消耗", "glyph": "coin",
                "head": "采集层不可用", "badge": "warn",
                "lines": ["本机工作的采集层没读到（hostwork 模块不在或报错）"]}
    if not t["total"]:
        return {"id": "token", "title": "昨日消耗", "glyph": "coin",
                "head": "昨天没有 AI 会话", "badge": "ok",
                "lines": ["昨天这台机器上没有 AI 客户端的用量记录"]}
    money = "—"
    if t["cost"] is not None:
        money = f"{t['currency']}{t['cost']:.2f}"
    elif t["points"] is not None:
        money = f"{t['points']:.0f} 积分"
    lines = [
        f"合计 {_wan(t['total'])} token（入 {_wan(t['in'])} / 出 {_wan(t['out'])}）",
        f"{t['calls']} 次请求 · 缓存命中 "
        + (f"{t['cache_hit'] * 100:.0f}%" if t.get("cache_hit") is not None else "—"),
        f"折算 {money}",
    ]
    for r in t["models"][:3]:
        lines.append(f"· {_cut(r['model'], 26)} {_wan(r['in'] + r['out'])}"
                     + (f" ≈ {t['currency']}{r['cost']:.2f}" if r.get("cost") is not None else ""))
    if t["unpriced"]:
        lines.append("未配单价：" + "、".join(_cut(x, 18) for x in t["unpriced"]))
    head = f"{_wan(t['total'])} token · {money}"
    return {"id": "token", "title": "昨日消耗", "glyph": "coin",
            "head": head, "badge": "ok", "lines": lines}


def _sec_work(w: dict, yest: str, today: str) -> dict:
    lines = []
    if w["done"]:
        lines.append(f"昨天做了 {len(w['done'])} 件：")
        lines += ["· " + x for x in w["done"]]
    else:
        lines.append("昨天没有记录到明确的活（工作流水、会话标题都空着）")
    if w["plan"]:
        lines.append("")
        lines.append(f"今天 {len(w['plan'])} 项：")
        lines += ["· " + x for x in w["plan"]]
    else:
        lines.append("")
        lines.append("今天待办和行程都是空的")
    head = f"昨 {len(w['done'])} 件 · 今 {len(w['plan'])} 项"
    return {"id": "work", "title": "工作与计划", "glyph": "list",
            "head": head, "badge": "ok", "lines": lines}


def _sec_news(n: dict) -> dict:
    if not n["n"]:
        return {"id": "news", "title": "新闻热点", "glyph": "news",
                "head": "没有抓取产物", "badge": "warn",
                "lines": ["10-系统记录/新闻/days 里没有可用的文件"]}
    lines = []
    for i, x in enumerate(n["items"], 1):
        lines.append(f"{i}. {x['name']}")
        bits = [v for v in (x["what"], x["heat"], ("档 " + x["tier"]) if x["tier"] else "") if v]
        if bits:
            lines.append("   " + " · ".join(bits))
    head = f"{n['n']} 条 · AI 相关 {n['ai']}"
    return {"id": "news", "title": "新闻热点", "glyph": "news",
            "head": head, "badge": "ok", "lines": lines}


def _sec_follow(f: dict) -> dict:
    if not f["n"] and not f["text"]:
        return {"id": "follow", "title": "关注更新", "glyph": "follow",
                "head": "昨天没有新更新", "badge": "ok",
                "lines": ["关注的那些人昨天没发新东西（或抓取没跑）"]}
    parts = [f"{k} {v}" for k, v in sorted(f["by"].items(), key=lambda kv: -kv[1])]
    lines = []
    if parts:
        lines.append("按平台：" + " · ".join(parts))
    if f["text"]:
        if lines:
            lines.append("")
        lines.append(f["text"])
    if f["points"]:
        lines.append("")
        lines += ["· " + p for p in f["points"]]
    if f["picks"]:
        lines.append("")
        if f.get("stale"):
            lines.append("（昨天那批已被新一轮覆盖，下面是此刻最新的）")
        for p in f["picks"]:
            who = p["author"] or p["platform"]
            body = p["text"] or p["summary"]
            lines.append(f"· [{p['platform']}] {who}：{body}")
    head = (f"{f['n']} 条新动态 · {len(f['by'])} 个平台" if f["n"]
            else "昨日摘要 · 原文已翻页")
    return {"id": "follow", "title": "关注更新", "glyph": "follow",
            "head": head, "badge": "ok", "lines": lines}


# ---------------------------------------------------------------- 推送正文

_NUM = {"middle": 1, "token": 2, "work": 3, "news": 4, "follow": 5}


def render_text(sec: dict, day: str, total: int = 5) -> str:
    """一段 → 一条微信。

    第一条带日期头，其余只带「2/5」编号：五条连着进同一个会话，
    每条都写一遍日期是纯噪声，而且把 4B 的上下文白占掉。
    """
    n = _NUM.get(sec["id"], 0)
    lines = [f"{sec['title']}（{n}/{total}）· {day[5:]} {_weekday_cn(day)}"]
    lines += sec["lines"]
    txt = "\n".join(lines)
    return txt[:TEXT_MAX]


# ---------------------------------------------------------------- 对外

def brief_dir(root: Path) -> Path:
    d = root / "10-系统记录" / "简报"
    try:
        d.mkdir(parents=True, exist_ok=True)
    except OSError:
        pass
    return d


def build(day: str = "", cfg: dict | None = None, force: bool = False) -> dict:
    """算一份简报。day 是「简报属于哪一天」，内容看它的**前一天**。"""
    day = day or date.today().isoformat()
    cfg = cfg or {}
    if (not force and _MEM["data"] and _MEM["day"] == day
            and time.time() - _MEM["at"] < _MEM_TTL):
        return _MEM["data"]

    root = _root(cfg)
    out_path = brief_dir(root) / f"{day}.json"
    if not force:
        cached = _load(out_path, None)
        if isinstance(cached, dict) and cached.get("sections"):
            _MEM.update({"day": day, "at": time.time(), "data": cached})
            return cached

    yest = _yest_of(day)
    m = _middle(root, cfg)
    m["gallery_day"] = _gallery_day(root, yest)
    tok = _tokens(yest)

    sections = []
    errs = []
    for sid, fn in (("middle", lambda: _sec_middle(m, yest)),
                    ("token", lambda: _sec_token(tok, yest)),
                    ("work", lambda: _sec_work(_works(root, yest, day, tok,
                                                     m.get("payload") or {}), yest, day)),
                    ("news", lambda: _sec_news(_news(root, yest))),
                    ("follow", lambda: _sec_follow(_follow(root, yest)))):
        try:
            sections.append(fn())
        except Exception as e:             # noqa: BLE001  一段坏了不拖累其余四段
            errs.append(f"{sid}: {type(e).__name__}: {e}")
            sections.append({"id": sid, "title": sid, "glyph": "pocket",
                             "head": "这一段取数失败", "badge": "warn",
                             "lines": [f"{type(e).__name__}: {e}"[:160]]})

    for s in sections:
        s["text"] = render_text(s, day)

    data = {
        "day": day, "yest": yest, "weekday": _weekday_cn(day),
        "generated_at": datetime.now().strftime("%Y-%m-%d %H:%M:%S"),
        "sections": sections,
        "errors": errs,
    }
    try:
        out_path.write_text(json.dumps(data, ensure_ascii=False, indent=1), encoding="utf-8")
        _write_md(root, data)
    except OSError:
        pass
    _MEM.update({"day": day, "at": time.time(), "data": data})
    return data


def _write_md(root: Path, data: dict) -> None:
    lines = [f"# 简报 · {data['day']} {data['weekday']}", "",
             f"> 生成于 {data['generated_at']}；覆盖 {data['yest']} 的账。", ""]
    for s in data["sections"]:
        lines.append(f"## {s['title']} — {s['head']}")
        lines.append("")
        lines += list(s["lines"])
        lines.append("")
    (brief_dir(root) / f"{data['day']}.md").write_text("\n".join(lines), encoding="utf-8")


def days(root: Path, limit: int = 14) -> list[str]:
    d = brief_dir(root)
    try:
        out = sorted((p.stem for p in d.glob("*.json")), reverse=True)
    except OSError:
        out = []
    return out[:limit]


def days_for(cfg: dict | None = None, limit: int = 14) -> list[str]:
    """给页面拉日期下拉用：已经有简报的日子，新的在前。"""
    return days(_root(cfg or {}), limit)


def push(data: dict, cfg: dict | None = None, dry: bool = False,
         only: list[str] | None = None) -> dict:
    """逐条发微信。**一条一条发**是这个功能的前提，不是优化项。"""
    if dry:
        return {"ok": True, "dry": True,
                "sent": [{"id": s["id"], "text": s["text"]} for s in data["sections"]]}
    try:
        import wechat_push
    except Exception as e:                 # noqa: BLE001
        return {"ok": False, "error": f"wechat_push 不可用：{e}"}
    sent, fail = [], []
    for s in data["sections"]:
        if only and s["id"] not in only:
            continue
        ok = False
        try:
            ok = bool(wechat_push.send(s["text"]))
        except Exception as e:             # noqa: BLE001
            fail.append({"id": s["id"], "error": str(e)[:160]})
            continue
        if ok:
            sent.append(s["id"])
        else:
            fail.append({"id": s["id"], "error": "网关返回失败"})
        time.sleep(1.2)                    # 连着灌五条，网关那边会挤在一起
    return {"ok": not fail, "sent": sent, "fail": fail,
            "day": data.get("day")}


# ---------------------------------------------------------------- 定时推送

# 窗口而不是时刻：早上 8 点人不一定开着机。口袋是常驻进程（launchd KeepAlive），
# 只要 8–11 点之间它活着、今天还没推过，就补一次。11 点以后不再补 —— 下午才收到
# 「昨天的账」是打扰，不如等明天。
PUSH_FROM, PUSH_TO = 8, 11


def _push_state(root: Path) -> Path:
    return brief_dir(root) / "_推送.json"


def _claim(day: str, root: Path) -> bool:
    """抢「今天这条由我来推」的牌子。

    为什么要有这块牌子：口袋是 8321（HTTPS）与 8323（明文）**同一份代码两个
    uvicorn 实例**，startup 各跑一次，两个调度线程会在同一分钟同时判定「今天
    还没推」—— 结果是十条。O_CREAT|O_EXCL 是原子的：谁先建出 <日期>.push
    谁推，另一个默默退出。落盘的 _推送.json 是给人看的账，不是锁。
    """
    p = brief_dir(root) / f"{day}.push"
    try:
        fd = os.open(p, os.O_CREAT | os.O_EXCL | os.O_WRONLY)
        os.write(fd, str(int(time.time())).encode())
        os.close(fd)
        return True
    except FileExistsError:
        return False
    except OSError:
        return True          # 建不出来（权限/磁盘）就别挡着推送，宁可重一条


def _write_push_state(root: Path, day: str) -> None:
    try:
        _push_state(root).write_text(
            json.dumps({"last": day, "at": datetime.now().strftime("%Y-%m-%d %H:%M:%S")},
                       ensure_ascii=False),
            encoding="utf-8")
    except OSError:
        pass


def scheduler_loop(cfg: dict, log=None) -> None:
    """每天早上把简报逐条推出去，推过就落盘记一笔。

    为什么不用 launchd 管这件事（2026-11-21）：这套沙箱里 launchctl bootstrap
    报 EIO，装不进去；而且「8 点」在关机时根本不会触发。口袋自己就是常驻进程，
    把闹钟放进它里面最稳 —— 开机晚了，9 点、10 点进来也能补上。

    去重靠 _推送.json 的 last：进程被重启十次，今天也只推一次。
    """
    root = _root(cfg)
    while True:
        try:
            now = datetime.now()
            if PUSH_FROM <= now.hour < PUSH_TO:
                today = now.date().isoformat()
                last = (_load(_push_state(root), {}) or {}).get("last")
                if last != today and _claim(today, root):
                    data = build(today, cfg)
                    r = push(data, cfg)
                    if r.get("ok"):
                        _write_push_state(root, today)
                        if log:
                            log({"event": "brief_push", "day": today,
                                 "sent": len(r.get("sent") or [])})
                    elif log:
                        log({"event": "brief_push_fail", "day": today,
                             "fail": r.get("fail")})
        except Exception as e:             # noqa: BLE001  闹钟自己不能把进程带崩
            if log:
                log({"event": "brief_loop_error", "error": str(e)[:200]})
        time.sleep(60)


def _cfg() -> dict:
    """脚本自己跑时用的配置（手动补跑 / 排错，没有 server 的 CFG）。"""
    cfg = {"allowed_roots": [str(Path.home() / "Documents/AI项目")],
           "inbox_dir": str(Path.home() / "Documents/AI项目/07-收件箱")}
    cfg.update(_load(BASE / "config.json", {}) or {})
    return cfg


def main(argv=None) -> int:
    import argparse
    ap = argparse.ArgumentParser(description="每日简报：生成 / 打印 / 推送")
    ap.add_argument("--day", default="", help="简报属于哪一天，默认今天")
    ap.add_argument("--force", action="store_true", help="重算，不吃落盘缓存")
    ap.add_argument("--push", action="store_true", help="逐条发到微信")
    ap.add_argument("--dry", action="store_true", help="只打印将发的内容，不发")
    a = ap.parse_args(argv)
    data = build(a.day, _cfg(), force=a.force)
    if a.push or a.dry:
        r = push(data, _cfg(), dry=a.dry)
        print(json.dumps(r, ensure_ascii=False, indent=1))
        # 退出码给 launchd 看：发失败要能在日志里一眼找出来，别都记成成功
        return 0 if r.get("ok") else 1
    for s in data["sections"]:
        print(s["text"])
        print("-" * 42)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
