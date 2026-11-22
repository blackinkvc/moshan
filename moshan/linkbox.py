#!/usr/bin/env python3
"""链接入库：手机粘一条链接 → 调 cuntu 抓取归档 → 把结果回给前端。

刻意不经过本地 4B：判别平台、抓图、命名、落盘全由 cuntu 完成，
这里只做三件事：排队、限并发、把子进程输出翻译成前端好渲染的结构。

为什么异步：一篇 22 图的公众号要十几秒，手机 Safari 等不了那么久，
所以立刻返回任务号，前端每 1.5 秒查一次状态。
"""
from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
import threading
import time
import uuid
from pathlib import Path

import judge
import pending
import article as article_mod

# 交付版没有 cuntu 扩展；MOSHAN_CUNTU 可指向别处（与 article.py 同一约定）。
CUNTU_DIR = Path(os.path.expandvars(os.path.expanduser(
    os.environ.get("MOSHAN_CUNTU") or "~/Documents/cuntu")))
CUNTU_PY = CUNTU_DIR / ".venv" / ("Scripts/python.exe" if os.name == "nt" else "bin/python")
CUNTU_SCRIPT = CUNTU_DIR / "cuntu.py"
# 存图与阅读的落点走资料库，不写死 ~/Documents/AI项目（见 article.py 的说明）
import paths as _paths  # noqa: E402

ARCHIVE_ROOT = _paths.data_root() / "04-存图"
READ_ROOT = _paths.data_root() / "08-阅读"
READ_UNCAT = READ_ROOT / "未分类"
HERE = Path(__file__).resolve().parent
ARTICLE_SCRIPT = HERE / "article.py"
SIPS = Path("/usr/bin/sips")

TIMEOUT = 600          # 单篇上限：22 张大图并发 4 实测约 20 秒，留足余量
MAX_CONCURRENT = 2     # 同时最多两篇，避免抢带宽把每篇都拖慢
KEEP_JOBS = 60         # 只保留最近任务，防止内存里越攒越多
MAX_TEXT = 8000        # 粘贴内容上限（一次贴十几条链接不该被截断）
MAX_LINKS = 10         # 一条消息里最多拆这么多条，再多也不是正常用法
DUP_WINDOW = 180       # 同一个链接 3 分钟内重复请求 → 直接回上次结果

# 从「随手一粘」的整段分享文案里抠出链接，用来判重。
_URL_RE = re.compile(r"https?://[^\s，。、；）)\]】\"'<>]+", re.I)


def split_links(text: str) -> list[str]:
    """一条消息里所有链接，按出现顺序、去重。

    以前只管第一条：贴三条链接只存第一条，另外两条静默丢掉，用户以为
    都存进去了（2026-11-19 用户报的）。现在由调用方按条起任务。
    """
    out: list[str] = []
    seen: set[str] = set()
    for m in _URL_RE.finditer(text or ""):
        u = m.group(0).rstrip(".,;:!?)]}>）】")
        if not u or u in seen:
            continue
        seen.add(u)
        out.append(u)
        if len(out) >= MAX_LINKS:
            break
    return out


def url_key(text: str) -> str:
    """同一篇笔记的判重键。取第一条链接，去掉句末标点。

    注意**不能转小写**：小红书短链是 base62，`7IhdBG0NkI7` 和 `7ihdbg0nki7`
    是两个不同笔记，抹平大小写会把它们当成同一条而漏存。
    """
    links = split_links(text)
    return links[0] if links else ""


def human(n: int) -> str:
    v = float(n)
    for unit in ("B", "KiB", "MiB", "GiB"):
        if v < 1024 or unit == "GiB":
            return f"{v:.0f} {unit}" if unit == "B" else f"{v:.1f} {unit}"
        v /= 1024
    return f"{v:.1f} GiB"


def _first_line(s: str) -> str:
    return (s or "").strip().splitlines()[0] if (s or "").strip() else ""


