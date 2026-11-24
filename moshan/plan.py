#!/usr/bin/env python3
"""AI 工作计划 —— 「立项之后 AI 写的那张长期表」在中台里的落点。

为什么单独一个模块：这份数据有两个写者 —— 人和别的 AI（MCP / HTTP）。
待办那套是「前端整份覆盖写」，两个写者一起用就会互相冲掉；所以这里的数据
以服务端为准，所有写操作走 `act()` 一个入口，串行加锁、原子落盘、rev 自增，
前端每次写完重取整份。谁写都一样，不会丢别人的一条。

三个结构性决定：

  1. 树是嵌套的（children），不是扁平 + parent 指针。这份数据的读写都按整棵树
     来（折叠、进度、导入合并），嵌套写起来最直白；代价是移动节点要自己搬子树。

  2. 与「AI 项目 / token」的联通全部是确定性代码 —— 归一化字符串打分、空间/
     目录精确绑定，**不调任何模型**。模型只在「收件箱这句话该挂哪个子任务」
     这种自然语言归类上兜底（server.py 侧调用），而且它的输出必须落回候选 id，
     落不回去就当「新建子任务」—— 猜错了顶多多一条子任务，不会把账算错项目。

  3. 完成任务时给项目 token 拍一张快照（tok_at_done）。相邻两次完成之间的差值
     就是「这一步烧了多少 token」，不需要另外埋点。项目累计 token 来自
     hostwork.pulse() 的会话账，跟「本机工作」那几张卡同源，不另算一套。

数据：<数据根>/19-AI工作计划/计划.json（另出一份 计划.md 镜像供人看）
"""
from __future__ import annotations

import contextlib
import hashlib
import json
import re
import secrets
import sys
import threading
import time
from datetime import datetime
from pathlib import Path

import paths   # 配置在可写目录：打包后读不到 .app 内部

try:
    import fcntl
except ImportError:                                    # 非 POSIX（理论上用不到）
    fcntl = None

PLAN_DIR = "19-AI工作计划"
PLAN_FILE = "计划.json"
LOG_MAX = 200

# 读-改-写要串行；RLock 是因为 act() 里可能顺手调 payload()。
_LOCK = threading.RLock()
_HOST_CACHE: dict = {"at": 0.0, "data": None}

STATUS = ("active", "paused", "done")


# ---------------------------------------------------------------- 路径

def default_root() -> Path:
    """没有 cfg 时（MCP / 命令行）自己找数据根：config.json 的 allowed_roots[0]。"""
    here = paths.base_dir()
    try:
        cfg = json.loads((here / "config.json").read_text(encoding="utf-8"))
        roots = cfg.get("allowed_roots") or []
        if roots:
            return Path(roots[0])
        if cfg.get("inbox_dir"):
            return Path(cfg["inbox_dir"]).parent
    except (OSError, ValueError):
        pass
    return Path.home() / "Documents" / "AI项目"


def root_of(cfg: dict | None = None) -> Path:
    if not cfg:
        return default_root()
    roots = cfg.get("allowed_roots") or []
    if roots:
        return Path(roots[0])
    return Path(cfg.get("inbox_dir", ".")).parent


def plan_dir(root) -> Path:
    return Path(root) / PLAN_DIR


def plan_path(root) -> Path:
    return plan_dir(root) / PLAN_FILE


def _now() -> str:
    return datetime.now().isoformat(timespec="seconds")


def _today() -> str:
    return datetime.now().strftime("%Y-%m-%d")


def _uid(prefix: str, taken: set) -> str:
    for _ in range(50):
        v = prefix + secrets.token_hex(3)
        if v not in taken:
            taken.add(v)
            return v
    return prefix + secrets.token_hex(6)


# ---------------------------------------------------------------- 读写

def _json(path: Path, default):
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return default


def _write(path: Path, obj) -> None:
    """原子写：tmp + replace。写到一半崩了，读到的还是上一份完整数据。"""
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(json.dumps(obj, ensure_ascii=False, indent=1), encoding="utf-8")
    tmp.replace(path)


@contextlib.contextmanager
def _xlock(root):
    """跨进程写锁。`_LOCK` 只管同一个进程里的线程；MCP 服务和口袋服务是两个进程，
    同时改就会「后写的整份覆盖前一份」。flock 一层，读-改-写整段都在锁里。"""
    p = plan_dir(root) / ".lock"
    p.parent.mkdir(parents=True, exist_ok=True)
    if fcntl is None:
        yield
        return
    f = open(p, "a+")
    try:
        fcntl.flock(f.fileno(), fcntl.LOCK_EX)
        yield
    finally:
        try:
            fcntl.flock(f.fileno(), fcntl.LOCK_UN)
        finally:
            f.close()


def _norm_node(n: dict, taken: set) -> dict:
    """节点白名单规范化。字段漏一个就会被静默丢掉，加字段要同步这里。"""
    n = n if isinstance(n, dict) else {"t": str(n)}
    tid = str(n.get("id") or "")
    if not tid or tid in taken:
        tid = _uid("tk_", taken)
    else:
        taken.add(tid)
    kids = [c for c in (n.get("children") or []) if isinstance(c, dict)]
    return {
        "id": tid,
        "t": str(n.get("t") or n.get("title") or "").strip()[:300],
        "done": bool(n.get("done")),
        "at": str(n.get("at") or ""),
        "due": str(n.get("due") or "")[:10],
        "note": str(n.get("note") or "")[:2000],
        # link：这一条归属的另一个项目 id（空 = 跟父级/项目走）
        "link": str(n.get("link") or ""),
        "src": str(n.get("src") or "manual")[:16],
        # 完成时该项目累计 token 的快照，用于算「这一步烧了多少」
        "tok": int(n.get("tok") or 0) if str(n.get("tok") or "0").lstrip("-").isdigit() else 0,
        # 推过 agent 总线的戳：标题指纹 + 时间。白名单漏掉就会被保存静默抹掉，
        # 然后每次同步都当「没推过」重推一遍（踩过）。
        "agent_h": str(n.get("agent_h") or "")[:16],
        "agent_at": str(n.get("agent_at") or "")[:19],
        "children": [_norm_node(c, taken) for c in kids],
    }


