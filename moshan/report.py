#!/usr/bin/env python3
"""使用报告：把机器指标日聚合渲染成日报/周报/月报/季报/半年报/年报。

数据只有一处来源：`metrics.load_daily()`（10-系统记录/使用报告/_数据/日聚合.jsonl）。
原始快照只留 7 天，日聚合才是长期真源——所以这里**从不读快照**，
日报也一样，避免出现"上周的日报重算一次数字就变了"。

文件平铺在同一层，靠前缀区分粒度，不进子文件夹：
    日报-2026-11-18.md   周报-2026-W47.md   月报-2026-11.md
    季报-2026-Q4.md      半年报-2026-H2.md  年报-2026.md
排序上同前缀按日期倒序，最新的在最上面（日期一律补零，否则 11-9 会排到 11-18 后面）。

滚雪球归档：日报留 30 天、周报留 12 周，更早的删掉——它们的内容已经并进月报/季报。
月报留 36 个月，季报/半年报/年报永久。所以目录规模稳定在七八十个文件，不会失控。
"""
from __future__ import annotations

import calendar
import json
import os
import re
from datetime import date, datetime, timedelta
from pathlib import Path

import metrics

BASE = Path(__file__).resolve().parent
HUB = metrics.REPORT_DIR
INDEX_PATH = HUB / "_索引.json"

KINDS = ("day", "week", "month", "quarter", "half", "year")
PREFIX = {"day": "日报", "week": "周报", "month": "月报",
          "quarter": "季报", "half": "半年报", "year": "年报"}
NAME = {"day": "日报", "week": "周报", "month": "月报",
        "quarter": "季报", "half": "半年报", "year": "年报"}

KEEP = {"day": 30, "week": 12, "month": 36}      # 未列出的永久保留

WEEKDAY = "一二三四五六日"
BAR = "█"
_BLOCKS = "▁▂▃▄▅▆▇█"


# ---------------------------------------------------------------- 周期

def key_of(kind: str, d: date) -> str:
    if kind == "day":
        return d.isoformat()
    if kind == "week":
        y, w, _ = d.isocalendar()
        return f"{y}-W{w:02d}"
    if kind == "month":
        return f"{d.year}-{d.month:02d}"
    if kind == "quarter":
        return f"{d.year}-Q{(d.month - 1) // 3 + 1}"
    if kind == "half":
        return f"{d.year}-H{1 if d.month <= 6 else 2}"
    return str(d.year)


def span_of(kind: str, key: str) -> tuple[date, date]:
    """周期的第一天与最后一天（含）。"""
    if kind == "day":
        d = date.fromisoformat(key)
        return d, d
    if kind == "week":
        y, w = key.split("-W")
        mon = date.fromisocalendar(int(y), int(w), 1)
        return mon, mon + timedelta(days=6)
    if kind == "month":
        y, m = (int(x) for x in key.split("-"))
        return date(y, m, 1), date(y, m, calendar.monthrange(y, m)[1])
    if kind == "quarter":
        y, q = int(key[:4]), int(key[-1])
        m0 = (q - 1) * 3 + 1
        m1 = m0 + 2
        return date(y, m0, 1), date(y, m1, calendar.monthrange(y, m1)[1])
    if kind == "half":
        y, h = int(key[:4]), int(key[-1])
        m0, m1 = (1, 6) if h == 1 else (7, 12)
        return date(y, m0, 1), date(y, m1, calendar.monthrange(y, m1)[1])
    y = int(key)
    return date(y, 1, 1), date(y, 12, 31)


def title_of(kind: str, key: str) -> str:
    a, b = span_of(kind, key)
    if kind == "day":
        return f"{a.month} 月 {a.day} 日 · 周{WEEKDAY[a.weekday()]} · 使用日报"
    if kind == "week":
        return (f"第 {int(key.split('-W')[1])} 周 · "
                f"{a.month} 月 {a.day}–{b.day} 日 · 使用周报")
    if kind == "month":
        return f"{a.year} 年 {a.month} 月 · 使用月报"
    if kind == "quarter":
        return f"{a.year} 年 Q{key[-1]}（{a.month}–{b.month} 月）· 使用季报"
    if kind == "half":
        return f"{a.year} 年 {'上' if key[-1] == '1' else '下'}半年 · 使用半年报"
    return f"{a.year} 年 · 使用年报"


def file_of(kind: str, key: str) -> Path:
    return HUB / f"{PREFIX[kind]}-{key}.md"


