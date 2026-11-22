#!/usr/bin/env python3
"""AI 工作计划的 MCP 服务（stdio，零依赖）。

为什么要有这一层：这份计划有两个写者 —— 人是工作台那张卡，AI 是 agent。
agent 要能「读计划 → 照目标补子任务 → 把自由记录归位」，就得有个稳定的工具面，
而不是让每个客户端自己去拼 HTTP 地址和令牌。

协议：JSON-RPC 2.0，一行一条 JSON（MCP stdio transport），支持
    initialize / notifications/initialized / ping / tools/list / tools/call
所有工具都落在 plan.py 上 —— 跟卡片同一份数据、同一把跨进程文件锁，
所以 agent 改完刷新页面立刻能看到，也不会跟人同时改时互相覆盖。

接入方式（Claude Code / WorkBuddy / Codex / Cursor 都吃这套）：
  在 MCP 配置里加一条，command 用跑 Moshan 的那个 python：
  {"mcpServers": {"ai-work-plan": {
      "command": "<Moshan 的 python 绝对路径，打包版即 Moshan 可执行文件>",
      "args": ["<本文件绝对路径>"]}}}

手动试：
  echo '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | python3 plan_mcp.py
"""
from __future__ import annotations

import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import plan  # noqa: E402

VERSION = "1.0.0"


class _Bad(Exception):
    pass


# ---------------------------------------------------------------- 工具

TOOLS = [
    {
        "name": "plan_list",
        "description": "读 AI 工作计划：项目、多级任务（含 id / 完成状态 / 子任务）、"
                       "收件箱待归位条目，以及每个项目关联到的 AI 项目与 token。"
                       "动手改之前先读一遍，用返回的 id 操作。",
        "inputSchema": {
            "type": "object",
            "properties": {
                "proj": {"type": "string", "description": "只看某个项目 id（可选）"},
            },
            "additionalProperties": False,
        },
    },
    {
        "name": "plan_import",
        "description": "把一份 AI 写的长期工作计划导入成多级任务树。认 # 标题、- [ ] 勾选、"
                       "1. 编号，缩进自动变成子任务。apply 不传只预览（返回解析出的树），"
                       "apply=true 才真正写进数据。同名任务合并，不会重复。",
        "inputSchema": {
            "type": "object",
            "properties": {
                "text": {"type": "string", "description": "计划原文（Markdown 或缩进列表）"},
                "name": {"type": "string", "description": "项目名；留空则取一级标题"},
                "proj": {"type": "string", "description": "并入已有项目 id（可选）"},
                "apply": {"type": "boolean", "description": "true = 落库；默认只预览"},
                "mode": {"type": "string", "enum": ["merge", "replace"],
                         "description": "merge（默认）只补新的，replace 整份替换"},
            },
            "required": ["text"],
            "additionalProperties": False,
        },
    },
    {
        "name": "plan_add",
        "description": "加一条任务。给 parent 就挂成它的子任务（多级）；不给就加到项目根。"
                       "扩充 AI 写的计划时主要用这个。",
        "inputSchema": {
            "type": "object",
            "properties": {
                "proj": {"type": "string", "description": "项目 id"},
                "title": {"type": "string", "description": "任务标题"},
                "parent": {"type": "string", "description": "父任务 id（可选）"},
                "due": {"type": "string", "description": "截止日 YYYY-MM-DD（可选）"},
                "note": {"type": "string", "description": "备注（可选）"},
            },
            "required": ["proj", "title"],
            "additionalProperties": False,
        },
    },
    {
        "name": "plan_toggle",
        "description": "打钩 / 取消打钩。会顺手给项目 token 拍一张快照，"
                       "所以「这一步烧了多少」能算出来。",
        "inputSchema": {
            "type": "object",
            "properties": {
                "proj": {"type": "string"},
                "id": {"type": "string", "description": "任务 id"},
                "done": {"type": "boolean", "description": "true 完成 / false 撤销；不传就翻转"},
            },
            "required": ["proj", "id"],
            "additionalProperties": False,
        },
    },
    {
        "name": "plan_inbox_add",
        "description": "往底部「自由添加」写一条：还不知道该归哪个子任务的想法、"
                       "一句话需求。之后用 plan_triage 拿建议、plan_apply 落位。",
        "inputSchema": {
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"],
            "additionalProperties": False,
        },
    },
    {
        "name": "plan_triage",
        "description": "对收件箱条目给出**候选**（不落库）。每条带代码算好的 hint"
                       "（proj / task_id / 建议标题 / 理由）。你可以据此决定，"
                       "也可以自己判；决定后用 plan_apply 落库。",
        "inputSchema": {
            "type": "object",
            "properties": {
                "ids": {"type": "array", "items": {"type": "string"},
                        "description": "只看这几条；不传就是全部"},
            },
            "additionalProperties": False,
        },
    },
    {
        "name": "plan_apply",
        "description": "把归位决定落库：把收件箱条目放进项目的某个子任务下，"
                       "parent 留空则在项目根新建一条；proj 也留空则按这条新建一个计划项目。",
        "inputSchema": {
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {"type": "string", "description": "收件箱条目 id"},
                            "proj": {"type": "string"},
                            "parent": {"type": "string"},
                            "title": {"type": "string"},
                        },
                        "required": ["id"],
                    },
                },
            },
            "required": ["items"],
            "additionalProperties": False,
        },
    },
    {
        "name": "plan_sync_agent",
        "description": "把计划任务推进 Agent 总线（_工作台/agents/events.jsonl），"
                       "别的 AI 就能读到「现在有哪些待办」。不填 proj 推全部项目 + 收件箱；"
                       "给 id 只推一条。重复调用不会重复写（按任务 id + 标题指纹去重）。",
        "inputSchema": {
            "type": "object",
            "properties": {
                "proj": {"type": "string", "description": "项目 id（可选）"},
                "id": {"type": "string", "description": "任务 id（可选，单条推）"},
                "scope": {"type": "string", "description": "open=只推未完成（默认）/ all=全都推"},
            },
            "additionalProperties": False,
        },
    },
    {
        "name": "plan_link",
        "description": "把一个计划项目绑到具体的 AI 空间 / 项目目录上，"
                       "token 账就算到它头上。k 的写法来自 plan_list 里的 tok_from "
                       "或「space:名字」「proj:目录 slug」。不传某个字段表示不动它。",
        "inputSchema": {
            "type": "object",
            "properties": {
                "proj": {"type": "string"},
                "spaces": {"type": "array", "items": {"type": "string"}},
                "projs": {"type": "array", "items": {"type": "string"}},
                "keys": {"type": "array", "items": {"type": "string"},
                         "description": "模糊匹配关键词（这个项目的别的叫法）"},
            },
            "required": ["proj"],
            "additionalProperties": False,
        },
    },
]