def _norm_project(p: dict, taken: set) -> dict:
    p = p if isinstance(p, dict) else {}
    pid = str(p.get("id") or "")
    if not pid or pid in taken:
        pid = _uid("pr_", taken)
    else:
        taken.add(pid)
    m = p.get("match") if isinstance(p.get("match"), dict) else {}
    st = str(p.get("status") or "active")
    kids = [t for t in (p.get("tasks") or []) if isinstance(t, dict)]
    return {
        "id": pid,
        "name": str(p.get("name") or "未命名计划").strip()[:80],
        "note": str(p.get("note") or "")[:2000],
        "status": st if st in STATUS else "active",
        "created": str(p.get("created") or _now()),
        "due": str(p.get("due") or "")[:10],
        "match": {
            # 存 hostwork pulse 里的 key（"space:xxx" / "proj:xxx"），精确绑定
            "spaces": [str(x)[:120] for x in (m.get("spaces") or []) if str(x).strip()][:12],
            "projs": [str(x)[:120] for x in (m.get("projs") or []) if str(x).strip()][:12],
            # 模糊匹配用的关键词（项目名之外的叫法）
            "keys": [str(x).strip()[:40] for x in (m.get("keys") or []) if str(x).strip()][:12],
        },
        "tasks": [_norm_node(t, taken) for t in kids],
    }


def _norm_doc(d: dict) -> dict:
    d = d if isinstance(d, dict) else {}
    taken: set = set()
    projs = [_norm_project(p, taken) for p in (d.get("projects") or []) if isinstance(p, dict)]
    inbox = []
    for it in (d.get("inbox") or []):
        if not isinstance(it, dict):
            continue
        iid = str(it.get("id") or "")
        if not iid or iid in taken:
            iid = _uid("ib_", taken)
        else:
            taken.add(iid)
        inbox.append({
            "id": iid,
            "t": str(it.get("t") or it.get("text") or "").strip()[:300],
            "at": str(it.get("at") or _now()),
            "note": str(it.get("note") or "")[:500],
            "agent_h": str(it.get("agent_h") or "")[:16],
            "agent_at": str(it.get("agent_at") or "")[:19],
        })
    log = [x for x in (d.get("log") or []) if isinstance(x, dict)][-LOG_MAX:]
    return {
        "v": 1,
        "rev": int(d.get("rev") or 0),
        "updated_at": str(d.get("updated_at") or ""),
        "projects": projs,
        "inbox": inbox,
        "log": log,
    }


def load(root) -> dict:
    return _norm_doc(_json(plan_path(root), {}))


def save(root, doc: dict) -> dict:
    doc = _norm_doc(doc)
    doc["rev"] = int(doc.get("rev") or 0) + 1
    doc["updated_at"] = _now()
    doc["log"] = doc["log"][-LOG_MAX:]
    _write(plan_path(root), doc)
    try:
        export_md(root, doc)
    except OSError:
        pass                      # md 只是给人看的镜像，写不出来不能拖垮保存
    return doc


def _log(doc: dict, act: str, name: str = "", detail: str = "") -> None:
    doc.setdefault("log", []).append(
        {"at": _now(), "act": act, "name": str(name)[:120], "detail": str(detail)[:200]})


# ---------------------------------------------------------------- 树工具

def walk(nodes, parent=None):
    """深度优先，产出 (node, siblings, parent_node)。改结构时用得上。"""
    for n in nodes or []:
        yield n, nodes, parent
        yield from walk(n.get("children") or [], n)


def find_node(nodes, tid):
    for n in nodes or []:
        if n.get("id") == tid:
            return n
        hit = find_node(n.get("children") or [], tid)
        if hit is not None:
            return hit
    return None


def find_parent_list(nodes, tid):
    for n in nodes or []:
        if n.get("id") == tid:
            return nodes
        got = find_parent_list(n.get("children") or [], tid)
        if got is not None:
            return got
    return None


def find_project(doc, pid):
    for p in doc.get("projects") or []:
        if p.get("id") == pid:
            return p
    return None


def count(nodes) -> tuple[int, int]:
    done = total = 0
    for n, _s, _p in walk(nodes):
        total += 1
        if n.get("done"):
            done += 1
    return done, total


def flatten_names(nodes, depth=0, out=None) -> list[dict]:
    out = [] if out is None else out
    for n in nodes or []:
        out.append({"id": n["id"], "t": n["t"], "depth": depth, "done": n["done"]})
        flatten_names(n.get("children") or [], depth + 1, out)
    return out


# ---------------------------------------------------------------- 解析导入

_RE_HEAD = re.compile(r"^(#{1,6})\s+(.*)$")
_RE_ITEM = re.compile(r"^(\s*)(?:[-*+•]|\d+[.)])\s+(.*)$")
_RE_BOX = re.compile(r"^\[([ xX✓✔])\]\s*(.*)$")


def _clean_line(s: str, keep_indent: bool = False) -> str:
    """去掉 markdown 噪声。keep_indent=True 时保留前导空格 —— 列表层级全靠它，
    strip 掉就把所有子任务压平了（这个坑踩过一次：一墙的子任务全变成同级）。"""
    s = s.replace("\u200b", "").rstrip()
    # 「**粗体**」「`代码`」这类写法留着只是噪声
    s = re.sub(r"\*\*(.+?)\*\*", r"\1", s)
    s = re.sub(r"`(.+?)`", r"\1", s)
    return s if keep_indent else s.strip()