def _shift(kind: str, d: date, n: int) -> date:
    """把 d 往前推 n 个周期，返回该周期的某一天（用来枚举历史周期）。"""
    if kind == "day":
        return d - timedelta(days=n)
    if kind == "week":
        return d - timedelta(weeks=n)
    if kind == "month":
        y, m = d.year, d.month - n
        while m <= 0:
            m += 12
            y -= 1
        return date(y, m, 1)
    if kind == "quarter":
        q = (d.month - 1) // 3 + 1 - n
        y = d.year
        while q <= 0:
            q += 4
            y -= 1
        return date(y, (q - 1) * 3 + 1, 1)
    if kind == "half":
        h = (0 if d.month <= 6 else 1) - n
        y = d.year
        while h < 0:
            h += 2
            y -= 1
        return date(y, 1 if h == 0 else 7, 1)
    return date(d.year - n, 1, 1)


def past(kind: str, today: date, back: int) -> list[str]:
    """最近 back 个「已结束」周期的 key，从近到远。

    已结束的判定：周期最后一天 < today。所以今天的日报不算已结束（要等明天），
    本月月报也要等 1 号。这样即便关机几天，开机后补出来的仍是完整周期。
    """
    out, seen = [], set()
    for i in range(1, back * 3 + 4):
        d = _shift(kind, today, i)
        k = key_of(kind, d)
        if k in seen:
            continue
        seen.add(k)
        if span_of(kind, k)[1] < today:
            out.append(k)
        if len(out) >= back:
            break
    return out


# ---------------------------------------------------------------- 数字

def hhmm(minutes: float) -> str:
    m = int(round(minutes))
    if m < 60:
        return f"{m} 分钟"
    h, m = divmod(m, 60)
    return f"{h} 小时 {m} 分" if m else f"{h} 小时"


def hours(minutes: float) -> str:
    return f"{minutes / 60:.1f}"


def bar(minutes: float, top: float, width: int = 14) -> str:
    if not top or minutes <= 0:
        return ""
    n = int(round(min(1.0, minutes / top) * width))
    return BAR * n if n else "·"


def spark(vals: list[float], hi: float | None = None) -> str:
    if not vals:
        return ""
    top = hi or max(vals) or 1
    return "".join(_BLOCKS[min(len(_BLOCKS) - 1,
                               max(0, int(round(v / top * (len(_BLOCKS) - 1)))))]
                   for v in vals)


def delta(a: float, b: float, unit: str = "", nd: int = 1) -> str:
    d = b - a
    if abs(d) < (0.05 if nd == 1 else 0.005):
        return "持平"
    arrow = "↑" if d > 0 else "↓"
    return f"{arrow} {abs(d):.{nd}f}{unit}"


# ---------------------------------------------------------------- 区间合并

def days_of(kind: str, key: str, daily: dict[str, dict]) -> list[dict]:
    a, b = span_of(kind, key)
    return [daily[d] for d in sorted(daily) if a.isoformat() <= d <= b.isoformat()]


def combine(recs: list[dict]) -> dict:
    """把若干天的聚合压成一个区间汇总。按天等权，不让开机久的那天压倒一切。"""
    if not recs:
        return {}
    n = len(recs)
    mem_avg = sum(r["mem"]["avg"] for r in recs) / n
    load_avg = sum(r["load"]["avg"] for r in recs) / n
    hourly = [0] * 24
    for r in recs:
        for i, v in enumerate(r.get("hourly") or []):
            if i < 24:
                hourly[i] += int(v or 0)
    top: dict[str, dict] = {}
    for r in recs:
        for p in r.get("top") or []:
            cur = top.setdefault(p["n"], {"n": p["n"], "sum": 0.0, "max": 0.0, "hits": 0})
            cur["sum"] += p["avg"] * max(1, p["hits"])
            cur["max"] = max(cur["max"], p["max"])
            cur["hits"] += p["hits"]
    ranked = [{"n": v["n"], "avg": round(v["sum"] / max(1, v["hits"]), 1),
               "max": round(v["max"], 1), "hits": v["hits"]}
              for v in sorted(top.values(), key=lambda x: -x["sum"])[:8]]
    busiest = max(recs, key=lambda r: r.get("awake_min") or 0)
    mem_peak = max(recs, key=lambda r: (r.get("mem") or {}).get("max") or 0)
    load_peak = max(recs, key=lambda r: (r.get("load") or {}).get("max") or 0)
    return {
        "days": n,
        "awake": sum(r.get("awake_min") or 0 for r in recs),
        "mem_avg": round(mem_avg, 1),
        "mem_max": round(max((r.get("mem") or {}).get("max") or 0 for r in recs), 1),
        "mem_max_day": mem_peak.get("date", ""),
        "load_avg": round(load_avg, 2),
        "load_max": round(max((r.get("load") or {}).get("max") or 0 for r in recs), 2),
        "load_max_day": load_peak.get("date", ""),
        "disk_start": (recs[0].get("disk") or {}).get("start") or 0,
        "disk_end": (recs[-1].get("disk") or {}).get("end") or 0,
        "min_free": min((r.get("disk") or {}).get("min_free") or 999 for r in recs),
        "boots": sum(r.get("boots") or 0 for r in recs),
        "hourly": hourly,
        "top": ranked,
        "busiest": {"date": busiest.get("date"), "awake": busiest.get("awake_min") or 0},
        "cpus": next((r.get("cpus") for r in recs if r.get("cpus")), 0) or 10,
    }


