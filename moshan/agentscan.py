"""扫本机装了哪些 AI agent、会话数据落在哪、有多大、能不能折成账。

为什么单独一个模块：
  agent_link 那张表回答的是「怎么把自己的说明书装到别人家里」（往各家的
  skills 目录投 SKILL.md），判据是「有没有那个目录 = 装没装」。这里回答的是
  另一个问题：「这家跑过的活，数据在哪儿」。两者路径有重叠但目的不同 ——
  装不装跟有没有账是两件事，别混在一起改。

用途：
  「Agent 监控」卡片上那颗「导入数据」按钮。点开先列一遍本机有什么，用户勾
  哪几个导哪几个；列表里没有的可以手选目录（登记路径，先不解析）。

边界：
  - 只读。除了 paths.data_root() 下的导入产物，一个字节都不写。
  - 算体积时跳过符号链接，避免在 exFAT / 网络盘上绕圈。
  - 单目录超过 MAX_FILES 个文件就停止计数（只影响显示，不影响导入）。
"""

from __future__ import annotations

import os
import time
from pathlib import Path

MAX_FILES = 40000          # 单来源最多数这么多个文件，防止在大目录上卡死
_SCAN_BUDGET = 2.5         # 单来源数文件的秒数上限

# parser 非空 = 能把它的会话折成账；空 = 先只登记路径（格式还没摸清 / 还没装）。
PROBES = (
    {
        "id": "workbuddy", "name": "WorkBuddy", "parser": "workbuddy",
        "paths": [("home", ".workbuddy/projects")],
        "note": "按会话 jsonl 解析：轮次、工具调用、token",
    },
    {
        "id": "cherry", "name": "Cherry Studio", "parser": "cherry",
        "paths": [("appsupport", "CherryStudio/Data"),
                  ("appsupport", "CherryStudio")],
        "note": "读它的 sqlite（只读打开）：消息数、模型、token",
    },
    {
        "id": "claude", "name": "Claude Code", "parser": "claude",
        "paths": [("home", ".claude/projects")],
        "note": "按会话 jsonl 解析",
    },
    {
        "id": "trae", "name": "TRAE", "parser": "trae",
        "paths": [("home", ".trae-cn/memory/projects")],
        "note": "memory 目录下的会话记忆（明文）",
    },
    {
        "id": "codex", "name": "Codex", "parser": "codex",
        "paths": [("home", ".codex/sessions")],
        "note": "rollout-*.jsonl，按年月日分目录",
    },
    {
        "id": "deepseek", "name": "DeepSeek Harness", "parser": None,
        "paths": [("home", ".deepseek"), ("home", ".deepseek-harness"),
                  ("home", ".dsh"),
                  ("appsupport", "DeepSeek Harness"),
                  ("appsupport", "deepseek-harness"),
                  ("appsupport", "dsh")],
        "note": "dsh：桌面版或 npx，会话在本地 store",
    },
    {
        "id": "qwenwork", "name": "千问办公 QwenWork", "parser": "qwenwork",
        "paths": [("home", ".qwenworkcn"), ("home", ".qwenwork"),
                  ("appsupport", "QwenWork"),
                  ("appsupport", "qwenwork")],
        "note": "projects/*.jsonl，格式同 Claude Code，但不写 usage（token 读不到）",
    },
    {
        "id": "openworker", "name": "OpenWorker", "parser": None,
        "paths": [("home", ".openworker"), ("home", ".openwork"),
                  ("appsupport", "OpenWorker"),
                  ("appsupport", "openworker"),
                  ("appsupport", "com.openworker.app")],
        "note": "Tauri 壳 + Python agent server，本地 store",
    },
    {
        "id": "lmstudio", "name": "LM Studio", "parser": None,
        "paths": [("home", ".lmstudio"), ("home", ".cache/lm-studio"),
                  ("appsupport", "LM Studio")],
        "note": "会话在 ~/.lmstudio/conversations，模型另算",
    },
    {
        "id": "cursor", "name": "Cursor", "parser": None,
        "paths": [("appsupport", "Cursor/User/globalStorage"),
                  ("home", ".cursor")],
        "note": "对话在加密库里，只能看用量",
    },
)


