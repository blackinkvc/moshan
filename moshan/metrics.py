#!/usr/bin/env python3
"""机器指标采集 + 日聚合：给「使用报告」攒长期数据。

与 `cuntu/syscheck.py::machine()` 同源（statvfs / vm_stat / getloadavg / ps），
字段口径保持一致——那边是「此刻看一眼」，这边是「每两分钟记一笔」。
**改采集口径要两边一起改**，否则巡检页和使用报告会给出两个数。

两级采样，按开销分开：
  light  120 秒一次：磁盘、内存、负载、uptime。只有一个 vm_stat 子进程。
  full   10 分钟一次：再加 ps 的 top5。ps 要遍历全表（约 40ms），不适合高频。

落盘两级：
  快照  10-系统记录/机器/快照.jsonl     滚动保留 7 天，供"今天/本周"细粒度看
  日聚合 10-系统记录/使用报告/_数据/日聚合.jsonl  一天一行，永久
报告全部从日聚合算——原始快照迟早会被滚动删掉，聚合才是长期真源。

为什么不把指标混进 磁盘/磁盘.jsonl：那份是总览页在读，字段只有三列，
塞进内存/负载会让总览页的重建逻辑和旧记录都对不上。新开一路，互不干扰。
"""
from __future__ import annotations

import json
import os
import re
import subprocess
import threading
import time
from datetime import date, datetime, timedelta
from pathlib import Path

BASE = Path(__file__).resolve().parent
HOME = Path.home()
import paths  # noqa: E402  （交付版：配置与数据根都走 paths）


def _root() -> Path:
    """数据根：优先取 config.json 里 inbox_dir 的父目录，退化到资料库。

    配置在状态目录，不在程序目录 —— 以前读 BASE/config.json，打包后永远读不到，
    于是每台机器都退化到写死的 ~/Documents/AI项目。
    """
    try:
        cfg = json.loads(paths.config_file().read_text(encoding="utf-8"))
        inbox = cfg.get("inbox_dir")
        if inbox:
            p = Path(inbox).expanduser()
            if p.parent.exists():
                return p.parent
    except (OSError, ValueError):
        pass
    return paths.data_root()


SYS_DIR = _root() / "10-系统记录"
SNAP_DIR = SYS_DIR / "机器"
SNAP_PATH = SNAP_DIR / "快照.jsonl"
REPORT_DIR = SYS_DIR / "使用报告"
DATA_DIR = REPORT_DIR / "_数据"
DAILY_PATH = DATA_DIR / "日聚合.jsonl"

KEEP_SNAPSHOT_DAYS = 7          # 原始快照只留一周，聚合是永久的
FULL_EVERY = 5                  # 每 5 次 light 采样做一次 full（10 分钟）

_LOCK = threading.Lock()
_TICK = {"n": 0}


# ---------------------------------------------------------------- 采集

def _run(argv: list[str], timeout: float = 5) -> str:
    try:
        r = subprocess.run(argv, capture_output=True, text=True, timeout=timeout)
        return r.stdout or ""
    except (OSError, subprocess.SubprocessError):
        return ""


def _disk() -> dict:
    try:
        # statvfs 是 Unix 专有；Windows 走 shutil.disk_usage（语义相同）
        target = str(SYS_DIR if SYS_DIR.exists() else HOME)
        if hasattr(os, "statvfs"):
            st = os.statvfs(target)
            total = st.f_blocks * st.f_frsize
            free = st.f_bavail * st.f_frsize
        else:
            import shutil as _shutil
            _u = _shutil.disk_usage(target)
            total, free = _u.total, _u.free
    except (OSError, AttributeError, ValueError):
        return {"disk_pct": 0.0, "free_gb": 0.0, "total_gb": 0.0}
    used = total - free
    gb = 1073741824.0
    return {"disk_pct": round(used * 100 / total, 1) if total else 0.0,
            "free_gb": round(free / gb, 1),
            "total_gb": round(total / gb, 1)}