# ---------------------------------------------------------------- 产出

def _pocket_events(a: str, b: str) -> dict:
    """区间内的入库动作，数从 pocket.jsonl 现算（不落第二份）。"""
    out = {"upload": 0, "images": 0, "galleries": 0, "articles": 0, "text": 0, "chat": 0}
    p = BASE / "pocket.jsonl"
    if not p.exists():
        return out
    try:
        for ln in p.read_text(encoding="utf-8").splitlines():
            if not ln.strip():
                continue
            try:
                e = json.loads(ln)
            except ValueError:
                continue
            ts = str(e.get("ts") or "")[:10]
            if not ts or ts < a or ts > b:
                continue
            ev = e.get("event")
            if ev == "upload":
                out["upload"] += int(e.get("saved") or 0)
            elif ev == "link_done" and e.get("state") == "done":
                out["galleries"] += 1
                out["images"] += int(e.get("images") or 0)
            elif ev == "article_done" and e.get("state") == "done":
                out["articles"] += 1
            elif ev == "chat":
                out["chat"] += 1
    except OSError:
        pass
    return out


def _flow_days(a: str, b: str) -> dict:
    """工作流水：区间内记了几条、勾掉几条。文件不在就当没有。"""
    out = {"items": 0, "done": 0, "days": 0}
    p = metrics._root() / "09-个人工作台" / "流水" / "流水.json"
    if not p.exists():
        return out
    try:
        data = json.loads(p.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return out
    for day in data.get("days") or []:
        d = str(day.get("date") or "")
        if not d or d < a or d > b:
            continue
        items = day.get("items") or []
        out["days"] += 1
        out["items"] += len(items)
        out["done"] += sum(1 for it in items if it.get("done") is True)
    return out


# ---------------------------------------------------------------- 渲染片段

def _hour_table(hourly: list[int]) -> str:
    """每 3 小时一行，横条长度＝该时段活跃时长。

    不用 24 格块图：块字符和中文混排一定对不齐，表格怎么都不会歪。
    跨多天的报告里总量动辄几千分钟，单位自动换成小时。
    """
    buckets = [(hourly[i] + hourly[i + 1] + hourly[i + 2])
               for i in range(0, 24, 3)]
    top = max(buckets) or 1
    total = sum(buckets)
    unit = "小时" if total > 600 else "分钟"
    rows = ["| 时段 | 活跃 | |", "|---|---|---|"]
    for i, v in enumerate(buckets):
        span = f"{i * 3:02d}–{i * 3 + 3:02d}"
        if not v:
            rows.append(f"| {span} | — | |")
            continue
        shown = f"{v / 60:.1f}" if unit == "小时" else str(v)
        rows.append(f"| {span} | {shown} {unit} | {bar(v, top)} |")
    return "\n".join(rows)


def _top_table(top: list[dict], limit: int = 5) -> str:
    if not top:
        return "_这一段时间没有采到进程数据。_"
    rows = ["| 进程 | 均 CPU | 峰 CPU | 出现 |", "|---|---|---|---|"]
    for p in top[:limit]:
        rows.append(f"| `{p['n']}` | {p['avg']}% | {p['max']}% | {p['hits']} 次 |")
    return "\n".join(rows)


def _seg_text(segs: list[list[str]]) -> str:
    if not segs:
        return "—"
    return " · ".join(f"{a}–{b}" for a, b in segs)


def _output_block(a: str, b: str) -> str:
    ev = _pocket_events(a, b)
    fl = _flow_days(a, b)
    bits = []
    if ev["galleries"]:
        bits.append(f"存图 {ev['galleries']} 组（{ev['images']} 张）")
    if ev["articles"]:
        bits.append(f"存文章 {ev['articles']} 篇")
    if ev["upload"]:
        bits.append(f"手机传文件 {ev['upload']} 个")
    if ev["chat"]:
        bits.append(f"对话 {ev['chat']} 次")
    line1 = "、".join(bits) if bits else "这段时间没有入库动作"
    line2 = (f"工作流水 {fl['items']} 条，勾掉 {fl['done']} 条"
             f"（记了 {fl['days']} 天）" if fl["items"] else "工作流水没有记录")
    return f"- {line1}\n- {line2}"


def _anomalies(recs: list[dict]) -> list[str]:
    out = []
    hot = [r for r in recs if (r.get("mem") or {}).get("max", 0) >= 90]
    if hot:
        dates = "、".join(r["date"][5:] for r in hot[:3])
        out.append(f"内存冲到 90% 以上：{dates}" + ("（等）" if len(hot) > 3 else ""))
    tight = [r for r in recs if (r.get("disk") or {}).get("min_free", 999) < 20]
    if tight:
        out.append(f"可用磁盘低于 20 GB：{len(tight)} 天，最低 "
                   f"{min((r['disk']['min_free']) for r in tight):.1f} GB")
    load_top = sorted(recs, key=lambda r: -(r.get("load") or {}).get("max", 0))[:1]
    if load_top and (load_top[0].get("load") or {}).get("max", 0) >= 8:
        r = load_top[0]
        out.append(f"负载到过 {r['load']['max']}（{r['date'][5:]} {r['load']['max_at']}，"
                   f"{r.get('cpus') or 10} 核）")
    return out


# ---------------------------------------------------------------- 各周期

def render_day(key: str, daily: dict[str, dict]) -> str:
    rec = daily.get(key)
    if not rec:
        return ""
    mem, load, disk = rec["mem"], rec["load"], rec["disk"]
    awake = rec.get("awake_min") or 0
    span = rec.get("span_min") or 0
    # 只有「机器开着一整天、人只用了几小时」时这个比例才有信息量；
    # 96% 这种夹在括号里纯属噪音，不显示。
    ratio = int(awake * 100 / span) if span >= awake > 0 else 100
    cover = f"，占在机 {ratio}%" if ratio < 90 else ""

    live = key == date.today().isoformat()
    lines = [
        f"# {title_of('day', key)}" + ("（进行中）" if live else ""),
        "",
        f"> 活跃 **{hhmm(awake)}**（{rec['first']}–{rec['last']}{cover}）；"
        f"内存均价 {mem['avg']}%；磁盘 {disk['start']:.1f}% → {disk['end']:.1f}%。",
        "",
    ]
    if rec["n"] < 300:
        lines += [f"⚠️ 本日只有 {rec['n']} 条采样（{rec['first']} 起才有记录），"
                  f"此前的时段没采到，上面的活跃时长只代表这一段时间。", ""]
    lines += [
        "## 什么时候在用",
        "",
        f"**{_seg_text(rec.get('segs') or [])}**",
        "",
        _hour_table(rec.get("hourly") or [0] * 24),
        "",
        "## 资源",
        "",
        "| 指标 | 读数 |",
        "|---|---|",
        (f"| 内存 | 均值 {mem['avg']}% · 峰 {mem['max']}%（{mem['max_at']}）· 低 {mem['min']}% |"
         if mem["avg"] else "| 内存 | 本日没有内存采样 |"),
        (f"| 负载 | 均值 {load['avg']} · 峰 {load['max']}（{load['max_at']}，{rec.get('cpus') or 10} 核） |"
         if load["avg"] else "| 负载 | 本日没有负载采样 |"),
        f"| 磁盘 | {disk['start']:.1f}% → {disk['end']:.1f}%（{delta(disk['start'], disk['end'], ' pt')}）"
        f" · 最少剩 {disk['min_free']:.1f} GB |",
        f"| 重启 | {rec.get('boots') or 0} 次 · 样本 {rec['n']} 条 |",
        "",
        "## 占用前五",
        "",
        _top_table(rec.get("top") or []),
        "",
        "## 今天的活",
        "",
        _output_block(key, key),
    ]
    an = _anomalies([rec])
    if an:
        lines += ["", "## 需要留意", ""] + [f"- {x}" for x in an]
    return "\n".join(lines) + "\n"


def render_period(kind: str, key: str, daily: dict[str, dict]) -> str:
    recs = days_of(kind, key, daily)
    if not recs:
        return ""
    c = combine(recs)
    a, b = span_of(kind, key)
    per_day = c["awake"] / max(1, c["days"])

    head = [f"# {title_of(kind, key)}", ""]
    if kind == "week":
        head.append(f"> 有记录 {c['days']} 天，活跃合计 **{hours(c['awake'])} 小时**（日均 "
                    f"{hours(per_day)}）。内存均价 {c['mem_avg']}%，最忙是 "
                    f"{c['busiest']['date'][5:]}（{hhmm(c['busiest']['awake'])}）。")
    elif kind == "month":
        head.append(f"> {c['days']} 天有记录，活跃 **{hours(c['awake'])} 小时**，"
                    f"日均 {hours(per_day)} 小时。"
                    f"磁盘 {c['disk_start']:.1f}% → {c['disk_end']:.1f}%。")
    else:
        head.append(f"> {c['days']} 天有记录，活跃 **{hours(c['awake'])} 小时**；日均 "
                    f"{hours(per_day)} 小时。磁盘 {c['disk_start']:.1f}% → {c['disk_end']:.1f}%"
                    f"（{delta(c['disk_start'], c['disk_end'], ' pt')}）。")

    sec = {"week": "每天", "month": "日历", "quarter": "日历"}.get(kind, "每月")
    lines = head + ["", f"## {sec}", ""]

    if kind == "week":
        lines += ["| 日期 | 活跃 | 时段 | 内存峰 | 负载峰 | 磁盘 |",
                  "|---|---|---|---|---|---|"]
        for r in recs:
            d = date.fromisoformat(r["date"])
            lines.append(
                f"| {d.month}-{d.day:02d} 周{WEEKDAY[d.weekday()]} | {hours(r['awake_min'])}h | "
                f"{r['first']}–{r['last']} | {r['mem']['max']}% | {r['load']['max']} | "
                f"{r['disk']['start']:.1f}→{r['disk']['end']:.1f}% |")
    elif kind in ("month", "quarter"):
        # 日历热力：一行一周，块高＝当天在电脑前多久。
        # 对齐靠宽度互补：中文占 2 个半角宽、块字符占 1 个，所以表头 1 空格分隔、
        # 格子 2 空格分隔，两边每列都是 3 宽。用 3 空格分隔表头会整列往右漂。
        top = max((r["awake_min"] for r in recs), default=1) or 1
        by_day = {r["date"]: r for r in recs}
        lines += ["```", "         " + " ".join("一二三四五六日")]
        row_start = a - timedelta(days=a.weekday())
        while row_start <= b:
            days7 = [row_start + timedelta(days=i) for i in range(7)]
            if not any(d2.isoformat() in by_day for d2 in days7):
                row_start += timedelta(days=7)
                continue                    # 整周都没数据就不画，别留一排空点
            cells = []
            for d2 in days7:
                if d2 < a or d2 > b:
                    cells.append(" ")                     # 不属于这一期
                else:
                    r = by_day.get(d2.isoformat())
                    cells.append(spark([r["awake_min"]], top) if r else "·")
            label = row_start if row_start >= a else a
            lines.append(f"{label.month:02d}/{label.day:02d}".ljust(9) + "  ".join(cells))
            row_start += timedelta(days=7)
        lines += ["```", ""]
        lines += ["每格一天：块越高，当天在电脑前越久；`·`＝那天没开机；空白＝不在本期。", ""]
        lines += ["## 每周", "", "| 周 | 活跃 | 日均 | 内存峰 |", "|---|---|---|---|"]
        weeks: dict[str, list[dict]] = {}
        for r in recs:
            weeks.setdefault(key_of("week", date.fromisoformat(r["date"])), []).append(r)
        for wk in sorted(weeks):
            wr = weeks[wk]
            aw = sum(x["awake_min"] for x in wr)
            lines.append(f"| {wk} | {hours(aw)}h | {hours(aw / len(wr))}h | "
                         f"{max(x['mem']['max'] for x in wr)}% |")
    else:
        # 半年/年报：按月列
        lines += ["| 月份 | 有记录 | 活跃 | 日均 | 内存峰 | 磁盘 |", "|---|---|---|---|---|---|"]
        months: dict[str, list[dict]] = {}
        for r in recs:
            months.setdefault(r["date"][:7], []).append(r)
        for mk in sorted(months):
            mr = months[mk]
            aw = sum(x["awake_min"] for x in mr)
            lines.append(f"| {mk} | {len(mr)} 天 | {hours(aw)}h | {hours(aw / len(mr))}h | "
                         f"{max(x['mem']['max'] for x in mr)}% | "
                         f"{mr[0]['disk']['start']:.1f}→{mr[-1]['disk']['end']:.1f}% |")

    lines += ["", "## 一天里的分布", "", _hour_table(c["hourly"]), ""]
    lines += ["## 占用榜", "", _top_table(c["top"], 8), ""]
    lines += ["## 极值", "",
              f"- 最忙：{c['busiest']['date']}（{hhmm(c['busiest']['awake'])}）",
              f"- 内存最高：{c['mem_max']}%（{c['mem_max_day']}）",
              f"- 负载最高：{c['load_max']}（{c['load_max_day']}，{c.get('cpus') or 10} 核）",
              f"- 磁盘：{c['disk_start']:.1f}% → {c['disk_end']:.1f}%，期间最少剩 {c['min_free']:.1f} GB",
              f"- 重启：{c['boots']} 次", ""]
    lines += ["## 产出", "", _output_block(a.isoformat(), b.isoformat()), ""]

    # 与上一个同长度周期比
    prev_list = past(kind, a, 1)
    prev_key = prev_list[-1] if prev_list else ""
    if prev_key:
        pr = days_of(kind, prev_key, daily)
        if pr:
            pc = combine(pr)
            lines += ["## 对比上一期", "",
                      f"- 活跃：{hours(c['awake'])}h vs {hours(pc['awake'])}h"
                      f"（{delta(pc['awake'], c['awake'], 'h')}）",
                      f"- 日均：{hours(per_day)}h vs {hours(pc['awake'] / max(1, pc['days']))}h",
                      f"- 内存均价：{c['mem_avg']}% vs {pc['mem_avg']}%（"
                      f"{delta(pc['mem_avg'], c['mem_avg'], ' pt')}）", ""]
    an = _anomalies(recs)
    if an:
        lines += ["## 需要留意", ""] + [f"- {x}" for x in an] + [""]
    return "\n".join(lines)


def build(kind: str, key: str, daily: dict[str, dict] | None = None) -> str:
    daily = daily if daily is not None else metrics.load_daily()
    if kind == "day":
        return render_day(key, daily)
    return render_period(kind, key, daily)


# ---------------------------------------------------------------- 落盘

def write(kind: str, key: str, force: bool = False,
          daily: dict[str, dict] | None = None) -> Path | None:
    p = file_of(kind, key)
    # 已定稿的不重写；"进行中"那版允许被覆盖（跨天后要定稿）
    if p.exists() and not force and not _is_live(p):
        return None
    md = build(kind, key, daily)
    if not md.strip():
        return None
    if p.exists():
        try:
            if p.read_text(encoding="utf-8") == md:
                return None          # 每小时刷一次今天的日报，没变化就别动 mtime
        except OSError:
            pass
    HUB.mkdir(parents=True, exist_ok=True)
    p.write_text(md, encoding="utf-8")
    return p


def _is_live(p: Path) -> bool:
    """这份日报是不是「进行中」那版。

    今天的日报每小时都在刷新，文件名叫的就是今天；到第二天 due() 一看文件存在
    就直接跳过的话，它会永远停在「（进行中）」。所以判断"有没有出过"不能只看
    文件在不在，还要看它是不是还没定稿。
    """
    try:
        with p.open(encoding="utf-8") as f:
            return "（进行中）" in (f.readline() or "")
    except OSError:
        return False


def due(today: date | None = None) -> list[tuple[str, str]]:
    """该出但还没有的周期，从近到远、从细到粗。

    不用"今天是不是周一/1 号"来判断——那样关机几天就永久漏一期。
    改成看「已结束的周期里有没有缺文件」，开机后自动补齐。
    """
    today = today or date.today()
    daily = metrics.load_daily()
    plan: list[tuple[str, str]] = []
    for kind, back in (("day", metrics.KEEP_SNAPSHOT_DAYS), ("week", 5), ("month", 4),
                       ("quarter", 3), ("half", 2), ("year", 2)):
        for k in past(kind, today, back):
            if not days_of(kind, k, daily):
                continue
            p = file_of(kind, k)
            if p.exists() and not _is_live(p):
                continue
            plan.append((kind, k))
    return plan


def run(force: bool = False) -> dict:
    daily = metrics.rollup_all()
    made = []
    for kind, key in due():
        p = write(kind, key, force=force, daily=daily)
        if p:
            made.append({"kind": kind, "key": key, "file": p.name})
    # 今天的日报随时刷新（标「进行中」）；到明天 due() 会把它定稿
    today = date.today().isoformat()
    if today in daily:
        p = write("day", today, force=True, daily=daily)
        if p:
            made.append({"kind": "day", "key": today, "file": p.name, "live": True})
    removed = cleanup()
    write_index()
    return {"made": made, "removed": removed}


# ---------------------------------------------------------------- 归档与索引

def _parse(kind: str, p: Path) -> str | None:
    pat = {"day": r"^日报-(\d{4}-\d{2}-\d{2})\.md$",
           "week": r"^周报-(\d{4}-W\d{2})\.md$",
           "month": r"^月报-(\d{4}-\d{2})\.md$",
           "quarter": r"^季报-(\d{4}-Q\d)\.md$",
           "half": r"^半年报-(\d{4}-H\d)\.md$",
           "year": r"^年报-(\d{4})\.md$"}[kind]
    m = re.match(pat, p.name)
    return m.group(1) if m else None


def cleanup() -> list[str]:
    """滚雪球：日报/周报/月报只留最近 N 期，更早的删掉（内容已在更粗的报告里）。

    只删 `.md`，`_数据/` 和 `_索引.json` 不动。删掉的键会在索引里标 archived，
    这样"某天的日报在不在"有据可查，而不是凭空消失。
    """
    gone = []
    if not HUB.exists():
        return gone
    for kind, keep in KEEP.items():
        files = []
        for p in HUB.glob(f"{PREFIX[kind]}-*.md"):
            k = _parse(kind, p)
            if k:
                files.append((k, p))
        files.sort(key=lambda x: x[0], reverse=True)
        for k, p in files[keep:]:
            try:
                p.unlink()
                gone.append(p.name)
            except OSError:
                pass
    return gone


def write_index() -> Path:
    HUB.mkdir(parents=True, exist_ok=True)
    items = []
    for kind in KINDS:
        for p in HUB.glob(f"{PREFIX[kind]}-*.md"):
            k = _parse(kind, p)
            if not k:
                continue
            head = ""
            try:
                for ln in p.read_text(encoding="utf-8").splitlines():
                    s = ln.strip()
                    if s.startswith("> "):
                        head = s[2:]
                        break
            except OSError:
                pass
            items.append({"kind": kind, "key": k, "file": p.name,
                          "title": title_of(kind, k), "head": head,
                          "bytes": p.stat().st_size,
                          "at": datetime.fromtimestamp(p.stat().st_mtime)
                          .strftime("%Y-%m-%d %H:%M")})
    order = {k: i for i, k in enumerate(KINDS)}
    items.sort(key=lambda x: (order[x["kind"]], x["key"]), reverse=False)
    out = {"updated_at": datetime.now().strftime("%Y-%m-%d %H:%M:%S"),
           "count": len(items), "items": items}
    tmp = INDEX_PATH.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(out, ensure_ascii=False, indent=1), encoding="utf-8")
    os.replace(tmp, INDEX_PATH)
    return INDEX_PATH


def latest(kind: str = "day") -> Path | None:
    best = None
    for p in HUB.glob(f"{PREFIX[kind]}-*.md"):
        k = _parse(kind, p)
        if k and (best is None or k > best[0]):
            best = (k, p)
    return best[1] if best else None


if __name__ == "__main__":
    import sys
    if "--all" in sys.argv or not any(x.startswith("--") for x in sys.argv[1:]):
        print(json.dumps(run(), ensure_ascii=False, indent=2))
    if "--force" in sys.argv:
        print(json.dumps(run(force=True), ensure_ascii=False, indent=2))
    for i, a in enumerate(sys.argv):
        if a == "--build" and i + 2 < len(sys.argv):
            print(build(sys.argv[i + 1], sys.argv[i + 2])[:3000])
