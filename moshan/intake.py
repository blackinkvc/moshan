#!/usr/bin/env python3
"""微信入站：链接由代码接管，4B 不参与前半程。

以前的链路是「4B 读消息 → 决定调哪个工具 → 工具同步阻塞等抓完 → 4B 复述结果」。
四个坑全在这条链上：小模型会漏调、会把存图叫成存文章、一次两条以上必超时、
它复述的还可能是上一轮的旧结论。

现在改成三段，各管一段，互不依赖：

  ① 入站（这里，纯代码）：正则抽出消息里**全部**链接 → 按原话判存图/文章 →
     一条一个任务丢进队列 → 立刻返回一句模板回执。不调模型、不等抓取。
  ② 抓取（cuntu/4B 各干各的）：cuntu 抓图抓正文，判定用抓到的内容，
     判错了就在本地转存，不再重抓一遍网页。
  ③ 收尾（这里）：这一批全部进终态 → 拼一句「已存入《…》，N 张图」
     → wechat_push 直接发回微信，同样不经过模型。

4B 只剩它擅长的活：给文件夹起名、给文章分主题打标签（naming / article）。
那些都在抓完之后跑，失败了也只是名字难看一点，不影响东西有没有存下来。
"""
from __future__ import annotations

import threading
import time

import judge
import linkbox
import pending
import wechat_push

WAIT_TICK = 1.5          # 每轮查状态的间隔
MAX_WAIT = 900           # 一批最多等这么久（cuntu 单篇上限 600 秒）
DUP_WINDOW = 180         # 同一链接这么久之内不重复入队

_TERMINAL = ("done", "error", "unsupported", "routed")
_ASK = "ask"             # 两边都像，正等着用户回「存图 / 存文章」


def _short(s: str, n: int = 60) -> str:
    s = " ".join(str(s or "").split())
    return s if len(s) <= n else s[:n] + "…"