def _task_view(nodes):
    out = []
    for n in nodes or []:
        out.append({"id": n["id"], "t": n["t"], "done": n["done"],
                    "due": n.get("due") or "", "note": n.get("note") or "",
                    "done_at": n.get("at") or "", "tok_at_done": n.get("tok") or 0,
                    "children": _task_view(n.get("children") or [])})
    return out


def _root():
    return plan.default_root()


def _t_list(a):
    v = plan.payload(_root())
    projs = []
    for p in v["projects"]:
        if a.get("proj") and p["id"] != a["proj"]:
            continue
        projs.append({
            "id": p["id"], "name": p["name"], "status": p["status"],
            "stat": p["stat"], "tok": p["usage"]["tok"],
            "tok_from": [{"k": x["k"], "n": x["n"], "tok": x["tok"]}
                         for x in p["usage"]["items"]],
            "tasks": _task_view(p["tasks"]),
        })
    return {"rev": v["rev"], "updated_at": v["updated_at"], "stat": v["stat"],
            "projects": projs, "inbox": v["inbox"], "dir": v["dir"]}


def _t_import(a):
    if not str(a.get("text") or "").strip():
        raise _Bad("text 不能为空")
    r = plan.act(_root(), {"action": "import", "text": a["text"],
                           "name": a.get("name") or "", "proj": a.get("proj") or "",
                           "apply": bool(a.get("apply")), "mode": a.get("mode") or "merge"})
    if not r.get("ok"):
        raise _Bad(r.get("error") or "导入失败")
    if r.get("dry"):
        return {"dry": True, "parsed": r["parsed"],
                "hint": "这是预览。确认无误后带 apply=true 再调一次。"}
    return {"dry": False, "proj": r.get("proj"), "added": r.get("added"),
            "parsed_name": (r.get("parsed") or {}).get("name")}


def _t_add(a):
    r = plan.act(_root(), {"action": "task.add", "proj": a.get("proj"),
                           "parent": a.get("parent") or "", "title": a.get("title"),
                           "due": a.get("due") or "", "note": a.get("note") or "",
                           "src": "agent"})
    if not r.get("ok"):
        raise _Bad(r.get("error") or "添加失败")
    return {"ok": True, "id": r.get("id")}


