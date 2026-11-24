#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""每日工作总结（workstat）：一张卡，四个分页 —— 时长 / Token / 提示词 / 效率。

数据来自三本账，各答一段，互不冒充：

  1. **WorkBuddy 会话流水**（`~/.workbuddy/projects/<项目>/<会话>.jsonl`）
     一行一条事件。这里只关心三类行（先用子串筛，再决定要不要解 JSON）：
       · `role=user` 且带 `<user_query>` —— 一条提示词。字数、链接、图片从这儿数。
       · `totalTokens` —— 一次 API 请求的用量，同一轮里几十次工具调用各带一份。
       · `mcp__` —— 这轮动过哪个连接器（插件）。
     会话文件是 append-only，按 offset 增量读；这里存的是**每一轮**（rid）的账，
     不是小时桶 —— 因为一轮的 token 落在请求时刻，而提示词的字数落在提问时刻，
     先分桶会把两者搅在一起，平均值的分母就对不上了。

  2. **系统屏幕使用时间**（`~/Library/Application Support/Knowledge/knowledgeC.db`）
     `/app/usage` 是「某个 App 在前台」的区间，同一份数据就是「屏幕使用时间」里
     那几个条。**人的时长**取的是这些区间的并集：屏幕亮着不等于有人在用，
     所以 `/display/isBacklit` 只作对照、不进分母。这张库 macOS 只留约四周，
     所以「人」的格子和「AI」的格子长度不一样，缺的那几天画成空格而不是 0。

  3. **别家 agent**（TRAE / Cherry Studio / Claude Code）
     它们没有 token 流水，只有轮次。这条走 dash 现成的逐轮解析（`_trae_items`
     等），本模块只数轮次和耗时 —— 不为了一张卡把四家的格式再抄一遍。

口径（改口径先改这段注释，这张卡最怕「看着有个数字，其实是两种算法拼的」）：
  · AI 时长 = 一轮请求的首末时间并集。并行几轮只算一遍，不会因为开了 5 个窗口翻五倍。
  · 人时长   = 系统记录里「有 App 在前台」的区间并集。
  · 每条提示词耗时 = 这一轮（rid）的首末跨度；没有提示词的手工操作不算分母。
  · 平均 token / 条 = 窗口内总 token ÷ 提示词条数（token 是请求级的，提示词是
    提问级的，两者本来就不一一对应，这个比值回答的是「平均一问烧多少」）。
