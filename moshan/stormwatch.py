#!/usr/bin/env python3
"""终端风暴看门狗：把「桌面上一串黑框」变成一条能回查的记录。

为什么需要它：2026-11-25 01:13，EVO 上的终端进程内存从正常的 103MB 涨到 6673MB，
桌面同时冒出几百个窗口（TRAE 自查写下 orphan conhost/openconsole = 600），
系统卡到要重启。当时没有任何地方留下客观记录 —— 事后只能靠人回忆「好像弹了很多」。

判据只用一个数：进程采样里终端类进程（WindowsTerminal / conhost / OpenConsole /
Terminal / iTerm2）的常驻内存合计。Windows 上正常值 100–200MB，一旦反复起控制台
就会堆到 GB 级；Mac 上 Terminal.app 正常 100–300MB。取 1200MB 作门槛，
约等于「平时十倍」，误报极少，真出事一定抓得到。

记录落在两处：
  1. `_本机工作/异常.json` —— 结构化，给页面和工作台看图；
  2. `_本机工作/Agent自述.jsonl` —— 一句人话，工作台卡片直接能读到。

同一轮风暴只记一次（连续 45 分钟内的采样合并成一条，峰值取最大），
避免每 2 分钟刷一条把真记录挤下去。

用法：
  python stormwatch.py --once                  # 看本机，超阈值就记账
  python stormwatch.py --once --dry            # 只看判定，不写
  python stormwatch.py --file 采样.jsonl --host EVO-X2 --dry   # 判别人的采样
"""
from __future__ import annotations

import argparse
import json
import os
import sys
import threading
import time
from datetime import datetime
from pathlib import Path

BASE = Path(__file__).resolve().parent
if str(BASE) not in sys.path:
    sys.path.insert(0, str(BASE))
import paths  # noqa: E402

# 终端类进程：小写比对，去掉 .exe 也能匹配
TERM_NAMES = {"windowsterminal", "wt", "conhost", "openconsole",
              "terminal", "iterm2", "iterm", "alacritty", "wezterm"}
STORM_MB = 1200.0          # 合计常驻内存门槛
MERGE_SECS = 45 * 60       # 同一轮风暴的合并窗口
LOOKBACK_SECS = 3 * 3600   # 只看最近 3 小时的采样，老账不翻
MAX_ITEMS = 60             # 异常.json 只留最近 60 条

_LOCK = threading.Lock()


def own_dir() -> Path:
    return paths.ensure(Path(paths.data_root()) / "_本机工作")


def _is_term(name: str) -> bool:
    n = str(name or "").lower().strip()
    if n.endswith(".exe"):
        n = n[:-4]
    return n in TERM_NAMES


def _parse_at(s: str) -> float:
    try:
        return datetime.strptime(str(s)[:19], "%Y-%m-%d %H:%M:%S").timestamp()
    except (ValueError, TypeError):
        return 0.0


def _term_of(row: dict) -> tuple[float, list]:
    """一条采样里终端占多少 MB、都是谁。

    采样器从 2026-11-25 起会带 `terms`（全量筛出来的，空转的 conhost 也算），
    优先用它；老文件没有这个字段，退回从 top15 里凑 —— 那份必然漏掉空转的，
    所以只看老文件时数值会偏小，这是已知的、只对历史数据成立的偏差。
    """
    tm = row.get("terms")
    if isinstance(tm, dict) and tm.get("n") is not None:
        return float(tm.get("mb") or 0), list(tm.get("top") or [])
    total, detail = 0.0, []
    for it in row.get("top") or []:
        try:
            name, mem = str(it[0]), float(it[2])
        except (IndexError, TypeError, ValueError):
            continue
        if _is_term(name):
            total += mem
            detail.append([name, round(mem, 1)])
    return total, detail


def _rows(path: Path, since: float) -> list[dict]:
    """读采样文件，只要 since 之后的。文件可能几十万行，从尾部倒着读。"""
    if not path.exists():
        return []
    try:
        lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError:
        return []
    out: list[dict] = []
    for ln in reversed(lines):
        ln = ln.strip()
        if not ln:
            continue
        try:
            d = json.loads(ln)
        except ValueError:
            continue
        t = _parse_at(d.get("at") or "")
        if not t:
            continue
        if t < since:
            break                      # 倒序读，早于窗口就停
        out.append({"at": d.get("at"), "ts": t, "top": d.get("top") or [],
                    "terms": d.get("terms")})
    out.reverse()
    return out


