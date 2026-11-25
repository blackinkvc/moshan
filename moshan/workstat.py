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
  · 「字数」= 提示词原文的**字符数**：一个汉字、一个字母、一个标点各算 1（不是
    字节，也不是 token）。卡上那颗「token」按钮切的是同一段原文按 DeepSeek 官方
    口径折出来的估数（中文 0.6 / 其他 0.3），跟上面那个「总 token」不是一回事：
    usage 里的 inputTokens 是整个上下文（每条几百万，含缓存重发与几十轮工具调用），
    拿它当「这条提示词多长」会差三个数量级。
"""
from __future__ import annotations

import errno
import json
import os
import plistlib
import re
import sqlite3
import subprocess
import sys
import threading
import time
from datetime import date, datetime, timedelta
from pathlib import Path

import paths

WB_ROOT = Path.home() / ".workbuddy" / "projects"
CACHE_DIR = paths.data_root() / "_本机工作" / "缓存"
CACHE = CACHE_DIR / "工作统计.json"
NAME_CACHE = CACHE_DIR / "应用名.json"
# 永久存档：一天一条，落在数据根里（跟 Agent自述.jsonl / 进程.jsonl 同一个地方），
# 缓存可以删，这个不能 —— 会话流水会被客户端清理、屏幕库只留四周，
# 不存下来「上个月每天干了多久」过一阵就再也算不出来了。
ARCHIVE = paths.data_root() / "_本机工作" / "工作日报.json"
ARCHIVE_SAVE_S = 120.0   # 秒：别每来一次请求就写一遍（热力图/切页会连着打好几次）

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
# 提示词的小时桶：n / 字数 / 链接 / 图片 + 字数直方图，末尾再追加一格「折算 token」。
# 追加项一律放在**直方图后面** —— 老桶没有这一格时读成 0，不会把直方图串位。
_B_HIST = 4                                   # 字数直方图起点
_B_PTOK = _B_HIST + len(LENS) + 1             # 折算 token（追加项）
_B_LEN = _B_PTOK + 1
# 折算口径用 DeepSeek 官方那句：1 个中文字符 ≈ 0.6 token，1 个英文字符 ≈ 0.3。
# 中文字符是**逐字数**出来的（不是拿总字数乘常数），所以中英混排也准。
PTOK_CJK = 0.6
PTOK_OTHER = 0.3

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


def _ptok(s: str) -> int:
    """把一条提示词的原文折成 token 数（中文 0.6 / 其他 0.3，DeepSeek 口径）。

    为什么是折不是真分词：会话流水里只留了提示词的**原文**，没有平台侧的 token
    计数（usage 里的 inputTokens 是整个上下文，几百万，跟「这句话多长」不是一回事）。
    真要精确就得在这儿挂一个分词器，还得跟各家模型的词表对齐 —— 一张卡不值得。
    所以逐字数中文字符再按官方换算率折，误差在 ±10% 量级，卡上写清是折算值。
    """
    cjk = 0
    for ch in s:
        o = ord(ch)
        if (0x3400 <= o <= 0x9FFF or 0xF900 <= o <= 0xFAFF
                or 0x3000 <= o <= 0x303F or 0xFF00 <= o <= 0xFFEF):
            cjk += 1
    return int(round(cjk * PTOK_CJK + (len(s) - cjk) * PTOK_OTHER))


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
                body = text.strip()
                n = len(body)
                key = _hour_of(int(tsm.group(1)))
                if key:
                    b = fph.get(key)
                    if b is None:
                        b = fph[key] = [0] * _B_LEN
                    elif len(b) < _B_LEN:       # 老缓存里的短桶（没折算格）就地补长
                        b.extend([0] * (_B_LEN - len(b)))
                    b[0] += 1
                    b[1] += n
                    b[2] += len(_RE_LINK.findall(text))
                    b[3] += len(_RE_IMG.findall(text))
                    b[_B_PTOK] += _ptok(body)
                    b[_B_HIST + _len_bucket(n)] += 1
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


def _kc_probe() -> str:
    """读不到时到底卡在哪一步：文件本身被系统挡住，还是 SQLite 打不开。

    「完全磁盘访问」没给的时候是 EPERM/EACCES —— 这时同一个解释器换个终端跑就能读，
    所以别去怀疑 SQL 或路径。别的错号（ENOENT/EBUSY/锁）另说。
    """
    try:
        fd = os.open(str(KC_DB), os.O_RDONLY)
    except OSError as e:
        if e.errno in (errno.EPERM, errno.EACCES):
            return "连打开都被拒（EPERM），是 TCC 没授权，不是路径或 SQL 的问题"
        if e.errno == errno.ENOENT:
            return "文件不存在（这台机器没开过屏幕使用时间？）"
        return f"打不开库文件（errno={e.errno}）"
    else:
        os.close(fd)
        return "文件能打开，是 SQLite 那一层失败（库被占用或已损坏）"


def _self_exe() -> str:
    """要加进「完全磁盘访问」的那个二进制（跟着 venv 走的解释器本体）。"""
    try:
        return os.path.realpath(sys.executable)
    except Exception:                                  # noqa: BLE001
        return ""

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
        exe = _self_exe()
        fb["why"] = (f"系统屏幕使用库读不了（{type(e).__name__}: {e}）——{_kc_probe()}。"
                     + (f"要补历史：把 {exe} 加进「系统设置 · 隐私与安全性 · 完全磁盘访问」并打开开关；"
                        if exe else "要补历史：给本服务的解释器「完全磁盘访问」；")
                     + f"在那之前只用本机自己采的前台样本（{fb.get('span', ['', ''])[0]} 起）")
        fb["need_fda"] = True
        fb["exe"] = exe
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
HOUR_COLS = ("ai", "human", "tok", "tin", "tout", "prompts", "chars", "links", "imgs")
# 一天一条记录的形状（存档文件里也是它，`cols` 那行记着每个小时桶的列序）
DAY_COLS = ("ai", "human", "tok", "tin", "tout", "prompts", "chars", "links",
            "imgs", "reqs", "ptok")


def _blank_day(d: str) -> dict:
    return {
        "d": d, "ai": 0.0, "human": 0.0, "tok": 0, "tin": 0, "tout": 0,
        "prompts": 0, "chars": 0, "links": 0, "imgs": 0, "reqs": 0,
        "ptok": 0,                      # 提示词按 DeepSeek 口径折的 token（估数）
        "has": 0,                       # 那天有没有屏幕记录
        "hours": [[0] * len(HOUR_COLS) for _ in range(24)],
        "apps": {},                     # App 名 → 秒
        "agents": {}, "models": {}, "experts": {}, "mcps": {},
        "hist": [0] * (len(LENS) + 1),
    }


def _fold(state: dict, since_day: str) -> dict:
    """把每轮的账**按天**折好，返回 {"days": {日期: 明细}}。

    维度（agent / 模型 / 专家 / 插件 / 字数直方图）也挂在当天头上，所以
    7/30/90 天的合计一律由「那几天相加」得到 —— 好处有两条：每天能被永久存档
    （会话流水会被客户端清、屏幕库只留四周），而且榜单之和永远等于合计，
    不会再出现「各 agent 加起来 1400 条、合计写着 1112 条」。
    """
    days: dict = {}

    def dayrec(d: str) -> dict:
        r = days.get(d)
        if r is None:
            r = days[d] = _blank_day(d)
        return r

    def addh(d: str, h: int, col: str, v) -> None:
        dayrec(d)["hours"][min(23, max(0, h))][HOUR_COLS.index(col)] += v

    def add_agent(d: str, name: str) -> dict:
        return dayrec(d)["agents"].setdefault(
            name, {"prompts": 0, "reqs": 0, "secs": 0.0, "tin": 0, "tout": 0,
                   "chars": 0})

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
            rw = dayrec(d)
            rw["reqs"] += 1
            rw["tin"] += tin
            rw["tout"] += tout
            rw["tok"] += tin + tout
            # 一轮的跨度封顶 4 小时：绝大多数轮次 7 分钟上下（实测 p50 7.1、p90 25），
            # 但窗口挂一夜没关的那几条能写出 40 小时，不封顶会把「平均每条多久」
            # 整个带偏。
            dur = min(max(0, t1 - t0), TURN_CAP_MS) / 1000.0
            a = add_agent(d, "WorkBuddy")
            a["reqs"] += 1
            a["tin"] += tin
            a["tout"] += tout
            a["secs"] += dur
            if model:
                # 同一个模型在日志里有大小写两种写法（DeepSeek-V4.1-Flash /
                # Deepseek-V4.1-Flash），按小写归并，榜上只留第一次见到的名字。
                mk = model.strip().lower()
                m = rw["models"].setdefault(mk, {"name": model.strip(), "calls": 0,
                                                 "tin": 0, "tout": 0})
                m["calls"] += calls or 1
                m["tin"] += tin
                m["tout"] += tout
            if expert:
                e = rw["experts"].setdefault(expert, {"name": expert, "reqs": 0,
                                                      "secs": 0.0, "tok": 0})
                e["reqs"] += 1
                e["secs"] += dur
                e["tok"] += tin + tout
            for srv in [x for x in mcp.split(",") if x][:4]:
                q = rw["mcps"].setdefault(srv, {"name": srv, "reqs": 0, "secs": 0.0,
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
                    addh(d, hk.hour, "tok", int((tin + tout) * frac))
                    addh(d, hk.hour, "tin", int(tin * frac))
                    addh(d, hk.hour, "tout", int(tout * frac))
                    cur = seg
            else:
                hh = datetime.fromtimestamp(t0 / 1000.0).hour
                addh(d, hh, "tok", tin + tout)
                addh(d, hh, "tin", tin)
                addh(d, hh, "tout", tout)

    # 提示词桶（WorkBuddy）：n / chars / links / imgs / 字数直方图
    for f in (state.get("files") or {}).values():
        for key, b in (f.get("ph") or {}).items():
            d = str(key)[:10]
            if d < since_day:
                continue
            try:
                h = int(str(key)[11:13])
            except ValueError:
                h = 0
            rw = dayrec(d)
            rw["prompts"] += int(b[0] or 0)
            rw["chars"] += int(b[1] or 0)
            rw["ptok"] += int(b[_B_PTOK] or 0) if len(b) > _B_PTOK else 0
            rw["links"] += int(b[2] or 0)
            rw["imgs"] += int(b[3] or 0)
            addh(d, h, "prompts", int(b[0] or 0))
            addh(d, h, "chars", int(b[1] or 0))
            addh(d, h, "links", int(b[2] or 0))
            addh(d, h, "imgs", int(b[3] or 0))
            a = add_agent(d, "WorkBuddy")
            a["prompts"] += int(b[0] or 0)
            a["chars"] += int(b[1] or 0)
            for i in range(len(LENS) + 1):
                if _B_HIST + i < len(b):
                    rw["hist"][i] += int(b[_B_HIST + i] or 0)

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
        rw = dayrec(d)
        rw["prompts"] += 1
        rw["reqs"] += 1
        tok = it.get("tokens") or {}
        tin = int(tok.get("in") or 0)
        tout = int(tok.get("out") or 0)
        rw["tin"] += tin
        rw["tout"] += tout
        rw["tok"] += tin + tout
        secs = float(it.get("secs") or 0)
        a = add_agent(d, client)
        a["prompts"] += 1
        a["reqs"] += 1
        a["secs"] += secs
        a["tin"] += tin
        a["tout"] += tout
        hh = datetime.fromtimestamp(t0 / 1000.0).hour
        addh(d, hh, "prompts", 1)
        addh(d, hh, "tok", tin + tout)
        addh(d, hh, "tin", tin)
        addh(d, hh, "tout", tout)
        # 这些客户端只有「这一轮的某一刻」，没有起止（TRAE 的会话记忆只写一个
        # message_summary_time）。给它在分钟格上点一笔 —— 至少「那天 AI 动过」
        # 能被看见，而不是和「一整天没开」画成同一个颜色。
        try:
            _mark(marks, t0)
            if secs > 0:
                _mark(marks, t0 + int(secs * 1000))
        except Exception:                    # noqa: BLE001
            pass
        f = it.get("facets") or {}
        if f.get("expert"):
            e = rw["experts"].setdefault(str(f["expert"]),
                                         {"name": str(f["expert"]), "reqs": 0,
                                          "secs": 0.0, "tok": 0})
            e["reqs"] += 1
            e["secs"] += secs
            e["tok"] += tin + tout
        for c in (f.get("connectors") or []):
            nm = str((c or {}).get("name") or "")
            if not nm:
                continue
            q = rw["mcps"].setdefault(nm, {"name": nm, "reqs": 0, "secs": 0.0,
                                           "tok": 0})
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
        dayrec(d)["ai"] += secs
        try:
            h = int(str(key)[11:13])
        except ValueError:
            h = 0
        addh(d, h, "ai", secs)

    # 专家名：dash 有一份「最近用过的几位」清单，查不到就留 id 尾 6 位
    label_of: dict = {}
    try:
        import hostwork
        dash = hostwork._rounds_parser()
    except Exception:                        # noqa: BLE001
        dash = None
    for r in days.values():
        for key, e in r["experts"].items():
            if key not in label_of:
                nm = ""
                if dash is not None and hasattr(dash, "_expert_name"):
                    try:
                        nm = dash._expert_name(key) or ""
                    except Exception:        # noqa: BLE001
                        nm = ""
                label_of[key] = nm or (key[-6:] if key.startswith("ex_") else key)
            e["label"] = label_of[key]

    return {"days": days}


def _with_screen(days_map: dict, sc: dict, since_day: str) -> dict:
    """把屏幕使用时间并进每天：人时长、每小时的人时长、当天的分 App 秒数。

    合并写到「天」这一层，因为存档就是按天存的 —— 记账和存盘必须是同一份数据。
    """
    if not sc.get("ok"):
        return sc
    names = sc.get("names") or {}
    for d, row in (sc.get("days") or {}).items():
        if not d or d < since_day:
            continue
        # 那天可能一条提示词都没发，但人确实在电脑前 —— 也要留一条
        rec = days_map.get(d) or days_map.setdefault(d, _blank_day(d))
        rec["human"] = round(float(row.get("total") or 0), 1)
        rec["has"] = 1
        apps: dict = {}
        for b, secs in (row.get("apps") or {}).items():
            nm = names.get(b) or _NAME_FIX.get(b) or b
            apps[nm] = apps.get(nm, 0.0) + float(secs or 0)
        rec["apps"] = {k: round(v, 1) for k, v in apps.items()}
    hi = HOUR_COLS.index("human")
    for d, hs in (sc.get("hours") or {}).items():
        if not d or d < since_day:
            continue
        rec = days_map.get(d) or days_map.setdefault(d, _blank_day(d))
        for h in range(24):
            rec["hours"][h][hi] = round(float(hs[h] or 0), 1)
    return sc


def _heat(days_map: dict, weeks: int = 13) -> dict:
    """GitHub 那种格子：按周排，一列一周，从上到下周一→周日。

    「人」在系统库还没覆盖到的日子 `has:0`，前端画成空描边 ——
    和「那天真的是 0」必须能分开，否则会以为自己没干活。
    """
    today = date.today()
    # 从今天所在周的周一开始，往前推 weeks 周
    start = today - timedelta(days=today.weekday()) - timedelta(weeks=weeks - 1)
    cells = []
    cur = start
    while cur <= today:
        ds = cur.isoformat()
        r = days_map.get(ds) or {}
        cells.append({
            "d": ds,
            "ai": round(float(r.get("ai") or 0), 1),
            "human": round(float(r.get("human") or 0), 1),
            "has": 1 if r.get("has") else 0,
            "tok": int(r.get("tok") or 0),
            "prompts": int(r.get("prompts") or 0),
        })
        cur += timedelta(days=1)
    return {"from": start.isoformat(), "to": today.isoformat(),
            "cells": cells, "weeks": weeks}


# ---------------------------------------------------------------- 永久存档
# 为什么要它：会话流水会被客户端清理（换版本、清缓存），系统屏幕库只留四周。
# 不落盘的话「上个月每天干了多久」过一阵就再也算不出来 —— 缓存目录可以随便删，
# 这份不行。跟同目录的 Agent自述.jsonl / 进程.jsonl 一个待遇。
_ARCH = {"days": {}, "at": 0.0, "loaded": False}
_ARCH_LOCK = threading.Lock()


def _max_merge(a, b):
    """同一份账取两边的大者。

    源数据只会变少（会话被清、屏幕库滚动），重算不该比存过的少：取大既能
    把今天还在长的数接上，也不会因为源没了把历史抹成 0。真要按源重建，
    用 `workstat.py --rebuild`（force）。
    """
    if isinstance(a, dict) and isinstance(b, dict):
        out = dict(a)
        for k, v in b.items():
            out[k] = _max_merge(out[k], v) if k in out else v
        return out
    if isinstance(a, list) and isinstance(b, list):
        n = max(len(a), len(b))
        out = []
        for i in range(n):
            x = a[i] if i < len(a) else None
            y = b[i] if i < len(b) else None
            out.append(y if x is None else (x if y is None else _max_merge(x, y)))
        return out
    if isinstance(a, bool) or isinstance(b, bool):
        return bool(a) or bool(b)
    if isinstance(a, (int, float)) and isinstance(b, (int, float)):
        return max(a, b)
    if b is None or b == "":
        return a
    if a is None or a == "":
        return b
    return b


def _load_archive() -> dict:
    """读一次进内存。文件缺了/坏了就当没有，下次照常重写一份。"""
    with _ARCH_LOCK:
        if _ARCH["loaded"]:
            return _ARCH["days"]
        days: dict = {}
        try:
            raw = json.loads(ARCHIVE.read_text(encoding="utf-8"))
            if isinstance(raw.get("days"), dict):
                days = raw["days"]
            _ARCH["at"] = float(raw.get("at") or 0)
        except (OSError, ValueError, AttributeError):
            days = {}
        _ARCH["days"] = days
        _ARCH["loaded"] = True
        return days


def _write_archive(days: dict) -> None:
    try:
        ARCHIVE.parent.mkdir(parents=True, exist_ok=True)
        payload = {"v": 1, "at": time.time(),
                   "updated": datetime.now().strftime("%Y-%m-%d %H:%M"),
                   "cols": list(HOUR_COLS), "day_cols": list(DAY_COLS),
                   "note": "每天一条，只增不减；workstat.py --rebuild 才按源重建",
                   "days": days}
        tmp = ARCHIVE.with_suffix(".tmp")
        tmp.write_text(json.dumps(payload, ensure_ascii=False, sort_keys=True),
                       encoding="utf-8")
        os.replace(tmp, ARCHIVE)
    except OSError:
        pass                      # 盘满/只读都别把接口弄挂，缓存还在


def _absorb(new_days: dict, force: bool = False) -> dict:
    """把刚算出来的几天并进存档，返回「存档 ∪ 新算」的全量。"""
    days = _load_archive()
    if not new_days and not force:
        return days
    with _ARCH_LOCK:
        for d, rec in (new_days or {}).items():
            old = days.get(d)
            days[d] = rec if (force or old is None) else _max_merge(old, rec)
        now = time.time()
        if force or now - float(_ARCH["at"] or 0) >= ARCHIVE_SAVE_S:
            _write_archive(days)
            _ARCH["at"] = now
    return days


def _row_out(rec: dict, d: str) -> dict:
    r = {"d": d, "ai": round(float(rec.get("ai") or 0), 1),
         "human": round(float(rec.get("human") or 0), 1)}
    for k in ("tok", "tin", "tout", "prompts", "chars", "links", "imgs", "reqs",
              "ptok"):
        r[k] = int(rec.get(k) or 0)
    return r


def view(days: int = 30, day: str = "", fresh: bool = False,  # noqa: FBT001
         rebuild: bool = False) -> dict:  # noqa: FBT001
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

    bundle = _fold(state, since_day)
    sc = screen(max(days, (today - datetime.strptime(
        since_day, "%Y-%m-%d").date()).days + 1))
    _with_screen(bundle["days"], sc, since_day)
    # 并进永久存档：源数据过期了也还在（force 只有 CLI --rebuild 会用）
    days_map = _absorb(bundle["days"], force=rebuild)
    heat = _heat(days_map, heat_weeks)

    # 明细行：窗口内每一天都得有一行（0 也要），老的日子从存档里补
    rows = [_row_out(days_map.get((today - timedelta(days=i)).isoformat()) or {},
                     (today - timedelta(days=i)).isoformat())
            for i in range(days - 1, -1, -1)]
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
        "ptok": sum(r["ptok"] for r in rows),
    }
    active_days = sum(1 for r in rows if r["prompts"] or r["reqs"])
    human_days = sum(1 for r in rows if days_map.get(r["d"], {}).get("has"))

    # 分维度：窗口内那几天相加（存档里的老日子也算进来）
    ag_t: dict = {}
    md_t: dict = {}
    ex_t: dict = {}
    mc_t: dict = {}
    hist = [0] * (len(LENS) + 1)
    for d, rec in sorted(days_map.items()):     # 按日期升序：同一天的账与顺序无关
        if d < win_from:
            continue
        for nm, a in (rec.get("agents") or {}).items():
            t = ag_t.setdefault(nm, {"name": nm, "prompts": 0, "reqs": 0,
                                     "secs": 0.0, "tin": 0, "tout": 0, "chars": 0})
            for k in ("prompts", "reqs", "tin", "tout", "chars"):
                t[k] += int(a.get(k) or 0)
            t["secs"] += float(a.get("secs") or 0)
        for mk, m in (rec.get("models") or {}).items():
            t = md_t.setdefault(mk, {"name": mk, "calls": 0, "tin": 0, "tout": 0})
            # 名字只认最早那天的写法：同一个模型每天的拼写可能在大小写间跳
            # （DeepSeek / Deepseek），按天覆盖会让榜上的名字跟着最后一天变。
            if t["name"] == mk and m.get("name"):
                t["name"] = m["name"]
            for k in ("calls", "tin", "tout"):
                t[k] += int(m.get(k) or 0)
        for ek, e in (rec.get("experts") or {}).items():
            t = ex_t.setdefault(ek, {"name": e.get("name") or ek,
                                     "label": e.get("label") or ek,
                                     "reqs": 0, "secs": 0.0, "tok": 0})
            t["reqs"] += int(e.get("reqs") or 0)
            t["secs"] += float(e.get("secs") or 0)
            t["tok"] += int(e.get("tok") or 0)
        for qk, q in (rec.get("mcps") or {}).items():
            t = mc_t.setdefault(qk, {"name": qk, "reqs": 0, "secs": 0.0, "tok": 0})
            t["reqs"] += int(q.get("reqs") or 0)
            t["secs"] += float(q.get("secs") or 0)
            t["tok"] += int(q.get("tok") or 0)
        h = rec.get("hist") or []
        for i in range(len(hist)):
            if i < len(h):
                hist[i] += int(h[i] or 0)

    # 各维度的「每条平均」：token 用请求级总量除提示词数，耗时用轮次跨度除轮次
    agents = []
    for a in ag_t.values():
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

    models = sorted(md_t.values(), key=lambda x: -(int(x["tin"]) + int(x["tout"])))
    for m in models:
        m["tok"] = int(m["tin"]) + int(m["tout"])
        m["avg_tok"] = round(m["tok"] / max(1, int(m["calls"])), 1)

    experts = sorted(ex_t.values(), key=lambda x: -int(x["reqs"]))[:12]
    for e in experts:
        e["secs"] = round(float(e["secs"]), 1)
        e["tok"] = int(e["tok"])
        e["avg_sec"] = round(float(e["secs"]) / max(1, int(e["reqs"])), 1)
        e["avg_tok"] = round(e["tok"] / max(1, int(e["reqs"])), 1)
    mcps = sorted(mc_t.values(), key=lambda x: -int(x["reqs"]))[:12]
    for q in mcps:
        q["secs"] = round(float(q["secs"]), 1)
        q["tok"] = int(q["tok"])
        q["avg_sec"] = round(float(q["secs"]) / max(1, int(q["reqs"])), 1)
        q["avg_tok"] = round(q["tok"] / max(1, int(q["reqs"])), 1)

    turn_secs = sum(float(a["secs"]) for a in ag_t.values())
    # 当天那 24 个小时桶：从并存的天记录里取，缺的补 0
    drec = days_map.get(day) or {}
    hs_day = drec.get("hours") or [[0] * len(HOUR_COLS) for _ in range(24)]
    hours = [{c: hs_day[h][i] for i, c in enumerate(HOUR_COLS)} for h in range(24)]
    # 窗口内按钟点累计（各天相加）：卡片上「分小时」看的是长期作息。
    # 只给今天那 24 桶的话，昨天以前全看不见 —— 7 点开工还是 23 点开工，
    # 靠一天的桶根本判不出来。返回里两份都给：hours = 今天，hours_win = 整窗。
    hs_win = [[0.0] * len(HOUR_COLS) for _ in range(24)]
    windays = 0
    for r in rows:
        dd = days_map.get(r.get("d")) or {}
        hh = dd.get("hours")
        if not hh:
            continue
        windays += 1
        for h in range(min(24, len(hh))):
            grp = hh[h] or []
            for i in range(min(len(HOUR_COLS), len(grp))):
                try:
                    hs_win[h][i] += float(grp[i] or 0)
                except (TypeError, ValueError):
                    continue
    hours_win = [{c: round(hs_win[h][i], 1) for i, c in enumerate(HOUR_COLS)}
                 for h in range(24)]
    day_apps = sorted(([k, round(float(v), 1)] for k, v in (drec.get("apps") or {}).items()),
                      key=lambda kv: -kv[1])[:12]
    # 存档本身的状态：卡片上要告诉用户「这些数存在哪、从哪天起」
    try:
        arch_n = len(days_map)
        arch_from = min(days_map) if days_map else ""
        arch_kb = int(ARCHIVE.stat().st_size / 1024) if ARCHIVE.exists() else 0
    except (OSError, ValueError):
        arch_n, arch_from, arch_kb = 0, "", 0
    return {
        "ok": True,
        "at": datetime.now().strftime("%H:%M:%S"),
        "today": date.today().isoformat(),
        "day": day,
        "days": days,
        "range": [since_day, date.today().isoformat()],
        "rows": rows,
        "hours": hours,
        "hours_win": hours_win,
        "hours_win_days": windays,
        "heat": heat,
        "screen": {
            "ok": bool(sc.get("ok")), "why": sc.get("why") or "",
            "src": sc.get("src") or "", "need_fda": bool(sc.get("need_fda")),
            "n_days": int(sc.get("n_days") or 0),
            "span": sc.get("span") or ["", ""],
            "day": day,
            "apps": day_apps,
            "names": sc.get("names") or {},
            "exe": sc.get("exe") or "",
            "from_store": 0 if (sc.get("days") or {}).get(day) else 1 if day_apps else 0,
        },
        "store": {
            "file": str(ARCHIVE), "days": arch_n, "from": arch_from,
            "to": max(days_map) if days_map else "", "kb": arch_kb,
            "day_has": 1 if day in days_map else 0,
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
            # 「人」只按「真有屏幕记录的天数」平摊 —— 系统库只留四周，
            # 除以 90 天会算出「每天 1 小时」这种假数字
            "human_hours_per_day": round(tot["human"] / 3600.0
                                         / max(1, human_days or days), 2),
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
    ap.add_argument("--rebuild", action="store_true",
                    help="按数据源重建存档（默认只增不减，源被清了也不减）")
    a = ap.parse_args(argv)
    if a.screen:
        d = screen(a.days)
        print(json.dumps(d, ensure_ascii=False, indent=1)[:4000])
        return 0
    v = view(a.days, a.day, fresh=a.rebuild, rebuild=a.rebuild)
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
    print(f"\n存档：{v['store']['file']}  {v['store']['days']} 天 "
          f"({v['store']['from']} ~ {v['store']['to']})  {v['store']['kb']} KB")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
