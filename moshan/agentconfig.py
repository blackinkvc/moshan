"""扫各家 agent 的「配置面」：分组 / 任务 / 插件 / Skill / MCP。

跟 agentscan 的分工：
  agentscan 回答「它跑过的活存在哪、有多大」——数据面，会去数几万个文件。
  这里回答「它被怎么配置了」——配置面，只读几个清单文件和小库，毫秒级。

五面各是什么（各家叫法不一样，统一成这五档）：
  groups   分组  会话按工作目录/项目分的组（WorkBuddy 的 projects/<slug>、
                 TRAE 的 memory/projects、Cherry 的 agent）
  tasks    任务  待办、定时任务、按任务开的工作区（WorkBuddy 的 tasks/*.json、
                 Cherry 的 scheduled_tasks、TRAE 的 mcps/s_<标题>）
  plugins  插件  扩展包（带 marketplace 的那种）
  skills   技能  SKILL.md 那种「说明书」，有的带使用计数
  mcp      MCP   接进来的外部工具服务

写这个模块的三条约定：
  1. **只读**。所有 sqlite 用 mode=ro 打开，配置只读不写。
  2. **单面失败不影响其余四面**。每家一个大 try，每个面一个小 try —— 某家某天
     改了格式，不该让整张卡变白。
  3. **不猜**。读不出来的字段留空并写进 notes。特别是 TRAE 的分组目录名里
     中文被替换成了 '-'，还原不回来，就照实说「目录名不可还原」，不编。
"""

from __future__ import annotations

import json
import os
import re
import sqlite3
import time
from pathlib import Path

CACHE_TTL = 180          # 秒。配置面变化不频繁，点开卡片不该每次都全盘重扫
_BUDGET = 6.0            # 整体秒数上限，超了就把还没扫的面标成 partial

_cache: dict = {"at": 0.0, "data": None}

# 每条 SKILL.md / 插件清单最多取多少个，避免把几百项塞进接口
MAX_ITEMS = 60


# ---------------------------------------------------------------- 小工具

def _home() -> Path:
    return Path.home()


def _asupport() -> Path:
    return _home() / "Library" / "Application Support"


def _read_json(p: Path, default=None):
    try:
        return json.loads(p.read_text(encoding="utf-8", errors="replace"))
    except (OSError, ValueError):
        return default


def _mtime(p: Path) -> float:
    try:
        return p.stat().st_mtime
    except OSError:
        return 0.0


def _day(ts: float) -> str:
    if not ts:
        return ""
    try:
        return time.strftime("%Y-%m-%d", time.localtime(ts))
    except (ValueError, OSError):
        return ""


def _rel(p: Path) -> str:
    """把绝对路径缩成 ~/… 形式，界面上短一半。"""
    try:
        s = str(p)
    except OSError:
        return ""
    h = str(_home())
    return "~" + s[len(h):] if s.startswith(h) else s


def _front(text: str, limit: int = 0) -> dict:
    """抠 SKILL.md 头部的 `--- … ---` 里那几个键。

    不引 yaml：这里只要 name / description 两个短标量，自己解比引依赖划算。
    description 可能是 `|` / `>` 折叠块，也可能是带引号的单行。
    """
    out: dict = {}
    if not text.startswith("---"):
        return out
    end = text.find("\n---", 3)
    if end < 0:
        return out
    for line in text[3:end].splitlines():
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        if ":" not in line:
            continue
        k, _, v = line.partition(":")
        k = k.strip()
        v = v.strip()
        if v in ("|", ">", "|-", ">-", "|+", ">+"):
            v = ""
        if len(v) >= 2 and v[0] == v[-1] and v[0] in "\"'":
            v = v[1:-1]
        if k and v:
            out[k] = v
    if limit and out.get("description"):
        out["description"] = out["description"][:limit]
    return out


def _skill(p: Path) -> dict | None:
    """读一个技能目录：优先 SKILL.md，兜底同目录的任意 .md。"""
    if not p.is_dir():
        return None
    f = p / "SKILL.md"
    if not f.exists():
        cands = sorted(p.glob("*.md"))
        if not cands:
            return None
        f = cands[0]
    try:
        txt = f.read_text(encoding="utf-8", errors="replace")[:6000]
    except OSError:
        return None
    meta = _front(txt, 160)
    return {
        "name": meta.get("name") or p.name,
        "desc": meta.get("description") or "",
        "path": _rel(p),
        "at": _day(_mtime(f)),
    }