def _mem() -> dict:
    """口径与 syscheck.machine 一致：active + wired + compressor。"""
    out = _run(["/usr/bin/vm_stat"], 4)
    m = re.search(r"page size of (\d+) bytes", out)
    page = int(m.group(1)) if m else 4096
    pages: dict[str, int] = {}
    for line in out.splitlines():
        mm = re.match(r"(.+?):\s+(\d+)\.", line)
        if mm:
            pages[mm.group(1).strip()] = int(mm.group(2))
    used = (pages.get("Pages active", 0) + pages.get("Pages wired down", 0)
            + pages.get("Pages occupied by compressor", 0)) * page
    mt = re.search(r"(\d+)", _run(["/usr/sbin/sysctl", "-n", "hw.memsize"], 4))
    total = int(mt.group(1)) if mt else 0
    gb = 1073741824.0
    return {"mem_pct": round(used * 100 / total, 1) if total else 0.0,
            "mem_gb": round(used / gb, 1),
            "mem_total_gb": round(total / gb, 1) if total else 0.0}


def _uptime_h() -> float:
    bt = _run(["/usr/sbin/sysctl", "-n", "kern.boottime"], 4)
    mb = re.search(r"sec = (\d+)", bt)
    if not mb:
        return 0.0
    return round(max(0, time.time() - int(mb.group(1))) / 3600, 1)


def _procs(limit: int = 5) -> list[dict]:
    """ps 按 CPU 降序取前若干，同名进程合并（Electron 常开一堆）。"""
    line_out = _run(["/bin/ps", "-A", "-o", "%cpu=,rss=,comm=", "-r"], 6)
    agg: dict[str, dict] = {}
    for line in line_out.splitlines()[:40]:
        parts = line.split(None, 2)
        if len(parts) < 3:
            continue
        try:
            cpu = float(parts[0])
            rss = int(parts[1]) * 1024
        except ValueError:
            continue
        name = Path(parts[2]).name
        if not name:
            continue
        cur = agg.setdefault(name, {"n": name, "c": 0.0, "m": 0.0})
        cur["c"] = round(cur["c"] + cpu, 1)
        cur["m"] = round(cur["m"] + rss / 1048576.0, 1)
    rows = sorted(agg.values(), key=lambda x: -x["c"])[:limit]
    return rows


def sample(full: bool = False) -> dict:
    """采一次。full=True 才跑 ps（约 40ms），120 秒一次的常规采样不跑。"""
    try:
        load = [round(x, 2) for x in os.getloadavg()]
    except (OSError, AttributeError):
        # Windows 根本没有 getloadavg，是 AttributeError 不是 OSError
        load = [0.0, 0.0, 0.0]
    rec = {"at": datetime.now().strftime("%Y-%m-%d %H:%M:%S")}
    rec.update(_disk())
    rec.update(_mem())
    rec.update({"load1": load[0], "load5": load[1], "load15": load[2],
                "cpus": os.cpu_count() or 1, "uptime_h": _uptime_h()})
    if full:
        p = _procs()
        if p:
            rec["procs"] = p
    return rec


# ---------------------------------------------------------------- 快照

def append_sample(full: bool | None = None) -> dict:
    """采一次并落快照；每 5 次自动做一次 full。返回采到的记录。"""
    with _LOCK:
        _TICK["n"] += 1
        if full is None:
            full = (_TICK["n"] % FULL_EVERY == 1)   # 第一轮就带进程，别等 10 分钟
        rec = sample(full=full)
        try:
            SNAP_DIR.mkdir(parents=True, exist_ok=True)
            with SNAP_PATH.open("a", encoding="utf-8") as f:
                f.write(json.dumps(rec, ensure_ascii=False) + "\n")
        except OSError:
            pass
        return rec


