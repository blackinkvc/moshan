#!/usr/bin/env python3
"""文章入库的任务队列：仿 linkbox，抓正文 + 4B 分类都异步，前端轮询状态。

一篇网页抓取 1-3 秒、4B 分类 5 秒，同步等会把手机浏览器拖住，
所以 start() 立刻返回任务号，线程里跑完，前端每 1.2 秒查一次。
"""
from __future__ import annotations

import subprocess
import threading
import time
import uuid
from pathlib import Path

import linkbox          # 判重键与重复窗口跟存图共用一套，别各写一份
import pending

HERE = Path(__file__).resolve().parent
# 解释器路径跟 linkbox 走同一份约定（cuntu 位置可配 + Windows 的 Scripts/python.exe）
PY = linkbox.CUNTU_PY
SCRIPT = HERE / "article.py"

TIMEOUT = 180
MAX_CONCURRENT = 2
KEEP_JOBS = 40


class ArticleBox:
    def __init__(self, on_finish=None, on_gallery=None) -> None:
        self._jobs: dict[str, dict] = {}
        self._lock = threading.Lock()
        self._sem = threading.Semaphore(MAX_CONCURRENT)
        self._on_finish = on_finish
        # 判成图集的链接：交给存图那条路，别让用户看到「存失败」
        self._on_gallery = on_gallery

    def start(self, text: str, hint: str = "") -> dict:
        text = (text or "").strip()[:linkbox.MAX_TEXT]
        if not text:
            return {"error": "缺少链接"}
        key = linkbox.url_key(text) if linkbox else ""
        # 同一个链接短时间重复进来就复用上次结果，跟存图那条一样的道理
        if key:
            now = time.time()
            with self._lock:
                for old in self._jobs.values():
                    if old.get("url_key") != key or old.get("kind") == "classify":
                        continue
                    if (old.get("state") in ("queued", "running", "ask")
                            or now - float(old.get("finished") or 0) < linkbox.DUP_WINDOW):
                        snap = dict(old)
                        snap["dup"] = True
                        return snap
        jid = uuid.uuid4().hex[:12]
        job = {"id": jid, "state": "queued", "text": text[:120],
               "title": "", "platform": "", "topic": "", "chars": 0, "images": 0,
               "rel": "", "error": "", "started": time.time(), "routed": "",
               "hint": (hint or text)[:300], "url": linkbox.url_key(text),
               "url_key": key, "finished": 0.0}
        with self._lock:
            self._jobs[jid] = job
            self._trim()
        threading.Thread(target=self._run, args=(jid, text, hint), daemon=True).start()
        return self.get(jid) or {"error": "任务创建失败"}

    def start_classify(self, rel: str = "") -> dict:
        jid = uuid.uuid4().hex[:12]
        job = {"id": jid, "state": "queued", "kind": "classify",
               "text": rel or "未分类全部", "title": "", "platform": "",
               "topic": "", "chars": 0, "rel": rel, "error": "",
               "started": time.time()}
        with self._lock:
            self._jobs[jid] = job
            self._trim()
        threading.Thread(target=self._run_classify, args=(jid, rel), daemon=True).start()
        return self.get(jid) or {"error": "任务创建失败"}

    def get(self, jid: str) -> dict | None:
        with self._lock:
            return dict(self._jobs[jid]) if jid in self._jobs else None

    def resolve(self, entry: dict, mode: str) -> dict:
        """用户答了「文章」或「图集」：按那个方向把这个任务再跑一遍。

        这里没有落盘过的目录可以复用（存文章那条在判定阶段就退出了），
        所以是真的再抓一次；hint 写死成用户的选择，judge 看到就直达，不会再问。
        """
        jid = str(entry.get("job") or "")
        url = str(entry.get("url") or "")
        if mode == "article":
            if jid and self.get(jid):
                threading.Thread(target=self._run, args=(jid, url, "存文章"),
                                 daemon=True).start()
                return {"ok": True, "job": {"id": jid, "kind": "article"},
                        "message": "好，按文章存，抓完发你。"}
            j = self.start(url, hint="存文章")
            return {"ok": True, "job": {"id": j.get("id"), "kind": "article"},
                    "message": "好，按文章存，抓完发你。"}
        j = self._on_gallery(url, hint="存图") if self._on_gallery else {}
        if not j or j.get("error"):
            return {"ok": False, "message": "改存图失败：" + str((j or {}).get("error") or "未知")}
        return {"ok": True, "job": {"id": j.get("id"), "kind": "link"},
                "message": "好，按图集存，抓完发你。"}

    def _set(self, jid: str, **kw) -> None:
        with self._lock:
            if jid in self._jobs:
                self._jobs[jid].update(kw)
                if kw.get("state") in ("done", "error", "routed"):
                    self._jobs[jid]["finished"] = time.time()

    def _trim(self) -> None:
        if len(self._jobs) <= KEEP_JOBS:
            return
        for k in sorted(self._jobs, key=lambda x: self._jobs[x]["started"])[:len(self._jobs) - KEEP_JOBS]:
            self._jobs.pop(k, None)

    def _call(self, cmd: list[str]) -> dict:
        if not PY.exists() or not SCRIPT.exists():
            return {"ok": False, "error": "article.py 或 cuntu 的 .venv 不存在"}
        try:
            p = subprocess.run([str(PY), str(SCRIPT)] + cmd,
                               capture_output=True, text=True, timeout=TIMEOUT,
                               cwd=str(HERE))
        except subprocess.TimeoutExpired:
            return {"ok": False, "error": f"超时（>{TIMEOUT} 秒）"}
        except OSError as e:
            return {"ok": False, "error": f"启动失败：{e}"}
        s = (p.stdout or "").strip()
        i, j = s.find("{"), s.rfind("}")
        if i < 0 or j <= i:
            return {"ok": False, "error": (p.stderr or "无输出").strip().splitlines()[-1][:200] if p.stderr.strip() else "无输出"}
        import json
        try:
            return json.loads(s[i:j + 1])
        except ValueError:
            return {"ok": False, "error": "输出不是 JSON"}

    def _run(self, jid: str, text: str, hint: str = "") -> None:
        with self._sem:
            try:
                self._set(jid, state="running")
                r = self._call(["fetch", text, hint])
                if not r.get("ok") and r.get("kind") == "ask":
                    # 两边都像：不建目录，挂起来问一句（用户回「存图/存文章」再跑一遍，
                    # 那一次 hint 会被写死成「存文章」或「存图」，不会二次发问）
                    job = self.get(jid) or {}
                    e = pending.add("article", job=jid, url=job.get("url") or "",
                                    title=str(r.get("title") or ""),
                                    platform=str(r.get("platform") or ""),
                                    text_len=int(r.get("chars") or 0),
                                    n_img=int(r.get("images") or 0),
                                    why=str(r.get("why") or ""))
                    self._set(jid, state="ask", pid=e["id"], ambiguous=True,
                              title=str(r.get("title") or ""),
                              platform=str(r.get("platform") or ""),
                              chars=int(r.get("chars") or 0),
                              images=int(r.get("images") or 0),
                              error=pending.question(e))
                    return
                if not r.get("ok") and r.get("kind") == "gallery":
                    # 内容判定说这是图集：改存图（图集进阅读只会剩几十字正文）
                    job = self.get(jid) or {}
                    url = job.get("url") or ""
                    if self._on_gallery and url:
                        try:
                            self._on_gallery(url)
                            self._set(jid, state="routed", kind="gallery",
                                      routed="gallery", images=int(r.get("images") or 0),
                                      chars=int(r.get("chars") or 0),
                                      error=f"这是图集（{r.get('chars')} 字），已改存图")
                            return
                        except Exception as e:  # noqa: BLE001
                            self._set(jid, kind="gallery",
                                      error=f"改存图失败：{str(e)[:100]}")
                if r.get("ok"):
                    self._set(jid, state="done", title=r.get("title", ""),
                              platform=r.get("platform", ""), topic=r.get("topic", ""),
                              chars=r.get("chars", 0), rel=r.get("rel", ""),
                              images=int(r.get("images") or 0))
                else:
                    self._set(jid, state="error", platform=r.get("platform", ""),
                              kind=r.get("kind", ""),
                              error=(r.get("error") or "抓取失败")[:300])
            finally:
                self._notify(jid)

    def _run_classify(self, jid: str, rel: str) -> None:
        with self._sem:
            try:
                self._set(jid, state="running")
                if rel:
                    target = linkbox.READ_ROOT / rel
                    r = self._call(["classify", str(target)])
                    if r.get("ok"):
                        self._set(jid, state="done", title=r.get("title", ""),
                                  topic=r.get("topic", ""), rel=r.get("rel", ""))
                    else:
                        self._set(jid, state="error", error=(r.get("error") or "分类失败")[:300])
                else:
                    r = self._call(["classify", "--all"])
                    if r.get("ok"):
                        self._set(jid, state="done",
                                  text=f"分了 {r.get('total', 0)} 篇，成功 {len(r.get('done', []))}",
                                  topic="批量", rel="")
                    else:
                        self._set(jid, state="error", error=(r.get("error") or "批量分类失败")[:300])
            finally:
                self._notify(jid)

    def _notify(self, jid: str) -> None:
        if not self._on_finish:
            return
        try:
            self._on_finish(self.get(jid) or {})
        except Exception:
            pass