def _dirs(p: Path) -> list[Path]:
    try:
        return sorted([x for x in p.iterdir() if x.is_dir() and not x.name.startswith(".")])
    except OSError:
        return []


def _files(p: Path, pattern: str = "*") -> list[Path]:
    try:
        return sorted([x for x in p.glob(pattern) if x.is_file() and not x.name.startswith("._")])
    except OSError:
        return []


def _sqlite_ro(p: Path):
    """只读打开。库可能正被别的进程写着，用 immutable 会读不到最新页，所以只用 mode=ro。"""
    return sqlite3.connect(f"file:{p}?mode=ro", uri=True, timeout=1.2)


def _q(con, sql: str, args: tuple = ()) -> list:
    try:
        return con.execute(sql, args).fetchall()
    except sqlite3.Error:
        return []


def _pack(groups=None, tasks=None, plugins=None, skills=None, mcp=None,
          notes=None, error="") -> dict:
    """一家的结果，五面缺哪面就填空数组 —— 前端不用判 null。"""
    return {
        "groups": groups or [],
        "tasks": tasks or {"total": 0, "by_status": {}, "items": []},
        "plugins": plugins or [],
        "skills": skills or [],
        "mcp": mcp or [],
        "notes": notes or [],
        "error": error,
    }


# ---------------------------------------------------------------- WorkBuddy

def _wb_groups(root: Path) -> list[dict]:
    """projects/<slug>/ 一目录一组。

    目录名是「工作目录路径把 / 换成 -」，中文原样保留，但路径里本来带 - 的
    还原不回来。所以真名去会话里读 cwd —— 只读最大的那个文件的头几十行，
    不整份解析。
    """
    out = []
    for d in _dirs(root / "projects"):
        files = _files(d, "*.jsonl")
        if not files:
            continue
        big = max(files, key=lambda x: _mtime(x))
        cwd = ""
        try:
            with open(big, encoding="utf-8", errors="replace") as fh:
                for i, line in enumerate(fh):
                    if i > 60:
                        break
                    try:
                        o = json.loads(line)
                    except ValueError:
                        continue
                    if isinstance(o, dict) and o.get("cwd"):
                        cwd = str(o["cwd"])
                        break
        except OSError:
            pass
        last = max((_mtime(f) for f in files), default=0.0)
        shown = cwd or d.name.replace("Users-", "~/").replace("-", "/")
        if cwd:
            h = str(_home())
            if shown.startswith(h):
                shown = "~" + shown[len(h):]
        out.append({
            "name": shown,
            "label": d.name,
            "path": _rel(d),
            "count": len(files),
            "last": _day(last),
            "from": "cwd" if cwd else "slug",
        })
    out.sort(key=lambda x: x["last"], reverse=True)
    return out


def _wb_tasks(root: Path) -> dict:
    """tasks/<会话id>/<序号>.json —— 每条一个 subject/status。

    这就是中台里「任务清单」那张卡的原始数据，一个会话一个目录、里面按序号
    存每一步。这里只汇总，不按会话拆（拆了在这一层没意义）。
    """
    by_status: dict = {}
    items = []
    for d in _dirs(root / "tasks"):
        for f in _files(d, "*.json"):
            o = _read_json(f, None)
            if not isinstance(o, dict):
                continue
            st = str(o.get("status") or "未知")
            by_status[st] = by_status.get(st, 0) + 1
            items.append({
                "name": str(o.get("subject") or o.get("activeForm") or f.stem)[:80],
                "status": st,
                "group": d.name[:8],
                "at": _day((o.get("updatedAt") or 0) / 1000) if o.get("updatedAt") else _day(_mtime(f)),
            })
    items.sort(key=lambda x: x["at"], reverse=True)
    return {"total": len(items), "by_status": by_status,
            "items": items[:MAX_ITEMS], "dirs": len(_dirs(root / "tasks"))}


