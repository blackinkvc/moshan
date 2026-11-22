#!/usr/bin/env python3
"""把别的 Agent 接进中台：认人 → 装说明 → 收回报 → 导数据。

为什么不是「给用户一段提示词让他贴进 Trae」：
    Trae CN 的用户级技能目录 ~/.trae-cn/skills/<名字>/SKILL.md 是**每次会话都会列出来**的，
    装一次等于永久生效；贴一次提示词只管一轮。所以这里把说明书直接写进那个目录，
    用户点一下按钮，Trae 下次开会话就知道中台是什么、该把什么报回来。

为什么还要「收回报」：
    中台自己也能把候选路径挨个 stat 一遍，但只有 agent 自己知道它这一版把数据放哪了
    （换了版本、用户改过目录、或者还有一份中台根本不知道的账）。所以先给它猜一份，
    让它核对/补全再 POST 回来，导入按它说的走 —— 这就是「让 Trae 告诉中台去哪找数据」。

导的是真数据，不是截图 OCR：
    Trae 的对话正文在加密库里读不到，但 memory/projects 下每个会话有一份
    session_memory_*.jsonl（intent / actions / outcome / learned 四段），那是它自己写的
    结构化成因后果。导进中台自报账（agents/events.jsonl）后，「最近工作」里就能看见
    「Trae 那天想干什么、动了什么、结论是什么」。
"""
from __future__ import annotations

import hashlib
import json
import os
import platform
import re
import time
from datetime import datetime
from pathlib import Path

try:
    import agentbus
except Exception:                                       # noqa: BLE001  （单独跑调试时）
    agentbus = None

V = 1
MAX_REPORT_PATHS = 40

# ---------------------------------------------------------------- 认人
# 每家一套候选根目录：装没装看根在不在，数据在哪看里面的子目录。
# 路径模板里的 home/appdata/appsupport 由 _root_of() 展开，Windows 与 macOS 共用一份表。
AGENTS = [
    {
        "id": "trae", "name": "Trae", "kind": "skill",
        "note": "对话正文在加密库里，但 memory/ 下的会话记忆是明文",
        "roots": {
            "darwin": [("home", ".trae-cn"), ("appsupport", "Trae CN"),
                       ("appsupport", "Trae")],
            "win": [("home", ".trae-cn"), ("appdata", "Trae CN"),
                    ("appdata", "Trae")],
            "linux": [("home", ".trae-cn")],
        },
        "skill": ("home", ".trae-cn/skills/pocket-bridge/SKILL.md"),
    },
    {
        "id": "cursor", "name": "Cursor", "kind": "rule",
        "note": "规则是 .cursor/rules/*.mdc，alwaysApply 才会每轮带上",
        "roots": {
            "darwin": [("home", ".cursor"), ("appsupport", "Cursor")],
            "win": [("home", ".cursor"), ("appdata", "Cursor")],
            "linux": [("home", ".cursor")],
        },
        "skill": ("home", ".cursor/rules/pocket-bridge.mdc"),
    },
    {
        "id": "claude", "name": "Claude Code", "kind": "skill",
        "note": "技能目录 ~/.claude/skills/",
        "roots": {"any": [("home", ".claude")]},
        "skill": ("home", ".claude/skills/pocket-bridge/SKILL.md"),
    },
    {
        "id": "workbuddy", "name": "WorkBuddy", "kind": "skill",
        "note": "技能目录 ~/.workbuddy/skills/",
        "roots": {"any": [("home", ".workbuddy")]},
        "skill": ("home", ".workbuddy/skills/pocket-bridge/SKILL.md"),
    },
]


def _plat() -> str:
    if os.name == "nt":
        return "win"
    return "darwin" if platform.system() == "Darwin" else "linux"


def _root_of(kind: str) -> Path | None:
    if kind == "home":
        return Path.home()
    if kind == "appdata":
        v = os.environ.get("APPDATA")
        return Path(v) if v else None
    if kind == "appsupport":
        return Path.home() / "Library" / "Application Support"
    if kind == "localappdata":
        v = os.environ.get("LOCALAPPDATA")
        return Path(v) if v else None
    return None