def scan(path: Path | None = None, since: float | None = None) -> list[dict]:
    """扫一轮采样，把连续超阈值的采样归成一簇事件。

    一次事故常常是「爆发 → 关掉 → 又爆发」，中间回落过就是两件事，
    不能拿整段窗口的峰值当一条 —— 那样起止时间会拉成十几个小时，没法读。
    """
    p = path or (own_dir() / "进程.jsonl")
    since = since if since is not None else time.time() - LOOKBACK_SECS
    rows = _rows(p, since)
    hot: list[dict] = []
    for r in rows:
        total, detail = _term_of(r)
        if total >= STORM_MB:
            hot.append({"at": r["at"], "ts": r["ts"], "mb": total, "procs": detail})
    if not hot:
        return []
    clusters: list[list[dict]] = [[hot[0]]]
    for h in hot[1:]:
        if h["ts"] - clusters[-1][-1]["ts"] > MERGE_SECS:
            clusters.append([h])
        else:
            clusters[-1].append(h)
    out: list[dict] = []
    for c in clusters:
        pk = max(c, key=lambda x: x["mb"])
        out.append({
            "kind": "terminal_storm",
            "start": c[0]["at"],
            "end": c[-1]["at"],
            "peak_mb": round(pk["mb"], 1),
            "peak_at": pk["at"],
            "procs": sorted(pk["procs"], key=lambda x: -x[1])[:6],
            "samples": len(c),
        })
    return out


def _load(path: Path) -> dict:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {"items": []}


def _save(path: Path, doc: dict) -> None:
    tmp = path.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(doc, ensure_ascii=False, indent=1), encoding="utf-8")
    os.replace(tmp, path)


def record(ev: dict, host: str, note_json: Path | None = None,
           note_line: Path | None = None) -> dict:
    """写一条异常；同一 host + kind 在合并窗口内不重复记，只把峰值和收尾时间推后。"""
    own = own_dir()
    nj = note_json or (own / "异常.json")
    nl = note_line or (own / "Agent自述.jsonl")
    now = time.time()
    with _LOCK:
        doc = _load(nj)
        items = [x for x in (doc.get("items") or []) if isinstance(x, dict)]
        merged = False
        # 同一个「起点分钟」就是同一簇：看门狗每 5 分钟重扫一遍，
        # 不认这个键的话同一场风暴会被记成几十条。
        key = str(ev.get("start") or "")[:16]
        hit = None
        for x in items:
            if (x.get("host") == host and x.get("kind") == ev["kind"]
                    and str(x.get("start") or "")[:16] == key):
                hit = x
                break
        if hit is None:
            # 兜底：起点因采样边界差一两分钟时，用收尾时间贴近来认同一轮
            for x in reversed(items):
                if x.get("host") != host or x.get("kind") != ev["kind"]:
                    continue
                pe = _parse_at(x.get("end") or "")
                ne = _parse_at(ev.get("end") or "")
                if pe and ne and abs(ne - pe) <= MERGE_SECS:
                    hit = x
                break
        if hit is not None:
            hit["end"] = max(hit.get("end") or "", ev["end"])
            if ev["peak_mb"] > float(hit.get("peak_mb") or 0):
                hit["peak_mb"] = ev["peak_mb"]
                hit["peak_at"] = ev["peak_at"]
                hit["procs"] = ev["procs"]
            hit["samples"] = int(ev.get("samples") or 0)
            hit["updated_at"] = now
            merged = True
        if not merged:
            items.append({
                "host": host, "kind": ev["kind"],
                "start": ev["start"], "end": ev["end"],
                "peak_mb": ev["peak_mb"], "peak_at": ev["peak_at"],
                "procs": ev["procs"], "samples": ev.get("samples") or 0,
                "created_at": now, "updated_at": now,
            })
        items.sort(key=lambda x: str(x.get("end") or ""))
        doc = {"items": items[-MAX_ITEMS:], "updated_at": now}
        _save(nj, doc)
        if not merged:
            # 人话这一笔留给工作台卡片：它只读自述文件，读不到异常.json
            who = " · ".join(f"{n} {m:.0f}MB" for n, m in ev["procs"][:3])
            line = {
                "at": now, "agent": "stormwatch",
                "task": f"终端风暴：{host} 终端进程峰值 {ev['peak_mb']:.0f}MB",
                "summary": (f"{ev['peak_at'][5:16]} 采样里终端类进程合计 {ev['peak_mb']:.0f}MB"
                            f"（平时约 100–200MB），{who}。窗口是自动认出来的，"
                            f"不是谁事后补写的。"),
                "project": "_本机工作", "source": "stormwatch 自动",
            }
            try:
                nl.parent.mkdir(parents=True, exist_ok=True)
                with nl.open("a", encoding="utf-8") as f:
                    f.write(json.dumps(line, ensure_ascii=False) + "\n")
            except OSError:
                pass
        return {"merged": merged, "peak_mb": ev["peak_mb"],
                "count": len(doc["items"])}