def _wb_plugins(root: Path) -> list[dict]:
    """installed_plugins.json 是「装了什么」，configured-plugins.json 是「哪些开着」。"""
    inst = _read_json(root / "plugins" / "installed_plugins.json", {}) or {}
    cfg = _read_json(root / "plugins" / "configured-plugins.json", {}) or {}
    on = {str(x.get("name")) for x in (cfg.get("builtInMarketPlugins") or [])
          if isinstance(x, dict)}
    out = []
    for key, arr in (inst.get("plugins") or {}).items():
        name, _, market = str(key).partition("@")
        first = arr[0] if isinstance(arr, list) and arr else {}
        if not isinstance(first, dict):
            first = {}
        out.append({
            "name": name,
            "market": market or "—",
            "ver": str(first.get("version") or "")[:22],
            "on": (name in on) if on else True,
            "at": str(first.get("lastUpdated") or first.get("installedAt") or "")[:10],
            "path": _rel(Path(str(first.get("installPath") or ""))) if first.get("installPath") else "",
        })
    out.sort(key=lambda x: (not x["on"], x["name"]))
    return out


def _wb_skills(root: Path) -> list[dict]:
    out = []
    for d in _dirs(root / "skills"):
        if d.name.startswith("_"):
            continue
        s = _skill(d)
        if s:
            out.append(s)
    return out


def _wb_mcp(root: Path) -> list[dict]:
    d = _read_json(root / "mcp.json", {}) or {}
    srv = d.get("mcpServers") or {}
    # 每个 server 缓存了工具清单（mcp-tool-list.json），键是配置的哈希，
    # 没法可靠反查是哪台，所以只在「只有一个 server」时把工具数挂上去。
    tools = _read_json(root / "mcp-tool-list.json", {}) or {}
    entries = tools.get("entries") or {}
    only = sum(len(v or []) for v in entries.values()) if len(srv) == 1 else 0
    out = []
    for name, cfg in srv.items():
        if not isinstance(cfg, dict):
            continue
        out.append({
            "name": name,
            "kind": "http" if cfg.get("url") else ("stdio" if cfg.get("command") else "?"),
            "target": str(cfg.get("url") or cfg.get("command") or "")[:70],
            "tools": only if len(srv) == 1 else 0,
            "on": True,
        })
    return out


def _workbuddy() -> dict:
    root = _home() / ".workbuddy"
    if not root.is_dir():
        return _pack()
    notes = []
    g = t = p = s = m = None
    try:
        g = _wb_groups(root)
    except Exception as exc:                                    # noqa: BLE001
        notes.append(f"分组读失败：{type(exc).__name__}")
    try:
        t = _wb_tasks(root)
    except Exception as exc:                                    # noqa: BLE001
        notes.append(f"任务读失败：{type(exc).__name__}")
    try:
        p = _wb_plugins(root)
    except Exception as exc:                                    # noqa: BLE001
        notes.append(f"插件读失败：{type(exc).__name__}")
    try:
        s = _wb_skills(root)
    except Exception as exc:                                    # noqa: BLE001
        notes.append(f"Skill 读失败：{type(exc).__name__}")
    try:
        m = _wb_mcp(root)
    except Exception as exc:                                    # noqa: BLE001
        notes.append(f"MCP 读失败：{type(exc).__name__}")
    return _pack(g, t, p, s, m, notes)


# ---------------------------------------------------------------- Cherry Studio