def parse_plan(text: str, name: str = "") -> dict:
    """把「AI 写的那张表」解析成树。纯规则，不调模型。

    认三种写法，混着写也行：
      # / ## / ### 标题  → 层级
      - [ ] / - [x]     → 任务，缩进决定父子
      1. / 2. 编号列表   → 同上
    返回 {"name": 项目名, "tasks": [...], "stats": {...}}
    """
    name = (name or "").strip()
    lines = [l for l in str(text or "").splitlines() if l.strip()]
    # 缩进单位：所有非空缩进里最小的那个（2 空格 / 4 空格 / tab 都能认）
    units = []
    for l in lines:
        m = _RE_ITEM.match(_clean_line(l, keep_indent=True))
        if m and m.group(1):
            units.append(len(m.group(1).replace("\t", "  ")))
    unit = min(units) if units else 2
    unit = max(1, unit)

    roots: list[dict] = []
    # 栈里存 (depth, node)，depth 是统一的缩进层级
    stack: list[tuple[int, dict]] = []
    head_depth = 0          # 最近一个标题的 depth，列表项从它下面挂

    for raw in lines:
        # 两份：line_raw 保留缩进（列表父子靠它），line 是用于标题/文本匹配的
        line_raw = _clean_line(raw, keep_indent=True)
        line = line_raw.strip()
        if not line:
            continue
        mh = _RE_HEAD.match(line)
        if mh:
            level = len(mh.group(1))
            title = _clean_line(mh.group(2))
            if not title:
                continue
            # 一级标题当项目名（没显式给 name 时），不再单独成为任务
            if level == 1 and not name:
                name = title
                head_depth = 0
                stack = []
                continue
            depth = level
            head_depth = depth
            node = {"t": title, "done": False, "due": "", "note": "",
                    "src": "import", "children": []}
            while stack and stack[-1][0] >= depth:
                stack.pop()
            (stack[-1][1]["children"] if stack else roots).append(node)
            stack.append((depth, node))
            continue

        mi = _RE_ITEM.match(line_raw)
        if mi:
            indent = len(mi.group(1).replace("\t", "  "))
            body = _clean_line(mi.group(2))
            done = False
            mb = _RE_BOX.match(body)
            if mb:
                done = mb.group(1).strip().lower() in ("x", "✓", "✔")
                body = _clean_line(mb.group(2))
            if not body:
                continue
            depth = head_depth + 1 + indent // unit
            node = {"t": body, "done": done, "due": "", "note": "",
                    "src": "import", "children": []}
            while stack and stack[-1][0] >= depth:
                stack.pop()
            (stack[-1][1]["children"] if stack else roots).append(node)
            stack.append((depth, node))
            continue

        # 普通一行：只有当它不像「说明文字」时才当任务。
        # 太长的句子（>120 字）多半是段落说明，丢掉免得把计划撑成一堆废话。
        if len(line) <= 120 and not line.endswith(("：", ":")):
            depth = head_depth + 1
            while stack and stack[-1][0] >= depth:
                stack.pop()
            body = _clean_line(line.lstrip("-*+ "))
            done = False
            mb = _RE_BOX.match(body)
            if mb:
                done = mb.group(1).strip().lower() in ("x", "✓", "✔")
                body = _clean_line(mb.group(2))
            if body:
                node = {"t": body, "done": done, "due": "", "note": "",
                        "src": "import", "children": []}
                (stack[-1][1]["children"] if stack else roots).append(node)
                stack.append((depth, node))

    # 只留「像计划」的结构：至少有一个任务
    taken: set = set()
    tasks = [_norm_node(t, taken) for t in roots]
    done, total = count(tasks)
    return {"name": name or "导入的计划", "tasks": tasks,
            "stats": {"done": done, "total": total}}


def merge_tasks(dst: list, src: list) -> int:
    """按标题合并（同名同层不重复加），返回新增条数。用于「再导一次」不炸成两份。"""
    added = 0
    def key(n):
        return (str(n.get("t") or "").strip(), bool(n.get("done")))
    have = {key(n): n for n in dst}
    for s in src:
        k = key(s)
        if k in have:
            # 已存在：把完成状态和子任务并进去。只有「原来没完成、这次导入标了完成」
            # 才盖上今天的日期 —— 导入源里本来就打钩的条目不动 at，
            # 否则「今日完成」会凭空多出几十条历史任务。
            tgt = have[k]
            if s.get("done") and not tgt.get("done"):
                tgt["done"] = True
                tgt["at"] = _now()
            added += merge_tasks(tgt.setdefault("children", []), s.get("children") or [])
        else:
            dst.append(s)
            have[k] = s
            added += 1 + sum(1 for _x, _s, _p in walk(s.get("children") or []))
    return added


# ---------------------------------------------------------------- 与 AI 项目 / token 的联通（纯代码）

def norm_text(s: str) -> str:
    s = str(s or "").lower()
    s = re.sub(r"[\s\u3000]+", "", s)
    s = re.sub(r"[·、，,。.：:；;（）()\[\]【】「」\"'“”‘’/\\|_\-—–~!！?？]+", "", s)
    return s


_CJK = re.compile(r"[\u4e00-\u9fff]")


def _grams(s: str) -> set:
    s = norm_text(s)
    if not s:
        return set()
    if _CJK.search(s):
        out = {s[i:i + 2] for i in range(max(1, len(s) - 1))}
        out |= {c for c in s}
        return out
    return {w for w in re.split(r"[^a-z0-9]+", s) if w}


def sim(a: str, b: str) -> float:
    """归一化后的字符/词交集比。0~1。短标题对长标题不会因为长度吃亏。"""
    A, B = _grams(a), _grams(b)
    if not A or not B:
        return 0.0
    return len(A & B) / min(len(A), len(B))


def host_projects(root=None, ttl: float = 120.0) -> dict:
    """AI 项目清单 + token：直接读 hostwork 的会话账（跟本机工作那几张卡同源）。

    hostwork 没装上（没 cuntu 目录）就返回空表，卡片照常能用，只是没有 token 徽标。
    """
    now = time.time()
    cached = _HOST_CACHE.get("data")
    if cached is not None and now - float(_HOST_CACHE.get("at") or 0) < ttl:
        return cached
    data = {"spaces": [], "projs": [], "at": "", "err": ""}
    try:
        try:
            import hostwork  # noqa: PLC0415
        except ImportError:
            cuntu = Path.home() / "Documents" / "cuntu"
            if cuntu.is_dir():
                sys.path.insert(0, str(cuntu))
            import hostwork  # noqa: PLC0415
        p = hostwork.pulse(90, "")
        opts = p.get("opts") or {}
        data = {"spaces": opts.get("spaces") or [], "projs": opts.get("projs") or [],
                "at": p.get("at") or datetime.now().strftime("%H:%M:%S"), "err": ""}
    except Exception as e:                             # noqa: BLE001
        data["err"] = f"{type(e).__name__}: {e}"
    _HOST_CACHE.update({"at": now, "data": data})
    return data