def _homes() -> list[Path]:
    """这台机器的「家目录」候选。

    Windows 上中台常常是以服务身份跑的（计划任务 default 用户 / SYSTEM），那时
    Path.home() 会指到 C:\\WINDOWS\\system32\\config\\systemprofile —— 那里面
    当然没有 Trae 的数据，真正的在 C:\\Users\\<你>\\.trae-cn 里。所以家目录要
    按目录扫一遍，谁有 .trae-cn / .cursor 就在谁那儿干活。
    """
    out = [Path.home()]
    if os.name == "nt":
        skip = {"public", "default", "default user", "all users", "defaultapppool"}
        try:
            users = Path(os.environ.get("SystemDrive", "C:") + "/Users")
            for d in users.iterdir():
                if d.is_dir() and d.name.lower() not in skip:
                    out.append(d)
        except OSError:
            pass
    seen, uniq = set(), []
    for p in out:
        k = str(p).lower()
        if k not in seen:
            seen.add(k)
            uniq.append(p)
    return uniq


# 公开别名：本机工作那边也要认这几个家目录（Windows 服务账号下 Path.home()
# 指到 systemprofile，扫不到真人的数据）。同一份逻辑，别再抄第二遍。
homes = _homes


def _expand(spec) -> list[Path]:
    if isinstance(spec[0], (list, tuple)):
        spec = spec[0]
    kind = spec[0]
    if kind in ("home", "appdata", "localappdata"):
        # APPDATA 环境变量在服务账号下同样指错人（systemprofile），一样按家目录推
        sub = {"home": "", "appdata": "AppData/Roaming",
               "localappdata": "AppData/Local"}[kind]
        out = []
        for h in _homes():
            base = (h / sub) if sub else h
            out.append((base / spec[1]) if len(spec) > 1 else base)
        return out
    base = _root_of(kind)
    return [base / spec[1] if len(spec) > 1 else base] if base else []


def _roots_of(a: dict) -> list[Path]:
    table = a.get("roots") or {}
    spec = table.get(_plat()) or table.get("any") or []
    out: list[Path] = []
    for s in spec:
        out.extend(_expand(s))
    return out


def _skill_path(a: dict) -> Path | None:
    """技能该写到哪个家目录下：挑那个根目录（.trae-cn / .cursor / .claude）真存在的。
    一个都不存在时退回第一个 —— 装说明本来就是为了让它存在。"""
    cands = _expand(a["skill"])
    for p in cands:
        for h in _homes():
            try:
                rel = p.relative_to(h)
            except ValueError:
                continue
            if (h / rel.parts[0]).exists():
                return p
    return cands[0] if cands else None


_SCAN_CACHE: dict = {"at": 0.0, "root": "", "val": None}
SCAN_TTL = 20          # 秒。面板反复开关不该每次都把几百 MB 走一遍


def _walk_stats(p: Path, cap: int = 5000) -> dict:
    """目录有多少文件、多大、最后改到什么时候。走不完就按走到的算，不报错。"""
    n, size, newest, stopped = 0, 0, 0.0, False
    if not p.exists():
        return {"files": 0, "mb": 0.0, "newest": "", "stopped": False}
    try:
        for i, f in enumerate(p.rglob("*")):
            if i >= cap:
                stopped = True
                break
            try:
                if f.is_file():
                    st = f.stat()
                    n += 1
                    size += st.st_size
                    newest = max(newest, st.st_mtime)
            except OSError:
                continue
    except OSError:
        pass
    return {"files": n, "mb": round(size / 1048576, 1), "stopped": stopped,
            "newest": (datetime.fromtimestamp(newest).strftime("%Y-%m-%d %H:%M")
                       if newest else "")}


# ---------------------------------------------------------------- Trae 的证据点
def _count(pattern: str, base: Path, cap: int = 5000) -> tuple[int, str]:
    """数一下这类文件有多少条、最后一条是什么时候。数不完就按数到的算。"""
    n, newest = 0, 0.0
    try:
        for i, f in enumerate(base.rglob(pattern)):
            if i >= cap:
                break
            n += 1
            try:
                newest = max(newest, f.stat().st_mtime)
            except OSError:
                continue
    except OSError:
        pass
    return n, (datetime.fromtimestamp(newest).strftime("%Y-%m-%d %H:%M") if newest else "")


