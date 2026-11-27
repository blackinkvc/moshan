#!/usr/bin/env python3
"""拿不准是图集还是文章时，挂起一条、**在微信聊天窗里问一句**、等用户回一句话。

用户原话（2026-11-20，第二次强调）：「存图和存文章，还是有的分不清，以后有疑惑的，
直接 clawbot 聊天窗发对话问我好了，我直接回复文字」。

所以这里只做三件事：

  ① 挂：judge 说两边都像（ask=True）→ add() 记一条，**立刻**排进发送队列；
  ② 问：后台线程把「问句」发到微信（wechat_push，不经过 4B）——一条一并发，
     多条合成一条消息，免得手机上一串叮咚。发失败会退避重试，服务重启后
     仍会自动补发（`pushed_at` 存在 _待定.json 里，重启不丢）；
  ③ 认：用户回「存图」「存文章」「算了」这类话（回 1 / 2 / 3 也认），answer()
     找到那一条，交给 linkbox / articlebox 注册进来的处理函数落库，办完
     再把结果发回聊天窗。

为什么问句的发送要放在这里，而不是让 intake 顺手发：
intake 的批次看门狗活在内存里，口袋一重启就没了；网页上贴链接进来的那条
更没有批次。问句是**挂起单自己的事**，所以发送也归它管 —— 只要 _待定.json
里有一条没发出去，后台线程两秒内就会补上。

存放位置：`04-存图/_待定.json`（下划线开头，扫描器与查重都会跳过）。
一个问题活 7 天；只回一个数字时，只认 6 小时内问过的，免得误伤正常聊天。
"""
from __future__ import annotations

import json
import re
import threading
import time
import uuid
from pathlib import Path

import paths  # 交付版：待定队列挂在资料库下，不写死开发机的目录

PEND_PATH = paths.data_root() / "04-存图" / "_待定.json"
MAX_AGE = 7 * 86400           # 挂了这么久还没答，就不再认了
SHORT_AGE = 6 * 3600          # 只回「1 / 2」这种，只认这么久之内问过的
KEEP_DONE = 40                # 已答的留几条，够回溯就行
RETRY_MAX = 600               # 发不出去时最长退避到 10 分钟

_lock = threading.RLock()
_RESOLVERS: dict[str, callable] = {}

# 回答的词。短句允许「包含」，长句只认全串 ——
# 「这篇文章不要存」里也有「文章」，按包含判就答反了。
_G = re.compile(r"^(?:1|g|gallery|图|图集|存图|存图片|照片|相册|要图|按图|存成图|"
                r"1[.、)]?存图|存图[。.!！]?)(?:吧|啊|呗|嗯)?$", re.I)
_A = re.compile(r"^(?:2|a|article|文|文章|正文|阅读|存文章|要文章|按文章|存成文章|"
                r"2[.、)]?存文章|存文章[。.!！]?)(?:吧|啊|呗|嗯)?$", re.I)
_D = re.compile(r"^(?:3|算了|不存|不存了|别存|别存了|不要了|取消|丢掉|删掉|都不用了|"
                r"都不用|都不要)(?:吧|啊|呗|嗯)?$")
_ALL = ("都", "全", "两条", "全部", "一起", "俩")
_G_WORDS = ("存图", "图集", "存图片", "照片", "相册")
_A_WORDS = ("存文章", "文章", "正文", "阅读")
_D_WORDS = ("算了", "不存", "别存", "不用了", "都不要", "取消", "丢掉", "删掉")
SHORT = 14                    # 「包含」判定只对这么长的句子生效


def set_resolvers(link=None, article=None) -> None:
    """由 server.py 在装配完 LinkBox / ArticleBox 之后注册。

    link(entry, mode) / article(entry, mode) 要返回
    {"ok": bool, "job": 任务快照或 {"id","kind"}, "message": 一句人话}。
    """
    if link:
        _RESOLVERS["link"] = link
    if article:
        _RESOLVERS["article"] = article


# ---------------- 存取 ----------------