def usage_of(proj: dict, host: dict | None = None) -> dict:
    """一个计划项目关联到的 token 与来源。

    三段判断，全部确定性：
      1. 手绑的 spaces / projs（最准，UI 里点出来的）
      2. 关键词命中（match.keys 是「这个项目还叫什么」）
      3. 项目名本身命中
    同一份空间既被关键词也被名字命中只算一次。
    """
    host = host if host is not None else host_projects()
    m = proj.get("match") or {}
    want = set(m.get("spaces") or []) | set(m.get("projs") or [])
    keys = [str(k) for k in (m.get("keys") or []) if str(k).strip()]
    nm = str(proj.get("name") or "")
    hits: dict[str, dict] = {}
    for kind, arr in (("spaces", host.get("spaces") or []), ("projs", host.get("projs") or [])):
        for it in arr:
            k = str(it.get("k") or "")
            n = str(it.get("n") or "")
            ok = k in want
            if not ok:
                for kk in keys:
                    if len(norm_text(kk)) >= 2 and norm_text(kk) in norm_text(n):
                        ok = True
                        break
            if not ok and len(norm_text(nm)) >= 2:
                a, b = norm_text(nm), norm_text(n)
                # 「影音室 2.0」对「影音室」、「个人中台 v2」对「个人中台」都要算中，
                # 光靠互为子串会漏（版本号把整串改得不再包含）。
                ok = bool(a) and (a in b or b in a or sim(nm, n) >= 0.7)
            if ok:
                hits[k] = {"k": k, "n": n, "kind": kind, "tok": int(it.get("tok") or 0)}
    items = sorted(hits.values(), key=lambda x: -x["tok"])
    return {"tok": sum(x["tok"] for x in items), "items": items[:6], "n": len(items)}


def score_project(text: str, proj: dict) -> float:
    """一句话对某个项目的相关度。项目名 0.5、关键词 0.8、直接子任务标题看相似度。"""
    best = sim(text, proj.get("name") or "")
    for k in (proj.get("match") or {}).get("keys") or []:
        best = max(best, sim(text, k) * 1.1)
    for n, _s, _p in walk(proj.get("tasks") or []):
        best = max(best, sim(text, n.get("t") or "") * 1.05)
    return round(best, 3)


def best_task(text: str, proj: dict) -> dict | None:
    """在项目里找最可能承接这句话的子任务。

    一条打分规则：**这条标题能在这句话里解释掉多少字**（字符 bigram 交集）。
    原样命中的再加 2 分；覆盖率不足 55% 的不参与竞争。
    不用「覆盖率最高」直接排序 —— 短标题（「影音室」）永远满分，
    会把「标签词表按片源分两套」这种真正该挂的具体子任务挤下去。
    """
    head = norm_text(text)
    A = _grams(text)
    pool: list[tuple] = []      # (权重, 命中字数, 深度, 标题长度, node)

    def rec(nodes, d):
        for n in nodes or []:
            if not n.get("done"):
                t = str(n.get("t") or "")
                B = _grams(t)
                if B:
                    ov = len(A & B)
                    hit = len(norm_text(t)) >= 3 and norm_text(t) in head
                    # 参与竞争的条件：原样命中，或这条标题的字有 55% 以上能在这句话里找到。
                    # 胜负看「解释掉多少字」（ov），原样命中再加 2 分。
                    # 不能只看覆盖率：短标题（「影音室」）永远满分，会把具体子任务挤掉。
                    if hit or ov / len(B) >= 0.55:
                        pool.append((ov + (2 if hit else 0), ov, d, len(B), n))
            rec(n.get("children") or [], d + 1)

    rec(proj.get("tasks") or [], 0)
    if not pool:
        return None
    pool.sort(key=lambda x: (x[0], x[2], x[3]), reverse=True)
    _w, ov, _d, lb, node = pool[0]
    return {"id": node["id"], "t": node["t"],
            "score": round(min(1.0, ov / max(1, lb)), 3)}


def candidates(root, ids=None) -> list[dict]:
    """收件箱每条的**代码建议**（不落库）。score 高才给建议，低了就明说「没把握」。"""
    doc = load(root)
    want = set(ids or [])
    out = []
    for it in doc.get("inbox") or []:
        if want and it["id"] not in want:
            continue
        text = it["t"]
        ranked = sorted(((score_project(text, p), p) for p in doc.get("projects") or []),
                        key=lambda x: -x[0])
        top = ranked[0] if ranked else (0.0, None)
        hint = {"proj": "", "proj_name": "", "task": "", "task_id": "",
                "new_task": True, "score": 0.0, "why": ""}
        if top[1] is not None and top[0] >= 0.45:
            p = top[1]
            hint.update({"proj": p["id"], "proj_name": p["name"], "score": float(top[0]),
                         "why": f"与「{p['name']}」的标题/关键词最接近"})
            bt = best_task(text, p)
            if bt:
                # 标题用**原话**，不要把承接任务的标题抄过来 —— 抄过来就变成
                # 「界面评审 › 界面评审」这种同名嵌套，等于什么都没做。
                # 归属用 task_id（挂成它的子任务），原话本身还没被拆解，正好当第一条。
                hint.update({"task": text, "task_id": bt["id"],
                             "new_task": False, "score": bt["score"],
                             "why": f"挂到「{bt['t']}」下面"})
            else:
                hint["task"] = text
                hint["why"] = f"归到「{p['name']}」，但项目里没有对应子任务，建议新建"
        else:
            hint["task"] = text
            hint["why"] = "没有足够接近的项目，建议新建一个计划项目" if doc.get("projects") else "还没有任何计划项目"
        out.append({**it, "hint": hint})
    return out


# ---------------------------------------------------------------- 写操作

def _project_move_tokens(doc: dict, proj: dict, host: dict) -> int:
    return int(usage_of(proj, host).get("tok") or 0)


def act(root, body: dict) -> dict:
    """所有写操作的唯一入口。body = {action:..., ...}。

    返回 {ok, doc, ...}；ok=False 时 error 是给人看的中文原因。
    每个 action 都很小：读整份 → 改 → 存整份。文件几百 KB，这个开销可忽略，
    换来的是「两个写者同时改也不会互相覆盖」。
    """
    b = body if isinstance(body, dict) else {}
    action = str(b.get("action") or "").strip()
    # _LOCK 管进程内线程，_xlock 管跨进程（MCP / 命令行）。两个都持住才安全。
    with _LOCK, _xlock(root):
        doc = load(root)
        try:
            return _dispatch(root, doc, action, b)
        except _Bad as e:
            return {"ok": False, "error": str(e), "rev": doc.get("rev") or 0}