def trae_evidence() -> list[dict]:
    """Trae 这边真正能读的三处明文。读不到的那份加密库也列出来，好让人知道为什么。"""
    out = []
    for base in _roots_of(AGENTS[0]):
        if base.name == ".trae-cn":
            mem = base / "memory" / "projects"
            if mem.exists():
                n, at = _count("session_memory_*.jsonl", mem)
                out.append({"key": "memory", "label": "会话记忆", "path": str(mem),
                            "what": "intent/actions/outcome/learned，明文，能直接导入",
                            "n": n, "at": at})
            for sub, label in (("mcps", "MCP 任务"), ("skills", "技能"),
                               ("builtin", "内置映射")):
                q = base / sub
                if q.exists():
                    out.append({"key": sub, "label": label, "path": str(q), "what": "辅助"})
        else:
            cmd = base / "ModularData" / "ai-agent" / "command-execution"
            if cmd.exists():
                n, at = _count("*/*", cmd, cap=2000)
                out.append({"key": "commands", "label": "命令执行", "path": str(cmd),
                            "what": "每条命令的 cwd / 起止时间，能算「真干了多久」",
                            "n": n, "at": at})
            db = base / "ModularData" / "ai-agent" / "database.db"
            if db.exists():
                st = db.stat()
                head = b""
                try:
                    with db.open("rb") as f:
                        head = f.read(4)
                except OSError:
                    pass
                out.append({"key": "db", "label": "对话库", "path": str(db),
                            "what": "整库加密（文件头 %s），读不到正文，只能拿体积和时间"
                                    % head.hex() if head else "整库加密，读不到正文",
                            "locked": True, "mb": round(st.st_size / 1048576, 1)})
    return out


# ---------------------------------------------------------------- 状态
def _links_path(root: Path) -> Path:
    return Path(root) / "_工作台" / "agents" / "links.json"


def _load_links(root: Path) -> dict:
    p = _links_path(root)
    try:
        d = json.loads(p.read_text(encoding="utf-8"))
        return d if isinstance(d, dict) else {}
    except (OSError, ValueError):
        return {}


def _save_links(root: Path, d: dict) -> None:
    p = _links_path(root)
    try:
        p.parent.mkdir(parents=True, exist_ok=True)
        tmp = p.with_suffix(".json.tmp")
        tmp.write_text(json.dumps(d, ensure_ascii=False, indent=2), encoding="utf-8")
        tmp.replace(p)
    except OSError:
        pass


def scan(root, force: bool = False) -> dict:
    """这台机器上装了哪些 agent、数据在哪、说明书装没装、上次回报和导入是什么时候。

    走目录是这里最慢的一步（Trae 的数据动辄几百 MB），所以结果缓存 20 秒：
    用户关掉面板再点开，不该再等一遍。
    """
    root = Path(root)
    key = f"{root}|{time.time() // SCAN_TTL}"
    if not force and _SCAN_CACHE["val"] is not None and _SCAN_CACHE["key"] == key:
        return _SCAN_CACHE["val"]
    links = _load_links(root)
    items = []
    for a in AGENTS:
        roots = []
        for r in _roots_of(a):
            if not r.exists():
                roots.append({"path": str(r), "exists": False})
                continue
            st = _walk_stats(r)
            roots.append({"path": str(r), "exists": True, **st})
        sp = _skill_path(a)
        skill = {"path": str(sp) if sp else "", "installed": bool(sp and sp.exists()),
                 "at": ""}
        if skill["installed"]:
            try:
                skill["at"] = datetime.fromtimestamp(
                    sp.stat().st_mtime).strftime("%Y-%m-%d %H:%M")
            except OSError:
                pass
        ev = trae_evidence() if a["id"] == "trae" else []
        rec = links.get(a["id"]) or {}
        items.append({
            "id": a["id"], "name": a["name"], "kind": a["kind"], "note": a.get("note", ""),
            "found": any(x["exists"] for x in roots),
            "roots": roots, "skill": skill,
            "evidence": [{"key": e["key"], "label": e["label"], "path": e["path"],
                          "what": e.get("what", ""), "locked": bool(e.get("locked")),
                          "n": e.get("n"), "at": e.get("at", "")}
                         for e in ev],
            "report": rec.get("report") or {},
            "import": rec.get("import") or {},
        })
    out = {"ok": True, "v": V, "platform": _plat(),
           "now": datetime.now().strftime("%Y-%m-%d %H:%M"),
           "agents": items}
    _SCAN_CACHE.update({"key": key, "val": out})
    return out


