#!/usr/bin/env python3
"""TRAE 工作记录采集器（纯代码，不依赖 TRAE 界面）。

TRAE 把对话正文锁在加密库里，直接解库不现实。这里走三条**看得见的证据**：

  1. 命令执行目录  ~/Library/Application Support/Trae CN/ModularData/ai-agent/command-execution/
     每条执行有 cwd / 起止时间 / 输出，按 owner 分组成一轮活，能从输出认出干了什么。
  2. 进程历史      ~/Documents/AI项目/_本机工作/进程.jsonl
     hostwork 每 5 分钟记一次各应用 CPU%。Trae CN 连续在跑就是一段工作时段，
     哪怕对话正文一条也读不到，这段「机器确实干了活」也留得下来。
  3. 截图 OCR 补录  ~/Documents/AI项目/_本机工作/TRAE任务.jsonl
     侧栏任务名本来只在 UI 上，用本地 Vision OCR 读出来存一份，按截图时间入账。
     这是补录，不伪造步骤时间：detail 里明说来源。

窗外再采一个轻量信号：CoreGraphics 窗口标题。权限足够时能拿到用户手改过的
窗口名，写进 TRAE窗口.jsonl，给进程时段当标题用。拿不到也不影响另外三路。

被 hostwork.py 调用：trae_work.items(since_ms) → /api/rounds
也可以单独跑：python trae_work.py --seed <图片> [时间] 把截图侧栏任务录进去。
"""
from __future__ import annotations

import argparse
import ctypes
import json
import math
import os
import re
import subprocess
import sys
import threading
import time
import urllib.parse
from datetime import datetime
from pathlib import Path

HOME = Path.home()
try:                                   # 数据根跟着 paths.py 走（别人机器上是别的目录）
    import paths
    OWN_DIR = paths.ensure(paths.data_root() / "_本机工作")
except Exception:                      # noqa: BLE001
    OWN_DIR = HOME / "Documents" / "AI项目" / "_本机工作"
CMD_ROOT = (HOME / "Library" / "Application Support" / "Trae CN"
            / "ModularData" / "ai-agent" / "command-execution")
PROC_PATH = OWN_DIR / "进程.jsonl"
WIN_PATH = OWN_DIR / "TRAE窗口.jsonl"
SEED_PATH = OWN_DIR / "TRAE任务.jsonl"
OCR_SWIFT = Path(__file__).resolve().parent / "bin" / "trae_ocr.swift"

GENERIC_TITLES = {"trae cn", "agent", "trae", "tr"}
_WINDOW_GAP_MS = 30 * 60 * 1000     # 命令间隔超过半小时算新一轮
_PROC_GAP_S = 15 * 60               # 进程样本间隔超过 15 分钟算新时段
_MIN_CPU = 0.2                      # 低于这个数的 CPU 当噪声

# TRAE 也有「模式/专家」这类上下文，只是叫法不同：它讲的是**代理类型**
# （solo_agent / general_purpose_task / …）。名字从两个地方来：
#   ~/.trae-cn/mcps/s_<任务名>-<hash>/<agent_type>/…   每个任务实际用过哪些代理
#   ~/.trae-cn/builtin/manifests/mapping.json          内部代号 → 中文类别
# 这比 dash.py 里硬塞的 mode=Agent 真实：每个任务用的是哪类代理，能对上号。
MCP_ROOT = HOME / ".trae-cn" / "mcps"
AGENT_MAP_FILE = (HOME / ".trae-cn" / "builtin" / "manifests" / "mapping.json")
# mapping.json 把 solo_agent→trae / solo_work_lite→work 这种内部 category
# 翻出来，再翻一层成人话。general_purpose_task 不在表里，给个直译。
_CATEGORY_CN = {"trae": "TRAE 代理", "code": "代码代理", "work": "工作代理",
                "design": "设计代理"}
_AGENT_CN = {"general_purpose_task": "通用任务"}
_AGENT_CACHE: dict[str, object] = {"mtime": -1.0, "by_task": {}}


def _ms(ts) -> str:
    try:
        return datetime.fromtimestamp(float(ts) / 1000).strftime("%Y-%m-%d %H:%M:%S")
    except (ValueError, TypeError, OSError, OverflowError):
        return ""


def _clip(s, n=320) -> str:
    s = re.sub(r"\s+", " ", str(s or "")).strip()
    return s if len(s) <= n else s[:n - 1] + "…"


def _ts_ms(txt) -> float:
    """'2026-11-22 23:46:24' → 毫秒时间戳；解析不了返回 0（比较时当空值）。"""
    try:
        return datetime.strptime(str(txt)[:19], "%Y-%m-%d %H:%M:%S").timestamp() * 1000
    except (ValueError, TypeError, OSError):
        return 0.0


def _secs(a: str, b: str) -> int | None:
    try:
        ta = datetime.strptime(a[:19], "%Y-%m-%d %H:%M:%S")
        tb = datetime.strptime(b[:19], "%Y-%m-%d %H:%M:%S")
    except (ValueError, TypeError):
        return None
    return max(0, int((tb - ta).total_seconds()))


# ---------------------------------------------------------------- 代理类型（TRAE 的「模式」）

def _agent_category() -> dict[str, str]:
    """solo_agent → trae / solo_work_lite → work，读 mapping.json，带缓存。"""
    try:
        mt = AGENT_MAP_FILE.stat().st_mtime
    except OSError:
        return {}
    if mt != _AGENT_CACHE["mtime"]:
        amap: dict[str, str] = {}
        try:
            d = json.loads(AGENT_MAP_FILE.read_text(encoding="utf-8"))
            amap = dict((d.get("directory_mapping") or {}).get("agentTypes") or {})
        except (OSError, ValueError):
            pass
        _AGENT_CACHE["cat"] = amap
        _AGENT_CACHE["mtime"] = mt
    return dict(_AGENT_CACHE.get("cat") or {})


def _agent_cn(agent_type: str) -> str:
    """内部类型 → 人话：solo_agent → TRAE 代理。"""
    at = str(agent_type or "")
    if at in _AGENT_CN:
        return _AGENT_CN[at]
    cat = _agent_category().get(at)
    if cat:
        return _CATEGORY_CN.get(cat, cat)
    # solo_work_remote / solo_design_lite 这类没精确命中，按前缀归族
    for prefix, cn in (("solo_work", "工作代理"), ("solo_design", "设计代理"),
                       ("solo_agent", "TRAE 代理"), ("general", "通用任务")):
        if at.startswith(prefix):
            return cn
    return at