def tick(host: str | None = None, dry: bool = False) -> dict:
    evs = scan()
    if not evs:
        return {"ok": True, "storm": False, "events": 0}
    if dry:
        return {"ok": True, "storm": True, "events": len(evs), "items": evs}
    h = host or _hostname()
    out = [record(ev, h) for ev in evs]
    return {"ok": True, "storm": True, "events": len(evs),
            "peaks": [o.get("peak_mb") for o in out]}


def _hostname() -> str:
    try:
        import socket
        return socket.gethostname().split(".")[0] or "本机"
    except OSError:
        return "本机"


def payload(limit: int = 8) -> dict:
    """给工作台看的那一份：最近的账 + 此刻终端占多少（卡片上要「现在是好是坏」）。"""
    own = own_dir()
    doc = _load(own / "异常.json")
    items = [x for x in (doc.get("items") or []) if isinstance(x, dict)]
    items.sort(key=lambda x: str(x.get("end") or ""), reverse=True)
    now_mb, base_mb, at = 0.0, 0.0, ""
    rows = _rows(own / "进程.jsonl", time.time() - 3 * 3600)
    if rows:
        at = rows[-1]["at"]
        now_mb = round(_term_of(rows[-1])[0], 1)
        # 基线只拿「带 terms 的采样」算：老文件没这个字段，推算出来必然是漏的，
        # 掺进来只会把基线压到 0，看着像「这台机器从来没有终端」。
        tv = [float(r["terms"].get("mb") or 0) for r in rows
              if isinstance(r.get("terms"), dict)]
        if tv:
            base_mb = round(min(tv), 1)
    return {
        "items": items[:limit],
        "total": len(items),
        "now_mb": now_mb,
        "base_mb": base_mb,
        "at": at,
        "sampled": bool(at),      # 有采样 ≠ 有终端进程：Mac 上常常一个都没有
        "host": _hostname(),
        "threshold_mb": STORM_MB,
        "updated_at": doc.get("updated_at") or 0,
    }


def scheduler_loop(cfg: dict | None = None, on_done=None,
                   interval: int = 300) -> None:
    """跟在 pocket 里的常驻线程：每 5 分钟看一次，出事才写。

    头一分钟先等一等 —— 服务刚起来时采样文件可能正在被写。
    """
    enabled = ((cfg or {}).get("stormwatch") or {}).get("enabled", True)
    if not enabled:
        return
    time.sleep(60)
    while True:
        try:
            out = tick()
            if out.get("storm") and on_done:
                on_done(out)
        except Exception:                     # 看门狗自己不能把服务带崩
            pass
        time.sleep(max(60, int(interval)))


def main() -> int:
    ap = argparse.ArgumentParser(description="终端风暴看门狗")
    ap.add_argument("--once", action="store_true", help="跑一次")
    ap.add_argument("--dry", action="store_true", help="只判定，不写文件")
    ap.add_argument("--file", type=Path, help="判别人的采样文件（默认本机进程.jsonl）")
    ap.add_argument("--host", default="", help="采样属于哪台机器")
    ap.add_argument("--since", type=float, default=0, help="只判最近 N 小时（默认 3）")
    a = ap.parse_args()
    if a.file:
        evs = scan(a.file, time.time() - (a.since or 3) * 3600)
        if not evs:
            print(json.dumps({"storm": False}, ensure_ascii=False))
            return 0
        if a.dry:
            print(json.dumps(evs, ensure_ascii=False, indent=1))
            return 0
        host = a.host or _hostname()
        print(json.dumps([record(ev, host) for ev in evs], ensure_ascii=False))
        return 0
    out = tick(dry=a.dry)
    print(json.dumps(out, ensure_ascii=False, indent=1))
    return 0


if __name__ == "__main__":
    sys.exit(main())