def _cherry() -> dict:
    base = _asupport() / "CherryStudio" / "Data"
    if not base.is_dir():
        return _pack()
    notes: list[str] = []
    groups: list[dict] = []
    tasks = {"total": 0, "by_status": {}, "items": []}
    plugins: list[dict] = []
    skills: list[dict] = []
    mcp: list[dict] = []

    # agents.db：智能体（分组）、会话、定时任务、技能
    adb = base / "agents.db"
    if adb.exists():
        try:
            con = _sqlite_ro(adb)
            try:
                rows = _q(con, "select id,name,type,model from agents")
                sess = dict(_q(con, "select agent_id,count(*) from sessions group by agent_id") or [])
                for r in rows:
                    groups.append({
                        "name": str(r[1] or r[0]), "label": str(r[2] or ""),
                        "path": str(r[0]), "count": int(sess.get(r[0], 0) or 0),
                        "last": "", "from": "agent",
                        "model": str(r[3] or "")[:40],
                    })
                by_st: dict = {}
                for r in _q(con, "select name,schedule_type,schedule_value,status,last_run,agent_id "
                                "from scheduled_tasks"):
                    by_st[str(r[3] or "?")] = by_st.get(str(r[3] or "?"), 0) + 1
                    tasks["items"].append({
                        "name": str(r[0] or ""), "status": str(r[3] or ""),
                        "group": str(r[1] or ""),
                        "at": str(r[4] or "")[:10],
                        "note": f"{r[1]} {r[2]}".strip(),
                    })
                runs = dict(_q(con, "select status,count(*) from task_run_logs group by status") or [])
                tasks["by_status"] = by_st
                tasks["total"] = len(tasks["items"])
                tasks["runs"] = {str(k): int(v) for k, v in runs.items()}
                # 技能表在 agents.db 里叫 skills（agent_global_skill 在另一个库，别搞混）
                for r in _q(con, "select name,description,source from skills"):
                    skills.append({"name": str(r[0] or ""), "desc": str(r[1] or "")[:160],
                                   "path": "", "at": "", "src": str(r[2] or "")})
            finally:
                con.close()
        except sqlite3.Error:
            notes.append("agents.db 读不全（版本差异），技能改按目录扫")
        except Exception as exc:                                # noqa: BLE001
            notes.append(f"agents.db：{type(exc).__name__}")

    # cherrystudio.sqlite：MCP 服务、绑定了几个 agent
    sdb = base / "cherrystudio.sqlite"
    if sdb.exists():
        try:
            con = _sqlite_ro(sdb)
            try:
                bind = dict(_q(con, "select mcp_server_id,count(*) from agent_mcp_server "
                                    "group by mcp_server_id") or [])
                for r in _q(con, "select id,name,type,base_url,command from mcp_server"):
                    mcp.append({
                        "name": str(r[1] or r[0]), "kind": str(r[2] or ""),
                        "target": str(r[3] or r[4] or "")[:70],
                        "tools": int(bind.get(r[0], 0) or 0), "on": True,
                    })
            finally:
                con.close()
        except Exception as exc:                                # noqa: BLE001
            notes.append(f"cherrystudio.sqlite：{type(exc).__name__}")

    # 技能兜底：Data/Skills/<dir>/SKILL.md。表里通常只有「用户装的」，
    # 目录里还有内置那批，两边合起来才是全部。
    have = {s["name"] for s in skills}
    for d in _dirs(base / "Skills"):
        s = _skill(d)
        if s and s["name"] not in have:
            s["src"] = s.get("src") or "内置"
            skills.append(s)
            have.add(s["name"])
    if not any(mcp):
        notes.append("没读到 MCP 绑定（可能这版把服务存在别处）")
    return _pack(groups, tasks, plugins, skills, mcp, notes)


# ---------------------------------------------------------------- TRAE

_TRAE_TAIL = re.compile(r"-(?:p\d+-)?[0-9a-z]{10,}$")


def _trae_slug(name: str) -> str:
    """TRAE / 千问办公的分组目录名是把路径里的 / 和**所有非 ASCII 字符**都换成 -。

    所以 `Documents/AI项目/xx` 只会留下 `Documents/AI` + 一长串 -，中文回不来。
    这里只做尽力还原：砍掉尾部 p2-<hash>，遇到连续 3 个以上的 - 就当「这里
    丢了一段」，截断并加 …；单个 - 还原成 /。界面上标 from=slug，说明这个名字
    不全，不假装它准。
    """
    s = _TRAE_TAIL.sub("", name).lstrip("-")
    cut = s.find("---")
    lost = cut >= 0
    if lost:
        s = s[:cut]
    s = re.sub(r"/{2,}", "/", s.replace("-", "/"))
    parts = s.split("/")
    if len(parts) > 1 and parts[0] == "Users":
        # Users-<用户名>-Documents-… → ~/Documents/…（用户名那段吞掉）
        s = "~/" + "/".join(parts[2:])
    elif parts and parts[0] == "Volumes":
        s = "/" + s
    return s + ("…" if lost else "")


def _mcp_name(raw: str) -> str:
    """TRAE 的 MCP 目录名：mcp_Computer_Use / mcp_plugin_github_github。

    `plugin_<插件>_<服务>` 里前两段是来源不是名字，取最后一段；剩下把下划线
    换成空格（Computer_Use → Computer Use）。
    """
    s = raw[4:] if raw.startswith("mcp_") else raw
    if s.startswith("plugin_"):
        s = s.rsplit("_", 1)[-1]
    return s.replace("_", " ") or raw