def agents_by_task() -> dict[str, list[str]]:
    """{任务名: [代理类型中文名]} —— mcps 目录名形如 s_国内灰产研究-23c66f80。

    目录名末尾的 -<8位hash> 剥掉；一个任务可能同时用过几种代理，去重保序。
    """
    try:
        mt = MCP_ROOT.stat().st_mtime
    except OSError:
        return {}
    if mt != _AGENT_CACHE.get("task_mtime") or not _AGENT_CACHE.get("by_task"):
        by: dict[str, list[str]] = {}
        try:
            dirs = sorted(MCP_ROOT.glob("s_*"))
        except OSError:
            dirs = []
        for d in dirs:
            name = re.sub(r"-[\da-f]{6,12}$", "", d.name[2:])  # 去 s_ 前缀和 hash
            for sub in sorted(d.iterdir()):
                if not sub.is_dir():
                    continue
                cn = _agent_cn(sub.name)
                if cn and cn not in by.get(name, []):
                    by.setdefault(name, []).append(cn)
        _AGENT_CACHE["by_task"] = by
        _AGENT_CACHE["task_mtime"] = mt
    return dict(_AGENT_CACHE.get("by_task") or {})


def agents_for(task: str) -> list[str]:
    """按任务名找它用过的代理类型；精确名不行就用包含关系（OCR 名字长短不一）。"""
    by = agents_by_task()
    if not by or not task:
        return []
    t = str(task).strip()
    if t in by:
        return by[t]
    for name, ags in by.items():
        if t in name or name in t:
            return ags
    return []


def _item(rid: str, task: str, detail: str, start_ms: float, end_ms: float,
          children: list[dict], source: str, evidence: str = "",
          agents: list[str] | None = None) -> dict:
    """跟 dash.py 的 _req_item 同形；工作台和 8775 页面共用这一套字段。"""
    a, b = _ms(start_ms), _ms(end_ms)
    src = [{"kind": "tool", "name": "TRAE"}, {"kind": "code", "name": source}]
    kids = children[-18:]
    facets: dict[str, object] = {}
    if agents:
        facets["agent"] = agents[:4]     # TRAE 的「模式」= 代理类型，用 agent 这个 key
    # 跟 dash.py 的 _CTX_PRESETS 对齐：TRAE 讲「代理」不讲「模式」。
    ctx_labels = {"agent": "代理", "skills": "技能", "connectors": "MCP"}
    return {
        "id": "tr" + str(rid)[-10:],
        "task": _clip(task),
        "detail": _clip(detail, 160),
        "rest": _clip(evidence or detail, 160),
        "status": "ok",
        "start": a, "end": b, "secs": _secs(a, b),
        "sources": src,
        "facets": facets,
        "ctx_labels": ctx_labels,
        "client": "TRAE",
        "children": kids,
        "more_steps": max(0, len(children) - len(kids)),
        "project": source,
        "tokens": {"in": 0, "out": 0, "cached": 0, "total": 0},
    }


# ---------------------------------------------------------------- 命令执行目录

# TRAE 的 command-execution 只留最近一小批目录（实测 7 个，隔两天就滚掉），
# 而且 execution.json 里**没有命令行原文**（只有 requestDigest 哈希 + cwd + 输出）。
# 所以这里自己抄一份存档：不抄的话，那一轮活过两天只剩「几点跑过命令」。
ARCHIVE_PATH = OWN_DIR / "TRAE命令.jsonl"
_ARCH_LOCK = threading.Lock()

# git status 的这几行永远排在最前面，却完全说不清这一轮要干什么
_GIT_NOISE = ("位于分支", "on branch", "changes not staged", "changes to be committed",
              "nothing to commit", "working tree clean", "untracked files",
              'use "git', "no changes added", "head detached", "your branch is",
              "not staged for commit", "git add")

_HINT_RULES = (
    (("load averages", "processes:"), "检查本机运行状态"),
    (("mach virtual memory statistics",), "查看内存占用"),
    (("kb/t", "tps"), "查看磁盘与 CPU 排序"),
    (("pid", "%cpu", "command"), "查看进程占用"),
)
_ERR_WORDS = ("error", "fail", "denied", "not accessible", "not found", "exit=",
              "fatal:", "refused", "no such file", "permission", "traceback",
              "cannot", "unable to")
# TRAE 有时把「改文件」的输出整段落进来（diff / 源码片段），这类行当标题毫无意义
_CODE_NOISE = ("+ ", "- ", "@@", "except ", "def ", "class ", "import ", "from ",
               "return ", "} else", "try:", "with ", "while ", "print(", "self.")


def _key_line(text: str) -> str:
    """从命令输出里挑一行「能说明这轮出了什么事」的。

    以前整轮标题直接取输出第一行，于是 `git status` 的「位于分支 main」变成了
    任务名 —— 它只是碰巧最先打出来，不是这轮的目的。这里按可信度挑：
    语义报错 > 其它含 err/fail 的行 > 第一条真输出。顺手排掉三类噪声：
    git 状态、裸 JSON 错误体（`{"message":"Not Found",...}` 读起来毫无信息）、
    分隔线。
    """
    lines = [x.strip() for x in (text or "").splitlines()]
    lines = [x for x in lines
             if len(x) > 5 and not x.startswith(("---", "===", "{", "["))]
    kept = [x for x in lines
            if not any(n in x.lower() for n in _GIT_NOISE)
            and "documentation_url" not in x.lower()
            and not x.startswith(_CODE_NOISE)
            and not re.search(r"exit\s*[=:]\s*0\b", x, re.I)]
    strong = ("permission denied", "not accessible", "fatal:", "error:", "eacces",
              "modulenotfounderror", "traceback", "no such file", "command not found")
    for ln in kept:
        if any(n in ln.lower() for n in strong):
            return _clip(ln, 48)
    for ln in kept:
        if any(n in ln.lower() for n in _ERR_WORDS):
            return _clip(ln, 48)
    return _clip(kept[0], 48) if kept else ""


def _hint_from_output(text: str, cwd: str) -> str:
    """执行记录里没有命令行原文，只能从输出反推这一轮在干什么。"""
    low = (text or "").lower()
    for keys, label in _HINT_RULES:
        if all(k in low for k in keys):
            return label
    return _key_line(text) or ("TRAE 命令任务 · "
                               + (str(cwd or "").rstrip("/").split("/")[-1] or "工作目录"))


def _archive_load() -> dict[str, dict]:
    """读归档，按 executionId 索引。一行一条，几百 KB 封顶，直接读不吃亏。"""
    out: dict[str, dict] = {}
    if not ARCHIVE_PATH.exists():
        return out
    try:
        with ARCHIVE_PATH.open("r", encoding="utf-8", errors="replace") as f:
            for ln in f:
                try:
                    d = json.loads(ln)
                except ValueError:
                    continue
                k = str(d.get("id") or "")
                if k:
                    out[k] = d
    except OSError:
        pass
    return out


_MAX_ARCH = 3000


def _archive_trim() -> None:
    """归档超过 4MB 就只留最近 3000 条。

    一行一条命令（输出截到 800 字），一天几十条的话一年也就几万行；
    但不设上限的话，几年后读一次要几百毫秒，划不来。
    """
    try:
        if ARCHIVE_PATH.stat().st_size < 4 * 1024 * 1024:
            return
        lines = ARCHIVE_PATH.read_text(encoding="utf-8", errors="replace").splitlines()
        if len(lines) <= _MAX_ARCH:
            return
        tmp = ARCHIVE_PATH.with_suffix(".tmp")
        tmp.write_text("\n".join(lines[-_MAX_ARCH:]) + "\n", encoding="utf-8")
        os.replace(tmp, ARCHIVE_PATH)
    except OSError:
        pass


