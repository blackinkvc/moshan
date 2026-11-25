"""把本机已有的 agent 历史账导进中台。

为什么需要这一步：
  「本机工作」卡片上的账是**现算**的 —— 每次刷新去扫 `~/.workbuddy/projects`
  下的会话 jsonl、读 Cherry Studio 的 sqlite。源码在、数据也在时这条路最好，
  一个字节都不用存第二份。但它救不了两种场景：

  1. 换了电脑只拷数据根：源目录在新机上不存在，卡片就永远 0；
  2. 用户想在中台里**留一份**历史，而不是长期依赖那个外部目录还在。

  导入做的是把已有历史**折成基线账**写进数据根，字段与 agent 自述完全一致，
  卡片那套读取逻辑一行都不用改。

边界：
  - 只读外部目录，绝不写（改坏别人的会话库是不可接受的代价）。
  - 不搬原始会话内容，只留聚合数（轮次 / token / 模型 / 时间跨度）。
  - 重复导入先删旧的那几条再写，不堆叠。
"""

from __future__ import annotations

import json
import os
import sys
import time
from datetime import datetime
from pathlib import Path

import paths

NOTE_NAME = "Agent自述.jsonl"
SOURCE_TAG = "导入"

# 导入只认这两个来源：它们的账能直接从本机现成数据里算出来。
SOURCES = ("WorkBuddy", "Cherry Studio")


def _note_path() -> Path:
    return paths.data_root() / "_本机工作" / NOTE_NAME


def _hostwork():
    """采集层（在 ~/Documents/cuntu）。拿不到就如实说，不假装有数。"""
    try:
        import hostwork                                  # noqa: PLC0415
        return hostwork
    except Exception as exc:                             # noqa: BLE001
        print(f"[import] hostwork 不可用: {exc}", file=sys.stderr, flush=True)
        return None


def _fmt_tokens(n: int) -> str:
    if n >= 1_000_000_000:
        return f"{n / 1_000_000_000:.1f}G"
    if n >= 1_000_000:
        return f"{n / 1_000_000:.1f}M"
    if n >= 1000:
        return f"{n / 1000:.0f}k"
    return str(n)


def _day_str(ts: float) -> str:
    try:
        return datetime.fromtimestamp(float(ts)).strftime("%Y-%m-%d")
    except (ValueError, OSError, OverflowError):
        return ""


def _wb_stat(hw) -> dict | None:
    """WorkBuddy：复用采集层已经算好的聚合，days=0 表示全部历史。"""
    try:
        cfg = hw.price_config()
        rows = hw.clients_view(0, cfg) or []
    except Exception as exc:                             # noqa: BLE001
        return {"name": "WorkBuddy", "ok": False, "err": f"{type(exc).__name__}: {exc}"}
    for c in rows:
        if str(c.get("id") or "") != "workbuddy":
            continue
        t = c.get("tokens") or {}
        tin, tout = int(t.get("in") or 0), int(t.get("out") or 0)
        cached = int(t.get("cached") or 0)
        models = [str(k.get("name") or "") for k in (c.get("kids") or [])
                  if str(k.get("kind") or "") == "model" and k.get("name")]
        return {
            "name": "WorkBuddy", "ok": True,
            "sessions": int(c.get("sessions") or 0),
            "calls": int(c.get("calls") or 0),
            "asks": int(c.get("asks") or 0),
            "tin": tin, "tout": tout, "cached": cached,
            "models": models[:6],
            "last": str(c.get("last") or ""),
        }
    return {"name": "WorkBuddy", "ok": True, "sessions": 0, "calls": 0, "asks": 0,
            "tin": 0, "tout": 0, "cached": 0, "models": [], "last": ""}