def _t_toggle(a):
    r = plan.act(_root(), {"action": "task.toggle", "proj": a.get("proj"),
                           "id": a.get("id"), "done": a.get("done")})
    if not r.get("ok"):
        raise _Bad(r.get("error") or "打钩失败")
    doc = r.get("doc") or {}
    return {"ok": True, "rev": doc.get("rev")}


def _t_inbox_add(a):
    r = plan.act(_root(), {"action": "inbox.add", "text": a.get("text")})
    if not r.get("ok"):
        raise _Bad(r.get("error") or "添加失败")
    return {"ok": True, "id": r.get("id")}


def _t_triage(a):
    ids = a.get("ids") or None
    return {"items": plan.candidates(_root(), ids)}


def _t_apply(a):
    items = [x for x in (a.get("items") or []) if isinstance(x, dict)]
    if not items:
        raise _Bad("items 不能为空")
    return plan.apply_ai_assign(_root(), items)


def _t_link(a):
    body = {"action": "proj.link", "id": a.get("proj")}
    for k in ("spaces", "projs", "keys"):
        if k in a:
            body[k] = a[k]
    r = plan.act(_root(), body)
    if not r.get("ok"):
        raise _Bad(r.get("error") or "绑定失败")
    return {"ok": True, "usage": r.get("usage")}


def _t_sync_agent(a):
    body = {"action": "agent.export", "scope": a.get("scope") or "open"}
    for k in ("proj", "id"):
        if a.get(k):
            body[k] = a[k]
    r = plan.act(_root(), body)
    if not r.get("ok"):
        raise _Bad(r.get("error") or "同步失败")
    return {k: r.get(k) for k in ("written", "dup", "tasks")}


DISPATCH = {
    "plan_list": _t_list, "plan_import": _t_import, "plan_add": _t_add,
    "plan_toggle": _t_toggle, "plan_inbox_add": _t_inbox_add,
    "plan_triage": _t_triage, "plan_apply": _t_apply, "plan_link": _t_link,
    "plan_sync_agent": _t_sync_agent,
}


# ---------------------------------------------------------------- JSON-RPC

def _send(obj) -> None:
    sys.stdout.write(json.dumps(obj, ensure_ascii=False) + "\n")
    sys.stdout.flush()


def _ok(mid, result) -> None:
    _send({"jsonrpc": "2.0", "id": mid, "result": result})


def _err(mid, code: int, msg: str) -> None:
    _send({"jsonrpc": "2.0", "id": mid, "error": {"code": code, "message": msg}})


def main() -> None:
    while True:
        line = sys.stdin.readline()
        if not line:
            break
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
            if not isinstance(msg, dict):
                raise ValueError
        except ValueError:
            _err(None, -32700, "解析不了这条 JSON")
            continue
        mid = msg.get("id")
        method = str(msg.get("method") or "")
        params = msg.get("params") if isinstance(msg.get("params"), dict) else {}

        if method == "initialize":
            _ok(mid, {
                "protocolVersion": params.get("protocolVersion") or "2024-11-05",
                "capabilities": {"tools": {"listChanged": False}},
                "serverInfo": {"name": "ai-work-plan", "version": VERSION},
            })
        elif method in ("notifications/initialized", "initialized", "notifications/cancelled"):
            continue                      # 通知没有 id，不回
        elif method == "ping":
            _ok(mid, {})
        elif method == "tools/list":
            _ok(mid, {"tools": TOOLS})
        elif method == "tools/call":
            name = str(params.get("name") or "")
            args = params.get("arguments") if isinstance(params.get("arguments"), dict) else {}
            fn = DISPATCH.get(name)
            if fn is None:
                _err(mid, -32601, f"没有这个工具：{name}")
                continue
            try:
                res = fn(args)
                _ok(mid, {"content": [{"type": "text",
                                       "text": json.dumps(res, ensure_ascii=False)}],
                          "isError": False})
            except _Bad as e:
                _ok(mid, {"content": [{"type": "text", "text": str(e)}], "isError": True})
            except Exception as e:                     # noqa: BLE001
                _ok(mid, {"content": [{"type": "text",
                                       "text": f"{type(e).__name__}: {e}"}],
                          "isError": True})
        else:
            _err(mid, -32601, f"不认识的方法：{method}")


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        pass