def _archive_sync(live: list[dict]) -> dict[str, dict]:
    """把现场新出现的执行记录抄进归档，返回「现场 ∪ 归档」。

    现场优先：同一条命令的 finishedAtMs 是执行完才补上的，先抄的那份可能是 0。
    归档里输出截到 800 字 —— 全存的话一天几十条就上兆，而前 800 字足够认事。
    """
    arch = _archive_load()
    fresh: list[dict] = []
    for r in live:
        k = str(r.get("id") or "")
        if not k:
            continue
        if k not in arch:
            fresh.append(dict(r, out=_clip(r.get("out") or "", 800)))
        arch[k] = r
    if fresh:
        with _ARCH_LOCK:
            try:
                OWN_DIR.mkdir(parents=True, exist_ok=True)
                with ARCHIVE_PATH.open("a", encoding="utf-8") as f:
                    for r in fresh:
                        f.write(json.dumps(r, ensure_ascii=False) + "\n")
                _archive_trim()
            except OSError:
                pass
    return arch


def command_rounds(since_ms: float) -> list[dict]:
    live: list[dict] = []
    try:
        files = sorted(CMD_ROOT.glob("*/execution.json")) if CMD_ROOT.exists() else []
    except OSError:
        files = []
    for f in files:
        try:
            d = json.loads(f.read_text(encoding="utf-8", errors="replace"))
            t0 = float(d.get("createdAtMs") or 0)
            t1 = float(d.get("finishedAtMs") or t0)
            out = (f.parent / "output.log").read_text(encoding="utf-8", errors="replace")
            live.append({"id": f.parent.name, "t0": t0, "t1": t1,
                         "owner": str(d.get("ownerHash") or ""),
                         "cwd": str(d.get("cwd") or ""), "out": out,
                         "exit": d.get("exitCode")})
        except (OSError, ValueError, TypeError):
            continue
    # 现场 ∪ 归档。TRAE 只留最近 7 个目录，滚掉之后归档里的 cwd/时间/输出
    # 就是唯一的证据；不这么合，一过夜这轮活就变回「TRAE 工作时段」。
    rows = [r for r in _archive_sync(live).values()
            if float(r.get("t0") or 0) >= since_ms]
    rows.sort(key=lambda x: float(x.get("t0") or 0))
    groups: list[list[dict]] = []
    for r in rows:
        cur = groups[-1] if groups else None
        if (cur and cur[-1].get("owner") == r.get("owner")
                and cur[-1].get("cwd") == r.get("cwd")
                and float(r.get("t0") or 0)
                - float(cur[-1].get("t1") or cur[-1].get("t0") or 0) < _WINDOW_GAP_MS):
            cur.append(r)
        else:
            groups.append([r])
    out: list[dict] = []
    for g in groups:
        first = g[0]
        cwd = str(first.get("cwd") or "")
        text = "\n".join(str(x.get("out") or "") for x in g)
        proj = cwd.rstrip("/").split("/")[-1] or "未记目录"
        hint = _key_line(text)
        # 标题从「输出第一行」改成「项目 · 几条命令（那轮的关键一行）」：
        # 第一行常常是 git status 的噪声，读者认不出这是哪摊活。
        title = proj + " · " + str(len(g)) + " 条命令" + ("（" + hint + "）" if hint else "")
        kids = []
        for x in g:
            one = _key_line(x.get("out") or "")
            if not one:      # 输出里没有能读的行，就退回退出码，别写「无输出」
                code = x.get("exit")
                one = "执行成功（无输出）" if str(code) in ("0", "None") \
                    else "退出码 " + str(code)
            kids.append({"start": _ms(x.get("t0")),
                         "detail": _clip("执行完成 · " + one, 48)})
        detail = "{} 条命令 · 目录 {} · 输出 {} 字".format(len(g), proj, len(text))
        t0 = float(first.get("t0") or 0)
        t1 = max(float(x.get("t1") or x.get("t0") or 0) for x in g)
        out.append(_item("cmd" + str(first.get("id") or str(int(t0)))[-12:],
                         title, detail, t0, t1, kids,
                         "命令记录",
                         "TRAE 不存命令行原文（只有 requestDigest 哈希），这轮只能从输出"
                         "反推；工作目录 " + (cwd or "未记录"),
                         agents_for(proj) or agents_for(hint)))
    return out


# ---------------------------------------------------------------- 进程证据

def _is_trae(name: str) -> bool:
    return "trae" in str(name or "").lower()


def process_rounds(since_ms: float) -> list[dict]:
    if not PROC_PATH.exists():
        return []
    samples: list[dict] = []
    try:
        with PROC_PATH.open("r", encoding="utf-8", errors="replace") as f:
            for ln in f:
                if '"at"' not in ln or "Trae" not in ln and "trae" not in ln:
                    continue
                try:
                    d = json.loads(ln)
                except ValueError:
                    continue
                try:
                    ts = datetime.strptime(d.get("at") or "", "%Y-%m-%d %H:%M:%S").timestamp()
                except ValueError:
                    continue
                if ts * 1000 < since_ms:
                    continue
                hit = None
                for row in (d.get("top") or []):
                    if isinstance(row, list) and len(row) >= 3 and _is_trae(row[0]):
                        hit = row
                        break
                if hit:
                    samples.append({"ts": ts, "cpu": float(hit[1] or 0),
                                    "mem": float(hit[2] or 0),
                                    "all_cpu": float(d.get("cpu_pct") or 0)})
    except OSError:
        return []
    groups: list[list[dict]] = []
    for s in samples:
        cur = groups[-1] if groups else None
        if cur and s["ts"] - cur[-1]["ts"] <= _PROC_GAP_S:
            cur.append(s)
        else:
            groups.append([s])
    out: list[dict] = []
    for g in groups:
        active = [x for x in g if x["cpu"] >= _MIN_CPU]
        if not active:
            continue
        t0, t1 = g[0]["ts"] * 1000, g[-1]["ts"] * 1000
        title = _title_for(t0, t1) or "TRAE 工作时段"
        cpus = [x["cpu"] for x in g]
        mems = [x["mem"] for x in g]
        kids = [{"start": _ms(x["ts"] * 1000),
                 "detail": _clip(f"CPU {x['cpu']:.1f}% · 内存 {x['mem']:.0f}MB", 48)}
                for x in g]
        detail = (f"{len(g)} 个采样 · CPU {min(cpus):.1f}–{max(cpus):.1f}% · "
                  f"内存 {min(mems):.0f}–{max(mems):.0f}MB")
        out.append(_item("proc" + str(int(t0))[-10:], title, detail, t0, t1, kids,
                         "进程证据", f"进程实测：{detail}；采样间隔 5 分钟",
                         agents_for(title)))
    return out


