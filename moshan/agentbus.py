#!/usr/bin/env python3
"""Agent 总线 —— 别的 AI（Trae / Cursor / Claude Code / 脚本）读写中台的唯一入口。

为什么单独一个模块：数据形状只定一次，三处共用 ——
  HTTP 接口（server.py 的 /api/agent/*）、
  统计口径（hostwork.py 的 hourly / overview）、
  磁盘上的 jsonl。
谁要改字段就改这里，不会出现「页面算一万、接口算一万二」。

目录（都在 <数据根>/_工作台/agents/，即 ~/Documents/AI项目/_工作台/agents/）：

  usage.jsonl    用量自报。Trae 这类把账本加密的客户端，让它自己报一笔；
                 以后接上真实采集，同 client + model 会自然合并，不用改前端。
  events.jsonl   通用事件。任何 agent 干完活写一条，人和别的 agent 都能读。
  README.md      GET /agent 生成的说明书（每次调用重写，不手改）。
  （小时桶缓存只在内存里，mtime/size 变了才重扫，口径同 hostwork 的小时账）

约定：
  - append-only。写坏了最多丢最后一行，前面的账不会因一次写失败全没。
  - 幂等。带 id 的重复 POST 直接吞掉；不带 id 的按「内容指纹」去重，
    同一个任务一分钟内重复上报只算一次。
  - 时间一律存两列：ts（毫秒整数，机器算）与 at（本地 ISO，人看）。
"""
from __future__ import annotations

import hashlib
import json
import os
import re
import threading
import time
from datetime import datetime, timedelta
from pathlib import Path

V = 1
MAX_TEXT = 4000          # 单条正文上限：够写一段总结，不够塞一整篇文档
MAX_BATCH = 200          # 一次最多几条
KEEP_DAYS = 400          # 自报账留多久（一年出头，够回看）

_LOCK = threading.Lock()


# ---------------------------------------------------------------- 路径

def agents_dir(root) -> Path:
    return Path(root) / "_工作台" / "agents"


def usage_path(root) -> Path:
    return agents_dir(root) / "usage.jsonl"


def events_path(root) -> Path:
    return agents_dir(root) / "events.jsonl"


def ready(root) -> Path:
    """确保目录在，返回它。"""
    d = agents_dir(root)
    d.mkdir(parents=True, exist_ok=True)
    return d


# ---------------------------------------------------------------- 小工具

def _now_ms() -> int:
    return int(time.time() * 1000)


def _iso(ms: int) -> str:
    try:
        return datetime.fromtimestamp(ms / 1000.0).strftime("%Y-%m-%dT%H:%M:%S")
    except (OverflowError, OSError, ValueError):
        return datetime.now().strftime("%Y-%m-%dT%H:%M:%S")


def _to_ms(v) -> int:
    """接受 epoch 秒 / 毫秒 / ISO 字符串；认不出就用现在。

    秒与毫秒靠量级分：小于 10^11 当秒（10^11 秒 = 5138 年，毫秒绝不可能这么小）。
    """
    if v is None or v == "":
        return _now_ms()
    if isinstance(v, (int, float)):
        x = float(v)
        return int(x * 1000) if x < 1e11 else int(x)
    s = str(v).strip()
    if re.fullmatch(r"\d+(\.\d+)?", s):
        return _to_ms(float(s))
    for fmt in ("%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M",
                "%Y-%m-%d %H:%M", "%Y-%m-%d"):
        try:
            return int(datetime.strptime(s[:len(fmt) + 2].strip()[:19], fmt).timestamp() * 1000)
        except ValueError:
            continue
    return _now_ms()


def _int(v) -> int:
    try:
        n = int(float(v))
    except (TypeError, ValueError):
        return 0
    return n if n > 0 else 0


def _num(v):
    if v is None or v == "":
        return None
    try:
        return round(float(v), 4)
    except (TypeError, ValueError):
        return None


def _str(v, n: int) -> str:
    s = str(v if v is not None else "").strip()
    return s[:n]


def _read_jsonl(path: Path) -> list[dict]:
    out: list[dict] = []
    if not path.exists():
        return out
    try:
        with path.open("r", encoding="utf-8", errors="replace") as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    d = json.loads(line)
                except ValueError:
                    continue          # 半行（写到一半断电）跳过，不拖垮整份
                if isinstance(d, dict):
                    out.append(d)
    except OSError:
        return out
    return out