def _line_day(ln: str) -> str:
    """从一行快照里取日期，不整行 json.loads。

    不能用 ln[:10]：行首是 `{"at": "`，前 10 个字符是键名不是日期
    （这一版最早就是这么写的，结果 trim 把所有行都判成过期，滚雪球变成清空）。
    """
    i = ln.find('"at"')
    if i < 0:
        return ""
    j = ln.find('"', i + 4)
    return ln[j + 1:j + 11] if j > 0 else ""


def trim_snapshot(days: int = KEEP_SNAPSHOT_DAYS) -> int:
    """滚动删掉 N 天前的快照行，返回删掉的行数（一周一次就够）。"""
    if not SNAP_PATH.exists():
        return 0
    cut = (date.today() - timedelta(days=days)).strftime("%Y-%m-%d")
    try:
        lines = SNAP_PATH.read_text(encoding="utf-8").splitlines()
    except OSError:
        return 0
    keep, dropped = [], 0
    for ln in lines:
        if not ln.strip():
            continue
        d = _line_day(ln)
        if d and d < cut:
            dropped += 1
            continue
        keep.append(ln)
    if not dropped:
        return 0
    try:
        tmp = SNAP_PATH.with_suffix(".jsonl.tmp")
        tmp.write_text("\n".join(keep) + ("\n" if keep else ""), encoding="utf-8")
        os.replace(tmp, SNAP_PATH)
    except OSError:
        return 0
    return dropped


def load_snapshot(day: str | None = None) -> list[dict]:
    """读快照；给了 day（YYYY-MM-DD）就只返回那天。"""
    if not SNAP_PATH.exists():
        return []
    out = []
    try:
        for ln in SNAP_PATH.read_text(encoding="utf-8").splitlines():
            if not ln.strip():
                continue
            if day and _line_day(ln) != day:
                continue
            try:
                out.append(json.loads(ln))
            except ValueError:
                continue
    except OSError:
        return []
    return out


def backfill_from_disk() -> int:
    """把 磁盘/磁盘.jsonl 的旧采样并进快照，补上「报告上线之前」那一段。

    只有磁盘三项，没有内存/负载/进程——那些以前根本没落过盘，编不出来。
    所以报告里最早那天的内存/负载会是空的，这是事实，不是 bug。
    已存在的时刻不重复写，可以反复跑。
    """
    src = SYS_DIR / "磁盘" / "磁盘.jsonl"
    if not src.exists():
        return 0
    have = {r.get("at") for r in load_snapshot()}
    cut = (date.today() - timedelta(days=KEEP_SNAPSHOT_DAYS)).strftime("%Y-%m-%d")
    rows = []
    try:
        for ln in src.read_text(encoding="utf-8").splitlines():
            if not ln.strip():
                continue
            try:
                r = json.loads(ln)
            except ValueError:
                continue
            at = str(r.get("at") or "")
            if len(at) < 19 or at[:10] < cut or at in have:
                continue
            # 旧文件用 "%Y-%m-%d %H:%M:%S"，个别是 ISO 带 T，统一成一致格式
            at = f"{at[:10]} {at[11:19]}"
            if at in have:
                continue
            have.add(at)
            rows.append({"at": at,
                         "disk_pct": round(float(r.get("used_pct") or 0), 1),
                         "free_gb": round(float(r.get("free_gb") or 0), 1),
                         "total_gb": round(float(r.get("total_gb") or 0), 1),
                         "backfill": 1})
    except OSError:
        return 0
    if not rows:
        return 0
    rows.sort(key=lambda x: x["at"])
    try:
        SNAP_DIR.mkdir(parents=True, exist_ok=True)
        existing = SNAP_PATH.read_text(encoding="utf-8") if SNAP_PATH.exists() else ""
        merged = sorted(existing.splitlines() + [json.dumps(r, ensure_ascii=False) for r in rows])
        tmp = SNAP_PATH.with_suffix(".jsonl.tmp")
        tmp.write_text("\n".join(x for x in merged if x.strip()) + "\n", encoding="utf-8")
        os.replace(tmp, SNAP_PATH)
    except OSError:
        return 0
    return len(rows)


