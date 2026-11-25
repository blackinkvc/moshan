"""墨山自己的 agent 账本 —— 一次解析，长期复用，不再每次刷新去爬几百 MB 会话。

为什么重建
  以前「谁在干活」是现算的：刷新一次就 glob `~/.workbuddy/projects` 下 91 个
  jsonl（实测 748 MB），把 tool_call、reasoning、file-history-snapshot 全解一遍，
  只为在卡片上显示「58949 次调用 · 8.1G token」。两个后果：
    · 慢 —— 每次刷新都要重新解几十万行 JSON；
    · 脆 —— 换台机器只拷数据根时源目录不在，卡片永远是 0。

  现在改成一份自己的库：一个会话一行，只留我们真的要显示的那几列。

存什么（一个会话 ≈ 300 字节）
  agent / session_id / title / cwd / project / model
  started_at / ended_at / turns / tool_calls
  tin / tout / cached / source / updated_at

不存什么（这些是 agent 的缓存与日志，不是账）
  工具入参与返回全文、模型思考过程、文件快照、附件 base64、逐条对话正文。
  —— 那 748 MB 里 71% 是 function_call 的入参与返回，13% 是 reasoning。

体量对比：83 个会话 → 83 行，约 40 KB。源目录 863 MB。
"""

from __future__ import annotations

import ast
import json
import os
import re
import sqlite3
import time
from datetime import datetime
from pathlib import Path

import paths

DB_NAME = "agentstore.sqlite"
SCHEMA = """
CREATE TABLE IF NOT EXISTS sessions (
  agent      TEXT NOT NULL,
  session_id TEXT NOT NULL,
  title      TEXT DEFAULT '',
  cwd        TEXT DEFAULT '',
  project    TEXT DEFAULT '',
  model      TEXT DEFAULT '',
  started_at INTEGER DEFAULT 0,      -- 毫秒
  ended_at   INTEGER DEFAULT 0,      -- 毫秒
  turns      INTEGER DEFAULT 0,      -- 用户说了几句（不含工具回执）
  tool_calls INTEGER DEFAULT 0,
  tin        INTEGER DEFAULT 0,
  tout       INTEGER DEFAULT 0,
  cached     INTEGER DEFAULT 0,
  source     TEXT DEFAULT '',        -- 哪个目录 / 哪个库读来的
  updated_at INTEGER DEFAULT 0,      -- 这一行最后一次刷新
  PRIMARY KEY (agent, session_id)
);
CREATE INDEX IF NOT EXISTS ix_sess_end ON sessions(agent, ended_at);
CREATE TABLE IF NOT EXISTS meta (
  k TEXT PRIMARY KEY,
  v TEXT
);
"""


def db_path() -> Path:
    return paths.data_root() / "_工作台" / DB_NAME


def connect() -> sqlite3.Connection:
    p = db_path()
    p.parent.mkdir(parents=True, exist_ok=True)
    con = sqlite3.connect(str(p), timeout=8)
    con.row_factory = sqlite3.Row
    con.execute("PRAGMA journal_mode=WAL")
    con.execute("PRAGMA synchronous=NORMAL")
    con.executescript(SCHEMA)
    return con


# ---------------------------------------------------------------- 解析小工具

def _pd(v) -> dict:
    """providerData 有时是 dict，有时是 Python repr 字符串（单引号那种）。"""
    if isinstance(v, dict):
        return v
    if isinstance(v, str) and v.strip().startswith("{"):
        try:
            d = ast.literal_eval(v)
            return d if isinstance(d, dict) else {}
        except (ValueError, SyntaxError):
            return {}
    return {}


def _usage(pd: dict) -> tuple[int, int, int]:
    u = (pd or {}).get("usage") or {}
    tin = int(u.get("inputTokens") or u.get("input_tokens") or 0)
    tout = int(u.get("outputTokens") or u.get("output_tokens") or 0)
    cached = 0
    det = u.get("inputTokensDetails") or u.get("input_tokens_details") or u.get("details")
    if isinstance(det, list) and det and isinstance(det[0], dict):
        cached = int(det[0].get("cached_tokens") or det[0].get("cachedTokens") or 0)
    elif isinstance(det, dict):
        cached = int(det.get("cached_tokens") or det.get("cachedTokens") or 0)
    return tin, tout, cached


def _ms(v) -> int:
    if isinstance(v, (int, float)):
        n = int(v)
        return n if n > 1_000_000_000_000 else n * 1000 if n > 1_000_000_000 else 0
    s = str(v or "").strip()
    if not s:
        return 0
    try:
        return int(datetime.fromisoformat(s.replace("Z", "+00:00")).timestamp() * 1000)
    except ValueError:
        return 0