def _title_for(t0_ms: float, t1_ms: float) -> str:
    """窗口标题是可选增强：有就用，没有就把进程时段留给 OCR/命令证据去命名。"""
    if not WIN_PATH.exists():
        return ""
    try:
        with WIN_PATH.open("r", encoding="utf-8", errors="replace") as f:
            for ln in reversed(f.readlines()[-200:]):
                try:
                    d = json.loads(ln)
                except ValueError:
                    continue
                t = float(d.get("at") or 0) * 1000
                if t0_ms - 10 * 60 * 1000 <= t <= t1_ms + 10 * 60 * 1000:
                    for name in (d.get("titles") or []):
                        if str(name).lower().strip() not in GENERIC_TITLES:
                            return _clip(name, 80)
    except OSError:
        pass
    return ""


# ---------------------------------------------------------------- 窗口标题采样

_cg = _cf = None


def _load_coregraphics():
    global _cg, _cf
    if _cg is not None:
        return _cg, _cf
    try:
        _cg = ctypes.CDLL("/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics")
        _cf = ctypes.CDLL("/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation")
        _cg.CGWindowListCopyWindowInfo.restype = ctypes.c_void_p
        _cg.CGWindowListCopyWindowInfo.argtypes = [ctypes.c_uint32, ctypes.c_uint32]
        _cf.CFArrayGetCount.argtypes = [ctypes.c_void_p]
        _cf.CFArrayGetCount.restype = ctypes.c_long
        _cf.CFArrayGetValueAtIndex.argtypes = [ctypes.c_void_p, ctypes.c_long]
        _cf.CFArrayGetValueAtIndex.restype = ctypes.c_void_p
        _cf.CFStringCreateWithCString.restype = ctypes.c_void_p
        _cf.CFStringCreateWithCString.argtypes = [ctypes.c_void_p, ctypes.c_char_p,
                                                  ctypes.c_uint32]
        _cf.CFDictionaryGetValue.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
        _cf.CFDictionaryGetValue.restype = ctypes.c_void_p
        _cf.CFStringGetCString.argtypes = [ctypes.c_void_p, ctypes.c_char_p,
                                           ctypes.c_long, ctypes.c_uint32]
        _cf.CFStringGetCString.restype = ctypes.c_bool
    except OSError:
        _cg = False
    return _cg, _cf


def window_titles() -> list[str]:
    cg, cf = _load_coregraphics()
    if not cg or not cf:
        return []
    enc = 0x08000100  # kCFStringEncodingUTF8

    def cs(v) -> str:
        if not v:
            return ""
        b = ctypes.create_string_buffer(2048)
        return b.value.decode("utf-8", "replace") if cf.CFStringGetCString(v, b, 2048, enc) else ""

    try:
        arr = cg.CGWindowListCopyWindowInfo(1 | 2, 0)   # on-screen, no desktop
        if not arr:
            return []
        out = []
        for i in range(cf.CFArrayGetCount(arr)):
            d = cf.CFArrayGetValueAtIndex(arr, i)
            own = cs(cf.CFDictionaryGetValue(
                d, cf.CFStringCreateWithCString(None, b"kCGWindowOwnerName", enc)))
            if not _is_trae(own):
                continue
            name = cs(cf.CFDictionaryGetValue(
                d, cf.CFStringCreateWithCString(None, b"kCGWindowName", enc))).strip()
            if name and name.lower() not in GENERIC_TITLES:
                out.append(name)
        return out
    except Exception:                                   # noqa: BLE001
        return []


_LAST_WIN: dict[str, object] = {"key": "", "at": 0.0}


def record_window(rec: dict | None = None) -> None:
    """每 10 秒被 hostwork 采样线程调一次；只在标题变化时落盘，不写噪声。"""
    names = window_titles()
    key = "|".join(names)
    now = time.time()
    if key == _LAST_WIN["key"] and now - float(_LAST_WIN["at"] or 0) < 300:
        return
    _LAST_WIN.update(key=key, at=now)
    apps = (rec or {}).get("apps") or []
    trae = next((a for a in apps if _is_trae(a.get("name"))), {}) if apps else {}
    line = json.dumps({"at": round(now, 1), "titles": names,
                       "cpu": trae.get("cpu", 0), "mem": trae.get("mem", 0)},
                      ensure_ascii=False)
    try:
        OWN_DIR.mkdir(parents=True, exist_ok=True)
        with WIN_PATH.open("a", encoding="utf-8") as f:
            f.write(line + "\n")
    except OSError:
        pass


# ---------------------------------------------------------------- 截图 OCR 补录

def seed_from_image(image: Path, at_ts: float | None = None, dedupe: bool = True) -> int:
    """读截图左侧任务栏，把任务名补录到 TRAE任务.jsonl。返回新增条数。"""
    if not OCR_SWIFT.exists():
        raise RuntimeError(f"缺 OCR 脚本：{OCR_SWIFT}")
    at = float(at_ts or time.time())
    try:
        r = subprocess.run(["swift", str(OCR_SWIFT), str(image)],
                           capture_output=True, text=True, timeout=180)
    except (OSError, subprocess.SubprocessError) as e:
        raise RuntimeError(f"OCR 跑不起来：{e}") from e
    names: list[str] = []
    skip = {"新建任务", "加载更多", "默认～", "默认", "tr ae 宝"}
    for ln in (r.stdout or "").splitlines():
        parts = ln.split("\t")
        if len(parts) < 5:
            continue
        try:
            x, y = float(parts[0]), float(parts[1])
        except ValueError:
            continue
        if x > 0.2 or not (0.04 < y < 0.96):   # 只取左侧任务栏，右上正文不算
            continue
        # 图标 OCR 常粘一个字母/方框在标题前；中文标题不能乱剔（「国内灰产」的「国」
        # 就是被这条规则吃掉过一次），所以中文只去符号，ASCII 才去单字母前缀。
        s = re.sub(r"^[\s④口〇•\-—\[［]+", "", parts[4]).strip()
        s = re.sub(r"^[A-Za-z]\s+(?=[A-Za-z0-9/])", "", s).strip()
        s = re.sub(r"\s*⋯+$", "", s).strip()
        if len(s) < 3 or s in skip or s.lower() in GENERIC_TITLES:
            continue
        if s not in names:
            names.append(s)
    if not names:
        return 0
    existing: set[str] = set()
    if dedupe and SEED_PATH.exists():
        try:
            for ln in SEED_PATH.read_text(encoding="utf-8", errors="replace").splitlines():
                d = json.loads(ln)
                existing.add(str(d.get("task") or ""))
        except (OSError, ValueError):
            pass
    added = 0
    try:
        OWN_DIR.mkdir(parents=True, exist_ok=True)
        with SEED_PATH.open("a", encoding="utf-8") as f:
            for i, name in enumerate(names):
                if dedupe and name in existing:
                    continue
                f.write(json.dumps({"at": at, "task": name, "seq": i,
                                    "source": "截图 OCR", "image": str(image)},
                                   ensure_ascii=False) + "\n")
                added += 1
    except OSError:
        return 0
    return added