# ---------------------------------------------------------------- 说明书（技能 / 规则）
_PROMPT = """---
name: pocket-bridge
description: 本机「个人中台」的接入说明。用户提到中台 / 工作台 / 记账 / 设备 / 存图 / 阅读 / 工作日志，或要把这一轮干的活记进中台、查中台现状时使用。首次使用会先核对数据目录并把结论回报给中台。
---

# 中台桥（{agent} → 个人中台）

这台机器上有一个「个人中台」，是本机数据的总台：账号账单、设备台账、存图、阅读、
工作流水、各 AI 客户端干的活，都在它那儿。它现在想知道**你（{agent}）的数据放在哪**，
以后好自己来取。

## 一、它是什么

| 项 | 值 |
|---|---|
| 服务地址 | `{base}`（本机回环免令牌） |
| 页面 | `{base}/dash` |
| 给 agent 的说明书 | `{base}/agent` |
| 能力清单 | `{base}/api/agent/schema` |
| 现状摘要 | `{base}/api/agent/brief` |

能做的三件事：报 token 用量、写一条事件（干完活留一笔）、读中台现状。
字段和口径全在 `/agent` 里，别自己发明新字段。

## 二、第一次：把你的数据目录告诉它

它先猜了一份，你核对：

```bash
curl -s "{base}/api/agent/link/scan"
```

看看 `agents[].evidence` 里那些路径，你自己机器上是不是这些地方：

- 会话记忆 `~/.trae-cn/memory/projects`（明文，一条一条的 intent / outcome）
- 命令执行 `.../ModularData/ai-agent/command-execution`（明文，有 cwd 和时间）
- 对话库 `database.db`（整库加密，读不到正文，别浪费时间）

**核对完把结论回报给它**（路径要真存在；新增的你也可以补上）：

```bash
curl -s -X POST "{base}/api/agent/link/report" \\
  -H 'content-type: application/json' \\
  -d '{{"agent":"{agent}","paths":[{{"key":"memory","path":"<你确认的路径>"}}],"note":"<一句话说明>"}}'
```

Windows PowerShell 版：

```powershell
Invoke-RestMethod -Uri "{base}/api/agent/link/report" -Method Post `
  -ContentType 'application/json' -Body '{{"agent":"{agent}","paths":[...]}}'
```

回报完它就能自己导了（你不用管，它按你给的路径读）：

```bash
curl -s -X POST "{base}/api/agent/link/import" -H 'content-type: application/json' -d '{{"agent":"{agent}"}}'
```

## 三、以后每一轮

任务结束（或用户说「记一下」）报一笔，别报问候和纯查询：

```bash
curl -s -X POST "{base}/api/agent/usage" -H 'content-type: application/json' \\
  -d '{{"client":"{agent}","model":"<模型>","in":<估>,"out":<估>,"calls":<轮数>,"task":"<20 字>"}}'
```

产出值得留档的（改了脚本、定了方案）再加一条事件：

```bash
curl -s -X POST "{base}/api/agent/event" -H 'content-type: application/json' \\
  -d '{{"ns":"工作","type":"task","title":"<一句话>","text":"<两三句结论>"}}'
```

token 估不出来就 in/out 写 0、只填 calls，**不要编数字**。

## 四、边界

- 只走 `{base}`，别改 `dash.html`，别动中台其它端口的配置。
- 只追加不删改：要更正就再写一条带 `ref` 的。
- 不要把令牌写进代码或日志。
"""


def prompt(agent_id: str, base: str = "http://127.0.0.1:8323") -> str:
    a = next((x for x in AGENTS if x["id"] == agent_id), None)
    if a is None:
        return ""
    return _PROMPT.format(agent=a["name"], base=base.rstrip("/"))