class Intake:
    def __init__(self, links, articles, log=None) -> None:
        self.links = links
        self.articles = articles
        self.log = log or (lambda r: None)
        self._lock = threading.Lock()
        self._batches: dict[str, dict] = {}
        self._recent: dict[str, float] = {}      # url -> 入队时刻

    # ---------------- 入站 ----------------

    def accept(self, text: str, hint: str = "", agent: str = "main",
               channel: str = wechat_push.CHANNEL, conv: str = "") -> dict:
        """抽链接、入队，立刻返回。抓取在后台跑，结果由 push 发出去。"""
        raw = (text or "").strip()
        urls = linkbox.split_links(raw)
        if not urls:
            # 没链接也可能是对我们那句问话的回答（「1」「存图」「文章」）。
            # 只有真的挂着问题时才认，否则「1」这种在正常聊天里太容易误伤。
            if pending.has_open():
                ans = pending.answer(raw, by="回复")
                if ans.get("matched"):
                    self.log({"event": "intake_answer", "text": raw[:40],
                              "mode": ans.get("mode"), "n": ans.get("count")})
                    return {"ok": True, "count": 0, "answered": True,
                            "receipt": ans.get("receipt") or "好，按你说的存。"}
            return {"ok": False, "reason": "no_link"}
        want = judge.keyword_of(hint or raw)
        now = time.time()
        items, skipped = [], []
        for u in urls:
            with self._lock:
                last = self._recent.get(u)
                if last and now - last < DUP_WINDOW:
                    skipped.append(u)
                    continue
                self._recent[u] = now
            if want == "article":
                job = self.articles.start(u, hint=hint or raw)
                kind = "article"
            else:
                job = self.links.start(u, hint=hint or raw)
                kind = "link"
            if job.get("error"):
                items.append({"url": u, "kind": kind, "id": "", "job": job,
                              "error": job["error"]})
                continue
            items.append({"url": u, "kind": kind, "id": str(job.get("id") or ""),
                          "dup": bool(job.get("dup")), "job": job})
            if not job.get("dup"):
                # 用和 /api/link 一样的「开始」事件名：微信守卫靠它判断
                # 「这条已经有人接了」，否则守卫会以为漏了、90 秒后再抓一遍。
                self.log({"event": ("article_start" if want == "article"
                                    else "link_start"),
                          "url": u, "id": job.get("id"), "head": u[:80],
                          "via": "intake"})
        if not items:
            return {"ok": True, "receipt": "这几条刚存过，没重复存。", "count": 0}

        bid = f"b{int(now * 1000) % 10**9}"
        batch = {"id": bid, "items": items, "agent": agent, "channel": channel,
                 "conv": conv, "at": now}
        with self._lock:
            self._batches[bid] = batch
        self.log({"event": "intake_batch", "id": bid, "n": len(items),
                  "urls": [i["url"] for i in items][:10]})
        threading.Thread(target=self._watch, args=(bid,), daemon=True).start()
        return {"ok": True, "batch": bid, "count": len(items),
                "receipt": self._receipt(items, skipped)}

    @staticmethod
    def _receipt(items: list[dict], skipped: list[str]) -> str:
        live = [i for i in items if not i.get("dup")]
        n = len(live)
        dup = len(items) - n
        if n <= 1 and not skipped and not dup:
            return "收到，开始存了，好了一条一条回你。"
        parts = []
        if n > 1:
            parts.append(f"收到 {n} 条链接，排上队了")
        elif n == 1:
            parts.append("收到 1 条，排上队了")
        if dup:
            parts.append(f"{dup} 条刚存过（没重复存）")
        if skipped:
            parts.append(f"{len(skipped)} 条刚才已在队列里")
        return "，".join(parts) + "。好一条回一条。"

    # ---------------- 收尾 ----------------

    def _watch(self, bid: str) -> None:
        deadline = time.time() + MAX_WAIT
        while time.time() < deadline:
            time.sleep(WAIT_TICK)
            with self._lock:
                batch = self._batches.get(bid)
            if not batch:
                return
            if not self._all_done(batch):
                continue
            text = self._summary(batch)
            if text:
                ok = wechat_push.send(text, agent=batch.get("agent") or "main",
                                      channel=batch.get("channel") or wechat_push.CHANNEL,
                                      log=self.log)
                self.log({"event": "intake_push", "id": bid, "ok": ok,
                          "text": text[:160]})
            with self._lock:
                self._batches.pop(bid, None)
            return
        self.log({"event": "intake_timeout", "id": bid})

    def _all_done(self, batch: dict) -> bool:
        for it in batch["items"]:
            if not it.get("id"):
                continue                    # 建任务就失败了，算终态
            job = (self.articles.get(it["id"]) if it["kind"] == "article"
                   else self.links.get(it["id"]))
            if not job:
                it["state"] = "error"
                it["message"] = "任务丢了（服务重启过）"
                continue
            st = job.get("state")
            # ask 也算「这一批走完了」：东西已经落盘，等的是用户一句话，
            # 不能拿它把整批卡到 900 秒超时。
            if st not in _TERMINAL and st != _ASK:
                return False
            it["state"] = st
            it["data"] = job
        return True

    @staticmethod
    def _summary(batch: dict) -> str:
        lines = []
        for it in batch["items"]:
            d = it.get("data") or {}
            state = it.get("state") or ""
            if it.get("error"):
                lines.append("没存上：" + _short(it["error"], 40))
            elif state == _ASK:
                continue        # 已经单独问过一句了，收尾时别再复述一遍
            elif state == "done":
                if it["kind"] == "article":
                    title = d.get("title") or ""
                    chars = int(d.get("chars") or 0)
                    lines.append(f"已存文章《{title}》，{chars} 字。")
                elif d.get("routed") == "article":
                    # 存图的任务被判成文章，已在本地转存进阅读库（不再重抓网页）
                    name = (d.get("dir_name") or "").split("/")[-1]
                    chars = int(d.get("chars") or 0)
                    if not name:      # 阅读库里本来就有这篇，图集已撤
                        lines.append("这条是文章，阅读库里已经有了，没重复存。")
                    else:
                        lines.append(f"这条其实是文章，已存进阅读《{name}》"
                                     + (f"，{chars} 字。" if chars else "。"))
                elif d.get("routed") == "gallery":
                    name = (d.get("dir_name") or "").split("/")[-1]
                    lines.append(f"已存入《{name}》，{int(d.get('images') or 0)} 张图。")
                else:
                    lines.append(_short(d.get("message") or "存好了", 80))
            elif state == "unsupported":
                lines.append("这个平台还不支持（只做了公众号和小红书）。")
            elif state == "routed":
                lines.append(_short(d.get("error") or "这是图集，已改存图", 60))
            else:
                lines.append("没存上：" + _short(d.get("message") or d.get("error") or "抓取失败", 40))
        out: list[str] = []
        for ln in lines:
            if ln and ln not in out:
                out.append(ln)
        return "\n".join(out[:10])