def seed_items(since_ms: float) -> list[dict]:
    if not SEED_PATH.exists():
        return []
    out = []
    try:
        lines = SEED_PATH.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError:
        return []
    for ln in lines:
        try:
            d = json.loads(ln)
            t = float(d.get("at") or 0) * 1000
        except (ValueError, TypeError):
            continue
        if t < since_ms:
            continue
        task = str(d.get("task") or "").strip()
        if not task:
            continue
        kids = [{"start": _ms(t), "detail": _clip(d.get("source") or "截图补录", 48)}]
        out.append(_item("ocr" + str(int(t))[-6:] + str(d.get("seq") or 0),
                         task, f"截图 OCR 补录 · 序号 {int(d.get('seq') or 0) + 1}",
                         t, t, kids, "截图补录",
                         f"从 TRAE 侧栏截图识别；原图 {Path(d.get('image') or '').name}",
                         agents_for(task)))
    return out


STATE_DB = (HOME / "Library" / "Application Support" / "Trae CN"
            / "User" / "globalStorage" / "state.vscdb")
SNAP_ROOT = (HOME / "Library" / "Application Support" / "Trae CN"
             / "ModularData" / "ai-agent" / "snapshot")
_SESS_CACHE: dict[str, object] = {"at": 0.0, "rows": []}


def _id_day(sid: str) -> str:
    """TRAE 的会话 id 前 8 位是十六进制的秒级时间戳（6ad08a90 → 2026-10-16）。

    state.vscdb 里只留了「这个会话用哪个目录」，没留起止时间；但 id 自己是
    带时间的，解出来就够把它排到某一天 —— 比全塞进「历史任务」强。
    解出来不在 2015–2035 之间的当不是时间戳（老格式 id 会落到 1993 这种）。
    """
    h = str(sid or "")[:8]
    try:
        ts = int(h, 16)
    except ValueError:
        return ""
    if not (1420070400 <= ts <= 2051222400):
        return ""
    return datetime.fromtimestamp(ts).strftime("%Y-%m-%d")


def _sid_ts(sid: str) -> float:
    """会话 id 前 8 位十六进制 = 这个会话**开启的秒级时刻**（6b030e44 → 23:46:12）。

    _id_day 只取到日期；补一段「TRAE 在场时间」需要精确到秒，所以再解一次。
    mcps 目录那种 s_<名字>-<hash> 解不出合法时间，返回 0 表示不参与。
    """
    h = str(sid or "")[:8]
    try:
        ts = int(h, 16)
    except ValueError:
        return 0.0
    if not (1420070400 <= ts <= 2051222400):
        return 0.0
    return float(ts)


_STATE_CACHE: dict[str, object] = {"at": 0.0, "rows": []}


def _state_rows() -> list[dict]:
    """从 state.vscdb 里读「会话 → 工作区」。明文库，但 TRAE 可能在写。

    复制一份再读（0.9MB，几十毫秒）：直接开原库碰上 WAL 锁会让整张任务
    清单转圈，不值当。60 秒缓存 —— 会话—目录的映射不会秒级变化，而这段
    现在有两个调用方（任务清单、会话在场时段），不缓存等于一页刷两次读两遍。
    """
    now = time.time()
    cached = _STATE_CACHE.get("rows")
    if cached and now - float(_STATE_CACHE.get("at") or 0) < 60:
        return list(cached)                            # type: ignore[arg-type]
    if not STATE_DB.exists():
        return []
    tmp = None
    try:
        import shutil
        import sqlite3
        import tempfile
        fd, tmp = tempfile.mkstemp(suffix=".vscdb")
        os.close(fd)
        shutil.copy2(STATE_DB, tmp)
        c = sqlite3.connect(f"file:{tmp}?mode=ro", uri=True, timeout=2.0)
        rows = c.execute("select key, value from ItemTable where key like"
                         " '%content-map%' or key like '%local-project-folders%'"
                         " or key = 'solo-lite.project-workspace-cache'"
                         ).fetchall()
        c.close()
    except Exception:                                  # noqa: BLE001
        return []
    finally:
        if tmp:
            try:
                os.unlink(tmp)
            except OSError:
                pass
    out: list[dict] = []
    for k, v in rows:
        try:
            data = json.loads(v or "{}")
        except ValueError:
            continue
        if not isinstance(data, dict):
            continue
        if "local-project-folders" in k:
            # {会话id: 项目目录}
            for sid, path in data.items():
                out.append({"sid": str(sid), "path": str(path or ""), "dir": ""})
            continue
        if "project-workspace-cache" in k:
            # {会话id: {identity: 绝对路径, folders: [...], name: 项目名}}
            # 比 content-map 多了 name，且能覆盖 content-map 已被清掉的老会话
            for sid, row in data.items():
                if not isinstance(row, dict):
                    continue
                path = str(row.get("identity") or "")
                if not path:
                    folders = row.get("folders") or []
                    path = str(folders[0]) if folders else ""
                out.append({"sid": str(sid), "path": path, "dir": ""})
            continue
        # content-map: {会话id: {uri: file-workspace:?file%3D…}}
        for sid, row in data.items():
            uri = str((row or {}).get("uri") or "") if isinstance(row, dict) else ""
            out.append({"sid": str(sid), "path": "", "dir": uri})
    _STATE_CACHE.update(at=now, rows=out)
    return out


def _uri_path(uri: str) -> str:
    """file-workspace:?file%3Dfile%253A%252F… 这种套了两层转义的地址 → 本地路径。"""
    q = uri.split("?", 1)[1] if "?" in uri else ""
    f = urllib.parse.parse_qs(urllib.parse.unquote(q)).get("file", [""])[0]
    p = urllib.parse.urlparse(f).path if f.startswith(("file:", "remote-sandbox:")) else f
    return urllib.parse.unquote(p)


def _snap_days() -> dict[str, dict]:
    """{会话id: {from, to}} —— ModularData/ai-agent/snapshot/<sid> 是会话工作区
    的快照仓库，git 提交日期比目录 mtime 准（mtime 被任何一次写入刷掉）。"""
    try:
        dirs = [d for d in SNAP_ROOT.iterdir() if d.is_dir()]
    except OSError:
        return {}
    out: dict[str, dict] = {}
    for d in dirs:
        one = ""
        try:
            one = datetime.fromtimestamp(d.stat().st_mtime).strftime("%Y-%m-%d")
        except OSError:
            pass
        first = last = one
        g = d / "v2"
        if (g / ".git").exists():
            try:
                r = subprocess.run(
                    ["git", "-C", str(g), "log", "--format=%ad", "--date=short"],
                    capture_output=True, text=True, timeout=4)
                ds = [x.strip() for x in (r.stdout or "").splitlines() if x.strip()]
                if ds:
                    first, last = min(ds), max(ds)
            except (OSError, subprocess.SubprocessError):
                pass
        out[d.name] = {"from": first, "to": last}
    return out


def _snap_ts() -> dict[str, float]:
    """{会话id: 快照目录最后一次写入的秒级时间戳}。

    snapshot/<sid> 是 TRAE 给会话工作区做的快照仓，每轮对话都会动它。
    目录 mtime 就是「这个会话最后一次活动」的下界 —— 比 git log 的日期细到秒，
    而且不用对 37 个目录挨个跑 subprocess。
    """
    try:
        dirs = [d for d in SNAP_ROOT.iterdir() if d.is_dir()]
    except OSError:
        return {}
    out: dict[str, float] = {}
    for d in dirs:
        try:
            out[d.name] = d.stat().st_mtime
        except OSError:
            pass
    return out