def _append(path: Path, recs: list[dict]) -> int:
    if not recs:
        return 0
    path.parent.mkdir(parents=True, exist_ok=True)
    blob = "".join(json.dumps(r, ensure_ascii=False) + "\n" for r in recs)
    with path.open("a", encoding="utf-8") as f:
        f.write(blob)
        f.flush()
        try:
            os.fsync(f.fileno())
        except OSError:
            pass
    return len(recs)


def _fingerprint(kind: str, r: dict) -> str:
    """内容指纹：同一个任务重复报只留一条。

    进指纹的是一眼能看出「同一笔」的字段；ts 取到分钟，避免同一轮里
    重试两次被算成两笔，又不至于把两小时的活压成一条。
    """
    key = "|".join(str(x) for x in (
        kind, r.get("client"), r.get("model"), int(r.get("ts") or 0) // 60000,
        r.get("in"), r.get("out"), r.get("calls"), r.get("task"), r.get("title"),
        r.get("text"),
    ))
    return hashlib.sha1(key.encode("utf-8")).hexdigest()[:16]


# ---------------------------------------------------------------- 写：用量

def norm_usage(body: dict, src: str = "agent") -> dict | None:
    """一条用量上报 → 落盘记录。字段不全就返回 None（由调用方报错）。"""
    b = body if isinstance(body, dict) else {}
    # client 必须点名：UA 或 "agent" 冒充客户端会把「按 agent」图例变成 curl/8.7.1
    client = _str(b.get("client") or b.get("app"), 40)
    if not client:
        return None
    tin, tout = _int(b.get("in")), _int(b.get("out"))
    cached, calls = _int(b.get("cached")), _int(b.get("calls"))
    if not (tin or tout or calls):
        return None                      # 三个都空 = 没东西可记
    ts = _to_ms(b.get("ts") or b.get("at") or b.get("time"))
    rec = {
        "v": V, "kind": "usage",
        "id": _str(b.get("id"), 64),
        "ts": ts, "at": _iso(ts),
        "client": client,
        "model": _str(b.get("model") or "未知模型", 60),
        "in": tin, "out": tout, "cached": cached,
        "calls": calls or 1,
        "cost": _num(b.get("cost")),
        "currency": _str(b.get("currency") or "CNY", 8),
        "task": _str(b.get("task") or b.get("note"), 120),
        "ref": _str(b.get("ref") or b.get("session") or b.get("url"), 200),
        "src": _str(b.get("src") or src, 40),
    }
    if not rec["id"]:
        rec["id"] = _fingerprint("usage", rec)
    return rec


def add_usage(root, body, src: str = "agent") -> dict:
    """写一条（或一批）用量。body 可以是 dict 或 list。"""
    items = body if isinstance(body, list) else [body]
    items = items[:MAX_BATCH]
    recs, bad = [], 0
    for it in items:
        r = norm_usage(it, src)
        if r is None:
            bad += 1
        else:
            recs.append(r)
    p = usage_path(root)
    with _LOCK:
        have = {str(x.get("id")) for x in _read_jsonl(p) if x.get("id")}
        fresh = []
        for r in recs:
            if r["id"] in have:
                continue
            have.add(r["id"])
            fresh.append(r)
        n = _append(p, fresh)
    # ok 表示「这条请求被正常处理了」：重复不算失败，否则 agent 重试时会以为出错
    return {"ok": bool(recs), "written": n, "dup": len(recs) - n, "bad": bad,
            "path": str(p), "ids": [r["id"] for r in fresh]}


# ---------------------------------------------------------------- 写：事件

_EVENT_KINDS = ("note", "metric", "task", "link", "file", "span")


def norm_event(body: dict, src: str = "agent") -> dict | None:
    b = body if isinstance(body, dict) else {}
    title = _str(b.get("title") or b.get("task"), 120)
    text = _str(b.get("text") or b.get("detail") or b.get("body"), MAX_TEXT)
    data = b.get("data")
    if not (title or text or data):
        return None
    kind = _str(b.get("kind") or "note", 16)
    if kind not in _EVENT_KINDS:
        kind = "note"
    ts = _to_ms(b.get("ts") or b.get("at") or b.get("time"))
    rec = {
        "v": V, "kind": "event",
        "id": _str(b.get("id"), 64),
        "ts": ts, "at": _iso(ts),
        "ns": _str(b.get("ns") or b.get("space") or "杂项", 40),
        "type": kind,
        "title": title,
        "text": text,
        "data": data if isinstance(data, (dict, list)) else None,
        "src": _str(b.get("src") or src, 40),
        "tags": [_str(t, 24) for t in (b.get("tags") or []) if str(t).strip()][:8]
        if isinstance(b.get("tags"), list) else [],
        "ref": _str(b.get("ref") or b.get("url"), 200),
    }
    if not rec["id"]:
        rec["id"] = _fingerprint("event", rec)
    return rec