class _Bad(Exception):
    """参数不对 / 找不到对象。带中文原因直接给用户看。"""


def _need_project(doc, pid):
    p = find_project(doc, str(pid or ""))
    if p is None:
        raise _Bad("找不到这个计划项目")
    return p


def _dispatch(root, doc, action, b) -> dict:
    now = _now()
    host = host_projects()

    # ---- 项目
    if action in ("proj.add", "project.add"):
        name = str(b.get("name") or "").strip()
        if not name:
            raise _Bad("项目名不能为空")
        taken = {p["id"] for p in doc["projects"]} | {x["id"] for x in doc["inbox"]}
        proj = _norm_project({"name": name, "note": b.get("note") or "",
                              "due": b.get("due") or "",
                              "match": {"keys": b.get("keys") or []}}, taken)
        proj["created"] = now
        doc["projects"].insert(0, proj)
        _log(doc, "proj.add", name)
        doc = save(root, doc)
        _emit(root, proj["name"], "note", f"新建计划：{proj['name']}",
              f"计划新建：{proj['name']}（0 条任务，等待扩充）",
              data={"plan": True, "event": "plan.proj", "proj": proj["id"],
                    "proj_name": proj["name"]},
              tags=["新建"], nid=f"plan:proj:{proj['id']}")
        return {"ok": True, "doc": doc, "id": proj["id"]}

    if action in ("proj.del", "project.del"):
        p = _need_project(doc, b.get("id"))
        doc["projects"] = [x for x in doc["projects"] if x["id"] != p["id"]]
        _log(doc, "proj.del", p["name"])
        return {"ok": True, "doc": save(root, doc)}

    if action in ("proj.rename", "project.rename"):
        p = _need_project(doc, b.get("id"))
        name = str(b.get("name") or "").strip()
        if not name:
            raise _Bad("项目名不能为空")
        old, p["name"] = p["name"], name[:80]
        _log(doc, "proj.rename", name, f"原：{old}")
        return {"ok": True, "doc": save(root, doc)}

    if action in ("proj.link", "project.link"):
        p = _need_project(doc, b.get("id"))
        m = p.setdefault("match", {"spaces": [], "projs": [], "keys": []})
        if "spaces" in b:
            m["spaces"] = [str(x) for x in (b.get("spaces") or []) if str(x).strip()][:12]
        if "projs" in b:
            m["projs"] = [str(x) for x in (b.get("projs") or []) if str(x).strip()][:12]
        if "keys" in b:
            m["keys"] = [str(x).strip() for x in (b.get("keys") or []) if str(x).strip()][:12]
        _log(doc, "proj.link", p["name"], ",".join(m["spaces"] + m["projs"] + m["keys"]))
        save(root, doc)
        return {"ok": True, "doc": load(root), "usage": usage_of(p, host)}

    if action in ("proj.status", "project.status"):
        p = _need_project(doc, b.get("id"))
        st = str(b.get("status") or "active")
        if st not in STATUS:
            raise _Bad("状态只能是 active / paused / done")
        p["status"] = st
        _log(doc, "proj.status", p["name"], st)
        return {"ok": True, "doc": save(root, doc)}

    # ---- 任务
    if action in ("task.add", "task.addChild"):
        p = _need_project(doc, b.get("proj"))
        title = str(b.get("title") or "").strip()
        if not title:
            raise _Bad("任务名不能为空")
        taken = _all_ids(doc)
        node = _norm_node({"t": title, "due": b.get("due") or "", "note": b.get("note") or "",
                           "src": str(b.get("src") or "manual")}, taken)
        pid = str(b.get("parent") or "")
        if pid:
            parent = find_node(p["tasks"], pid)
            if parent is None:
                raise _Bad("找不到这个父任务（可能刚被删了，刷新一下）")
            parent.setdefault("children", []).append(node)
        else:
            p["tasks"].append(node)
        _log(doc, "task.add", title, f"{p['name']}")
        # 人在卡片上手动加的一条，不指望他再去点一次「同步」—— 直接进总线
        _push_now(root, p, [node])
        return {"ok": True, "doc": save(root, doc), "id": node["id"]}

    if action in ("task.toggle", "task.done"):
        p = _need_project(doc, b.get("proj"))
        n = find_node(p["tasks"], str(b.get("id") or ""))
        if n is None:
            raise _Bad("找不到这条任务")
        want = b.get("done")
        n["done"] = (not n["done"]) if want is None else bool(want)
        n["at"] = now if n["done"] else ""
        # 完成时拍 token 快照：项目累计（含本项目其它子任务的消耗），
        # 前端算相邻两次完成的差值 = 「这一步烧了多少」。
        if n["done"]:
            n["tok"] = _project_move_tokens(doc, p, host)
            _emit_done(root, p, n)
        _log(doc, "task.toggle", n["t"], "done" if n["done"] else "undone")
        return {"ok": True, "doc": save(root, doc)}

    if action in ("task.rename", "task.edit"):
        p = _need_project(doc, b.get("proj"))
        n = find_node(p["tasks"], str(b.get("id") or ""))
        if n is None:
            raise _Bad("找不到这条任务")
        if "title" in b:
            t = str(b.get("title") or "").strip()
            if not t:
                raise _Bad("任务名不能为空")
            n["t"] = t[:300]
        for k in ("due", "note", "link"):
            if k in b:
                n[k] = str(b.get(k) or "")[:2000 if k == "note" else 120]
        _log(doc, "task.edit", n["t"])
        return {"ok": True, "doc": save(root, doc)}

    if action in ("task.del", "task.remove"):
        p = _need_project(doc, b.get("proj"))
        lst = find_parent_list(p["tasks"], str(b.get("id") or ""))
        if lst is None:
            raise _Bad("找不到这条任务")
        hit = next(x for x in lst if x["id"] == b.get("id"))
        lst.remove(hit)
        _log(doc, "task.del", hit.get("t") or "")
        return {"ok": True, "doc": save(root, doc)}

    if action == "task.move":
        p = _need_project(doc, b.get("proj"))
        tid = str(b.get("id") or "")
        node = find_node(p["tasks"], tid)
        if node is None:
            raise _Bad("找不到这条任务")
        lst = find_parent_list(p["tasks"], tid)
        if lst is None:
            raise _Bad("这条任务不在这个项目里")
        # 不能把节点拖进自己的子树里（否则整棵子树从树上脱落）
        dest = find_node(p["tasks"], str(b.get("parent") or ""))
        if dest is not None:
            inside = dest["id"] == tid or find_node(node.get("children") or [], dest["id"])
            if inside:
                raise _Bad("不能把任务拖进它自己的子任务里")
        lst.remove(node)
        if dest is not None:
            dest.setdefault("children", []).append(node)
        else:
            p["tasks"].append(node)
        _log(doc, "task.move", node["t"])
        return {"ok": True, "doc": save(root, doc)}

    # ---- 收件箱
    if action in ("inbox.add",):
        text = str(b.get("text") or "").strip()
        if not text:
            raise _Bad("写点什么再添加")
        taken = _all_ids(doc)
        iid = _uid("ib_", taken)
        doc["inbox"].append({"id": iid, "t": text[:300], "at": now,
                             "note": str(b.get("note") or "")[:500]})
        _log(doc, "inbox.add", text)
        _push_inbox_now(root, [doc["inbox"][-1]])
        return {"ok": True, "doc": save(root, doc), "id": iid}

    if action in ("inbox.del",):
        before = len(doc["inbox"])
        doc["inbox"] = [x for x in doc["inbox"] if x["id"] != str(b.get("id") or "")]
        _log(doc, "inbox.del", str(b.get("t") or ""))
        return {"ok": True, "doc": save(root, doc), "removed": before - len(doc["inbox"])}

    if action in ("inbox.assign", "inbox.apply"):
        """把一条自由记录落到某项目的某个子任务下；parent 空则新建一条子任务。"""
        iid = str(b.get("id") or "")
        item = next((x for x in doc["inbox"] if x["id"] == iid), None)
        if item is None:
            raise _Bad("收件箱里已经没有这条了")
        pid = str(b.get("proj") or "")
        parent = str(b.get("parent") or "")
        title = str(b.get("title") or item["t"]).strip()
        if not pid:
            # 没指定项目 = 允许「新建项目」。项目名取原话的第一小句、截断到 24 字 ——
            # 拿一整句 30 多字的自然语言当项目名，侧边栏和下拉框都会撑破。
            nm = re.split(r"[，,。.；;：:！!？?\n]", title)[0].strip() or title
            taken = _all_ids(doc)
            proj = _norm_project({"name": nm[:24], "note": "",
                                  "match": {"keys": []}}, taken)
            proj["created"] = now
            doc["projects"].insert(0, proj)
            item_node = proj
            pid = proj["id"]
        else:
            proj = _need_project(doc, pid)
            item_node = None
        node = None
        if item_node is None:
            if parent:
                host_node = find_node(proj["tasks"], parent)
                if host_node is None:
                    raise _Bad("找不到这个子任务（可能刚被删了）")
                taken = _all_ids(doc)
                node = _norm_node({"t": title, "src": "inbox"}, taken)
                host_node.setdefault("children", []).append(node)
            else:
                taken = _all_ids(doc)
                node = _norm_node({"t": title, "src": "inbox"}, taken)
                proj["tasks"].append(node)
        doc["inbox"] = [x for x in doc["inbox"] if x["id"] != iid]
        _log(doc, "inbox.assign", title, f"{proj['name']}")
        if node is not None:
            _push_now(root, proj, [node])
        else:
            _emit(root, proj["name"], "note", f"新建计划：{proj['name']}",
                  f"由收件箱归位新建：{title}",
                  data={"plan": True, "event": "plan.proj", "proj": proj["id"]},
                  tags=["新建"], nid=f"plan:proj:{proj['id']}")
        return {"ok": True, "doc": save(root, doc), "proj": pid}

    if action == "inbox.clear":
        n = len(doc["inbox"])
        doc["inbox"] = []
        _log(doc, "inbox.clear", "", f"{n} 条")
        return {"ok": True, "doc": save(root, doc), "removed": n}

    # ---- 导入
    if action == "import":
        text = str(b.get("text") or "")
        if not text.strip():
            raise _Bad("没有可导入的内容")
        parsed = parse_plan(text, str(b.get("name") or ""))
        if not parsed["tasks"]:
            raise _Bad("没解析出任何任务：内容里没有标题、列表或编号行")
        if not b.get("apply"):
            return {"ok": True, "dry": True, "parsed": parsed}
        pid = str(b.get("proj") or "")
        proj = find_project(doc, pid) if pid else None
        if proj is None:
            named = next((x for x in doc["projects"] if x["name"] == parsed["name"]), None)
            if named is not None and str(b.get("mode") or "merge") == "merge":
                proj = named
        if proj is None:
            taken = _all_ids(doc)
            proj = _norm_project({"name": parsed["name"], "note": str(b.get("note") or ""),
                                  "match": {"keys": [parsed["name"]]}}, taken)
            proj["created"] = now
            # 新建项目的这一支必须自己把任务塞进去：_norm_project 传进去的 dict
            # 没有 tasks，光 insert 项目会落一个空壳（导入成功但一条任务都没有）。
            proj["tasks"] = [_norm_node(t, taken) for t in parsed["tasks"]]
            doc["projects"].insert(0, proj)
            added = sum(1 for _n, _s, _p in walk(proj["tasks"]))
        else:
            taken = _all_ids(doc)
            src = [_norm_node(t, taken) for t in parsed["tasks"]]
            if str(b.get("mode") or "merge") == "replace":
                added = len(src)
                proj["tasks"] = src
            else:
                added = merge_tasks(proj["tasks"], src)
        _log(doc, "import", parsed["name"], f"{added} 条")
        doc = save(root, doc)
        _emit(root, proj["name"], "note", f"导入计划：{parsed['name']}",
              f"导入 {added} 条任务（{parsed['name']}）",
              data={"plan": True, "event": "plan.import", "proj": proj["id"],
                    "added": added},
              tags=["导入"])
        return {"ok": True, "doc": doc, "proj": proj["id"], "added": added,
                "parsed": parsed}

    # ---- 推给 agent 总线
    if action in ("agent.export", "agent.push"):
        pid = str(b.get("proj") or "")
        tid = str(b.get("id") or "")
        scope = str(b.get("scope") or "open")      # open=只推未完成 / all=全都推
        jobs = []
        if pid and tid:
            p = _need_project(doc, pid)
            node = find_node(p["tasks"], tid)
            if node is None:
                raise _Bad("找不到这条任务")
            jobs.append((p, [node]))
        elif pid:
            p = _need_project(doc, pid)
            jobs.append((p, [x for x, _s, _d in walk(p["tasks"])
                             if scope == "all" or not x.get("done")]))
        else:
            for p in doc["projects"]:
                nodes = [x for x, _s, _d in walk(p["tasks"])
                         if scope == "all" or not x.get("done")]
                if nodes:
                    jobs.append((p, nodes))
        out = {"written": 0, "dup": 0, "tasks": 0}
        for p, nodes in jobs:
            r = _push_now(root, p, nodes)
            out["written"] += r["written"]
            out["dup"] += r["dup"]
            out["tasks"] += len(nodes)
        if not pid and not tid:
            r = _push_inbox_now(root, list(doc["inbox"]))
            out["written"] += r["written"]
            out["dup"] += r["dup"]
        _log(doc, "agent.export", scope, f"{out['written']} 条")
        return {"ok": True, "doc": save(root, doc), **out}

    raise _Bad(f"不认识的 action：{action or '（空）'}")