def _trae() -> dict:
    root = _home() / ".trae-cn"
    if not root.is_dir():
        return _pack()
    notes: list[str] = []

    groups = []
    for d in _dirs(root / "memory" / "projects"):
        files = _files(d, "*.jsonl")
        sub = _dirs(d)                      # 里面还按日期分一层
        n = len(files) or sum(len(_files(x, "*.jsonl")) for x in sub)
        last = max([_mtime(f) for f in files] +
                   [_mtime(x) for x in sub] + [0.0])
        groups.append({"name": _trae_slug(d.name), "label": d.name,
                       "path": _rel(d), "count": n, "last": _day(last),
                       "from": "slug"})
    groups.sort(key=lambda x: x["last"], reverse=True)
    if groups:
        notes.append("分组目录名里的中文被 TRAE 换成了 '-'，还原不回来（按 slug 显示）")

    # 任务：mcps/s_<标题>-<hash>/ 一个任务一个工作区，标题是中文原样
    tasks = {"total": 0, "by_status": {}, "items": []}
    tdir = root / "mcps"
    for d in _dirs(tdir):
        if not d.name.startswith("s_"):
            continue
        title = _TRAE_TAIL.sub("", d.name[2:]).rsplit("-", 1)[0] or d.name
        servers = _dirs(d / "solo_agent")
        tasks["items"].append({"name": title[:80], "status": "工作区",
                               "group": f"{len(servers)} 个 MCP",
                               "at": _day(_mtime(d))})
    tasks["total"] = len(tasks["items"])
    tasks["by_status"] = {"工作区": len(tasks["items"])} if tasks["items"] else {}

    # 插件：plugin-config 决定开关，installed-plugins 提供元数据
    plugins = []
    try:
        cfg = (_read_json(root / "plugin-config.json", {}) or {}).get("plugins") or {}
        meta: dict = {}
        inst = _read_json(root / "installed-plugins.json", {}) or {}
        for mk in ((inst.get("data") or {}).get("marketplaces") or []):
            for pl in (mk.get("plugins") or []):
                pid = pl.get("plugin_id") or pl.get("name")
                if pid:
                    meta[pid] = pl
        for pid, st in cfg.items():
            m = meta.get(pid) or {}
            plugins.append({
                "name": str(m.get("display_name") or pid.split(":")[-1]),
                "market": str(m.get("registry") or pid.split(":")[0]),
                "ver": str(m.get("version") or ""),
                "on": bool((st or {}).get("user_enabled", True)),
                "at": "",
                "desc": str(m.get("description") or "")[:90],
            })
        plugins.sort(key=lambda x: (not x["on"], x["name"]))
    except Exception as exc:                                    # noqa: BLE001
        notes.append(f"插件读失败：{type(exc).__name__}")

    # 技能：内置 4 个 + 用户目录
    skills = []
    for d in _dirs(root / "builtin_skills") + _dirs(root / "skills"):
        s = _skill(d)
        if s:
            s["src"] = "内置" if "builtin" in s["path"] else "用户"
            skills.append(s)

    # MCP：任务工作区里的 solo_agent/mcp_* + 插件自带的 .mcp.json
    mcp = []
    seen = set()
    for wk in _dirs(tdir):
        for srv in _dirs(wk / "solo_agent"):
            n = srv.name
            if not n.startswith("mcp_") or n in seen:
                continue
            seen.add(n)
            meta = _read_json(srv / "SERVER_METADATA.json", {}) or {}
            tools = _dirs(srv / "tools")
            mcp.append({"name": _mcp_name(str(meta.get("server_name") or n)),
                        "kind": "builtin", "target": _rel(srv),
                        "tools": len(tools), "on": True})
    for f in _files(root / "plugins", "*/*/*/.mcp.json"):
        d = _read_json(f, {}) or {}
        for name, cfg in (d.get("mcpServers") or {}).items():
            if name in seen:
                continue
            seen.add(name)
            cfg = cfg if isinstance(cfg, dict) else {}
            # command 一律是 node / npx，光看这个等于没说；把脚本名带上
            tgt = str(cfg.get("url") or "")
            if not tgt:
                bits = [str(cfg.get("command") or "")]
                args = cfg.get("args") or []
                if args:
                    bits.append(Path(str(args[0])).name)
                tgt = " ".join(x for x in bits if x)
            mcp.append({"name": name, "kind": "plugin", "target": tgt[:70],
                        "tools": 0, "on": True})
    # 同一个服务会被多个插件各带一份 .mcp.json，按名字去重（先到的算）
    uniq: dict = {}
    for x in mcp:
        uniq.setdefault(x["name"], x)
    return _pack(groups, tasks, plugins, skills, list(uniq.values()), notes)