def add_event(root, body, src: str = "agent") -> dict:
    items = body if isinstance(body, list) else [body]
    items = items[:MAX_BATCH]
    recs, bad = [], 0
    for it in items:
        r = norm_event(it, src)
        if r is None:
            bad += 1
        else:
            recs.append(r)
    p = events_path(root)
    with _LOCK:
        have = {str(x.get("id")) for x in _read_jsonl(p) if x.get("id")}
        fresh = []
        for r in recs:
            if r["id"] in have:
                continue
            have.add(r["id"])
            fresh.append(r)
        n = _append(p, fresh)
    return {"ok": bool(recs), "written": n, "dup": len(recs) - n, "bad": bad,
            "path": str(p), "ids": [r["id"] for r in fresh]}


# ---------------------------------------------------------------- 读

def _since_ms(v) -> int:
    """'7d' / '24h' / 'today' / ISO / epoch → 毫秒起点。空或 all = 0。"""
    s = str(v or "").strip().lower()
    if not s or s in ("all", "0"):
        return 0
    m = re.fullmatch(r"(\d+)\s*([dhm])", s)
    if m:
        n = int(m.group(1))
        unit = {"d": 86400, "h": 3600, "m": 60}[m.group(2)]
        return _now_ms() - n * unit * 1000
    if s == "today":
        return int(datetime.now().replace(hour=0, minute=0, second=0,
                                          microsecond=0).timestamp() * 1000)
    if s in ("yesterday", "昨天"):
        t = datetime.now().replace(hour=0, minute=0, second=0, microsecond=0)
        return int((t - timedelta(days=1)).timestamp() * 1000)
    return _to_ms(v)


def read(root, ns: str = "all", since="7d", limit: int = 100, q: str = "",
         client: str = "") -> dict:
    """读自报账。ns: usage / events / all。"""
    since_ms = _since_ms(since)
    want = (ns or "all").strip().lower()
    try:
        limit = max(1, min(int(limit), 1000))
    except (TypeError, ValueError):
        limit = 100
    out: list[dict] = []
    if want in ("all", "usage", "用量"):
        out.extend(_read_jsonl(usage_path(root)))
    if want in ("all", "events", "event", "事件"):
        out.extend(_read_jsonl(events_path(root)))
    if since_ms:
        out = [x for x in out if int(x.get("ts") or 0) >= since_ms]
    if client:
        out = [x for x in out if str(x.get("client") or x.get("src") or "") == client]
    if q:
        ql = q.lower()

        def hit(x: dict) -> bool:
            return ql in json.dumps(x, ensure_ascii=False).lower()

        out = [x for x in out if hit(x)]
    out.sort(key=lambda x: int(x.get("ts") or 0), reverse=True)
    return {"ok": True, "n": len(out), "items": out[:limit],
            "since": _iso(since_ms) if since_ms else "all",
            "files": {"usage": str(usage_path(root)),
                      "events": str(events_path(root))}}


# ---------------------------------------------------------------- 统计：小时桶

_HOUR_CACHE: dict[str, object] = {"at": 0.0, "key": "", "hours": {}}


def _parse_usage_line(d: dict) -> tuple[str, dict] | None:
    ts = int(d.get("ts") or 0)
    if not ts:
        return None
    try:
        k = datetime.fromtimestamp(ts / 1000.0).strftime("%Y-%m-%dT%H")
    except (OverflowError, OSError, ValueError):
        return None
    tin, tout = _int(d.get("in")), _int(d.get("out"))
    cached, calls = _int(d.get("cached")), _int(d.get("calls")) or 1
    model = str(d.get("model") or "未知模型")[:60]
    app = str(d.get("client") or "agent")[:24]
    return k, {"in": tin, "out": tout, "cached": cached, "calls": calls,
               "models": {model: {"in": tin, "out": tout, "cached": cached, "calls": calls}},
               "apps": {app: {"in": tin, "out": tout, "cached": cached, "calls": calls}}}