def _all_ids(doc) -> set:
    out = {p["id"] for p in doc.get("projects") or []}
    for p in doc.get("projects") or []:
        for n, _s, _p in walk(p.get("tasks") or []):
            out.add(n["id"])
    out |= {x["id"] for x in doc.get("inbox") or []}
    return out


def _th(t: str) -> str:
    """标题短指纹。它进事件 id：改过名的任务能再推一次，没改的重推是空操作。"""
    return hashlib.sha1(norm_text(t or "").encode("utf-8")).hexdigest()[:6]


def _emit_many(root, bodies: list) -> dict:
    """批量写总线，一趟算完。失败一律吞掉 —— 写不出总线不能挡住人点勾。"""
    if not bodies:
        return {"written": 0, "dup": 0}
    try:
        import agentbus  # noqa: PLC0415
        r = agentbus.add_event(root, bodies, src="plan")
        return {"written": int(r.get("written") or 0), "dup": int(r.get("dup") or 0)}
    except Exception:                                  # noqa: BLE001
        return {"written": 0, "dup": 0}


def _emit(root, ns: str, kind: str, title: str, text: str, *,
          data=None, tags=None, nid: str = "") -> dict:
    body = {"kind": kind, "ns": ns or "AI 工作计划", "title": str(title)[:120],
            "text": str(text)[:4000], "data": data or {},
            "tags": list(tags or []) + ["计划"]}
    if nid:
        body["id"] = nid
    return _emit_many(root, [body])