def _rule_wrap(agent_id: str, body: str) -> str:
    """Cursor 的 .mdc 要 frontmatter，Trae 的技能是 SKILL.md —— 内容一样，头不一样。"""
    a = next((x for x in AGENTS if x["id"] == agent_id), None)
    if a and a["kind"] == "rule":
        b = body.split("---", 2)[-1].lstrip("\n")      # 去掉 SKILL.md 那层头
        return ("---\ndescription: 本机个人中台的接入说明（pocket 中台）\n"
                "globs:\nalwaysApply: true\n---\n\n" + b)
    return body


def install(root, agent_id: str, base: str = "http://127.0.0.1:8323") -> dict:
    a = next((x for x in AGENTS if x["id"] == agent_id), None)
    if a is None:
        return {"ok": False, "error": f"不认识这个 agent：{agent_id}"}
    p = _skill_path(a)
    if p is None:
        return {"ok": False, "error": "这台机器的路径拼不出来（缺 APPDATA？）"}
    txt = _rule_wrap(agent_id, prompt(agent_id, base))
    try:
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(txt, encoding="utf-8")
    except OSError as e:
        return {"ok": False, "error": f"写不进去：{e}", "path": str(p)}
    links = _load_links(Path(root))
    rec = links.setdefault(agent_id, {})
    rec["prompt"] = {"at": datetime.now().strftime("%Y-%m-%d %H:%M"),
                     "path": str(p), "base": base, "bytes": len(txt.encode("utf-8"))}
    _save_links(Path(root), links)
    return {"ok": True, "path": str(p), "agent": agent_id, "bytes": rec["prompt"]["bytes"]}


# ---------------------------------------------------------------- 收回报
def report(root, body: dict) -> dict:
    """agent 核对完路径后的回执。存起来，导入时优先按它说的走。"""
    root = Path(root)
    agent_id = str((body or {}).get("agent") or "").strip() or "unknown"
    paths = []
    raw = (body or {}).get("paths")
    if isinstance(raw, dict):
        raw = [{"key": k, "path": v} for k, v in raw.items()]
    for x in (raw or [])[:MAX_REPORT_PATHS]:
        if isinstance(x, str):
            paths.append({"key": "", "path": x, "exists": Path(x).exists()})
        elif isinstance(x, dict):
            p = str(x.get("path") or "").strip()
            if not p:
                continue
            paths.append({"key": str(x.get("key") or ""), "path": p,
                          "what": str(x.get("what") or ""),
                          "exists": Path(p).exists()})
    note = str((body or {}).get("note") or "")[:600]
    rec = {"at": datetime.now().strftime("%Y-%m-%d %H:%M"),
           "via": str((body or {}).get("via") or "skill")[:20],
           "platform": str((body or {}).get("platform") or _plat()),
           "paths": paths, "note": note}
    links = _load_links(root)
    links.setdefault(agent_id, {})["report"] = rec
    _save_links(root, links)
    if agentbus is not None:
        n = len([p for p in paths if p["exists"]])
        agentbus.add_event(root, {
            "ns": "系统", "type": "note", "src": "agent_link",
            "id": f"link-report-{agent_id}-{int(time.time())}",
            "title": f"{agent_id} 接上了中台（{n} 个路径）",
            "text": note or "agent 回报了数据目录，中台按它自己说的位置取数。",
            "data": {"agent": agent_id, "paths": paths[:MAX_REPORT_PATHS]},
            "tags": ["接入"],
        }, src="agent_link")
    return {"ok": True, "agent": agent_id, "paths": len(paths),
            "alive": len([p for p in paths if p["exists"]])}


# ---------------------------------------------------------------- 导入
_DAY_RE = re.compile(r"^\d{8}$")


def _proj_hint(name: str) -> str:
    """memory 的目录名是「路径把 / 换成 -」再加 --p2-<hash>。能还原成真实路径就还原，
    还原不出来（路径里有空格、有中文）就留空 —— 宁可少一行，也不要给人一个假路径。"""
    s = re.sub(r"--p2-[0-9a-f]+$", "", name).strip("-")
    if not s:
        return ""
    cand = "/" + s.replace("-", "/")
    return cand if Path(cand).exists() else ""


def _trae_memory_dirs(root: Path) -> list[Path]:
    """优先用 agent 回报的路径，其次按注册表猜。"""
    out = []
    rep = (_load_links(root).get("trae") or {}).get("report") or {}
    for p in rep.get("paths") or []:
        q = Path(p.get("path") or "")
        if q.exists() and q.is_dir() and ("memory" in str(q)):
            out.append(q / "projects" if (q / "projects").exists() else q)
    if not out:
        for base in _roots_of(AGENTS[0]):
            mem = base / "memory" / "projects"
            if mem.exists():
                out.append(mem)
    return out