def session_projects() -> list[dict]:
    """TRAE 会话 → 项目名 + 起止日，能对上多少算多少。

    对话正文在 SQLCipher 库里，读不出来；但「哪个会话开在哪个目录」是明文
    （state.vscdb），会话 id 前半段又带时间。三板斧拼起来：目录名当任务名、
    id 解日期、快照仓库的 git 提交把它拉成一段。剩下认不出名字的，仍然走
    OCR 那条老路（进「历史任务」），不硬编名字。
    """
    now = time.time()
    if now - float(_SESS_CACHE.get("at") or 0) < 120 and _SESS_CACHE.get("rows"):
        return list(_SESS_CACHE["rows"])
    snap = _snap_days()
    rows: list[dict] = []
    for r in _state_rows():
        sid = r["sid"]
        path = r["path"] or _uri_path(r["dir"])
        name = ""
        if path and "/" in path:
            p = Path(path)
            name = p.stem if p.suffix and p.stem else p.name
        if not name or name.startswith(("remote-sandbox", ".")):
            continue
        if len(name) > 60 or name in ("assistant", "temp") or name.startswith("temp_"):
            continue
        d = _id_day(sid)
        s = snap.get(sid) or {}
        f = s.get("from") or d
        t = s.get("to") or d
        rows.append({"task": name, "path": path, "sid": sid,
                     "from": f, "to": t, "source": "项目会话"})
    # mcps 目录：s_<任务名>-<hash>，目录 mtime = 这个任务最后一次用代理
    try:
        for d in sorted(MCP_ROOT.glob("s_*")):
            name = re.sub(r"-[\da-f]{6,12}$", "", d.name[2:]).strip()
            if not name:
                continue
            try:
                day = datetime.fromtimestamp(d.stat().st_mtime).strftime("%Y-%m-%d")
            except OSError:
                day = ""
            rows.append({"task": name, "path": "", "sid": d.name,
                         "from": day, "to": day, "source": "代理缓存"})
    except OSError:
        pass
    # 同名并成一条：起止取最宽那一段，来源按可信度排序（项目会话 > 代理缓存）
    merged: dict[str, dict] = {}
    for r in rows:
        old = merged.get(r["task"])
        if old is None:
            merged[r["task"]] = dict(r)
            continue
        if r["from"] and (not old["from"] or r["from"] < old["from"]):
            old["from"] = r["from"]
        if r["to"] and (not old["to"] or r["to"] > old["to"]):
            old["to"] = r["to"]
        if r.get("source") == "项目会话":
            old["source"] = "项目会话"
    out = sorted(merged.values(), key=lambda r: (r.get("to") or "", r.get("task") or ""),
                 reverse=True)
    _SESS_CACHE.update(at=now, rows=out)
    return list(out)


def _snap_for(sid: str, snap: dict[str, float]) -> float:
    """这个会话的快照末次写入时刻。

    snapshot 目录名用的是 workspace id，state.vscdb 里是 chat session id，
    两者前 7 位相同、后两位会差（今晚就是 6b030e44… 对 6b030e42…）。
    先精确匹配，不中再按前缀找 —— 硬匹配不上会让「在场时长」永远算成 1 分钟。
    """
    if sid in snap:
        return snap[sid]
    key = str(sid or "")[:7]
    if not key:
        return 0.0
    hits = [v for k, v in snap.items() if k[:7] == key]
    return max(hits) if hits else 0.0