def hour_buckets(root, days: int = 30) -> dict:
    """把 usage.jsonl 折成 hostwork 那种小时桶（"YYYY-MM-DDTH" → 四个数 + models/apps）。

    按文件大小增量：文件只追加，所以上次读到的字节偏移之后的才是新的。
    文件变小（被清空 / 换盘）就从头重扫。
    """
    p = usage_path(root)
    try:
        st = p.stat()
    except OSError:
        return {}
    key = f"{st.st_ino}:{st.st_size}:{int(st.st_mtime)}"
    with _LOCK:
        if _HOUR_CACHE.get("key") == key:
            return _HOUR_CACHE.get("hours") or {}
        hours: dict[str, dict] = _HOUR_CACHE.get("hours") or {}
        prev_key = str(_HOUR_CACHE.get("key") or "")
        prev_size = int(prev_key.split(":")[1]) if ":" in prev_key else 0
        prev_ino = prev_key.split(":")[0] if ":" in prev_key else ""
        same = prev_ino == str(st.st_ino) and prev_size <= st.st_size
        if not same:
            hours = {}
            prev_size = 0
        try:
            with p.open("rb") as f:
                f.seek(prev_size)
                buf = f.read()
        except OSError:
            return hours
        cut = buf.rfind(b"\n")
        if cut < 0:
            return hours
        for raw in buf[:cut + 1].splitlines():
            try:
                d = json.loads(raw)
            except ValueError:
                continue
            if not isinstance(d, dict):
                continue
            got = _parse_usage_line(d)
            if not got:
                continue
            k, v = got
            cur = hours.setdefault(k, {"in": 0, "out": 0, "cached": 0, "calls": 0,
                                       "models": {}, "apps": {}})
            for f in ("in", "out", "cached", "calls"):
                cur[f] += v[f]
            for dim in ("models", "apps"):
                for name, mv in v[dim].items():
                    c = cur[dim].setdefault(
                        name, {"in": 0, "out": 0, "cached": 0, "calls": 0})
                    for f in ("in", "out", "cached", "calls"):
                        c[f] += mv[f]
        keep = {(datetime.now() - timedelta(days=i)).strftime("%Y-%m-%d")
                for i in range(days)}
        hours = {k: v for k, v in hours.items() if k[:10] in keep}
        _HOUR_CACHE.update({"key": key, "hours": hours, "at": time.time()})
        return hours


# ---------------------------------------------------------------- 统计：按天 / 按模型

def by_day(root, days: int = 7) -> dict[str, dict]:
    """按天汇总，形状与 hostwork sessions_snapshot 的 days 一致，直接并进 token 总账。"""
    since = _now_ms() - days * 86400 * 1000 if days else 0
    out: dict[str, dict] = {}
    for d in _read_jsonl(usage_path(root)):
        ts = int(d.get("ts") or 0)
        if since and ts < since:
            continue
        day = _iso(ts)[:10]
        b = out.setdefault(day, {"in": 0, "out": 0, "cached": 0, "calls": 0, "models": {}})
        tin, tout = _int(d.get("in")), _int(d.get("out"))
        cached, calls = _int(d.get("cached")), _int(d.get("calls")) or 1
        b["in"] += tin
        b["out"] += tout
        b["cached"] += cached
        b["calls"] += calls
        m = b["models"].setdefault(str(d.get("model") or "未知模型"),
                                   {"in": 0, "out": 0, "cached": 0, "n": 0,
                                    "cost": None})
        m["in"] += tin
        m["out"] += tout
        m["cached"] += cached
        m["n"] += calls
        c = _num(d.get("cost"))
        if c is not None:
            m["cost"] = round((m["cost"] or 0) + c, 4)
    return out


def totals(root, days: int = 7) -> dict:
    rows = by_day(root, days)
    t = {"in": 0, "out": 0, "cached": 0, "calls": 0}
    for b in rows.values():
        for k in t:
            t[k] += b[k]
    t["total"] = t["in"] + t["out"]
    t["days"] = len(rows)
    return t


# ---------------------------------------------------------------- 说明书