def _push_event(p: dict, node: dict) -> dict:
    """一条任务 → 一条总线事件。agent 侧靠 src=plan / data.plan 区分来源。"""
    done = bool(node.get("done"))
    return {
        "id": f"plan:push:{p['id']}:{node['id']}:{_th(node.get('t') or '')}",
        "kind": "task", "ns": p.get("name") or "AI 工作计划",
        "title": node.get("t") or "",
        "text": (f"计划已完成：{node.get('t')}（{p.get('name')}）" if done
                 else f"计划待办：{node.get('t')}（{p.get('name')}）"),
        "data": {"plan": True, "event": "plan.push", "proj": p["id"],
                 "proj_name": p.get("name"), "task": node["id"],
                 "status": "done" if done else "open"},
        "tags": ["完成"] if done else ["待办"],
    }


def _push_now(root, p: dict, nodes: list) -> dict:
    """把这几条推给 agent 并打上「推过」的戳；推过且没改名的不再写。"""
    now = _now()
    bodies = []
    for node in nodes:
        h = _th(node.get("t") or "")
        if node.get("agent_h") == h and node.get("agent_at"):
            continue
        bodies.append(_push_event(p, node))
        node["agent_h"], node["agent_at"] = h, now
    return _emit_many(root, bodies)


def _push_inbox_now(root, items: list) -> dict:
    """收件箱的单条也进总线：它还没归位，agent 更需要知道「有这么件事」。"""
    bodies = []
    for it in items:
        h = _th(it.get("t") or "")
        if it.get("agent_h") == h and it.get("agent_at"):
            continue
        bodies.append({
            "id": f"plan:push:inbox:{it['id']}:{h}",
            "kind": "task", "ns": "AI 工作计划",
            "title": it.get("t") or "",
            "text": f"收件箱（还没归位）：{it.get('t')}",
            "data": {"plan": True, "event": "plan.inbox", "inbox": it["id"],
                     "status": "inbox"},
            "tags": ["收件箱"],
        })
        it["agent_h"], it["agent_at"] = h, _now()
    return _emit_many(root, bodies)


def _emit_done(root, proj, node) -> None:
    """完成一条任务就写进 agent 总线。别的卡（简报 / 流水）不必知道计划模块存在。"""
    _emit(root, proj.get("name") or "AI 工作计划", "task",
          node.get("t") or "",
          f"计划任务完成：{node.get('t')}（{proj.get('name')}）",
          data={"plan": True, "event": "plan.done", "proj": proj.get("id"),
                "proj_name": proj.get("name"), "task": node.get("id"),
                "tok": node.get("tok") or 0},
          tags=["完成"],
          nid=f"plan:done:{proj.get('id')}:{node.get('id')}:{int(time.time()) // 60}")


# ---------------------------------------------------------------- 视图

def _short_name(k: str, n: str) -> str:
    """`proj:` 的 n 是目录 slug（Users-pamphile-Documents-AI项目-foo），
    直接摆进下拉框没法读。剥掉固定前缀，剩下的就是项目名。"""
    s = str(n or "")
    if str(k).startswith("proj:"):
        for pre in ("Users-pamphile-Documents-AI项目-", "Users-pamphile-Documents-",
                    "Users-pamphile-WorkBuddy-", "Users-pamphile-"):
            if s.startswith(pre):
                s = s[len(pre):]
                break
    return s or str(n or "")