def session_rounds(since_ms: float, cmd_rows: list[dict]) -> list[dict]:
    """TRAE 开着、但没跑命令的那些时段：按「会话 → 项目目录」补一行在场记录。

    纯生成代码、翻文件、聊天时没有命令记录，进程采样又只说得出「TRAE 在跑」，
    于是那段时间在卡片上等于消失。这里用会话 id 解出的开启时刻 + 工作区快照的
    末次写入，拼出一段在场的起止，标题用项目目录名 —— 真正的任务名在加密库里，
    拿不到就不编。命令记录已经覆盖了同一项目的，不另起一行（否则同一摊活两行）。
    """
    snap = _snap_ts()
    by_proj: dict[str, dict] = {}
    for r in _state_rows():
        sid = str(r.get("sid") or "")
        t0 = _sid_ts(sid)
        if not t0 or t0 * 1000 < since_ms:
            continue
        path = str(r.get("path") or "") or _uri_path(str(r.get("dir") or ""))
        if not path or "/" not in path:
            continue
        name = Path(path).name
        if (not name or len(name) > 60 or name.startswith(("remote-sandbox", "."))
                or name in ("assistant", "temp") or name.startswith("temp_")):
            continue
        # 「最近打开」里混着文件（某个 zip 交付包就是这么冒出来的），那不是工作目录
        if re.search(r"\.(zip|png|jpe?g|gif|pdf|md|txt|json|py|html?|css|js|"
                     r"xlsx?|docx?|pptx?|mp4|mov|dmg)$", name, re.I):
            continue
        t1 = max(_snap_for(sid, snap) or t0, t0)
        cur = by_proj.get(name)
        if cur is None:
            by_proj[name] = {"t0": t0, "t1": t1, "n": 1}
        else:
            cur["t0"] = min(cur["t0"], t0)
            cur["t1"] = max(cur["t1"], t1)
            cur["n"] += 1
    out: list[dict] = []
    # 命令与「会话在场」常常只差几十秒（快照先写、命令后跑），严格按区间重叠判重
    # 会漏掉，于是同一摊活在卡片上出现两行。前后各放宽 30 分钟当同一段。
    pad = 30 * 60 * 1000
    for name, s in by_proj.items():
        s0, s1 = s["t0"] * 1000, s["t1"] * 1000
        covered = False
        for c in cmd_rows:
            if name not in str(c.get("task") or ""):
                continue
            cs = _ts_ms(c.get("start"))
            ce = _ts_ms(c.get("end") or c.get("start")) or cs
            if cs <= s1 + pad and s0 - pad <= ce:
                covered = True
                break
        if covered:
            continue
        span = int((s["t1"] - s["t0"]) // 60)
        # 快照没对上（老会话、或 TRAE 没给它做快照）时别硬写「在场约 1 分钟」——
        # 那是我不知道，不是它只待了一分钟。
        detail = ("TRAE 会话 · 在场约 " + str(span) + " 分钟" if span >= 2
                  else "TRAE 会话 · 开启于 " + _ms(s["t0"] * 1000)[11:16])
        out.append(_item("ses" + str(int(s["t0"]))[-10:], name, detail,
                         s["t0"] * 1000, s["t1"] * 1000, [],
                         "会话活跃",
                         "开启时刻取自会话 id 前缀，末次活动取自工作区快照；"
                         "对话正文在加密库里，只能证明在这个项目待过，说不出具体做了什么"))
    out.sort(key=lambda x: str(x.get("end") or ""), reverse=True)
    return out


def task_rows(since_ms: float) -> list[dict]:
    """OCR 补录的原始「任务/项目名」清单，给 /api/tasks 的任务清单卡用。

    跟 seed_items 的区别：seed_items 把每条伪装成一轮执行记录（带 start/end/
    children），这里只回任务名 + 时间 + 来源，明确是「有哪些任务」而非「做了什么」。
    """
    if not SEED_PATH.exists():
        return []
    out: list[dict] = []
    try:
        lines = SEED_PATH.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError:
        return []
    for ln in lines:
        try:
            d = json.loads(ln)
            t = float(d.get("at") or 0) * 1000
        except (ValueError, TypeError):
            continue
        if t < since_ms:
            continue
        task = str(d.get("task") or "").strip()
        if not task:
            continue
        out.append({"task": task, "at": _ms(t),
                    "date": _ms(t)[:10],
                    "source": str(d.get("source") or "截图 OCR"),
                    "image": Path(d.get("image") or "").name})
    # 同一张截图里按 seq 顺序（文件书写顺序）展示，不按时间倒序——它们时间是同一刻
    return out


# ---------------------------------------------------------------- 汇总

# ---------------------------------------------------------------- TRAE 自述（记忆 / 主动上报）
#
# 2026-11-23：TRAE 内核自己一直在写会话记忆，明文，字段比截图 OCR 全得多：
#   ~/.trae-cn/memory/projects/<项目>/<YYYYMMDD>/session_memory_<会话id>.jsonl
#   {"intent": "...", "actions": [...], "outcome": "...", "learned": [...],
#    "message_summary_time": "2026-11-22 23:55:41", "message_id": "..."}
# intent 就是「这轮要干什么」，outcome 是「结果如何」。这是唯一不用切窗口、
# 不用 OCR、也不依赖 TRAE 配合就能拿到的意图层。命令记录只管证据，两者互补。
MEM_ROOT = HOME / ".trae-cn" / "memory" / "projects"
NOTE_PATH = OWN_DIR / "TRAE自述.jsonl"          # skill 让 TRAE 收尾时补的那笔
_MEM_CACHE: dict[str, object] = {"at": 0.0, "rows": []}
_MEM_TTL = 60.0
_MEM_MERGE_MS = 90 * 60 * 1000     # 记忆时刻前后 90 分钟内的命令算同一轮


def _ep(s) -> float:
    """「YYYY-MM-DD HH:MM:SS」→ 毫秒时间戳；解析不了给 0。"""
    try:
        return datetime.strptime(str(s)[:19], "%Y-%m-%d %H:%M:%S").timestamp() * 1000
    except (ValueError, TypeError):
        return 0.0


def _proj_from_memdir(name: str) -> str:
    """-Volumes-AI---Moshan--p2-<hash> → Moshan。

    中文路径段被换成了一串 `-`（不可逆），只能取最后一个 ASCII 段；
    取不到就回 TRAE，不瞎猜。
    """
    head = name.split("--p2-")[0]
    segs = [s for s in head.split("-") if s]
    if not segs:
        return "TRAE"
    last = segs[-1]
    return last if len(last) >= 2 and last.isascii() else "TRAE"


def _mem_rows() -> list[dict]:
    """扫一遍记忆库，60 秒缓存。按 message_id 去重（同一条可能落两个目录）。"""
    now = time.time()
    if not MEM_ROOT.exists():
        return []
    if now - float(_MEM_CACHE["at"] or 0) < _MEM_TTL and _MEM_CACHE["rows"]:
        return _MEM_CACHE["rows"]           # type: ignore[return-value]
    rows: list[dict] = []
    seen: set[str] = set()
    try:
        files = sorted(MEM_ROOT.glob("*/[0-9]*/session_memory_*.jsonl"))
    except OSError:
        return []
    for p in files:
        proj = _proj_from_memdir(p.parent.parent.name)
        try:
            text = p.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        for ln in text.splitlines():
            ln = ln.strip()
            if not ln:
                continue
            try:
                d = json.loads(ln)
            except ValueError:
                continue
            ts = str(d.get("message_summary_time") or "")
            if not ts:
                continue
            key = str(d.get("message_id") or ts + (d.get("intent") or ""))
            if key in seen:
                continue
            seen.add(key)
            rows.append({
                "at": ts,
                "intent": str(d.get("intent") or "").strip(),
                "outcome": " ".join(str(d.get("outcome") or "").split()),
                "actions": [str(a).strip() for a in (d.get("actions") or []) if str(a).strip()],
                "learned": [str(a).strip() for a in (d.get("learned") or []) if str(a).strip()],
                "project": proj,
                "sid": str(d.get("message_id") or "")[:8],
            })
    rows.sort(key=lambda r: r["at"])
    _MEM_CACHE["at"], _MEM_CACHE["rows"] = now, rows
    return rows


def memory_rounds(since_ms: float) -> list[dict]:
    """TRAE 自述的「这一轮做了什么」。start 先取 end（不编时间），
    与命令轮合并后由 items() 往前扩到命令起点。"""
    out: list[dict] = []
    for r in _mem_rows():
        end_ms = _ep(r["at"])
        if not end_ms or end_ms < since_ms:
            continue
        intent = r["intent"] or "（TRAE 未写意图）"
        outcome = r["outcome"]
        kids = [{"start": r["at"], "detail": _clip(a, 120)} for a in r["actions"][:8]]
        detail = ("结果：" + outcome) if outcome else "只有意图，没留下结果"
        ev = ("经验：" + " / ".join(r["learned"][:2])) if r["learned"] else \
             "取自 TRAE 会话记忆（明文），不是猜的"
        out.append(_item("mem" + (r["sid"] or str(int(end_ms))[-8:]),
                         intent, detail, end_ms, end_ms, kids,
                         "TRAE 记忆", ev))
    return out


def note_rounds(since_ms: float) -> list[dict]:
    """skill 让 TRAE 收尾时主动补的那笔（TRAE自述.jsonl），补记忆库没覆盖的轻活。"""
    if not NOTE_PATH.exists():
        return []
    out: list[dict] = []
    try:
        lines = NOTE_PATH.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError:
        return []
    for ln in lines:
        try:
            d = json.loads(ln)
            t = float(d.get("at") or 0) * 1000
        except (ValueError, TypeError):
            continue
        if t < since_ms:
            continue
        task = str(d.get("task") or "").strip()
        if not task:
            continue
        summary = " ".join(str(d.get("summary") or "").split())
        proj = str(d.get("project") or "").strip()
        out.append(_item("note" + str(int(t))[-8:], task,
                         summary or "TRAE 主动上报，没写摘要", t, t, [],
                         str(d.get("source") or "TRAE 自述"), proj or "TRAE 收尾自报"))
    return out


def items(since_ms: float) -> list[dict]:
    cmd = command_rounds(since_ms)
    proc = process_rounds(since_ms)
def items(since_ms: float) -> list[dict]:
    cmd = command_rounds(since_ms)
    proc = process_rounds(since_ms)
    mem = memory_rounds(since_ms) + note_rounds(since_ms)
    mem.sort(key=lambda x: x.get("end") or "")

    # 记忆给意图、命令给证据：时刻相近的命令并进记忆那一轮，命令不再单占一行。
    # 一条记忆可以吃多轮零散命令（一次任务常常分几段敲），但一轮命令只归一条记忆。
    took_cmd: set[int] = set()
    for m in mem:
        me = _ep(m.get("end"))
        if not me:
            continue
        starts = [m.get("start") or ""]
        ends = [m.get("end") or ""]
        picked = 0
        lo, hi = me - _MEM_MERGE_MS, me + 10 * 60 * 1000   # 记忆是收尾写的，命令不该晚太多
        for i, c in enumerate(cmd):
            if i in took_cmd:
                continue
            ce = _ep(c.get("end") or "")
            if ce and lo <= ce <= hi:
                took_cmd.add(i)
                picked += 1
                starts.append(c.get("start") or "")
                ends.append(c.get("end") or "")
                if c.get("task"):
                    m["children"].append({"start": c.get("start") or "",
                                          "detail": _clip(c["task"], 120)})
                # 命令的步骤只在前面取几条：它动辄十几行输出，全塞进来会把
                # 记忆自己写的 actions 挤掉（最能说明意图的恰恰是那几条）。
                m["children"].extend((c.get("children") or [])[:4])
        if picked:
            seen_k: set[str] = set()
            kids = []
            for k in m["children"]:
                txt = str(k.get("detail") or "")
                if txt and txt not in seen_k:
                    seen_k.add(txt)
                    kids.append(k)
            m["children"] = kids[:18]           # 取前面：actions 比命令输出更像「做了什么」
            m["more_steps"] = max(0, len(kids) - len(m["children"]))
            m["start"] = min([s for s in starts if s])
            # end 以记忆写的时刻为准：它是「任务收尾」的时间戳，比命令输出可信。
            m["end"] = max(m["end"], min(max([s for s in ends if s] or [m["end"]]),
                                          _ms(me + 10 * 60 * 1000)))
            m["secs"] = _secs(m["start"], m["end"])
            m["detail"] = _clip((m.get("detail") or "") + f" · {picked} 条命令", 160)

    cmd_left = [c for i, c in enumerate(cmd) if i not in took_cmd]
    # 命令记录与进程时段常常是同一件事：进程证据并进已有那轮，不重复占两行。
    anchor = mem + cmd_left
    kept = []
    for p in proc:
        ps, pe = p.get("start") or "", p.get("end") or ""
        dup = False
        for c in anchor:
            cs, ce = c.get("start") or "", c.get("end") or ""
            if cs <= pe and ps <= ce:
                c["detail"] = _clip((c.get("detail") or "") + " · 进程已确认", 160)
                c["rest"] = _clip((p.get("rest") or "") + " · " + (c.get("rest") or ""), 160)
                if p.get("children") and not c.get("more_steps"):
                    c["children"] = (c.get("children") or []) + p["children"][-6:]
                dup = True
                break
        if not dup:
            # 只有 1 个采样、时长 0 秒的进程时段说不清任何事（TRAE 开着没动也会冒一条），
            # 留在流水里只会把真干活的记录挤下去。
            if not (p.get("secs") or 0) and len(p.get("children") or []) <= 1:
                continue
            kept.append(p)
    # 2026-11-19 修正：OCR 补录的是「侧栏任务名」= 项目/任务清单，不是
    # 「这一轮干了什么」的执行证据。混进逐轮记录会让人误以为 TRAE 今天
    # 做了这 19 件事（其实都是历史任务，时间还是补录那一刻的 17:04）。
    # 逐轮记录只留真实执行证据（命令 / 进程）；任务名交给 /api/tasks。
    out = mem + cmd_left + kept
    # 2026-11-22：再补一类「在场但没跑命令」的时段（会话→项目）。
    # 只取最近 12 段 —— 30 天窗口里会话能有几十个，全铺出来会把命令记录挤下去。
    # 2026-11-23：已经有记忆说清这轮干了什么的会话，不再单列「在场」。
    ses = session_rounds(since_ms, cmd_left)
    if mem:
        def _covered(s: dict) -> bool:
            se = _ep(s.get("end") or "")
            return bool(se) and any(
                _ep(m.get("end")) and abs(se - _ep(m.get("end"))) <= _MEM_MERGE_MS
                for m in mem)
        ses = [s for s in ses if not _covered(s)]
    out.extend(ses[:12])
    out.sort(key=lambda x: (x.get("end") or x.get("start") or ""), reverse=True)
    return out


def client_view(days: int) -> dict | None:
    """「谁在干活」里的 TRAE 一张卡：命令轮数 + 进程时段 + OCR 补录。"""
    since = (time.time() - days * 86400) * 1000 if days else 0
    its = items(since)
    if not its:
        return None
    kids = []
    def _src(x, name):
        return any(s.get("kind") == "code" and s.get("name") == name
                   for s in (x.get("sources") or []))
    n_cmd = sum(1 for x in its if _src(x, "命令记录"))
    n_proc = sum(1 for x in its if _src(x, "进程证据"))
    n_sess = sum(1 for x in its if _src(x, "会话活跃"))
    n_mem = sum(1 for x in its if _src(x, "TRAE 记忆") or _src(x, "TRAE 自述"))
    # OCR 补录不算「干活」，但任务清单里有几条仍值得在卡上标个数
    n_task = len(task_rows(since))
    for kind, n in (("自述", n_mem), ("命令记录", n_cmd), ("会话在场", n_sess),
                    ("进程时段", n_proc), ("任务清单", n_task)):
        if n:
            kids.append({"kind": "tool", "name": kind, "n": n})
    return {
        "id": "trae", "name": "TRAE", "glyph": "◈", "kind": "client",
        "calls": len(its), "asks": 0,
        "ms": sum(int(x.get("secs") or 0) * 1000 for x in its),
        "sessions": len(its),
        "last": max((str(x.get("end") or "")[5:16] for x in its), default=""),
        "tokens": {"in": 0, "out": 0, "cached": 0},
        "kids": kids,
    }


def main() -> int:
    ap = argparse.ArgumentParser(description="TRAE 工作记录采集器")
    ap.add_argument("--seed", type=Path, help="从截图 OCR 补录侧栏任务")
    ap.add_argument("--at", help="补录时间戳，如 '2026-11-19 17:04:00'")
    ap.add_argument("--list", action="store_true", help="列出当前采集到的轮次")
    ap.add_argument("--days", type=float, default=7)
    a = ap.parse_args()
    if a.seed:
        at = datetime.strptime(a.at, "%Y-%m-%d %H:%M:%S").timestamp() if a.at else None
        print(f"新增 {seed_from_image(a.seed, at)} 条")
    if a.list or not a.seed:
        since = (time.time() - a.days * 86400) * 1000
        for it in items(since):
            print(it["start"], it["client"], it["task"], "|", it["detail"])
    return 0


if __name__ == "__main__":
    sys.exit(main())