def _mem_ts(d: dict, fallback: float) -> float:
    """这一轮的真实时间（秒）。

    行里带着 message_summary_time 就用它。以前整份 jsonl 的每一行都拿文件
    mtime 当时间 —— 一个会话文件里写着几十轮历史，于是它们全撞在同一秒
    （EVO 上 60 条记录只剩两个时间，卡片看着像假的）。读不到才退 mtime。
    """
    raw = str(d.get("message_summary_time") or "").strip()[:19]
    if raw:
        try:
            return datetime.strptime(raw, "%Y-%m-%d %H:%M:%S").timestamp()
        except (ValueError, OSError):
            pass
    return fallback


def _import_trae(root: Path, days: int) -> dict:
    """memory/projects/<项目>/<yyyymmdd>/session_memory_*.jsonl → 中台事件。

    一条会话记忆就是一段「想干什么 / 动了什么 / 结果怎样」，正好是工作流水要的粒度。
    去重靠 id（项目+日期+会话），同一份重复导不会翻倍。
    """
    if agentbus is None:
        return {"ok": False, "error": "agentbus 没导进来"}
    since = time.time() - max(1, days) * 86400
    dirs = _trae_memory_dirs(root)
    if not dirs:
        return {"ok": False, "error": "找不到 Trae 的 memory/projects",
                "hint": "先点「装说明」，在 Trae 里让它核对一次路径"}
    scanned = imported = dup = bad = 0
    projects = set()
    wl_recs: list[dict] = []
    for base in dirs:
        try:
            proj_dirs = [p for p in base.iterdir() if p.is_dir()]
        except OSError:
            continue
        for proj in proj_dirs:
            for day in sorted([p for p in proj.iterdir() if p.is_dir()
                               and _DAY_RE.match(p.name)]):
                for f in sorted(day.glob("session_memory_*.jsonl")):
                    try:
                        if f.stat().st_mtime < since:
                            continue
                    except OSError:
                        continue
                    try:
                        lines = f.read_text(encoding="utf-8").splitlines()
                    except OSError:
                        continue
                    for ln in lines:
                        ln = ln.strip()
                        if not ln:
                            continue
                        try:
                            d = json.loads(ln)
                        except ValueError:
                            bad += 1
                            continue
                        if not isinstance(d, dict):
                            bad += 1
                            continue
                        scanned += 1
                        intent = re.sub(r"\s+", " ", str(d.get("intent") or "")).strip()
                        outcome = re.sub(r"\s+", " ", str(d.get("outcome") or "")).strip()
                        actions = [re.sub(r"\s+", " ", str(a)).strip()
                                   for a in (d.get("actions") or []) if str(a).strip()][:8]
                        learned = [re.sub(r"\s+", " ", str(a)).strip()
                                   for a in (d.get("learned") or []) if str(a).strip()][:6]
                        if not (intent or outcome or actions):
                            bad += 1
                            continue
                        sid = f.stem.replace("session_memory_", "")
                        key = hashlib.sha1(
                            f"{proj.name}|{day.name}|{sid}|{intent[:60]}".encode()).hexdigest()[:16]
                        try:
                            mtime = f.stat().st_mtime
                        except OSError:
                            mtime = time.time()
                        tsec = _mem_ts(d, mtime)
                        text = outcome
                        if learned:
                            text = (text + ("\n" if text else "") + "学到：" +
                                    "；".join(learned))[:agentbus.MAX_TEXT]
                        rec = {
                            "ns": "工作", "type": "task", "src": "agent_link",
                            "id": f"trae-mem-{key}",
                            "ts": int(tsec * 1000),
                            "title": (intent or outcome or "（Trae 没写意图）")[:120],
                            "text": text,
                            "data": {"agent": "TRAE", "session": sid,
                                     "day": f"{day.name[:4]}-{day.name[4:6]}-{day.name[6:]}",
                                     "project": proj.name,
                                     "project_path": _proj_hint(proj.name),
                                     "actions": actions,
                                     "learned": learned, "source": "trae-memory"},
                            "tags": ["Trae", "会话记忆"],
                        }
                        r = agentbus.add_event(root, rec, src="agent_link")
                        imported += int(r.get("written") or 0)
                        dup += int(r.get("dup") or 0)
                        projects.add(proj.name)
                        at = datetime.fromtimestamp(tsec).strftime("%Y-%m-%d %H:%M:%S")
                        day_s = f"{day.name[:4]}-{day.name[4:6]}-{day.name[6:]}"
                        wl_recs.append({
                            "id": f"tr-{key}",
                            "task": (intent or outcome or "Trae 会话")[:80],
                            "detail": (f"Trae 会话记忆 · {day_s}" +
                                       (f" · {outcome[:90]}" if outcome else ""))[:200],
                            "status": "ok", "start": at, "end": at,
                            "code": "", "ai": "TRAE", "client": "TRAE",
                            "created": at,
                        })
    return {"ok": True, "agent": "trae", "scanned": scanned, "written": imported,
            "dup": dup, "bad": bad, "projects": len(projects),
            "dirs": [str(d) for d in dirs],
            "path": str(agentbus.events_path(root)),
            "_wl_recs": wl_recs}