def _meta_probe(d: Path) -> tuple[int, str, str]:
    """图集目录里 meta.json 的 copy 就是原文正文（公众号/小红书全文，≤6000 字）。

    判「这是文章还是照片集」靠它——cuntu 抓完就把正文写进 meta 了，
    不用为了判一下再抓一遍网页。返回 (正文字数, 版面类型, 标题)。
    版面类型（layout）比字数可靠：公众号的「图片消息」写 465 字配文也还是照片集。
    """
    try:
        meta = json.loads((Path(d) / "meta.json").read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return 0, "", ""
    n = 0
    for k in ("copy", "plain_text", "desc"):
        s = str(meta.get(k) or "").strip()
        if s:
            n = len(s)
            break
    return n, str(meta.get("layout") or ""), str(meta.get("title") or "").strip()


def _trash_dir(d: Path) -> None:
    """判成文章的图集目录挪进 04-存图/_回收站/，不直接删（还能还原）。"""
    d = Path(d)
    if not d.exists():
        return
    stamp = time.strftime("%Y%m%d-%H%M%S")
    dest = ARCHIVE_ROOT / "_回收站" / stamp / d.name
    try:
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.move(str(d), str(dest))
    except OSError:
        pass


def _url_key(u: str) -> str:
    return (u or "").strip().rstrip("/").lower()


def _trash_by_url(url: str, keep: Path) -> list[str]:
    """同一链接以前已经存成图集的，一并挪走 —— 判成文章就只留在阅读库里。

    不这么做的话，同一条内容在 04-存图 和 08-阅读 各有一份，翻哪边都觉得重了。
    """
    key = _url_key(url)
    if not key:
        return []
    moved = []
    try:
        metas = list(ARCHIVE_ROOT.glob("*/*/meta.json"))
    except OSError:
        return []
    for mp in metas:
        d = mp.parent
        if d == Path(keep) or d.name.startswith(("_", ".")):
            continue
        try:
            meta = json.loads(mp.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            continue
        if _url_key(meta.get("url") or "") != key:
            continue
        _trash_dir(d)
        moved.append(d.name)
    return moved


class LinkBox:
    def __init__(self, on_finish=None) -> None:
        self._jobs: dict[str, dict] = {}
        self._lock = threading.Lock()
        self._sem = threading.Semaphore(MAX_CONCURRENT)
        self._on_finish = on_finish

    # ---------------- 对外接口 ----------------

    def start(self, text: str, hint: str = "") -> dict:
        text = (text or "").strip()[:MAX_TEXT]
        if not text:
            return {"error": "缺少链接"}
        key = url_key(text)
        # 同一个链接短时间反复进来（微信助手一轮里重复调工具、用户连点两下）
        # 不再开新任务：既省一次抓取，也让工作台不会同一件事记两笔。
        with self._lock:
            for old in self._jobs.values():
                if not key or old.get("url_key") != key:
                    continue
                # ask 也算「还没完」：同一条链接又发一次，要接着问那一句，
                # 而不是再抓一遍、再挂一条问一遍。
                if old.get("state") in ("queued", "running", "ask"):
                    snap = dict(old)
                    snap["dup"] = True
                    return snap
                if time.time() - float(old.get("finished") or 0) < DUP_WINDOW:
                    snap = dict(old)
                    snap["dup"] = True
                    return snap
        jid = uuid.uuid4().hex[:12]
        job = {
            "id": jid, "state": "queued", "platform": None, "dir_name": "",
            "dir": "", "images": 0, "bytes": 0, "size": "", "message": "",
            "text": text[:160], "started": time.time(), "hint": (hint or text)[:300],
            "url": key, "url_key": key, "finished": 0.0, "routed": "",
        }
        with self._lock:
            self._jobs[jid] = job
            self._trim()
        threading.Thread(target=self._run, args=(jid, text, hint), daemon=True).start()
        return self.get(jid) or {"error": "任务创建失败"}

    def get(self, jid: str) -> dict | None:
        with self._lock:
            job = self._jobs.get(jid)
            return dict(job) if job else None

    def resolve(self, jid: str, mode: str, entry: dict | None = None) -> dict:
        """用户说了算：按图集收尾，或者就地转成阅读条目。

        图集那条其实早就落盘了（挂起时只是没定归属），「存图」只需要把任务
        标成完成；「文章」用已经抓到手的图和正文就地转存，不再请求一次网页。
        服务重启过、内存里没这条任务时，用挂起记录里的字段补一个占位任务，
        免得「答了却办不了」。
        """
        e = entry or {}
        job = self.get(jid or "") or {}
        if not job:
            jid = self._stub_job(jid, e)
            job = self.get(jid) or {}
        dir_name = str(job.get("dir_name") or e.get("dir_name") or "")
        if not dir_name or not (ARCHIVE_ROOT / dir_name).exists():
            self._set(jid, state="error", ambiguous=False, pid="",
                      message="这条的图集目录不见了（可能被清理过）")
            return {"ok": False, "job": self.get(jid),
                    "message": "这条的图集目录不见了，只能按原链接重存一次"}
        if mode == "gallery":
            n = int(job.get("images") or e.get("n_img") or 0)
            self._set(jid, state="done", routed="gallery", ambiguous=False, pid="",
                      message=f"{n} 张已存入 {dir_name}")
            return {"ok": True, "job": self.get(jid),
                    "message": f"已按图集存好《{dir_name}》，{n} 张图。"}
        self._promote(jid, str(job.get("url") or e.get("url") or ""), dir_name,
                      str(job.get("platform") or e.get("platform") or ""), "你说的")
        snap = self.get(jid) or {}
        return {"ok": snap.get("state") == "done", "job": snap,
                "message": snap.get("message") or "已转存"}

    def _stub_job(self, jid: str, e: dict) -> str:
        """挂起记录补一个占位任务（服务重启过，内存里的任务早没了）。"""
        jid = jid or uuid.uuid4().hex[:12]
        job = {
            "id": jid, "state": "ask", "platform": e.get("platform") or "",
            "dir_name": e.get("dir_name") or "", "dir": "",
            "images": int(e.get("n_img") or 0), "bytes": 0, "size": "",
            "message": "", "text": e.get("url") or "", "started": time.time(),
            "hint": "", "url": e.get("url") or "", "url_key": "",
            "finished": 0.0, "routed": "", "title": e.get("title") or "",
            "text_len": int(e.get("text_len") or 0),
        }
        with self._lock:
            self._jobs[jid] = job
            self._trim()
        return jid

    # ---------------- 内部 ----------------

    def _set(self, jid: str, **kw) -> None:
        with self._lock:
            if jid in self._jobs:
                self._jobs[jid].update(kw)
                # 记下收尾时刻：判重要靠它算「多久之内算重复」
                if kw.get("state") in ("done", "error", "unsupported"):
                    self._jobs[jid]["finished"] = time.time()

    def _trim(self) -> None:
        """调用方已持锁。超量时丢最早的，避免长期运行内存膨胀。"""
        if len(self._jobs) <= KEEP_JOBS:
            return
        for k in sorted(self._jobs, key=lambda x: self._jobs[x]["started"])[:len(self._jobs) - KEEP_JOBS]:
            self._jobs.pop(k, None)

    def _run(self, jid: str, text: str, hint: str = "") -> None:
        with self._sem:
            try:
                self._do(jid, text, hint)
            finally:
                self._notify(jid)

    def _notify(self, jid: str) -> None:
        """任务进入终态时回调一次，调用方用来写日志。"""
        if not self._on_finish:
            return
        try:
            self._on_finish(self.get(jid) or {})
        except Exception:
            pass

    def _do(self, jid: str, text: str, hint: str = "") -> None:
        self._set(jid, state="running")
        if not CUNTU_PY.exists() or not CUNTU_SCRIPT.exists():
            self._set(jid, state="error",
                      message=f"cuntu 未就绪：{CUNTU_SCRIPT} 或 .venv 不存在")
            return
        cmd = [str(CUNTU_PY), str(CUNTU_SCRIPT), "--json", "--no-dedup",
               "--root", str(ARCHIVE_ROOT), text]
        try:
            p = subprocess.run(cmd, capture_output=True, text=True,
                               timeout=TIMEOUT, cwd=str(CUNTU_DIR))
        except subprocess.TimeoutExpired:
            self._set(jid, state="error", message=f"抓取超时（>{TIMEOUT} 秒）")
            return
        except OSError as e:
            self._set(jid, state="error", message=f"启动失败：{e}")
            return

        data = self._parse(p.stdout)
        if not data:
            detail = _first_line(p.stderr) or "cuntu 没有返回结果"
            self._set(jid, state="error", message=detail[:300])
            return

        r = data[0]
        platform = r.get("platform") or ""
        err = r.get("error") or ""
        # 短链解出来的真实地址写回任务：失败时工作台要靠它把链接还给用户
        if r.get("url"):
            self._set(jid, url=str(r["url"]))

        if r.get("stub"):
            # 抖音/视频号：识别到了，但只给指路，不产出文件
            self._set(jid, state="unsupported", platform=platform,
                      message=err.split("\n")[0].replace("【占位】", "").strip())
            return

        if not r.get("ok"):
            self._set(jid, state="error", platform=platform,
                      message=(err or "抓取失败")[:300])
            return

        saved = r.get("saved") or []
        dir_name = r.get("dir") or ""
        total = int(r.get("bytes") or 0)

        # ---- 内容判定：图多字少才是图集，字够长就是文章 ----
        # 抓到之前谁也不知道这条是长文还是照片集，所以判在这一步。
        # 判成文章就补抓成阅读条目，把刚建好的图集目录挪进回收站（不留两份）。
        # 两边都像的不许猜（judge.ask）：先挂起来问一句，图就留在原地等回答。
        if dir_name and saved:
            n_txt, layout, title = _meta_probe(ARCHIVE_ROOT / dir_name)
            verdict = judge.decide(n_txt, len(saved), platform, hint or text,
                                   layout or str(r.get("layout") or ""), title)
            if verdict.get("ask"):
                e = pending.add("link", job=jid, url=r.get("url") or "",
                                title=title, platform=platform, dir_name=dir_name,
                                text_len=n_txt, n_img=len(saved),
                                suggest=verdict.get("mode") or "",
                                why=verdict.get("why") or "")
                self._set(jid, state="ask", platform=platform, dir_name=dir_name,
                          dir=str(ARCHIVE_ROOT / dir_name), images=len(saved),
                          bytes=total, size=human(total), routed="", pid=e["id"],
                          ambiguous=True, why=verdict.get("why") or "",
                          title=title, text_len=n_txt,
                          message=pending.question(e))
                return
            if verdict["mode"] == "article":
                self._promote(jid, r.get("url") or "", dir_name, platform, verdict["why"])
                return

        self._set(
            jid, state="done", platform=platform, dir_name=dir_name, routed="gallery",
            dir=str(ARCHIVE_ROOT / dir_name) if dir_name else "",
            images=len(saved), bytes=total, size=human(total),
            message=f"{len(saved)} 张已存入 {dir_name}" if saved else "没有新文件",
        )

    def _promote(self, jid: str, url: str, dir_name: str, platform: str, why: str) -> None:
        """把已经抓成图集的一条改存成阅读文章。

        先用**已经抓到手的**东西落成阅读条目（正文在 meta.json、图在 images/），
        省掉再请求一次网页：公众号风控时那一次基本是白跑，还白等十几秒。
        本地转存失败（正文缺失/图片坏了）才退回重抓。
        """
        local = self._promote_local(jid, url, dir_name, platform, why)
        if local is not None:
            return
        r = self._fetch_article(url)
        # kind=dup：这篇本来就在阅读库里。那就更该把刚建的图集撤掉 ——
        # 否则「判成文章」的结果是文章还躺在存图里，正是用户抱怨的那个现象。
        dup = str(r.get("kind") or "") == "dup"
        if not r.get("ok") and not dup:
            # 转存失败就留着图集，别两头空
            self._set(jid, state="done", platform=platform, dir_name=dir_name,
                      routed="gallery",
                      dir=str(ARCHIVE_ROOT / dir_name),
                      message=f"已存入 {dir_name}（判成文章但转存失败：{str(r.get('error'))[:60]}）")
            return
        if dup:
            _trash_dir(ARCHIVE_ROOT / dir_name)
            moved = _trash_by_url(url, ARCHIVE_ROOT / dir_name)
            self._set(jid, state="done", platform=platform, routed="article",
                      dir_name="", dir="", images=0, trashed=moved,
                      message=f"这篇已经在阅读库里了（{why}），刚建的图集已撤掉")
            return
        _trash_dir(ARCHIVE_ROOT / dir_name)
        moved = _trash_by_url(url, ARCHIVE_ROOT / dir_name)
        title = r.get("title") or ""
        chars = int(r.get("chars") or 0)
        self._set(
            jid, state="done", platform=platform, routed="article",
            dir_name=r.get("rel") or title, dir="", images=0, chars=chars,
            size=f"{chars} 字", trashed=moved,
            message=f"这是一篇文章（{why}），已存入阅读《{title}》，{chars} 字",
        )

    def _promote_local(self, jid: str, url: str, dir_name: str, platform: str,
                       why: str) -> dict | None:
        """用已抓到的图集目录直接落成阅读条目。

        返回 None 表示「本地办不了，去重抓」；返回任务快照表示已经收尾。
        为什么先本地：抓取和判定本来就该用同一份数据。判错了就地把这份数据
        换个目录放，不必为了「确认一次」再请求一遍网页 —— 公众号遇到风控，
        第二次请求多半拿回一个验证页，白等还把图集撤了。
        """
        gdir = ARCHIVE_ROOT / dir_name
        try:
            meta = json.loads((gdir / "meta.json").read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return None
        copy = str(meta.get("copy") or meta.get("desc") or "").strip()
        if len(copy) < 80:                 # 正文太短，本地转存只会得到半个条目
            return None
        # 阅读库里已经有这篇：撤掉刚建的图集就完事，别再落一份
        try:
            if article_mod.read_hit(url or meta.get("url") or ""):
                _trash_dir(gdir)
                moved = _trash_by_url(url or meta.get("url") or "", gdir)
                self._set(jid, state="done", platform=platform, routed="article",
                          dir_name="", dir="", images=0, trashed=moved,
                          message=f"这篇已经在阅读库里了（{why}），刚建的图集已撤掉")
                return self.get(jid)
        except Exception:                  # noqa: BLE001
            pass
        title = article_mod.safe_name(
            meta.get("title") or str(dir_name).split("_")[-1] or "无标题")
        try:
            dest = article_mod.unique_dir(READ_UNCAT, title)
            dest.mkdir(parents=True, exist_ok=True)
            imgs = self._move_images(gdir / "images", dest / "images")
        except OSError:
            return None
        n_img = len(imgs)

        body = copy
        if imgs:
            body = copy + "\n\n" + "\n\n".join(f"![]({r})" for r in imgs)
        try:
            (dest / "正文.md").write_text(body, encoding="utf-8")
            stamp = article_mod.now()
            (dest / "article.json").write_text(json.dumps({
                "schema": 2, "title": title, "platform": platform,
                "source": f"{platform} · {meta.get('author') or ''}".rstrip(" ·"),
                "url": url or meta.get("url") or "",
                "canonical": meta.get("canonical") or "",
                "author": meta.get("author") or "",
                "published_at": meta.get("published_at") or "",
                "topic": article_mod.UNCATEGORIZED, "saved_at": stamp,
                "chars": len(copy), "source_text": url or meta.get("url") or "",
                "tags": [], "origin": meta.get("origin") or {},
                "milestones": {"submitted_at": stamp, "fetched_at": stamp,
                               "archived_at": stamp},
                "duration_sec": 0, "inline_images": n_img,
                "needs_ocr": False, "ocr_status": "skipped", "images": n_img,
                "note": "由存图就地转存（未重抓网页）",
            }, ensure_ascii=False, indent=1), encoding="utf-8")
        except OSError:
            return None

        # 4B 的分主题/打标签放在最后，失败也只是「未分类」，不影响东西存下来
        try:
            article_mod.classify_dir(dest)
        except Exception:                  # noqa: BLE001
            pass

        _trash_dir(gdir)
        moved = _trash_by_url(url or meta.get("url") or "", gdir)
        # 分类可能把目录挪到主题文件夹里，所以位置要按实际落点算，别用「未分类」
        rel = f"{dest.parent.name}/{dest.name}"
        self._set(jid, state="done", platform=platform, routed="article",
                  dir_name=rel, dir="", images=0, chars=len(copy),
                  size=f"{len(copy)} 字", trashed=moved,
                  message=f"这是一篇文章（{why}），已存入阅读《{title}》，{len(copy)} 字")
        return self.get(jid)

    @staticmethod
    def _move_images(src: Path, dest: Path) -> list[str]:
        """把图集的图挪进阅读条目目录；heic 转 jpg（阅读页里的浏览器不认 heic）。

        返回落盘后的相对路径（`images/01.jpg`），正文按这个顺序引用。
        """
        if not src.is_dir():
            return []
        files = sorted(p for p in src.iterdir() if p.is_file() and not p.name.startswith("."))
        if not files:
            return []
        dest.mkdir(parents=True, exist_ok=True)
        out: list[str] = []
        for p in files:
            ext = p.suffix.lower()
            if ext not in (".jpg", ".jpeg", ".png", ".gif", ".webp", ".heic", ".heif"):
                continue
            seq = len(out) + 1
            if ext in (".jpg", ".jpeg", ".png", ".gif", ".webp"):
                name = f"{seq:02d}{ext}"
                try:
                    shutil.copyfile(p, dest / name)
                except OSError:
                    continue
            else:
                name = f"{seq:02d}.jpg"
                try:
                    subprocess.run([str(SIPS), "-s", "format", "jpeg", "-s",
                                    "formatOptions", "85", str(p), "--out",
                                    str(dest / name)],
                                   capture_output=True, timeout=60)
                except (subprocess.SubprocessError, OSError):
                    continue
                if not (dest / name).exists():
                    continue
            out.append(f"images/{name}")
        return out

    def _fetch_article(self, url: str) -> dict:
        """用 cuntu 的 python 跑 article.py fetch（pocket 的 venv 没有 requests）。

        ⚠️ 这个 def 行曾经被误删，函数体顺势并进了 _move_images 的末尾 ——
        `_promote()` 一旦走到「本地转存办不了」那条分支就 AttributeError，
        任务永远停在 running，用户只看到「抓得比较久」。改这个文件时留心缩进。
        """
        if not url or not CUNTU_PY.exists():
            return {"ok": False, "error": "缺少链接或 cuntu 未就绪"}
        try:
            p = subprocess.run([str(CUNTU_PY), str(ARTICLE_SCRIPT), "fetch", url],
                               capture_output=True, text=True, timeout=TIMEOUT,
                               cwd=str(HERE))
        except (subprocess.TimeoutExpired, OSError) as e:
            return {"ok": False, "error": f"转存文章失败：{e}"}
        out = (p.stdout or "").strip()
        i = out.find("{")
        if i < 0:
            return {"ok": False, "error": _first_line(p.stderr) or "没有返回结果"}
        try:
            return json.loads(out[i:])
        except ValueError:
            return {"ok": False, "error": "返回内容不是 JSON"}

    @staticmethod
    def _parse(stdout: str) -> list[dict]:
        """cuntu --json 输出一个数组。容忍前面有杂行，从第一个 [ 起截。"""
        s = (stdout or "").strip()
        if not s:
            return []
        i, j = s.find("["), s.rfind("]")
        if i < 0 or j <= i:
            return []
        try:
            val = json.loads(s[i:j + 1])
        except ValueError:
            return []
        return val if isinstance(val, list) else []