def _cherry_stat() -> dict | None:
    """Cherry Studio：直接读它的 sqlite 聚合（只读打开）。"""
    try:
        import cherry_usage                              # noqa: PLC0415
        d = cherry_usage.view(0)
    except Exception as exc:                             # noqa: BLE001
        return {"name": "Cherry Studio", "ok": False, "err": f"{type(exc).__name__}: {exc}"}
    if not d.get("found"):
        return {"name": "Cherry Studio", "ok": False, "err": "本机没找到 Cherry Studio 的数据库"}
    t = d.get("tokens") or {}
    models = [str(m.get("model") or m.get("name") or "") for m in (d.get("models") or [])]
    by_day = d.get("by_day") or []
    days = sorted(str(x.get("day") or "") for x in by_day if x.get("day"))
    return {
        "name": "Cherry Studio", "ok": True,
        "sessions": 0,
        "calls": int(d.get("calls") or 0),
        "tin": int(t.get("in") or 0), "tout": int(t.get("out") or 0),
        "cached": int(t.get("cache") or 0),
        "cny": float(d.get("cny") or 0.0),
        "models": [m for m in models if m][:6],
        "last": days[-1] if days else "",
        "first": days[0] if days else "",
    }


def scan() -> dict:
    """只读扫描：返回每个来源的规模，不动磁盘。"""
    out: list[dict] = []
    hw = _hostwork()
    if hw is not None:
        wb = _wb_stat(hw)
        if wb:
            out.append(wb)
    ch = _cherry_stat()
    if ch:
        out.append(ch)
    tot_calls = sum(x.get("calls", 0) for x in out if x.get("ok"))
    tot_tok = sum(x.get("tin", 0) + x.get("tout", 0) for x in out if x.get("ok"))
    return {
        "ok": True,
        "sources": out,
        "totals": {"calls": tot_calls, "tokens": tot_tok},
        "note_path": str(_note_path()),
    }


def _summary_of(x: dict) -> str:
    bits: list[str] = []
    if x.get("sessions"):
        bits.append(f"{x['sessions']} 个会话")
    if x.get("calls"):
        bits.append(f"{x['calls']} 次调用")
    tok = int(x.get("tin") or 0) + int(x.get("tout") or 0)
    if tok:
        bits.append(f"{_fmt_tokens(tok)} token")
    if x.get("models"):
        bits.append("模型：" + "、".join(x["models"][:3]))
    if x.get("cny"):
        bits.append(f"实付 ¥{float(x['cny']):.2f}")
    span = " ~ ".join(s for s in (x.get("first") or "", x.get("last") or "") if s)
    if span:
        bits.append(span)
    return " · ".join(bits) or "没有可导入的账"


def _strip_imported(lines: list[str]) -> list[str]:
    """去掉上一次导入写的那几条，避免重复点堆成几十行。"""
    keep: list[str] = []
    for ln in lines:
        s = ln.strip()
        if not s:
            continue
        try:
            o = json.loads(s)
        except (ValueError, TypeError):
            keep.append(s)                # 认不出的行原样留着，不擅自丢别人的数据
            continue
        if isinstance(o, dict) and str(o.get("source") or "") == SOURCE_TAG:
            continue
        keep.append(s)
    return keep


def apply(names: list[str] | None = None) -> dict:
    """把扫描结果写成基线账，追加进 Agent自述.jsonl。"""
    data = scan()
    picked = [x for x in (data.get("sources") or [])
              if x.get("ok") and (not names or x.get("name") in names)]
    if not picked:
        return {"ok": False, "error": "没有可导入的来源（先看看本机有没有这两个客户端的数据库）",
                "sources": data.get("sources") or []}

    p = _note_path()
    p.parent.mkdir(parents=True, exist_ok=True)
    old = p.read_text(encoding="utf-8", errors="replace").splitlines() if p.exists() else []
    keep = _strip_imported(old)

    now = time.time()
    added: list[str] = []
    for x in picked:
        rec = {
            "at": round(now, 1),
            "agent": x["name"],
            "task": "历史数据导入",
            "summary": _summary_of(x),
            "project": "",
            "model": "、".join(x.get("models") or [])[:80],
            "tin": int(x.get("tin") or 0),
            "tout": int(x.get("tout") or 0),
            "cached": int(x.get("cached") or 0),
            "calls": int(x.get("calls") or 0),
            "source": SOURCE_TAG,
        }
        added.append(json.dumps(rec, ensure_ascii=False))

    body = "\n".join(keep + added) + "\n"
    tmp = p.with_suffix(".jsonl.tmp")
    tmp.write_text(body, encoding="utf-8")
    os.replace(tmp, p)

    return {
        "ok": True,
        "imported": [x["name"] for x in picked],
        "lines": len(added),
        "path": str(p),
        "summary": [{"name": x["name"], "text": _summary_of(x)} for x in picked],
    }