def _load() -> list[dict]:
    try:
        raw = json.loads(PEND_PATH.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return []
    items = raw.get("items") if isinstance(raw, dict) else raw
    return [i for i in (items or []) if isinstance(i, dict)]


def _save(items: list[dict]) -> None:
    try:
        PEND_PATH.parent.mkdir(parents=True, exist_ok=True)
        tmp = PEND_PATH.with_suffix(".tmp")
        tmp.write_text(json.dumps({"v": 1, "items": items}, ensure_ascii=False, indent=1),
                       encoding="utf-8")
        tmp.replace(PEND_PATH)
    except OSError:
        pass


def _prune(items: list[dict]) -> list[dict]:
    now = time.time()
    keep, done = [], 0
    for i in reversed(items):                    # 新的在前
        if i.get("state") != "open":
            done += 1
            if done > KEEP_DONE:
                continue
        elif now - float(i.get("asked_at") or 0) > MAX_AGE:
            i["state"] = "expired"
        keep.append(i)
    keep.reverse()
    return keep


# ---------------- 挂起 ----------------

def add(kind: str, job: str = "", url: str = "", title: str = "",
        platform: str = "", dir_name: str = "", text_len: int = 0,
        n_img: int = 0, n_video: int = 0, suggest: str = "", why: str = "",
        agent: str = "", channel: str = "", conv: str = "") -> dict:
    """挂一条，返回这条本身（含 id 与要发出去的问题）。

    agent/channel 留空就用微信那条默认会话 —— 用户只在聊天窗里回话，
    所以「没说是哪条渠道」等于「发微信」，不用调用方层层往下传。
    """
    e = {
        "id": "p" + uuid.uuid4().hex[:8],
        "kind": kind,                       # link（已经是图集目录） / article（还没落目录）
        "job": job, "url": url, "title": title, "platform": platform,
        "dir_name": dir_name, "text_len": int(text_len or 0), "n_img": int(n_img or 0),
        "n_video": int(n_video or 0),
        "suggest": suggest, "why": why,
        "agent": agent or "main", "channel": channel or _default_channel(), "conv": conv,
        "state": "open", "asked_at": time.time(), "answered_at": 0.0,
        "pushed_at": 0.0, "push_tries": 0, "next_try": 0.0,
    }
    with _lock:
        items = _prune(_load())
        items.append(e)
        _save(items)
    start()                                  # 后台两秒内把问句发出去
    return e


def question(e: dict) -> str:
    """单条的问句（工作台的问句行也拿它做文案）。

    2026-11-26 用户改的口径：三个选项都塞进一句话，用「回1/回2/回3」标号，
    不再另起一行括号说明 —— 手机上多一行就多一次滑动。数字解码见 parse_answer，
    `_G/_A/_D` 本来就认 1/2/3，改文案不用动解析。
    """
    return (_one_line(e) + "，两边都像 —— 回1「存图」；回2「存文章」；"
            "回3「算了」就当没存；")


def _one_line(e: dict) -> str:
    title = str(e.get("title") or "").strip()
    name = ("《%s》" % title) if title else "这条链接"
    n, c = int(e.get("n_img") or 0), int(e.get("text_len") or 0)
    v = int(e.get("n_video") or 0)
    # 视频说「段」：抖音一条作品就是一个视频，说成「1 张图」很别扭。
    if v and n <= v:
        what = f"{v} 段视频"
    elif v:
        what = f"{n - v} 张图 + {v} 段视频"
    else:
        what = f"{n} 张图"
    return f"{name}：{what}、{c} 字"


def compose(items: list[dict]) -> str:
    """一条问一句话；多条合成一条，别拿一串消息轰炸手机。"""
    if not items:
        return ""
    if len(items) == 1:
        return question(items[0])          # 选项已经并进问句，别再补一行括号
    lines = [f"有 {len(items)} 条拿不准，回一句就行："]
    for k, e in enumerate(items, 1):
        lines.append(f"{k}. {_one_line(e)}")
    lines.append("回「存图」/「存文章」＝最近那条；要全部一样就回「都存图」/「都存文章」；"
                 "「算了」＝都不存。")
    return "\n".join(lines)


def open_items() -> list[dict]:
    """还没答的，新的在前。顺手把过期的清掉。"""
    with _lock:
        items = _load()
        pruned = _prune(items)
        if len(pruned) != len(items):
            _save(pruned)
        return [dict(i) for i in reversed(pruned) if i.get("state") == "open"]


def get(pid: str) -> dict | None:
    for i in open_items():
        if i.get("id") == pid:
            return i
    return None


def has_open() -> bool:
    return bool(open_items())


# ---------------- 把问句发到聊天窗 ----------------

_DELIVERY = {"thread": None, "log": None}


def _default_channel() -> str:
    try:
        import wechat_push
        return wechat_push.CHANNEL
    except Exception:                             # noqa: BLE001
        return "openclaw-weixin"


def _log(rec: dict) -> None:
    fn = _DELIVERY.get("log")
    if fn:
        try:
            fn(rec)
        except Exception:                         # noqa: BLE001
            pass


def start(log=None) -> None:
    """起发送线程（幂等）。server 启动时调一次，add() 也会顺手调。"""
    if log:
        _DELIVERY["log"] = log
    with _lock:
        if _DELIVERY["thread"]:
            return
        t = threading.Thread(target=_loop, name="pending-deliver", daemon=True)
        _DELIVERY["thread"] = t
    t.start()


def _loop() -> None:
    while True:
        time.sleep(2.0)
        try:
            flush()
        except Exception as e:                    # noqa: BLE001
            _log({"event": "pending_push_error", "error": str(e)[:200]})


def flush() -> int:
    """把「还没发出去」的挂起单发到聊天窗。返回这一次发了几条。

    发送失败不清标记，只把 next_try 往后推（30s 起，翻倍到 10 分钟）。
    所以网关半夜挂了也不用管：它一活，最多十分钟内那句问话自己会到。
    """
    items = [i for i in open_items() if not i.get("pushed_at")]
    if not items:
        return 0
    now = time.time()
    ready = [i for i in items if now >= float(i.get("next_try") or 0)]
    if not ready:
        return 0
    text = compose(ready)
    ok = False
    try:
        import wechat_push
        ok = wechat_push.send(text, agent=str(ready[0].get("agent") or "main"),
                             channel=str(ready[0].get("channel") or _default_channel()),
                             log=_DELIVERY.get("log"))
    except Exception as e:                        # noqa: BLE001
        _log({"event": "pending_push_error", "error": str(e)[:200]})
    ids = [i["id"] for i in ready]
    with _lock:
        all_items = _load()
        for i in all_items:
            if i.get("id") not in ids:
                continue
            if ok:
                i["pushed_at"] = now
                i["next_try"] = 0.0
            else:
                tries = int(i.get("push_tries") or 0) + 1
                i["push_tries"] = tries
                i["next_try"] = now + min(RETRY_MAX, 30 * (2 ** (tries - 1)))
        _save(all_items)
    _log({"event": "pending_push", "ok": ok, "n": len(ready), "text": text[:200]})
    return len(ready) if ok else 0


# ---------------- 回答 ----------------

def _short(s: str) -> bool:
    return len(s) <= SHORT


def parse_answer(text: str) -> tuple[str, str]:
    """把一句话解析成 (mode, scope)。mode 为空表示「这不是回答」。

    scope = "one" 只答最近那条，"all" 答全部（「都存图」）。
    先认全串（防「这篇文章不要存」这种反话），再对短句放开包含匹配，
    这样「这个存图吧」「那就存文章」也能直接用。
    """
    s = re.sub(r"\s+", "", str(text or "")).strip("。.!！,，~ ")
    if not s:
        return "", ""
    # 模板写的是「回1/回2/回3」，用户很可能连「回」字一起打回来。
    # 先把开头那个「回(复)」剥掉，剩下的仍走下面同一套判定。
    s = re.sub(r"^回(?:复)?(?=[123])", "", s)
    if _D.match(s):
        return "drop", "one"
    for prefix in _ALL:
        if s.startswith(prefix) and len(s) <= 8:
            rest = s[len(prefix):]
            if _G.match(rest):
                return "gallery", "all"
            if _A.match(rest):
                return "article", "all"
    if _G.match(s):
        return "gallery", "one"
    if _A.match(s):
        return "article", "one"
    # 短句放开成「包含」：用户说话不会照着模板念。
    # 但要像是**在下指令**才算 —— 带「存」字、或者「…吧 / 呗 / 嗯」这种口气、
    # 或者极短（「图集」「文章」）。否则「这篇文章写得真好」也会被当成回答。
    if _short(s) and (_has_verb(s) or s.endswith(("吧", "呗", "啊", "嗯", "了", "的"))
                      or len(s) <= 4):
        if any(w in s for w in _D_WORDS):
            return "drop", "one"
        if any(w in s for w in _G_WORDS):
            return "gallery", "one"
        if any(w in s for w in _A_WORDS):
            return "article", "one"
    return "", ""


def _has_verb(s: str) -> bool:
    return "存" in s or "按" in s or "当" in s or "要" in s


def _clean_title(t: str) -> str:
    return re.sub(r"[\s，。！？、,.!?:：；;「」『』《》“”\"'()（）\[\]【】|｜/\\-—_·…]+", "",
                  str(t or ""))


def _pick_by_title(text: str, items: list[dict]) -> dict | None:
    """「《窗边》存图」这种指名道姓的回答：按标题片段找那一条。"""
    t = _clean_title(text)
    if len(t) < 3:
        return None
    for e in items:                               # 新的在前
        title = _clean_title(e.get("title") or "")
        if not title:
            continue
        for n in (10, 8, 6, 4, 3):
            frag = title[:n]
            if len(frag) >= 3 and frag in t:
                return e
    return None


def _scope_all(text: str) -> bool:
    s = re.sub(r"\s+", "", str(text or ""))
    return any(w in s for w in ("都", "全部", "两条", "俩", "一起", "全存", "全按"))


def answer(text: str, by: str = "回复") -> dict:
    """用户回了一句话：认出来就处理，认不出来就老实说没认出。

    返回 {"matched": bool, "receipt": 立刻回给用户的话, ...}。
    真正的落库在后台线程里做（转存/再抓要几秒，别卡住回话）。
    """
    mode, scope = parse_answer(text)
    if not mode:
        return {"matched": False, "reason": "not_an_answer"}
    items = open_items()
    if not items:
        return {"matched": False, "reason": "no_pending"}
    # 只回「1 / 2」这种，要求是最近问的，免得几天前的问题被一句闲聊带走
    bare = re.sub(r"\s+", "", str(text or ""))
    if len(bare) <= 1 and time.time() - float(items[0].get("asked_at") or 0) > SHORT_AGE:
        return {"matched": False, "reason": "too_old"}

    if scope == "all" or _scope_all(bare):
        todo = items
    else:
        hit = _pick_by_title(bare, items)
        todo = [hit] if hit else items[:1]

    for e in todo:
        _dispatch(e, mode, by)
    left = len(items) - len(todo)
    if mode == "drop":
        receipt = "好，这条不存了。" if len(todo) == 1 else "好，这 %d 条都不存了。" % len(todo)
    elif len(todo) > 1:
        receipt = "好，这 %d 条都按%s存，弄好回你。" % (
            len(todo), "图集" if mode == "gallery" else "文章")
    else:
        name = ("《%s》" % todo[0].get("title")) if todo[0].get("title") else "这条"
        receipt = "好，%s按%s存，弄好回你。" % (
            name, "图集" if mode == "gallery" else "文章")
    if left > 0:
        receipt += "另外还有 %d 条等你定。" % left
    return {"matched": True, "mode": mode, "count": len(todo),
            "ids": [e["id"] for e in todo], "receipt": receipt,
            "job": {"id": todo[0].get("job"), "kind": todo[0].get("kind")}}


def resolve(pid: str, mode: str, by: str = "选") -> dict:
    """点名处理某一条（手机页上点按钮走这里）。"""
    if mode not in ("gallery", "article", "drop"):
        return {"ok": False, "error": "mode 只能是 gallery / article / drop"}
    e = get(pid)
    if not e:
        return {"ok": False, "error": "这条已经处理过或过期了"}
    _dispatch(e, mode, by)
    return {"ok": True, "matched": True, "mode": mode,
            "receipt": "好，按%s存，弄好回你。" % (
                "你说的" if mode == "drop" else ("图集" if mode == "gallery" else "文章")),
            "job": {"id": e.get("job"), "kind": e.get("kind")}}


def _dispatch(e: dict, mode: str, by: str) -> None:
    # 先标一下「已经在办了」：处理要几秒，这期间别让同一个问题再被问/再被答
    with _lock:
        items = _load()
        for i in items:
            if i.get("id") == e.get("id"):
                i["state"] = "working"
                i["answered_at"] = time.time()
                i["by"] = by
                break
        _save(items)
    threading.Thread(target=_work, args=(dict(e), mode, by), daemon=True).start()


def _work(e: dict, mode: str, by: str) -> None:
    """后台真的去落库，完事把结果发回聊天窗。"""
    pid = e.get("id") or ""
    res: dict = {"ok": False, "message": ""}
    try:
        if mode == "drop":
            res = {"ok": True, "message": "这条不存了（已经落盘的留在原处）"}
        else:
            fn = _RESOLVERS.get(e.get("kind") or "")
            res = fn(e, mode) if fn else {"ok": False, "message": "没有对应的处理函数"}
    except Exception as exc:                      # noqa: BLE001
        res = {"ok": False, "message": "处理失败：%s" % str(exc)[:120]}
    _close(pid, mode, by, res)
    msg = str(res.get("message") or "").strip()
    if msg:
        # 不写渠道也发微信：这是挂起单的**答复**，用户就等在聊天窗那边
        ok = False
        try:
            import wechat_push
            ok = wechat_push.send(msg, agent=str(e.get("agent") or "main"),
                                  channel=str(e.get("channel") or "") or _default_channel(),
                                  log=_DELIVERY.get("log"))
        except Exception as exc:                  # noqa: BLE001
            _log({"event": "pending_done_push_error", "error": str(exc)[:160]})
        _log({"event": "pending_done_push", "ok": ok, "mode": mode,
              "text": msg[:120]})


def _close(pid: str, mode: str, by: str, res: dict) -> None:
    with _lock:
        items = _load()
        for i in items:
            if i.get("id") == pid:
                i["state"] = "done" if res.get("ok") else "failed"
                i["mode"] = mode
                i["answered_at"] = time.time()
                i["by"] = by
                i["result"] = str(res.get("message") or "")[:200]
                break
        _save(_prune(items))