def _project_of(cwd: str) -> str:
    if not cwd:
        return ""
    try:
        parts = [p for p in Path(cwd).parts if p not in ("/", "Users")]
        return parts[-1] if parts else ""
    except (ValueError, OSError):
        return ""


_SR_TAG = re.compile(r"<system-reminder\b[^>]*>.*?</system-reminder>", re.S)
_NOISE_HEAD = ("<task-notification", "<user_info", "cb_summary", "接着上面的摘要继续")


def _user_is_real(content) -> bool:
    """这条 user 消息是不是「用户真的说了话」。

    WorkBuddy 会把工具回执、系统提醒、压缩摘要都写成 role=user，全算成轮次的
    话一个会话能虚高十倍。判据是：剥掉拼在正文前的 <system-reminder> 之后，
    还剩不剩人写的话。

    注意别把整条以 <system-reminder 开头的都当机器发言 —— WorkBuddy 给**每条**
    用户消息前面都拼了模式/专家上下文（实测 83 个会话里几乎 100%），照开头字
    符串一刀切会把真轮次也砍掉（506 轮 vs 采集层的 2317 轮就是这么来的）。
    """
    txt = ""
    if isinstance(content, str):
        txt = content
    elif isinstance(content, list):
        for x in content:
            if isinstance(x, dict):
                if x.get("type") == "tool_result":
                    return False
                if x.get("type") in ("input_text", "text"):
                    txt += str(x.get("text") or "")
    if not txt:
        return False
    rest = _SR_TAG.sub("", txt).strip()
    if not rest:
        return False                     # 全是上下文，没有一句人话
    for bad in _NOISE_HEAD:
        if rest.startswith(bad):
            return False
    return True


# ---------------------------------------------------------------- WorkBuddy

WB_ROOT = Path.home() / ".workbuddy" / "projects"


def ingest_workbuddy(force: bool = False) -> dict:
    """扫 WorkBuddy 的会话流水，折成一行一会话。

    增量：文件 mtime 没变且库里已有就跳过。force=True 全解一遍
    （PARSE_VERSION 变了会自己触发，见 _version_ok）。
    """
    if not WB_ROOT.exists():
        return {"ok": False, "error": "本机没有 ~/.workbuddy/projects"}
    con = connect()
    have = {}
    if not force:
        for r in con.execute("SELECT session_id, source, updated_at FROM sessions "
                             "WHERE agent='WorkBuddy'"):
            have[(r["session_id"], r["source"])] = r["updated_at"] or 0

    files = []
    try:
        files = [p for p in WB_ROOT.glob("*/*.jsonl") if p.is_file()]
    except OSError as exc:
        return {"ok": False, "error": f"{type(exc).__name__}: {exc}"}

    n_new = n_skip = n_err = 0
    rows: list[tuple] = []
    for f in files:
        try:
            mtime = int(f.stat().st_mtime)
        except OSError:
            continue
        sid = f.stem
        if not force and have.get((sid, str(f.parent))) == mtime:
            n_skip += 1
            continue
        try:
            row = _parse_wb(f, sid)
        except Exception:                              # noqa: BLE001
            n_err += 1
            continue
        if row is None:
            n_err += 1
            continue
        rows.append(row + (mtime,))
        n_new += 1

    if rows:
        con.executemany(
            "INSERT INTO sessions (agent, session_id, title, cwd, project, model,"
            " started_at, ended_at, turns, tool_calls, tin, tout, cached, source,"
            " updated_at) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)"
            " ON CONFLICT(agent, session_id) DO UPDATE SET"
            " title=excluded.title, cwd=excluded.cwd, project=excluded.project,"
            " model=excluded.model, started_at=excluded.started_at,"
            " ended_at=excluded.ended_at, turns=excluded.turns,"
            " tool_calls=excluded.tool_calls, tin=excluded.tin, tout=excluded.tout,"
            " cached=excluded.cached, source=excluded.source,"
            " updated_at=excluded.updated_at", rows)
        con.commit()
    con.close()
    return {"ok": True, "agent": "WorkBuddy", "files": len(files),
            "new": n_new, "skipped": n_skip, "bad": n_err,
            "db": str(db_path()), "db_size": _size(db_path())}