# ---------------------------------------------------------------- 日聚合

def _minutes_between(a: str, b: str) -> float:
    try:
        t1 = datetime.strptime(a, "%Y-%m-%d %H:%M:%S")
        t2 = datetime.strptime(b, "%Y-%m-%d %H:%M:%S")
    except ValueError:
        return 0.0
    return (t2 - t1).total_seconds() / 60.0


def rollup_day(day: str, snaps: list[dict] | None = None) -> dict | None:
    """把一天的快照压成一条聚合。样本不足（<2 条）返回 None。

    活跃时长靠「相邻样本间隔」推：120 秒一采，间隔 ≤20 分钟就当还在电脑前；
    更长就是真离开了（睡觉/关机——pocket 随系统睡眠停摆，样本会断）。
    这比 uptime 诚实：uptime 含睡眠，会把整夜也算成"在用"。
    段内不足 1 分钟的抖动不计，免得同一时刻的重复采样堆出假时长。
    """
    rows = snaps if snaps is not None else load_snapshot(day)
    rows = [r for r in rows if r.get("at")]
    if len(rows) < 2:
        return None
    rows.sort(key=lambda r: r["at"])

    awake = 0.0
    hourly = [0.0] * 24
    hourly_mem = [[] for _ in range(24)]
    segs_idx: list[list[int]] = []      # 存样本下标，最后再换算成时间——
                                        # 直接拼 "YYYY-MM-DD HH:MM" 会缺秒，strptime 失败
    GAP = 20.0                     # 超过 20 分钟算离开，切段

    for i, r in enumerate(rows):
        try:
            h = int(r["at"][11:13])
        except (ValueError, IndexError):
            h = -1
        if 0 <= h < 24 and r.get("mem_pct") is not None:
            hourly_mem[h].append(float(r["mem_pct"]))
        if i == 0:
            segs_idx.append([0, 0])
            continue
        dt = _minutes_between(rows[i - 1]["at"], r["at"])
        if dt > GAP:                       # 断档：另起一段
            segs_idx.append([i, i])
            continue
        if dt >= 1.0:
            awake += dt
            if 0 <= h < 24:
                hourly[h] += dt
        if segs_idx:
            segs_idx[-1][1] = i

    # 只留够长的段（>8 分钟），否则列表会被几十条碎片占满
    segs = []
    for a_i, b_i in segs_idx:
        if _minutes_between(rows[a_i]["at"], rows[b_i]["at"]) >= 8:
            segs.append([rows[a_i]["at"][11:16], rows[b_i]["at"][11:16]])
    segs = segs[:10]

    mem = [float(r["mem_pct"]) for r in rows if r.get("mem_pct") is not None]
    load = [float(r["load1"]) for r in rows if r.get("load1") is not None]
    mem_max_at = max(rows, key=lambda r: float(r.get("mem_pct") or 0))["at"][11:16]
    load_max_at = max(rows, key=lambda r: float(r.get("load1") or 0))["at"][11:16]

    # 重启判定：uptime 比上一条小 0.5 小时以上（不是抖动）就算一次开机
    boots = 0
    for i in range(1, len(rows)):
        u0 = float(rows[i - 1].get("uptime_h") or 0)
        u1 = float(rows[i].get("uptime_h") or 0)
        if u0 > 0.5 and u1 + 0.5 < u0:
            boots += 1

    # Top 进程：只有 full 采样带 procs，按出现次数加权求均 CPU
    agg: dict[str, dict] = {}
    for r in rows:
        for p in (r.get("procs") or []):
            n = p.get("n")
            if not n:
                continue
            cur = agg.setdefault(n, {"n": n, "sum": 0.0, "max": 0.0, "hits": 0})
            cur["sum"] += float(p.get("c") or 0)
            cur["max"] = max(cur["max"], float(p.get("c") or 0))
            cur["hits"] += 1
    top = [{"n": v["n"], "avg": round(v["sum"] / max(1, v["hits"]), 1),
            "max": round(v["max"], 1), "hits": v["hits"]}
           for v in sorted(agg.values(), key=lambda x: -x["sum"])[:8]]

    first = rows[0]["at"][11:16]
    last = rows[-1]["at"][11:16]
    span = max(0.0, _minutes_between(rows[0]["at"], rows[-1]["at"]))

    return {
        "date": day,
        "n": len(rows),
        "full": sum(1 for r in rows if r.get("procs")),
        "awake_min": round(awake),
        "span_min": round(span),
        "first": first,
        "last": last,
        "boots": boots,
        "segs": segs,
        "mem": {"avg": round(sum(mem) / len(mem), 1) if mem else 0.0,
                "max": round(max(mem), 1) if mem else 0.0,
                "min": round(min(mem), 1) if mem else 0.0,
                "max_at": mem_max_at if mem else ""},
        "load": {"avg": round(sum(load) / len(load), 2) if load else 0.0,
                 "max": round(max(load), 2) if load else 0.0,
                 "max_at": load_max_at if load else ""},
        "disk": {"start": round(float(rows[0].get("disk_pct") or 0), 1),
                 "end": round(float(rows[-1].get("disk_pct") or 0), 1),
                 "min_free": round(min(float(r.get("free_gb") or 0) for r in rows), 1)},
        "uptime_max": max(float(r.get("uptime_h") or 0) for r in rows),
        "cpus": int(rows[-1].get("cpus") or 0),
        "hourly": [round(x) for x in hourly],
        "hourly_mem": [round(sum(v) / len(v), 1) if v else None for v in hourly_mem],
        "top": top,
    }