def _root_of(kind: str) -> Path | None:
    """跟 agent_link 同一套展开规则，但这里不 import 它 —— 采集层可能不在。"""
    if kind == "home":
        return Path.home()
    if kind == "appdata":
        v = os.environ.get("APPDATA")
        return Path(v) if v else None
    if kind == "localappdata":
        v = os.environ.get("LOCALAPPDATA")
        return Path(v) if v else None
    if kind == "appsupport":
        return Path.home() / "Library" / "Application Support"
    return None


def _measure(p: Path) -> dict:
    """数一个目录里的文件与字节。超预算就停，返回 partial=True。"""
    files = 0
    total = 0
    last = 0.0
    t0 = time.time()
    partial = False
    try:
        if p.is_file():
            st = p.stat()
            return {"files": 1, "bytes": st.st_size,
                    "last": st.st_mtime, "partial": False}
        for root, dirs, names in os.walk(p, followlinks=False):
            dirs[:] = [d for d in dirs if not d.startswith(".") or d in (".lmstudio",)]
            for n in names:
                if n.startswith("._"):        # exFAT 的元数据影子文件，不算
                    continue
                files += 1
                try:
                    st = os.stat(os.path.join(root, n))
                    total += st.st_size
                    if st.st_mtime > last:
                        last = st.st_mtime
                except OSError:
                    pass
                if files >= MAX_FILES or (files % 400 == 0
                                          and time.time() - t0 > _SCAN_BUDGET):
                    partial = True
                    break
            if partial:
                break
    except OSError as exc:
        return {"files": 0, "bytes": 0, "last": 0.0, "partial": False,
                "error": f"{type(exc).__name__}: {exc}"}
    return {"files": files, "bytes": total, "last": last, "partial": partial}


def _fmt_bytes(n: int) -> str:
    if n >= 1 << 30:
        return f"{n / (1 << 30):.1f} GB"
    if n >= 1 << 20:
        return f"{n / (1 << 20):.1f} MB"
    if n >= 1 << 10:
        return f"{n / (1 << 10):.0f} KB"
    return f"{n} B"


def detect() -> dict:
    """列一遍：本机哪些 agent 有数据、落在哪、多大、能不能导入。"""
    out: list[dict] = []
    for spec in PROBES:
        found: list[dict] = []
        for kind, rel in spec["paths"]:
            base = _root_of(kind)
            if base is None:
                continue
            p = base / rel
            try:
                if not p.exists():
                    continue
            except OSError:
                continue
            m = _measure(p)
            found.append({"path": str(p), **m})
        if not found:
            out.append({"id": spec["id"], "name": spec["name"],
                        "installed": False, "parsable": bool(spec["parser"]),
                        "note": spec["note"], "path": "", "files": 0,
                        "bytes": 0, "size": "", "last": ""})
            continue
        # 多个候选路径都命中时，取文件最多的那份当主路径（其余列在 alts 里）
        found.sort(key=lambda x: x["files"], reverse=True)
        main = found[0]
        out.append({
            "id": spec["id"], "name": spec["name"], "installed": True,
            "parsable": bool(spec["parser"]),
            "note": spec["note"],
            "path": main["path"],
            "alts": [f["path"] for f in found[1:]] if len(found) > 1 else [],
            "files": sum(f["files"] for f in found),
            "bytes": sum(f["bytes"] for f in found),
            "size": _fmt_bytes(sum(f["bytes"] for f in found)),
            "last": _day(main.get("last") or 0),
            "partial": any(f.get("partial") for f in found),
        })
    inst = [a for a in out if a["installed"]]
    return {
        "ok": True,
        "agents": out,
        "totals": {
            "installed": len(inst),
            "parsable": len([a for a in inst if a["parsable"]]),
            "bytes": sum(a["bytes"] for a in inst),
            "size": _fmt_bytes(sum(a["bytes"] for a in inst)),
        },
    }


def _day(ts: float) -> str:
    if not ts:
        return ""
    try:
        return time.strftime("%Y-%m-%d", time.localtime(ts))
    except (ValueError, OSError):
        return ""


def probe_manual(path: str) -> dict:
    """用户手选的目录：只量体积，先不解析。"""
    p = Path(path).expanduser()
    if not p.exists():
        return {"ok": False, "error": "这个路径不存在"}
    m = _measure(p)
    return {"ok": True, "path": str(p), "files": m["files"], "bytes": m["bytes"],
            "size": _fmt_bytes(m["bytes"]), "last": _day(m.get("last") or 0)}