# ---------------------------------------------------------------- 千问办公

def _qwenwork() -> dict:
    root = _home() / ".qwenworkcn"
    if not root.is_dir():
        return _pack()
    notes: list[str] = []
    groups = []
    for d in _dirs(root / "projects"):
        files = _files(d, "*.jsonl")
        last = max([_mtime(f) for f in files] + [0.0])
        groups.append({"name": _trae_slug(d.name), "label": d.name,
                       "path": _rel(d), "count": len(files),
                       "last": _day(last), "from": "slug"})
    groups.sort(key=lambda x: x["last"], reverse=True)

    # 任务：它没有独立的任务清单，只有 projectless 文件夹约定
    tasks = {"total": 0, "by_status": {}, "items": []}
    pl = _read_json(root / "projectless-task-folder-default.json", {}) or {}
    if pl.get("path"):
        tasks["items"].append({"name": "无项目任务落点", "status": "配置",
                               "group": str(pl.get("source") or ""),
                               "at": "", "note": str(pl.get("path"))})
        tasks["total"] = 1
        tasks["by_status"] = {"配置": 1}

    plugins = []
    for d in _dirs(root / "plugins" / "marketplaces"):
        sub = _dirs(d)
        plugins.append({"name": d.name, "market": "marketplace",
                        "ver": str(len(sub)), "on": True, "at": "",
                        "desc": f"{len(sub)} 个插件" if sub else ""})

    usage = _read_json(root / "skill-usage.json", {}) or {}
    skills = []
    for d in _dirs(root / "skills"):
        s = _skill(d)
        if not s:
            continue
        u = usage.get(s["name"]) or usage.get(d.name) or {}
        s["used"] = int(u.get("usageCount") or 0) if isinstance(u, dict) else 0
        s["last_used"] = _day((u.get("lastUsedAt") or 0) / 1000) if isinstance(u, dict) and u.get("lastUsedAt") else ""
        s["src"] = "用户"
        skills.append(s)
    skills.sort(key=lambda x: (-(x.get("used") or 0), x["name"]))

    mcp = []
    cache = root / "cache" / "mcp"
    if cache.is_dir():
        notes.append("MCP 只有市场缓存，没读到已配置的服务")
    return _pack(groups, tasks, plugins, skills, mcp, notes)


# ---------------------------------------------------------------- 汇总

SOURCES = (
    ("workbuddy", "WorkBuddy", _workbuddy),
    ("cherry", "Cherry Studio", _cherry),
    ("trae", "TRAE", _trae),
    ("qwenwork", "千问办公", _qwenwork),
)


def _count(cfg: dict, facet: str) -> int:
    v = cfg.get(facet)
    if isinstance(v, dict):
        return int(v.get("total") or 0)
    return len(v or [])


def scan(force: bool = False) -> dict:
    """五面一次扫完。未装的直接跳过（installed=False 也返回，前端好画灰名单）。"""
    now = time.time()
    if not force and _cache["data"] and now - _cache["at"] < CACHE_TTL:
        d = dict(_cache["data"])
        d["cached"] = True
        return d

    t0 = time.time()
    agents = []
    for aid, name, fn in SOURCES:
        try:
            cfg = fn()
        except Exception as exc:                                # noqa: BLE001
            cfg = _pack(error=f"{type(exc).__name__}: {exc}")
        inst = any(_count(cfg, f) for f in ("groups", "tasks", "plugins", "skills", "mcp")) \
            or bool(cfg.get("groups"))
        agents.append({
            "id": aid, "name": name, "installed": bool(inst),
            "counts": {f: _count(cfg, f) for f in
                       ("groups", "tasks", "plugins", "skills", "mcp")},
            **{k: cfg.get(k) for k in ("groups", "tasks", "plugins", "skills", "mcp", "notes", "error")},
        })

    live = [a for a in agents if a["installed"]]
    data = {
        "ok": True,
        "agents": agents,
        "totals": {
            "installed": len(live),
            **{f: sum(a["counts"][f] for a in live) for f in
               ("groups", "tasks", "plugins", "skills", "mcp")},
        },
        "ms": int((time.time() - t0) * 1000),
        "at": time.strftime("%H:%M", time.localtime()),
        "cached": False,
    }
    _cache.update({"at": now, "data": data})
    return data