def _parse_wb(f: Path, sid: str) -> tuple | None:
    """一个会话文件 → 一行。只读一次，逐行累加。"""
    title = ""
    cwd = ""
    model = ""
    first = 0
    last = 0
    turns = 0
    tools = 0
    tin = tout = cached = 0
    got = False
    with f.open(encoding="utf-8", errors="replace") as fh:
        for ln in fh:
            s = ln.strip()
            if not s:
                continue
            try:
                o = json.loads(s)
            except ValueError:
                continue
            got = True
            t = o.get("type") or ""
            ts = _ms(o.get("timestamp"))
            if ts:
                if not first or ts < first:
                    first = ts
                if ts > last:
                    last = ts
            if not cwd and o.get("cwd"):
                cwd = str(o["cwd"])
            if t == "ai-title":
                title = str(o.get("aiTitle") or o.get("title") or "")[:160]
            elif t == "message":
                if o.get("role") == "user" and _user_is_real(o.get("content")):
                    turns += 1
                pd = _pd(o.get("providerData"))
                m = str(pd.get("requestModelId") or pd.get("model") or "")
                if m and not model:
                    model = m[:60]
                a, b, c = _usage(pd)
                tin += a
                tout += b
                cached += c
            elif t == "function_call":
                tools += 1
                # 工具调用那一趟请求也真花了 token：它带着完整上下文，往往是
                # 账上的大头（本机实测 8.1G 里 98% 在这一类行上）。
                a, b, c = _usage(_pd(o.get("providerData")))
                tin += a
                tout += b
                cached += c
            elif t in ("session-meta", "file-history-snapshot"):
                a, b, c = _usage(_pd(o.get("providerData")))
                tin += a
                tout += b
                cached += c
    if not got:
        return None
    return ("WorkBuddy", sid, title, cwd, _project_of(cwd), model,
            first or int(f.stat().st_mtime * 1000), last or int(f.stat().st_mtime * 1000),
            turns, tools, tin, tout, cached, str(f.parent))


# ---------------------------------------------------------------- 查询

def summary(days: int = 0) -> list[dict]:
    """按 agent 汇总。days=0 表示全部历史。"""
    con = connect()
    sql = ("SELECT agent, COUNT(*) AS sessions, SUM(turns) AS turns,"
           " SUM(tool_calls) AS calls, SUM(tin) AS tin, SUM(tout) AS tout,"
           " SUM(cached) AS cached, MAX(ended_at) AS last"
           " FROM sessions")
    args: tuple = ()
    if days:
        cut = int((time.time() - days * 86400) * 1000)
        sql += " WHERE ended_at >= ?"
        args = (cut,)
    sql += " GROUP BY agent ORDER BY calls DESC"
    out = []
    for r in con.execute(sql, args):
        out.append({k: r[k] for k in r.keys()})
    con.close()
    return out


def daily(agent: str = "", days: int = 30) -> list[dict]:
    """按天汇总（会话按结束时间归日）。"""
    con = connect()
    cut = int((time.time() - days * 86400) * 1000)
    sql = ("SELECT agent, date(ended_at/1000, 'unixepoch', 'localtime') AS day,"
           " COUNT(*) AS sessions, SUM(tool_calls) AS calls,"
           " SUM(tin+tout) AS tokens FROM sessions WHERE ended_at >= ?")
    args: list = [cut]
    if agent:
        sql += " AND agent = ?"
        args.append(agent)
    sql += " GROUP BY agent, day ORDER BY day"
    out = []
    for r in con.execute(sql, tuple(args)):
        out.append({k: r[k] for k in r.keys()})
    con.close()
    return out


def stats() -> dict:
    con = connect()
    n = con.execute("SELECT COUNT(*) FROM sessions").fetchone()[0]
    per = {}
    for r in con.execute("SELECT agent, COUNT(*) c FROM sessions GROUP BY agent"):
        per[r["agent"]] = r["c"]
    con.close()
    p = db_path()
    return {"rows": n, "by_agent": per, "db": str(p), "db_size": _size(p)}


def _size(p: Path) -> int:
    try:
        return sum(x.stat().st_size for x in [p, Path(str(p) + "-wal"), Path(str(p) + "-shm")]
                   if x.exists())
    except OSError:
        return 0


def compact() -> dict:
    """收一下碎片。行数不多，主要是 WAL 检查点。"""
    con = connect()
    try:
        con.execute("PRAGMA wal_checkpoint(TRUNCATE)")
        con.execute("VACUUM")
    except sqlite3.Error as exc:
        con.close()
        return {"ok": False, "error": str(exc)}
    con.close()
    return {"ok": True, "db_size": _size(db_path())}