def payload(root, cfg: dict | None = None) -> dict:
    """卡片要的全部：项目树 + 每个项目的 token 联通 + 收件箱（带代码建议）+ 统计。"""
    doc = load(root)
    host = host_projects()
    projects = []
    for p in doc["projects"]:
        done, total = count(p["tasks"])
        u = usage_of(p, host)
        on = 0
        for n, _s, _pd in walk(p["tasks"]):
            synced = bool(n.get("agent_at")) and n.get("agent_h") == _th(n.get("t") or "")
            n["a"] = bool(synced or n.get("done"))     # 只给前端看，不落盘
            on += 1 if n["a"] else 0
        projects.append({**p, "stat": {"done": done, "total": total,
                                       "pct": round(done * 100 / total) if total else 0},
                         "usage": u, "agent": {"on": on, "total": total}})
    today = _today()
    done_today = sum(1 for p in projects for n, _s, _pd in walk(p["tasks"])
                     if n.get("done") and str(n.get("at") or "").startswith(today))
    all_done = sum(p["stat"]["done"] for p in projects)
    all_total = sum(p["stat"]["total"] for p in projects)
    return {
        "ok": True,
        "rev": doc["rev"],
        "updated_at": doc["updated_at"],
        "projects": projects,
        "inbox": candidates(root),
        "inbox_n": len(doc["inbox"]),
        "stat": {"done": all_done, "total": all_total, "done_today": done_today,
                 "pct": round(all_done * 100 / all_total) if all_total else 0},
        "host": {"spaces": [{"k": x["k"], "n": x["n"], "sn": _short_name(x["k"], x["n"]),
                             "tok": int(x.get("tok") or 0)} for x in (host.get("spaces") or [])],
                 "projs": [{"k": x["k"], "n": x["n"], "sn": _short_name(x["k"], x["n"]),
                            "tok": int(x.get("tok") or 0)} for x in (host.get("projs") or [])],
                 "at": host.get("at") or "", "err": host.get("err") or ""},
        "dir": str(plan_dir(root)),
    }


def triage_prompt(items: list[dict], projects: list[dict]) -> str:
    """给模型看的归位题面。候选只有「已有项目 + 已有子任务」，不许它凭空造 id。"""
    lines = ["你是任务归类助手。把每条「自由记录」归到下面某个计划的某个子任务；",
             "如果都不合适，就提出新建子任务或新建计划。只能从给出的编号里选。", ""]
    lines.append("【计划与子任务】")
    for p in projects:
        lines.append(f"- 项目 {p['id']}｜{p['name']}")
        for t in flatten_names(p.get("tasks") or []):
            lines.append(f"    - 子任务 {t['id']}｜{'  ' * t['depth']}{t['t']}")
    lines.append("")
    lines.append("【待归位的自由记录】")
    for it in items:
        lines.append(f"- 记录 {it['id']}｜{it['t']}")
    lines += ["", "只输出 JSON 对象，不要解释：",
              '{"items":[{"id":"记录id","proj":"项目id或空","parent":"子任务id或空",'
              '"title":"若新建，给出任务标题","new_proj":"若新建项目，给出项目名"}]}',
              "规则：挂到已有子任务就填 parent；归到项目但没对应子任务就 parent 留空并给 title；",
              "都不合适就 proj 留空并给 new_proj。"]
    return "\n".join(lines)


def apply_ai_assign(root, items: list[dict]) -> dict:
    """把模型/agent 的判断落库。每项独立成败，一条坏不影响别的。

    这里**不**自己持锁：act() 会加锁。外面再套一层的话，
    同一进程对同一锁文件开两个 fd 抢 flock 会自己把自己卡死。
    """
    ok, bad = 0, []
    for it in items:
        try:
            r = act(root, {"action": "inbox.assign", "id": it.get("id"),
                           "proj": it.get("proj") or "",
                           "parent": it.get("parent") or "",
                           "title": it.get("title") or ""})
            if r.get("ok"):
                ok += 1
            else:
                bad.append({"id": it.get("id"), "error": r.get("error")})
        except Exception as e:                         # noqa: BLE001
            bad.append({"id": it.get("id"), "error": f"{type(e).__name__}: {e}"})
    return {"ok": True, "applied": ok, "failed": bad}


# ---------------------------------------------------------------- markdown 镜像

def export_md(root, doc: dict | None = None) -> None:
    """人看的那一份：层级用缩进、完成用 [x]。给 Finder / 别的编辑器开。"""
    doc = doc if doc is not None else load(root)
    out = ["# AI 工作计划", "", f"_更新：{doc.get('updated_at') or ''}　rev {doc.get('rev')}_", ""]
    for p in doc["projects"]:
        done, total = count(p["tasks"])
        out.append(f"## {p['name']}（{done}/{total}）")
        if p.get("note"):
            out.append(f"> {p['note']}")
        def dump(nodes, d=0):
            for n in nodes:
                box = "x" if n.get("done") else " "
                due = f"  ·  截止 {n['due']}" if n.get("due") else ""
                out.append(f"{'  ' * d}- [{box}] {n.get('t')}{due}")
                dump(n.get("children") or [], d + 1)
        dump(p["tasks"])
        out.append("")
    if doc["inbox"]:
        out += ["## 待归位", ""] + [f"- {x['t']}" for x in doc["inbox"]] + [""]
    p = plan_dir(root) / "计划.md"
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text("\n".join(out), encoding="utf-8")


if __name__ == "__main__":                              # 手动看一眼/跑一次导入
    import argparse
    ap = argparse.ArgumentParser(description="AI 工作计划（命令行）")
    ap.add_argument("--root", default="")
    ap.add_argument("--import", dest="imp", default="", help="从文件导入（先预览）")
    ap.add_argument("--apply", action="store_true")
    ap.add_argument("--name", default="")
    a = ap.parse_args()
    r = a.root or default_root()
    if a.imp:
        text = Path(a.imp).read_text(encoding="utf-8")
        res = act(r, {"action": "import", "text": text, "name": a.name, "apply": a.apply})
    else:
        res = payload(r)
    print(json.dumps(res, ensure_ascii=False, indent=1)[:4000])