def _worklog_add(root: Path, recs: list[dict]) -> tuple[int, int]:
    """再往工作日志里放一份 —— 「本机工作」这一格读的是 worklog.json。

    为什么不只写 events.jsonl：那是给 agent 读的账，页面不认得。用户点一下「导入」，
    要能在卡片上直接看见这么多条 Trae 的活，否则等于没导。两份用同一个 id 去重：
    worklog.json 的 id 是我自己起的（tr-xxx），重导不会翻倍，也不会动别人补记的条目。
    """
    p = root / "_工作台" / "worklog.json"
    now = datetime.now().strftime("%Y-%m-%d %H:%M:%S")
    try:
        d = json.loads(p.read_text(encoding="utf-8")) if p.exists() else {}
    except (OSError, ValueError):
        d = {}
    items = d.get("items") if isinstance(d.get("items"), list) else []
    have = {str(x.get("id") or "") for x in items if isinstance(x, dict)}
    fresh = [r for r in recs if r["id"] not in have]
    if not fresh:
        return 0, len(recs)
    items.extend(fresh)
    items = items[-800:]                       # 跟 dash 的 _WL_KEEP 一个量级
    try:
        p.parent.mkdir(parents=True, exist_ok=True)
        tmp = p.with_suffix(".json.tmp")
        tmp.write_text(json.dumps({"items": items, "updated_at": now},
                                  ensure_ascii=False, indent=1), encoding="utf-8")
        tmp.replace(p)
    except OSError:
        return 0, len(recs)
    return len(fresh), len(recs) - len(fresh)


def do_import(root, agent_id: str, days: int = 30) -> dict:
    root = Path(root)
    days = max(1, min(int(days or 30), 365))
    if agent_id == "trae":
        res = _import_trae(root, days)
        # 顺手把同一批记录摆进工作日志，卡片上直接可见
        if res.get("ok"):
            # 卡片上只摆最近 60 条：半年的记忆一次全铺进去，这张卡就成了一本流水账，
            # 而它要回答的是「最近在忙什么」。全量的在 events.jsonl 里，agent 读得到。
            recs = sorted(res.pop("_wl_recs", []), key=lambda x: x.get("start") or "")[-60:]
            n, dupn = _worklog_add(root, recs)
            res["to_worklog"] = n
            res["worklog_dup"] = dupn
    else:
        return {"ok": False, "error": f"{agent_id} 还没有导入器",
                "hint": "先装说明，让它回报路径；导入器按那个格式再加"}
    links = _load_links(root)
    rec = links.setdefault(agent_id, {})
    prev = rec.get("import") or {}
    rec["import"] = {
        "at": datetime.now().strftime("%Y-%m-%d %H:%M"),
        "written": int(res.get("written") or 0) + int(prev.get("written") or 0)
                   if res.get("ok") else int(prev.get("written") or 0),
        "last": {"written": res.get("written"), "dup": res.get("dup"),
                 "scanned": res.get("scanned"), "days": days},
    }
    _save_links(root, links)
    return res