def readme(root) -> str:
    """给 agent 看的一页说明。内容随实际写入口径生成，不维护第二份文档。"""
    d = agents_dir(root)
    return f"""# 个人中台 · Agent 读写说明

这个中台是反重力猫头鹰的本机数据台。你是外部 agent（Trae / Cursor / 脚本），
下面是你**唯一需要知道**的一套接口。不要直接改 dash.html 或 pocket 的代码。

## 0. 怎么连

本机：`http://127.0.0.1:8323`（明文口，回环免令牌）
别处：同上地址换成主机 IP，并带 `-H "x-token: <令牌>"`（令牌在 pocket/config.json）

先自检：`curl -s http://127.0.0.1:8323/api/ping`

## 1. 写用量（trae 消耗的 token 就报这里）

```
POST /api/agent/usage
Content-Type: application/json

{{"client":"trae","model":"claude-sonnet-4.5",
 "in":12000,"out":3400,"cached":0,"calls":3,
 "task":"改合表列序","ref":"会话id或链接"}}
```

| 字段 | 必填 | 说明 |
|---|---|---|
| client | 是 | 哪个客户端在报：trae / cursor / claude-code / 你的名字 |
| in / out | 至少一个 >0 | 输入 / 输出 token；不知道就填 0，但不能全空 |
| cached | 否 | 缓存命中的输入 token |
| calls | 否 | 请求次数，缺省 1 |
| model | 否 | 模型名，缺省「未知模型」 |
| ts | 否 | 时间，缺省现在。支持 epoch 秒/毫秒、`2026-11-21T21:50` |
| cost | 否 | 真金白银（元）。订阅制不知道就不填，别自己编 |
| id | 否 | 幂等键。不填按内容指纹去重，同一任务一分钟内重复 POST 只算一次 |
| ref | 否 | 会话 id / 链接，方便回查 |

批量：`body` 直接给数组（最多 {MAX_BATCH} 条）。

## 2. 写事件（干完活留一条）

```
POST /api/agent/event
{{"ns":"设备","type":"note","title":"给空调加了定时",
 "text":"……","data":{{"k":"v"}},"tags":["自动化"]}}
```

`ns` 是归档槽位，建议用现有这些，别新造同义词：
`设备` `伙食` `账目` `工作` `阅读` `系统` `杂项`

`type`: note / metric / task / link / file / span。

## 3. 读

```
GET /api/agent/read?ns=all&since=7d&limit=100&client=trae&q=关键词
```

`ns`: all / usage / events。`since`: `24h` `7d` `today` 或具体日期。

## 4. 现状摘要（别一个个接口翻）

```
GET /api/agent/brief
```

返回今天的 token、花费、最近工作、设备数、自报账最近几条。

## 5. 数据在哪

{root}

  {usage_path(root)}
  {events_path(root)}

两份都是 append-only 的 JSONL，一行一条。你可以直接 tail / grep ——
写接口和磁盘是同一份数据，不存在同步问题。

## 6. 中台里还有什么

先读 `{root}/README.md`（完整地图）。常用的几处：

    13-设备/档案/       设备台账，一台一个 json
    09-个人工作台/      待办 / 行程 / 备忘 / 日记
    12-记账/            账目
    04-存图/ 08-阅读/   存下来的图文
    _工作台/dash.html   工作台页面本体（别手改；改前先 cp 一份 .bak）

要全量状态而不只是摘要：`GET /api/dash?k=<令牌>` —— 几百 KB，别每轮都拉。

## 7. 不想拼 curl？用命令行

```
{agents_dir(root)}/zt brief
{agents_dir(root)}/zt usage --client trae --model claude-sonnet-4.5 --in 12000 --out 3400 --task "改列序"
{agents_dir(root)}/zt log --ns 工作 --title "给合表加了列拖拽"
{agents_dir(root)}/zt read --ns usage --since 7d --limit 5
```

（`zt` 就是个包装层，底层还是上面那几个接口；从别的机器调时加
`--base https://<主机>:8321` 并设 `ZT_TOKEN=<令牌>`。）

## 8. 给中台加一个第三方程序

中台的插件分两种：自己的服务（有端口、有 .command）走 `plugins.json`；别人写的
桌面程序走「配方」—— 一条 JSON 说清怎么认装没装、怎么装、怎么开退。

    GET  /api/ext/spec            # 字段说明 + 示例 + 提示词（JSON）
    GET  /api/ext/prompt?goal=    # 纯文本提示词，可直接喂给模型
    POST /api/ext/parse           # {{"text":"<模型回答原文>"}} → 配方草稿（不落盘）
    POST /api/ext/add             # {{"recipe":{{...}}}} → 落盘并加进顶栏

提示词模板在 `{root}/_工作台/第三方插件-Prompt.md`（可改）。最容易出错的
是 `brew` 包名：桌面程序是 `brew install --cask <名>`，命令行是 `brew install <名>`；
不确定就别写 brew（用户装完照样能用），**不要编一个包名**。

## 9. 不要做的

- 不要写超过 {MAX_TEXT} 字的正文；大段材料放文件，事件里只放路径。
- 不要 `PATCH` / 删除：只追加。要改就再写一条新的（带 `ref` 指回旧的）。
- 不要把令牌写进日志或提交到仓库。
"""