"""
from __future__ import annotations

import json
import os
import plistlib
import re
import sqlite3
import subprocess
import threading
import time
from datetime import date, datetime, timedelta
from pathlib import Path

import paths

WB_ROOT = Path.home() / ".workbuddy" / "projects"
CACHE_DIR = paths.data_root() / "_本机工作" / "缓存"
CACHE = CACHE_DIR / "工作统计.json"
NAME_CACHE = CACHE_DIR / "应用名.json"

KEEP_DAYS = 200          # 每轮的账留多久（热力图最多看 26 周，留够余量）
SCAN_TTL = 60.0          # 秒：页面轮询别每次都重扫
MAX_FILES = 4000         # 防御：projects 下不可能有这么多会话文件

# ---------------------------------------------------------------------------
# 行级正则：全部在 bytes 上做。会话 jsonl 里一行能有几十 KB 的 reasoning，
# 为了拿 inputTokens 去 json.loads 整行是不划算的（656 MB 实测 1.3 秒 vs 20 秒）。
# ---------------------------------------------------------------------------
_RE_RID = re.compile(rb'"conversationRequestId":"([0-9a-fA-F-]{8,64})"')
_RE_TS = re.compile(rb'"timestamp":(\d{13})')
_RE_TOTAL = re.compile(rb'"totalTokens":(\d+)')
_RE_TIN = re.compile(rb'"inputTokens":(\d+)')
_RE_TOUT = re.compile(rb'"outputTokens":(\d+)')
_RE_CACHED = re.compile(rb'"cached_tokens":(\d+)')
_RE_MODEL = re.compile(rb'"requestModelName":"([^"]{1,60})"')
_RE_Q = re.compile(rb'<user_query>(.*?)</user_query>', re.S)
_RE_EXPERT = re.compile(rb"<current-expert>[\s\S]{0,200}?(ex_[A-Za-z0-9]{6,})")
_RE_MODE = re.compile(rb"You are now in ([A-Za-z][A-Za-z ]{2,14}?) mode")
_RE_MCP = re.compile(rb"mcp__([A-Za-z0-9_\-]+?)__")
_RE_LINK = re.compile(r"https?://")
_RE_IMG = re.compile(r"@image#\d+:|<image_local_path>")

# 提示词字数的分档（前端直方图用同一份边界）
LENS = (20, 50, 100, 200, 500, 1000)

_S = {"data": None, "at": 0.0, "busy": False, "err": "", "took": 0.0}
_LOCK = threading.Lock()


# ---------------------------------------------------------------- 小工具
def _now_ms() -> int:
    return int(time.time() * 1000)


def _day_of(ms: int) -> str:
    try:
        return datetime.fromtimestamp(ms / 1000.0).strftime("%Y-%m-%d")
    except (OverflowError, OSError, ValueError):
        return ""


def _hour_of(ms: int) -> str:
    try:
        return datetime.fromtimestamp(ms / 1000.0).strftime("%Y-%m-%dT%H")
    except (OverflowError, OSError, ValueError):
        return ""


def _norm_day(s: str) -> str:
    s = str(s or "").strip()
    if re.fullmatch(r"\d{4}-\d{2}-\d{2}", s):
        try:
            datetime.strptime(s, "%Y-%m-%d")
            return s
        except ValueError:
            pass
    return ""


def _jit(n: int) -> float:
    """固定加一点抖动，同秒内的多条提示词排序稳定（直方图分桶不用，只给人看）。"""
    return (n % 97) / 1000.0


def _unesc(body: bytes) -> str:
    """从原始行里抠出来的 `<user_query>` 内容是 JSON 字符串的**内胆**。

    直接当普通字节解码，`\\n` `\\"` 会多算字；包回引号交给 json 最准。
    """
    try:
        return json.loads(b'"' + body + b'"')
    except (ValueError, UnicodeDecodeError):
        return body.decode("utf-8", "replace")


def _len_bucket(n: int) -> int:
    for i, hi in enumerate(LENS):
        if n < hi:
            return i
    return len(LENS)


# ---------------------------------------------------------------- 「AI 在跑」的分钟格
# 一秒不差地记「哪几秒在跑」要存几万个区间，缓存立刻涨到几 MB；而这张卡上
# 一根柱子只占几个像素，秒级精度没有意义。所以按**分钟**打点：
# 发过请求的这一分钟标上，后面再补 2 分钟（工具跑完到下一条请求之间的空当）。
# 于是一天 24 个十六进制串（60 位）就把「AI 几点在干活」记全了，几 KB。
MARK_PAD_S = 120
# 一轮的跨度最多算 4 小时（见 _fold 里的注释：窗口挂一夜的那几条能到 40 小时）
TURN_CAP_MS = 4 * 3600 * 1000


def _mark(am: dict, ts_ms: int) -> None:
    """在分钟格上标一笔记账：这一分钟 + 之后 MARK_PAD_S 秒。"""
    t = int(ts_ms)
    end = t + MARK_PAD_S * 1000
    guard = 0
    while t <= end and guard < 8:
        guard += 1
        try:
            dt = datetime.fromtimestamp(t / 1000.0)
        except (OverflowError, OSError, ValueError):
            return
        key = dt.strftime("%Y-%m-%dT%H")
        h0 = dt.replace(minute=0, second=0, microsecond=0).timestamp() * 1000
        h1 = h0 + 3600000
        last = min(end, h1 - 1)
        a = max(0, min(59, int((t - h0) // 60000)))
        b = max(0, min(59, int((last - h0) // 60000)))
        bits = ((1 << (b - a + 1)) - 1) << a
        try:
            cur = int(am.get(key) or "0", 16)
        except ValueError:
            cur = 0
        am[key] = format(cur | bits, "x")
        t = h1


def _bits_secs(h: str) -> float:
    """一格字符串里有多少分钟是亮的 → 秒。"""
    try:
        return bin(int(h or "0", 16)).count("1") * 60.0
    except ValueError:
        return 0.0


# ---------------------------------------------------------------- 缓存读写
def _load() -> dict:
    try:
        raw = json.loads(CACHE.read_text(encoding="utf-8"))
        if isinstance(raw, dict) and raw.get("v") == 2:
            raw.setdefault("am", {})
            return raw
    except (OSError, ValueError):
        pass
    return {"v": 2, "files": {}, "am": {}, "at": 0.0}


def _save(d: dict) -> None:
    try:
        CACHE_DIR.mkdir(parents=True, exist_ok=True)
        tmp = CACHE.with_suffix(".tmp")
        tmp.write_text(json.dumps(d, ensure_ascii=False, separators=(",", ":")),
                       encoding="utf-8")
        os.replace(tmp, CACHE)
    except OSError as e:                     # 落盘失败不影响这一次的展示
        print(f"[workstat] 缓存写不进去：{e}", flush=True)


# ---------------------------------------------------------------- 会话扫描
def _sess_files(since_ms: int) -> list[Path]:
    if not WB_ROOT.is_dir():
        return []
    out: list[Path] = []
    try:
        for p in WB_ROOT.glob("*/*.jsonl"):
            if p.name.endswith(".file-rollback.ndjson"):
                continue                     # 回滚快照，不是会话
            try:
                if p.stat().st_mtime * 1000 >= since_ms:
                    out.append(p)
            except OSError:
                continue
            if len(out) >= MAX_FILES:
                break
    except OSError:
        return []
    return out


def _scan_file(path: Path, prev: dict | None, am: dict) -> dict:
    """增量读一个会话文件，返回 {off, ino, size, reqs, ph}。

    reqs: rid → [t0, t1, tin, tout, cached, calls, model, expert, mcp, mode]
      · t0/t1 是这一轮的首末时刻（毫秒）。提示词那一刻定 t0，最后一次请求定 t1。
      · tin/tout/cached/calls 累加这个 rid 下每一条带 usage 的行。
      · expert / mcp / mode 只回答「这轮是在什么上下文里跑的」，取第一次见到的。
    ph: 这个文件贡献的提示词小时桶 —— 条数 / 字数 / 链接 / 图片 / 字数直方图。
      就地累加，不按 rid 回捞，也就不必把几千条原文留在缓存里。
    """
    try:
        st = path.stat()
    except OSError:
        return prev or {}
    same = (prev is not None and prev.get("ino") == st.st_ino
            and int(prev.get("size") or 0) <= st.st_size)
    off = int(prev.get("off") or 0) if same else 0
    reqs: dict = dict(prev.get("reqs") or {}) if same else {}
    # 提示词的小时桶**按文件记**：一个会话文件被截断重写时会从 0 重扫，
    # 桶跟着这个文件一起归零 —— 如果桶是全局一份，重扫就等于把那些提示词
    # 又数了一遍（实测多出 170 条）。
    fph: dict = dict(prev.get("ph") or {}) if same else {}
    if not same:
        # 文件被换掉了（重装 / 重建会话）：这只影响这一个文件，
        # 但它上一次贡献的提示词桶已经进了 ph，这里不回头扣 —— 那种情况
        # 一年也遇不到一次，真遇到了按「今天多算了几条」看即可。
        pass
    if st.st_size == off:
        return {"off": off, "ino": st.st_ino, "size": st.st_size,
                "reqs": reqs, "ph": fph}

    try:
        with path.open("rb") as f:
            f.seek(off)
            buf = f.read()
    except OSError:
        return {"off": off, "ino": st.st_ino, "size": st.st_size,
                "reqs": reqs, "ph": fph}
    cut = buf.rfind(b"\n")
    if cut < 0:                        # 只有半行，等下次
        return {"off": off, "ino": st.st_ino, "size": st.st_size,
                "reqs": reqs, "ph": fph}
    chunk = buf[:cut + 1]

    for raw in chunk.splitlines():
        # 提示词：只认真的带 <user_query> 的用户消息。工具回执、system-reminder
        # 补充消息（没有标签）都不是「我发的那句话」。
        if b"<user_query>" in raw and b'"role":"user"' in raw:
            rid = _RE_RID.search(raw)
            tsm = _RE_TS.search(raw)
            q = _RE_Q.search(raw)
            if rid and tsm and q:
                text = _unesc(q.group(1))
                n = len(text.strip())
                key = _hour_of(int(tsm.group(1)))
                if key:
                    b = fph.get(key)
                    if b is None:
                        b = fph[key] = [0] * (4 + len(LENS) + 1)
                    b[0] += 1
                    b[1] += n
                    b[2] += len(_RE_LINK.findall(text))
                    b[3] += len(_RE_IMG.findall(text))
                    b[4 + _len_bucket(n)] += 1
                r = reqs.get(rid.group(1).decode())
                if r is None:
                    r = reqs[rid.group(1).decode()] = [int(tsm.group(1)), 0,
                                                       0, 0, 0, 0, "", "", "", ""]
                r[0] = min(r[0], int(tsm.group(1)))
                if not r[7]:
                    e = _RE_EXPERT.search(raw)
                    if e:
                        r[7] = e.group(1).decode("utf-8", "replace")
                if not r[9]:
                    m = _RE_MODE.search(raw)
                    if m:
                        r[9] = m.group(1).decode("utf-8", "replace").strip().title()
            continue

        if b"mcp__" in raw:
            rid = _RE_RID.search(raw)
            if rid:
                r = reqs.get(rid.group(1).decode())
                if r is not None:
                    for srv in set(x.decode("utf-8", "replace")
                                   for x in _RE_MCP.findall(raw)):
                        if srv and srv not in r[8]:
                            r[8] = (r[8] + "," + srv)[:80] if r[8] else srv

        if b'"usage"' in raw and _RE_TOTAL.search(raw):
            rid = _RE_RID.search(raw)
            tsm = _RE_TS.search(raw)
            if not rid or not tsm:
                continue
            k = rid.group(1).decode()
            ts = int(tsm.group(1))
            r = reqs.get(k)
            if r is None:
                r = reqs[k] = [ts, ts, 0, 0, 0, 0, "", "", "", ""]
            r[1] = max(r[1], ts)
            if not r[0]:
                r[0] = ts
            m = _RE_TIN.search(raw)
            r[2] += int(m.group(1)) if m else 0
            m = _RE_TOUT.search(raw)
            r[3] += int(m.group(1)) if m else 0
            m = _RE_CACHED.search(raw)
            r[4] += int(m.group(1)) if m else 0
            r[5] += 1
            if not r[6]:
                m = _RE_MODEL.search(raw)
                if m:
                    r[6] = m.group(1).decode("utf-8", "replace")
            _mark(am, ts)

    return {"off": off + cut + 1, "ino": st.st_ino, "size": st.st_size,
            "reqs": reqs, "ph": fph}


def scan(force: bool = False) -> dict:
    """把窗口内的会话文件增量扫一遍，返回整份状态（含 ph / am / files）。"""
    d = _load() if not force else {"v": 2, "files": {}, "am": {}, "at": 0.0}
    since_ms = _now_ms() - KEEP_DAYS * 86400000
    keep = (_now_ms() - KEEP_DAYS * 86400000)
    am: dict = d.get("am") or {}
    files: dict = d.get("files") or {}

    live: set = set()
    for p in _sess_files(since_ms):
        key = str(p)
        live.add(key)
        try:
            files[key] = _scan_file(p, files.get(key), am)
        except Exception as e:                    # noqa: BLE001
            # 单个会话文件读挂了不该让整页变空（以前这类地方都是静默吞，
            # 结果「为什么这块是 0」永远查不出来，这里至少留一行日志）
            print(f"[workstat] 读不了 {p.name}: {type(e).__name__}: {e}", flush=True)

    # 老文件的账清掉：会话被删了，它那条不该继续算在页面上
    for k in list(files):
        if k not in live:
            files.pop(k, None)

    # 每轮只留 KEEP_DAYS 内的
    cut_ms = keep
    for f in files.values():
        rq = f.get("reqs") or {}
        for rid in [k for k, r in rq.items() if int(r[0] or 0) < cut_ms]:
            rq.pop(rid, None)

    # 提示词桶跟着文件走（见 _scan_file 的注释），分钟格是全局的
    cut_day = (date.today() - timedelta(days=KEEP_DAYS)).isoformat()
    for f in files.values():
        f["ph"] = {k: v for k, v in (f.get("ph") or {}).items() if k[:10] >= cut_day}
    am = {k: v for k, v in am.items() if k[:10] >= cut_day}

    d.update({"v": 2, "files": files, "am": am, "at": time.time()})
    return d


# ---------------------------------------------------------------- 屏幕使用时间
KC_DB = (Path.home() / "Library" / "Application Support"
         / "Knowledge" / "knowledgeC.db")
_CF_EPOCH = 978307200.0          # Core Data 的时间戳原点（2001-01-01 UTC）
_NAME = {"m": {}, "at": 0.0}
_NAME_LOCK = threading.Lock()
# 兜底译名：系统盘扫不到的那几个（装在别处 / 沙箱里），手写比留一串 bundle id 强
_NAME_FIX = {
    "com.tencent.workbuddy.mac": "WorkBuddy",
    "cn.trae.app": "TRAE",
    "md.obsidian": "Obsidian",
    "com.apple.Safari": "Safari",
    "com.apple.Terminal": "Terminal",
    "com.apple.finder": "访达",
    "com.apple.dt.Xcode": "Xcode",
    "com.tencent.xinWeChat": "微信",
    "com.tencent.qq": "QQ",
    "com.netease.uuremote": "UU 远程",
    "com.kingsoft.wpsoffice.mac": "WPS Office",
}


def _app_names() -> dict:
    """bundle id → App 名。扫一遍系统里的 .app 里的 Info.plist，缓存一天。

    只扫一层（`/Applications/*.app/Contents/Info.plist`）：实测 143 个应用
    0.3 秒，比 mdfind 可靠（Spotlight 关掉就没了），也不会去碰别人的目录。
    """
    with _NAME_LOCK:
        if _NAME["m"] and time.time() - _NAME["at"] < 86400:
            return _NAME["m"]
        m: dict = dict(_NAME_FIX)
        try:
            m.update(json.loads(NAME_CACHE.read_text(encoding="utf-8")))
        except (OSError, ValueError):
            pass
        roots = [Path("/Applications"), Path("/System/Applications"),
                 Path("/System/Applications/Utilities"), Path("/Applications/Utilities"),
                 Path.home() / "Applications"]
        for r in roots:
            try:
                plists = list(r.glob("*.app/Contents/Info.plist"))
            except OSError:
                continue
            for p in plists:
                try:
                    with p.open("rb") as f:
                        d = plistlib.load(f)
                except Exception:                 # noqa: BLE001
                    continue
                bid = str(d.get("CFBundleIdentifier") or "").strip()
                nm = str(d.get("CFBundleDisplayName") or d.get("CFBundleName") or "").strip()
                if bid and nm and bid not in _NAME_FIX:
                    m[bid] = nm
        _NAME["m"], _NAME["at"] = m, time.time()
        try:
            CACHE_DIR.mkdir(parents=True, exist_ok=True)
            NAME_CACHE.write_text(json.dumps(m, ensure_ascii=False), encoding="utf-8")
        except OSError:
            pass
        return m


def _union(ivals: list) -> float:
    """区间并集（秒）。同一个 App 的区间会首尾相叠，直接加会虚高。"""
    if not ivals:
        return 0.0
    ivals.sort()
    tot = 0.0
    cs, ce = ivals[0]
    for s, e in ivals[1:]:
        if s > ce:
            tot += ce - cs
            cs, ce = s, e
        elif e > ce:
            ce = e
    tot += ce - cs
    return max(0.0, tot)


# ---------------------------------------------------------------- 前台采样（不靠授权）
# 系统那张屏幕使用库要「完全磁盘访问」，口袋是 launchd 起的，默认没有 —— 实测
# sqlite 直接报 `unable to open database file`。于是补一条**不需要任何授权**的路：
#   · 前台 App 用 `lsappinfo front`（系统自带的 LaunchServices 工具，普通用户就能问）；
#   · 键鼠空闲用 `ioreg -c IOHIDSystem` 的 HIDIdleTime。
# 每 60 秒记一行，人不在（空闲超过 3 分钟）就不记。数据从开始跑的那天起，
# 往前没有 —— 所以老日子仍然看系统库（授权之后两段数据都齐）。
SAMPLE_PATH = CACHE_DIR / "屏幕采样.jsonl"
SAMPLE_EVERY = 60.0
IDLE_MAX_S = 180.0
_SAMPLER = {"on": False}


def _front_app() -> str:
    try:
        asn = subprocess.run(["lsappinfo", "front"], capture_output=True,
                             timeout=4).stdout.decode("utf-8", "replace").strip()
        if not asn:
            return ""
        out = subprocess.run(["lsappinfo", "info", "-only", "name", asn],
                             capture_output=True, timeout=4
                             ).stdout.decode("utf-8", "replace")
        m = re.search(r'"([^"]{1,48})"', out)
        return m.group(1) if m else ""
    except (OSError, subprocess.SubprocessError):
        return ""


def _hid_idle() -> float:
    try:
        out = subprocess.run(["ioreg", "-c", "IOHIDSystem"], capture_output=True,
                             timeout=4).stdout.decode("utf-8", "replace")
        m = re.search(r'"HIDIdleTime"\s*=\s*(\d+)', out)
        return int(m.group(1)) / 1e9 if m else 0.0
    except (OSError, subprocess.SubprocessError, ValueError):
        return 0.0


def _sample_once() -> None:
    idle = _hid_idle()
    if idle > IDLE_MAX_S:
        return                                  # 人不在，不记（"机器开着"≠"人在用"）
    app = _front_app()
    if not app:
        return
    rec = {"ts": int(time.time()), "app": app, "idle": round(idle, 1)}
    try:
        CACHE_DIR.mkdir(parents=True, exist_ok=True)
        with SAMPLE_PATH.open("a", encoding="utf-8") as f:
            f.write(json.dumps(rec, ensure_ascii=False) + "\n")
    except OSError:
        pass


def _prune_samples(days: int = 60) -> None:
    """采样文件按天追加，一天 1440 行；留 60 天够热力图用，更老的删掉。"""
    if not SAMPLE_PATH.exists():
        return
    cut = time.time() - days * 86400
    try:
        keep = []
        with SAMPLE_PATH.open("r", encoding="utf-8", errors="replace") as f:
            for ln in f:
                try:
                    if float(json.loads(ln).get("ts") or 0) >= cut:
                        keep.append(ln if ln.endswith("\n") else ln + "\n")
                except (ValueError, AttributeError):
                    continue
        tmp = SAMPLE_PATH.with_suffix(".tmp")
        tmp.write_text("".join(keep), encoding="utf-8")
        os.replace(tmp, SAMPLE_PATH)
    except OSError:
        pass


def sampler_loop() -> None:
    n = 0
    while True:
        try:
            _sample_once()
        except Exception:                       # noqa: BLE001
            pass
        n += 1
        if n % 720 == 0:                        # 半天清一次，别让文件长到没边
            _prune_samples()
        time.sleep(SAMPLE_EVERY)


def start_sampler() -> None:
    """起前台采样线程（幂等）。放在服务启动时调，别等页面打开才开跑。"""
    if _SAMPLER["on"]:
        return
    _SAMPLER["on"] = True
    threading.Thread(target=sampler_loop, daemon=True, name="workstat-sample").start()


def _screen_from_samples(days: int) -> dict:
    """把自采的前台样本折成和系统库一样的形状（按天按 App、按小时）。"""
    out = {"ok": False, "why": "还没采到前台数据（服务刚起，每分钟一条）",
           "days": {}, "hours": {}, "names": {}, "span": ["", ""], "n_days": 0,
           "src": "sampler"}
    if not SAMPLE_PATH.exists():
        return out
    since = time.time() - days * 86400
    apps: dict = {}
    hours: dict = {}
    names: dict = {}
    first = last = 0
    try:
        with SAMPLE_PATH.open("r", encoding="utf-8", errors="replace") as f:
            for ln in f:
                try:
                    r = json.loads(ln)
                except ValueError:
                    continue
                ts = float(r.get("ts") or 0)
                if ts < since:
                    continue
                nm = str(r.get("app") or "").strip()
                if not nm:
                    continue
                first = first or ts
                last = max(last, ts)
                d = datetime.fromtimestamp(ts).strftime("%Y-%m-%d")
                h = datetime.fromtimestamp(ts).hour
                names[nm] = nm
                row = apps.setdefault(d, {})
                row[nm] = round(row.get(nm, 0.0) + SAMPLE_EVERY, 1)
                hs = hours.setdefault(d, [0.0] * 24)
                hs[h] = round(hs[h] + SAMPLE_EVERY, 1)
    except OSError:
        return out
    if not apps:
        return out
    out.update({
        "ok": True, "why": "",
        "days": {d: {"total": round(sum(v.values()), 1),
                     "apps": dict(sorted(v.items(), key=lambda kv: -kv[1])[:14])}
                 for d, v in apps.items()},
        "hours": hours, "names": names, "n_days": len(apps),
        "span": [datetime.fromtimestamp(first).strftime("%Y-%m-%d"),
                 datetime.fromtimestamp(last).strftime("%Y-%m-%d")] if first else ["", ""],
        "src": "sampler",
    })
    return out


def screen(days: int = 30) -> dict:
    """系统屏幕使用时间：每天每 App 多长、每小时「有人在用」多长。

    表结构就两张：`ZOBJECT` 里 `ZSTREAMNAME='/app/usage'` 是前台 App 区间；
    `ZVALUESTRING` 是 bundle id。
    """
    out = {"ok": False, "why": "", "days": {}, "hours": {}, "names": {},
           "span": ["", ""], "src": "system"}
    if not KC_DB.exists():
        # 没有系统库（Windows / 老系统）：直接用自己采的前台样本
        return _screen_from_samples(days)
    days = max(1, min(int(days or 30), 90))
    since = time.time() - days * 86400 - _CF_EPOCH
    try:
        con = sqlite3.connect(f"file:{KC_DB}?mode=ro", uri=True, timeout=2.0)
        rows = list(con.execute(
            "SELECT ZVALUESTRING, ZSTARTDATE, ZENDDATE FROM ZOBJECT "
            "WHERE ZSTREAMNAME='/app/usage' AND ZENDDATE>?", (since,)))
        con.close()
    except sqlite3.Error as e:
        # 最常见的就是没给「完全磁盘访问」（launchd 起的服务默认没有），
        # 退到自采样本：数据从服务开始跑的那天起，至少有今天的。
        fb = _screen_from_samples(days)
        fb["why"] = (f"系统屏幕使用库读不了（{type(e).__name__}: {e}）——"
                     "要在「隐私与安全性 · 完全磁盘访问」里给这个 python 授权；"
                     "在那之前用本服务自己采的前台样本")
        fb["need_fda"] = True
        return fb
    if not rows:
        return _screen_from_samples(days)

    names = _app_names()
    per_app: dict = {}          # (day, bundle) → [ivals]
    per_hour: dict = {}         # (day, hour) → [ivals]
    per_day: dict = {}          # day → [ivals]（不分 App 的并集：人在电脑前）
    first, last = 0.0, 0.0
    for bundle, s, e in rows:
        try:
            s, e = float(s), float(e)
        except (TypeError, ValueError):
            continue
        if not bundle or e <= s:
            continue
        e = min(e, s + 6 * 3600)          # 一条事件最多算 6 小时：机器睡得久时区间会离谱
        s += _CF_EPOCH
        e += _CF_EPOCH
        if not first or s < first:
            first = s
        if e > last:
            last = e
        # 跨天的区间按天切开（凌晨那几小时不能算到前一天头上）
        cur = s
        while cur < e:
            d0 = datetime.fromtimestamp(cur)
            day = d0.strftime("%Y-%m-%d")
            nxt = (d0.replace(hour=0, minute=0, second=0, microsecond=0)
                   + timedelta(days=1)).timestamp()
            seg = min(e, nxt)
            per_app.setdefault((day, bundle), []).append((cur, seg))
            per_day.setdefault(day, []).append((cur, seg))
            # 小时切片
            hcur = cur
            while hcur < seg:
                h0 = datetime.fromtimestamp(hcur)
                hnxt = (h0.replace(minute=0, second=0, microsecond=0)
                        + timedelta(hours=1)).timestamp()
                hend = min(seg, hnxt)
                per_hour.setdefault((day, h0.hour), []).append((hcur, hend))
                hcur = hend
            cur = seg

    days_out: dict = {}
    for (day, bundle), ivs in per_app.items():
        secs = _union(ivs)
        if secs < 1:
            continue
        row = days_out.setdefault(day, {})
        row[bundle] = row.get(bundle, 0.0) + secs
    hours_out: dict = {}
    for (day, h), ivs in per_hour.items():
        hours_out.setdefault(day, [0.0] * 24)[h] = _union(ivs)
    all_out: dict = {}
    for day, ivs in per_day.items():
        all_out[day] = _union(ivs)

    out.update({
        "ok": True,
        "n_days": len(days_out),
        "days": {d: {"total": round(all_out.get(d, 0.0), 1),
                     "apps": {b: round(v, 1) for b, v in sorted(
                         v0.items(), key=lambda kv: -kv[1])[:14]}}
                 for d, v0 in days_out.items()},
        "hours": {d: [round(x, 1) for x in hs] for d, hs in hours_out.items()},
        "names": names,
        "span": [datetime.fromtimestamp(first).strftime("%Y-%m-%d"),
                 datetime.fromtimestamp(last).strftime("%Y-%m-%d")] if first else ["", ""],
    })
    return out


# ---------------------------------------------------------------- 别家 agent
def _other_agents(since_ms: int) -> list[dict]:
    """TRAE / Cherry / Claude Code 的轮次。拿不到就返回空，不编。

    这份走 dash 的解析层（`hostwork._rounds_parser()` 认「开发机 cuntu 那份 →
    交付版包内那份」两条路），本模块不再抄一遍四家的格式。
    """
    try:
        import hostwork
        dash = hostwork._rounds_parser()
    except Exception:                        # noqa: BLE001
        return []
    if dash is None:
        return []
    out: list[dict] = []
    for fn_name in ("_trae_items", "_cherry_items", "_claude_items"):
        fn = getattr(dash, fn_name, None)
        if fn is None:
            continue
        try:
            items = fn(since_ms) or []
        except Exception:                    # noqa: BLE001
            continue
        for it in items:
            if not isinstance(it, dict):
                continue
            out.append(it)
    return out


# ---------------------------------------------------------------- 汇总
def _fold(state: dict, since_day: str, day: str, win_day: str = "") -> dict:
    """把每轮的账折成「每天 / 每小时 / 每个维度」。

    since_day 是热力图要的跨度（比窗口长），win_day 是明细窗口（7/30/90 天）。
    分维度的那几张榜只算窗口内的 —— 榜单和下面的「合计」必须是同一段时间，
    否则会出现「各 agent 加起来 1400 条、合计写着 1112 条」。
    """
    today = date.today()
    row = {}

    def drow(d: str) -> dict:
        return row.setdefault(d, {"d": d, "ai": 0.0, "human": 0.0, "tok": 0,
                                  "tin": 0, "tout": 0, "prompts": 0, "chars": 0,
                                  "links": 0, "imgs": 0, "reqs": 0})

    hour_rows: dict = {}         # day → [24] 小字典（token/提示词）
    agents: dict = {}
    models: dict = {}
    experts: dict = {}
    mcps: dict = {}

    def hrow(d: str, h: int) -> dict:
        hs = hour_rows.setdefault(d, [None] * 24)
        if hs[h] is None:
            hs[h] = {"ai": 0.0, "tok": 0, "tin": 0, "tout": 0, "prompts": 0,
                     "chars": 0, "links": 0, "imgs": 0}
        return hs[h]

    def add_agent(name: str) -> dict:
        return agents.setdefault(name, {"name": name, "prompts": 0, "reqs": 0,
                                        "secs": 0.0, "tin": 0, "tout": 0, "chars": 0})

    for f in (state.get("files") or {}).values():
        for rid, r in (f.get("reqs") or {}).items():
            try:
                t0, t1, tin, tout, _cached, calls = (int(r[0] or 0), int(r[1] or 0),
                                                     int(r[2] or 0), int(r[3] or 0),
                                                     int(r[4] or 0), int(r[5] or 0))
                model = str(r[6] or "")
                expert = str(r[7] or "")
                mcp = str(r[8] or "")
                _mode = str(r[9] or "")
            except (IndexError, TypeError, ValueError):
                continue
            if not t0:
                continue
            d = _day_of(t0)
            if not d or d < since_day:
                continue
            rw = drow(d)
            rw["reqs"] += 1
            rw["tin"] += tin
            rw["tout"] += tout
            rw["tok"] += tin + tout
            # 一轮的跨度封顶 4 小时：绝大多数轮次 7 分钟上下（实测 p50 7.1、p90 25），
            # 但窗口挂一夜没关的那几条能写出 40 小时，不封顶会把「平均每条多久」
            # 整个带偏。
            dur = min(max(0, t1 - t0), TURN_CAP_MS) / 1000.0
            win = (not win_day) or d >= win_day
            if win:
                a = add_agent("WorkBuddy")
                a["reqs"] += 1
                a["tin"] += tin
                a["tout"] += tout
                a["secs"] += dur
            if model and win:
                # 同一个模型在日志里有大小写两种写法（DeepSeek-V4.1-Flash /
                # Deepseek-V4.1-Flash），按小写归并，榜上只留第一次见到的名字。
                mk = model.strip().lower()
                m = models.setdefault(mk, {"name": model.strip(), "calls": 0,
                                           "tin": 0, "tout": 0})
                m["calls"] += calls or 1
                m["tin"] += tin
                m["tout"] += tout
            if expert and win:
                e = experts.setdefault(expert, {"name": expert, "reqs": 0,
                                                "secs": 0.0, "tok": 0})
                e["reqs"] += 1
                e["secs"] += dur
                e["tok"] += tin + tout
            for srv in ([x for x in mcp.split(",") if x][:4] if win else []):
                q = mcps.setdefault(srv, {"name": srv, "reqs": 0, "secs": 0.0,
                                          "tok": 0})
                q["reqs"] += 1
                q["secs"] += dur
                q["tok"] += tin + tout
            # 小时桶：这一轮按「它落在哪个小时」计 token；跨度超过一小时的
            # 逐小时摊（只摊 token，提示词不摊 —— 提示词只在自己那一刻算一条）。
            # 「AI 时长」不在这儿算：那一维走分钟格（见 _mark），
            # 否则一场四小时的会话会把中间打盹的时间也算成在干活。
            if t1 > t0 and (t1 - t0) > 3600 * 1000:
                span = (t1 - t0) / 1000.0
                cur = t0
                while cur < t1:
                    hk = datetime.fromtimestamp(cur / 1000.0).replace(
                        minute=0, second=0, microsecond=0)
                    nxt = (hk + timedelta(hours=1)).timestamp() * 1000
                    seg = min(t1, nxt)
                    frac = max(0.0, (seg - cur)) / (span * 1000.0)
                    hr = hrow(d, hk.hour)
                    hr["tok"] += int((tin + tout) * frac)
                    hr["tin"] += int(tin * frac)
                    hr["tout"] += int(tout * frac)
                    cur = seg
            else:
                hr = hrow(d, datetime.fromtimestamp(t0 / 1000.0).hour)
                hr["tok"] += tin + tout
                hr["tin"] += tin
                hr["tout"] += tout

    # 提示词桶（WorkBuddy）：n / chars / links / imgs / 字数直方图
    hist = [0] * (len(LENS) + 1)
    for f in (state.get("files") or {}).values():
        for key, b in (f.get("ph") or {}).items():
            d = str(key)[:10]
            if d < since_day:
                continue
            try:
                h = int(str(key)[11:13])
            except ValueError:
                h = 0
            rw = drow(d)
            rw["prompts"] += int(b[0] or 0)
            rw["chars"] += int(b[1] or 0)
            rw["links"] += int(b[2] or 0)
            rw["imgs"] += int(b[3] or 0)
            hr = hrow(d, min(23, max(0, h)))
            hr["prompts"] += int(b[0] or 0)
            hr["chars"] += int(b[1] or 0)
            hr["links"] += int(b[2] or 0)
            hr["imgs"] += int(b[3] or 0)
            if not win_day or d >= win_day:
                a = add_agent("WorkBuddy")
                a["prompts"] += int(b[0] or 0)
                a["chars"] += int(b[1] or 0)
                for i in range(len(LENS) + 1):
                    if 4 + i < len(b):
                        hist[i] += int(b[4 + i] or 0)

    # 别家 agent：只有轮次（和偶尔有的耗时），没有 token
    marks = state.get("am")
    if not isinstance(marks, dict):
        marks = state["am"] = {}
    since_ms = int((datetime.strptime(since_day, "%Y-%m-%d")).timestamp() * 1000)
    for it in _other_agents(since_ms):
        t0 = int(it.get("t0") or 0)
        if not t0:
            continue
        d = _day_of(t0)
        if not d or d < since_day:
            continue
        client = str(it.get("client") or "其他")
        rw = drow(d)
        rw["prompts"] += 1
        rw["reqs"] += 1
        tok = it.get("tokens") or {}
        tin = int(tok.get("in") or 0)
        tout = int(tok.get("out") or 0)
        rw["tin"] += tin
        rw["tout"] += tout
        rw["tok"] += tin + tout
        secs = float(it.get("secs") or 0)
        win = (not win_day) or d >= win_day
        if win:
            a = add_agent(client)
            a["prompts"] += 1
            a["reqs"] += 1
            a["secs"] += secs
            a["tin"] += tin
            a["tout"] += tout
        hr = hrow(d, datetime.fromtimestamp(t0 / 1000.0).hour)
        hr["prompts"] += 1
        hr["tok"] += tin + tout
        # 这些客户端只有「这一轮的某一刻」，没有起止（TRAE 的会话记忆只写一个
        # message_summary_time）。给它在分钟格上点一笔 —— 至少「那天 AI 动过」
        # 能被看见，而不是和「一整天没开」画成同一个颜色。
        try:
            _mark(marks, t0)
            if secs > 0:
                _mark(marks, t0 + int(secs * 1000))
        except Exception:                    # noqa: BLE001
            pass
        f = it.get("facets") or {} if win else {}
        if f.get("expert"):
            e = experts.setdefault(str(f["expert"]), {"name": str(f["expert"]),
                                                      "reqs": 0, "secs": 0.0, "tok": 0})
            e["reqs"] += 1
            e["secs"] += secs
            e["tok"] += tin + tout
        for c in (f.get("connectors") or []):
            nm = str((c or {}).get("name") or "")
            if not nm:
                continue
            q = mcps.setdefault(nm, {"name": nm, "reqs": 0, "secs": 0.0, "tok": 0})
            q["reqs"] += 1
            q["secs"] += secs
            q["tok"] += tin + tout

    # AI 时长：分钟格的并集（并行窗口、同一分钟多次请求都只算一次）
    for key, hx in (state.get("am") or {}).items():
        d = str(key)[:10]
        if d < since_day:
            continue
        secs = _bits_secs(hx)
        if secs <= 0:
            continue
        drow(d)["ai"] += secs
        try:
            h = int(str(key)[11:13])
        except ValueError:
            h = 0
        hrow(d, min(23, max(0, h)))["ai"] += secs

    rows = []
    d0 = datetime.strptime(since_day, "%Y-%m-%d").date()
    n = (today - d0).days + 1
    for i in range(n - 1, -1, -1):
        d = (today - timedelta(days=i)).isoformat()
        r = row.get(d) or {"d": d, "ai": 0.0, "human": 0.0, "tok": 0, "tin": 0,
                           "tout": 0, "prompts": 0, "chars": 0, "links": 0,
                           "imgs": 0, "reqs": 0}
        r["ai"] = round(float(r["ai"]), 1)
        rows.append(r)

    hours = [None] * 24
    for h in range(24):
        hours[h] = (hour_rows.get(day) or [None] * 24)[h] or {
            "ai": 0.0, "tok": 0, "tin": 0, "tout": 0, "prompts": 0,
            "chars": 0, "links": 0, "imgs": 0}

    # 专家名：dash 有一份「最近用过的几位」清单，查不到就留 id 尾 6 位
    try:
        import hostwork
        dash = hostwork._rounds_parser()
        if dash is not None and hasattr(dash, "_expert_name"):
            for e in experts.values():
                nm = dash._expert_name(e["name"])
                if nm:
                    e["label"] = nm
    except Exception:                        # noqa: BLE001
        pass
    for e in experts.values():
        e.setdefault("label", e["name"][-6:] if e["name"].startswith("ex_") else e["name"])

    return {"rows": rows, "hours": hours, "agents": list(agents.values()),
            "models": list(models.values()), "experts": list(experts.values()),
            "mcps": list(mcps.values()), "hist": hist,
            # 轮次跨度之和：答「每条提示词花了多久」用它，不用 AI 分钟格 ——
            # 格子是「几点在干活」，跨度是「这一轮从提问到答完」，两回事。
            "turn_secs": sum(float(a["secs"]) for a in agents.values())}


def _with_screen(bundle: dict, days: int, day: str) -> dict:
    """把屏幕使用时间并进 rows / hours（人时长），并给热力图补「人」那一维。"""
    sc = screen(days)
    if not sc.get("ok"):
        return sc
    names = sc.get("names") or {}
    for r in bundle["rows"]:
        d = r["d"]
        row = (sc["days"] or {}).get(d) or {}
        r["human"] = round(float(row.get("total") or 0), 1)
    hs = (sc.get("hours") or {}).get(day) or [0.0] * 24
    for h in range(24):
        bundle["hours"][h]["human"] = round(float(hs[h] or 0), 1)
    # 那一页的「屏幕使用时间」榜：按 App 名合并（同一个 App 有时两个 bundle id）
    apps: dict = {}
    for b, secs in ((sc["days"].get(day) or {}).get("apps") or {}).items():
        nm = names.get(b) or _NAME_FIX.get(b) or b
        apps[nm] = apps.get(nm, 0.0) + float(secs or 0)
    sc["day_apps"] = sorted(([k, round(v, 1)] for k, v in apps.items()),
                            key=lambda kv: -kv[1])[:12]
    return sc


def _heat(rows: list, sc: dict, weeks: int = 13) -> dict:
    """GitHub 那种格子：按周排，一列一周，从上到下周一→周日。

    「人」只有系统库覆盖的那几天有数；没有记录的日子 `has:0`，
    前端画成浅描边 —— 和「那天是 0」必须能分开，否则会以为自己没干活。
    """
    by = {r["d"]: r for r in rows}
    today = date.today()
    # 从今天所在周的周一开始，往前推 weeks 周
    start = today - timedelta(days=today.weekday()) - timedelta(weeks=weeks - 1)
    scr = sc.get("days") or {} if sc.get("ok") else {}
    cells = []
    cur = start
    while cur <= today:
        ds = cur.isoformat()
        r = by.get(ds) or {}
        h = scr.get(ds) or {}
        cells.append({
            "d": ds,
            "ai": round(float(r.get("ai") or 0), 1),
            "human": round(float(h.get("total") or 0), 1) if h else 0.0,
            "has": 1 if h else 0,
            "tok": int(r.get("tok") or 0),
            "prompts": int(r.get("prompts") or 0),
        })
        cur += timedelta(days=1)
    return {"from": start.isoformat(), "to": today.isoformat(),
            "cells": cells, "weeks": weeks}


def view(days: int = 30, day: str = "", fresh: bool = False) -> dict:  # noqa: FBT001
    """这一张卡要的全部数据。一次装好，切分页/切日期都不再打接口。

    `fresh=True` 跳过 60 秒缓存重扫会话流水（页面上的「刷新」按钮用）。
    """
    try:
        days = max(7, min(int(days or 30), 180))
    except (TypeError, ValueError):
        days = 30
    day = _norm_day(day) or date.today().isoformat()
    if day > date.today().isoformat():
        day = date.today().isoformat()

    start_sampler()          # 头一次来数据就把前台采样拉起来（幂等）
    with _LOCK:
        cached = _S.get("data")
        fresh_ok = (cached is not None
                    and time.time() - float(_S.get("at") or 0) < SCAN_TTL)
    if fresh or not fresh_ok:
        t0 = time.time()
        state = scan()
        _save(state)
        tk = time.time() - t0
        with _LOCK:
            _S.update({"data": state, "at": time.time(), "took": tk})
    state = _S["data"]

    # 热力图比窗口长：那格子本来就是用来看「这几个月怎么走的」，
    # 跟着 7/30 天的 chip 缩成一两列就没意义了。
    heat_weeks = 26
    today = date.today()
    heat_from = (today - timedelta(days=today.weekday())
                 - timedelta(weeks=heat_weeks - 1)).isoformat()
    win_from = (today - timedelta(days=days - 1)).isoformat()
    since_day = min(win_from, heat_from)

    bundle = _fold(state, since_day, day, win_from)
    sc = _with_screen(bundle, max(days, (today - datetime.strptime(
        since_day, "%Y-%m-%d").date()).days + 1), day)
    rows_all = bundle["rows"]
    heat = _heat(rows_all, sc, heat_weeks)

    rows = bundle["rows"]
    rows = rows[-days:]
    tot = {
        "ai": round(sum(r["ai"] for r in rows), 1),
        "human": round(sum(r["human"] for r in rows), 1),
        "tok": sum(r["tok"] for r in rows),
        "tin": sum(r["tin"] for r in rows),
        "tout": sum(r["tout"] for r in rows),
        "prompts": sum(r["prompts"] for r in rows),
        "chars": sum(r["chars"] for r in rows),
        "links": sum(r["links"] for r in rows),
        "imgs": sum(r["imgs"] for r in rows),
        "reqs": sum(r["reqs"] for r in rows),
    }
    active_days = sum(1 for r in rows if r["prompts"] or r["reqs"])
    # 各维度的「每条平均」：token 用请求级总量除提示词数，耗时用轮次跨度除轮次
    agents = []
    for a in bundle["agents"]:
        n = max(1, int(a["reqs"]))
        agents.append({
            "name": a["name"], "prompts": int(a["prompts"]), "reqs": int(a["reqs"]),
            "secs": round(float(a["secs"]), 1),
            "tin": int(a["tin"]), "tout": int(a["tout"]),
            "tok": int(a["tin"]) + int(a["tout"]),
            "avg_sec": round(float(a["secs"]) / n, 1) if a["reqs"] else 0,
            "avg_tok": round((int(a["tin"]) + int(a["tout"])) / max(1, int(a["prompts"] or a["reqs"])), 1),
            "avg_chars": round(float(a["chars"]) / max(1, int(a["prompts"])), 1),
        })
    agents.sort(key=lambda x: (-(x["tok"] + x["reqs"] * 1000)))

    models = sorted(bundle["models"], key=lambda x: -(int(x["tin"]) + int(x["tout"])))
    for m in models:
        m["tok"] = int(m["tin"]) + int(m["tout"])
        m["avg_tok"] = round(m["tok"] / max(1, int(m["calls"])), 1)

    experts = sorted(bundle["experts"], key=lambda x: -int(x["reqs"]))[:12]
    for e in experts:
        e["secs"] = round(float(e["secs"]), 1)
        e["tok"] = int(e["tok"])
        e["avg_sec"] = round(float(e["secs"]) / max(1, int(e["reqs"])), 1)
        e["avg_tok"] = round(e["tok"] / max(1, int(e["reqs"])), 1)
    mcps = sorted(bundle["mcps"], key=lambda x: -int(x["reqs"]))[:12]
    for q in mcps:
        q["secs"] = round(float(q["secs"]), 1)
        q["tok"] = int(q["tok"])
        q["avg_sec"] = round(float(q["secs"]) / max(1, int(q["reqs"])), 1)
        q["avg_tok"] = round(q["tok"] / max(1, int(q["reqs"])), 1)

    hist = bundle["hist"]
    turn_secs = float(bundle.get("turn_secs") or 0)
    return {
        "ok": True,
        "at": datetime.now().strftime("%H:%M:%S"),
        "today": date.today().isoformat(),
        "day": day,
        "days": days,
        "range": [since_day, date.today().isoformat()],
        "rows": rows,
        "hours": bundle["hours"],
        "heat": heat,
        "screen": {
            "ok": bool(sc.get("ok")), "why": sc.get("why") or "",
            "src": sc.get("src") or "", "need_fda": bool(sc.get("need_fda")),
            "n_days": int(sc.get("n_days") or 0),
            "span": sc.get("span") or ["", ""],
            "day": day,
            "apps": sc.get("day_apps") or [],
            "names": sc.get("names") or {},
        },
        "agents": agents,
        "models": models,
        "experts": experts,
        "mcps": mcps,
        "lens": {"bounds": list(LENS), "hist": hist},
        "tot": tot,
        "avg": {
            "tok_per_prompt": round(tot["tok"] / max(1, tot["prompts"]), 1),
            "out_per_prompt": round(tot["tout"] / max(1, tot["prompts"]), 1),
            "tok_per_call": round(tot["tok"] / max(1, tot["reqs"]), 1),
            "sec_per_prompt": round(turn_secs / max(1, tot["prompts"]), 1),
            "sec_per_req": round(turn_secs / max(1, tot["reqs"]), 1),
            "chars_per_prompt": round(tot["chars"] / max(1, tot["prompts"]), 1),
            "prompts_per_day": round(tot["prompts"] / max(1, days), 1),
            "prompts_per_active_day": round(tot["prompts"] / max(1, active_days), 1),
            "ai_hours_per_day": round(tot["ai"] / 3600.0 / max(1, active_days), 2),
            # 「人」只按系统库真有记录的天数平摊 —— 那张库只留四周，
            # 除以 90 天会算出「每天 1 小时」这种假数字
            "human_hours_per_day": round(tot["human"] / 3600.0
                                         / max(1, int(sc.get("n_days") or days)), 2),
        },
        "active_days": active_days,
        "built": {
            "files": len(state.get("files") or {}),
            "reqs": sum(len((f.get("reqs") or {})) for f in (state.get("files") or {}).values()),
            "scan_sec": round(float(_S.get("took") or 0), 1),
            "cache_at": datetime.fromtimestamp(float(state.get("at") or 0)).strftime("%m-%d %H:%M")
            if state.get("at") else "",
        },
    }


def main(argv=None) -> int:
    import argparse
    ap = argparse.ArgumentParser(description="每日工作总结的数据层")
    ap.add_argument("--days", type=int, default=30)
    ap.add_argument("--day", default="")
    ap.add_argument("--screen", action="store_true", help="只看屏幕使用时间")
    ap.add_argument("--json", action="store_true", help="原样输出 JSON")
    a = ap.parse_args(argv)
    if a.screen:
        d = screen(a.days)
        print(json.dumps(d, ensure_ascii=False, indent=1)[:4000])
        return 0
    v = view(a.days, a.day)
    if a.json:
        print(json.dumps(v, ensure_ascii=False, indent=1))
        return 0
    print(f"== {v['range'][0]} ~ {v['range'][1]}（{v['days']} 天）选中 {v['day']}")
    print(f"AI {v['tot']['ai']/3600:.1f}h · 人 {v['tot']['human']/3600:.1f}h · "
          f"提示词 {v['tot']['prompts']} 条 · token {v['tot']['tok']/1e6:.1f}M")
    print(f"平均每条：{v['avg']['tok_per_prompt']/1000:.1f}k token / "
          f"{v['avg']['sec_per_prompt']:.0f}s · 平均字数 {v['avg']['chars_per_prompt']:.0f}")
    print("\n-- 每天")
    for r in v["rows"][-10:]:
        print(f"  {r['d']}  AI {r['ai']/3600:5.2f}h  人 {r['human']/3600:5.2f}h  "
              f"提示 {r['prompts']:3d} 条  tok {r['tok']/1e6:7.1f}M")
    print("\n-- agent")
    for x in v["agents"]:
        print(f"  {x['name']:14s} 提示 {x['prompts']:4d} 轮 {x['reqs']:4d} "
              f"tok {x['tok']/1e6:6.1f}M 平均 {x['avg_tok']/1000:6.1f}k/条 {x['avg_sec']:5.0f}s/轮")
    print("\n-- 模型")
    for m in v["models"][:8]:
        print(f"  {m['name']:28s} 调用 {m['calls']:6d} tok {m['tok']/1e6:7.1f}M")
    print(f"\n屏幕：{v['screen']['ok']} {v['screen']['why']} span={v['screen']['span']}")
    for nm, secs in v["screen"]["apps"][:8]:
        print(f"  {nm:22s} {secs/60:6.1f} min")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