def load_daily() -> dict[str, dict]:
    """读日聚合，返回 {date: rec}。"""
    if not DAILY_PATH.exists():
        return {}
    out: dict[str, dict] = {}
    try:
        for ln in DAILY_PATH.read_text(encoding="utf-8").splitlines():
            if not ln.strip():
                continue
            try:
                rec = json.loads(ln)
            except ValueError:
                continue
            d = rec.get("date")
            if d:
                out[d] = rec
    except OSError:
        return {}
    return out


def save_daily(recs: dict[str, dict]) -> None:
    """整份重写（一年 365 行，几 KB，不值得搞增量）。按日期升序。"""
    DATA_DIR.mkdir(parents=True, exist_ok=True)
    tmp = DAILY_PATH.with_suffix(".jsonl.tmp")
    with tmp.open("w", encoding="utf-8") as f:
        for d in sorted(recs):
            f.write(json.dumps(recs[d], ensure_ascii=False) + "\n")
    os.replace(tmp, DAILY_PATH)


def rollup_all(min_day: str | None = None) -> dict[str, dict]:
    """把快照里所有日子重算一遍并合并进日聚合。

    只重算快照范围内（7 天）的日子；更早的聚合原样保留——快照删了就算不了，
    但也正因为删了，那些数字才必须留在日聚合里。
    """
    recs = load_daily()
    changed = 0
    days = sorted({r["at"][:10] for r in load_snapshot() if r.get("at")})
    for d in days:
        if min_day and d < min_day:
            continue
        rec = rollup_day(d)
        if rec:
            recs[d] = rec
            changed += 1
    if changed:
        save_daily(recs)
    return recs


if __name__ == "__main__":
    import sys
    if "--sample" in sys.argv:
        print(json.dumps(append_sample(full=True), ensure_ascii=False, indent=2))
    elif "--rollup" in sys.argv:
        recs = rollup_all()
        print(json.dumps(list(recs.values())[-2:], ensure_ascii=False, indent=2))
        print(f"共 {len(recs)} 天")
    elif "--trim" in sys.argv:
        print(f"清掉 {trim_snapshot()} 行")
    else:
        print(json.dumps(sample(full=True), ensure_ascii=False, indent=2))
