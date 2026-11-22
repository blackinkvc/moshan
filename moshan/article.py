#!/usr/bin/env python3
"""文章入库：粘一条链接 → 抓正文 → 存到 08-阅读/<主题>/<标题>/，再交给 4B 分主题。

跟 linkbox.py（存图）的区别：
  - 存图：cuntu 落图 + meta.json，重图片；这里只要正文文字，不下图。
  - 分类：存图是 4B 给一组图起名；这里是 4B 判断文章属于哪个主题、没有就新建。

复用：
  - 抓取走 cuntu.fetch_post()（公众号/小红书/通用网页都已实现），只取 Post.body。
  - 4B 走 naming.ask_4b()，同一个模型同一个接口，不重复造轮子。

落盘结构：
  08-阅读/
    未分类/<标题>/article.json + 正文.md      ← 新文章先落这
    人工智能/<标题>/...                        ← 4B 分过去
    经济/<标题>/...
  （主题目录按需新建，_主题.json 记录已知主题列表）

命令行：
  python article.py fetch '<链接>'            抓链接，落未分类，打印 JSON
  python article.py classify <文章目录>       4B 分类并移动，打印 JSON
  python article.py classify --all            分类未分类下所有文章
  python article.py tidy <文章目录>           重排已存正文（碎片行接成段落，幂等）
  python article.py tidy --all                重排所有文章的正文
  python article.py refetch <文章目录>        按原链接重抓（补回正文图与版式）
  python article.py refetch --all [平台]      批量重抓，默认只做公众号
"""
from __future__ import annotations

import html as html_mod
import json
import os
import re
import shutil
import sys
import time
import urllib.parse
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

# 交付版没有 cuntu 扩展（那是作者的插件集）；开发机上是 ~/Documents/cuntu。
# MOSHAN_CUNTU 可指向别处。目录不存在只是少几项能力，不该让整个服务起不来。
CUNTU_DIR = Path(os.path.expandvars(os.path.expanduser(
    os.environ.get("MOSHAN_CUNTU") or "~/Documents/cuntu")))
sys.path.insert(0, str(CUNTU_DIR))

import paths  # noqa: E402  （交付版：目录约定统一走 paths）

# 数据落在资料库下。原来写死 ~/Documents/AI项目/…，那是开发机自己的目录结构，
# 别人机器上既不该存在，也不该由程序 mkdir 出来（三处 mkdir 会污染 Documents）。
READ_ROOT = paths.data_root() / "08-阅读"
GALLERY_ROOT = paths.data_root() / "04-存图"
UNCATEGORIZED = "未分类"
TOPICS_FILE = "_主题.json"
DEFAULT_TOPICS = []          # 从零开始：由 4B 按文章内容新建，不预置
MAX_BODY = 20000             # 正文落盘上限
MAX_PROMPT_BODY = 1400       # 送 4B 的正文片段上限
MAX_INLINE_IMAGES = 40       # 一篇里最多存这么多张正文图（够一本图文书了）
MAX_IMG_BYTES = 12 * 1024 * 1024
MAX_INLINE_BYTES = 40 * 1024 * 1024   # 一篇正文图总量上限

# 图集门禁：图片是主角、文字只是配文的内容属于存图，不该出现在阅读里。
GALLERY_PLATFORMS = ("微博", "小红书")
GALLERY_MIN_IMAGES = 4        # 本次抓到这么多张图，才可能是图集
GALLERY_TEXT_LIMIT = 200      # 正文超过这个字数，即使存过图也当文章读

# 注意：cuntu 顶层要 requests，而 pocket 的 venv 没装。
#   分类/topics/list 等接口不需要 cuntu，所以这里不 import；
#   只有 fetch_text（抓链接）才用到，放到函数内部延迟导入。
#   实际运行中 fetch 由 articlebox 用 cuntu 的 .venv/python 子进程执行，那里有 requests。
import judge  # noqa: E402  （存图/存文章同一套内容判定）
import naming  # noqa: E402  （纯标准库，pocket venv 可导入）
import textfmt  # noqa: E402  （正文整理：HTML 段落还原 + OCR 断行接合）


_CUNTU = None


def _cuntu():
    """惰性导入 cuntu：只有抓链接时才需要 requests。"""
    global _CUNTU
    if _CUNTU is None:
        import cuntu
        _CUNTU = cuntu
    return _CUNTU


def now() -> str:
    from datetime import datetime, timedelta, timezone
    return datetime.now(timezone(timedelta(hours=8))).isoformat(timespec="seconds")


def safe_name(s: str, limit: int = 60) -> str:
    s = re.sub(r"[\\/:*?\"<>|\r\n\t]+", " ", s or "")
    s = html_mod.unescape(s)
    s = re.sub(r"\s+", " ", s).strip(" .。·-—_")
    return (s[:limit].rstrip() or "无标题")


def unique_dir(parent: Path, name: str) -> Path:
    target = parent / name
    if not target.exists():
        return target
    for i in range(2, 100):
        cand = parent / f"{name} ({i})"
        if not cand.exists():
            return cand
    return parent / f"{name} ({int(time.time())})"


def extract_url(text: str) -> str:
    """从一段分享口令里抠出第一个 http(s) 链接。"""
    m = re.search(r"https?://[^\s，,；;】\]）)+]+", text or "")
    return html_mod.unescape(m.group(0)) if m else ""


def _url_key(u: str) -> str:
    """URL 归一：只留「哪一条内容」，忽略协议、查询串和尾斜杠。

    同一个微博链接经常带着不同的 ?from=/&sudaref= 参数进来，直接字符串比会比不出来。
    """
    u = (u or "").strip()
    u = re.sub(r"^https?://", "", u, flags=re.I)
    u = re.split(r"[?#]", u, maxsplit=1)[0]
    return u.rstrip("/").lower()


def gallery_hit(url: str, min_images: int = GALLERY_MIN_IMAGES) -> dict | None:
    """这条链接是不是已经在 04-存图 里作为图集存过了。

    扫描走 layout.iter_articles —— 存图的索引/去重/看图页都用它，不另写一套遍历；
    只读 meta.json 和文件名，不联网也不重建索引，几十篇是毫秒级。
    命中就返回 {rel, images, title}，否则 None。
    """
    key = _url_key(url)
    if not key:
        return None
    try:
        import layout  # cuntu 目录已在 sys.path 里；纯标准库，pocket 的 venv 也能导入
        arts = layout.iter_articles(GALLERY_ROOT)
    except Exception:  # noqa: BLE001
        return None
    for it in arts:
        meta = it.get("meta") or {}
        if key not in (_url_key(meta.get("url", "")), _url_key(meta.get("canonical", ""))):
            continue
        n = len([p for p in (it.get("images") or [])
                 if p.suffix.lower() not in (".mp4", ".mov", ".m4v")])
        if n >= min_images:
            return {"rel": it.get("rel", ""), "images": n,
                    "title": meta.get("title") or it.get("title", "")}
    return None


def read_hit(url: str) -> dict | None:
    """这条链接是不是已经在 08-阅读 里了（同一条链接粘两次会存成两份）。

    回收站里的不算 —— 删掉的文章允许重新存回来。
    有多份时返回正文最长的那份：判断「要不要挡住」看的是最好的存档，
    不能因为先扫到一份空壳就放行。
    """
    key = _url_key(url)
    if not key:
        return None
    try:
        topics = [p for p in READ_ROOT.iterdir()
                  if p.is_dir() and not p.name.startswith(("_", "."))]
    except OSError:
        return None
    best: dict | None = None
    for topic in topics:
        for d in topic.iterdir():
            if not d.is_dir():
                continue
            meta = article_meta(d)
            if key not in (_url_key(meta.get("url", "")), _url_key(meta.get("canonical", ""))):
                continue
            item = {"rel": f"{topic.name}/{d.name}", "chars": int(meta.get("chars") or 0),
                    "title": meta.get("title") or d.name}
            if best is None or item["chars"] > best["chars"]:
                best = item
    return best


def body_to_markdown(raw: str, is_html: bool = True) -> str:
    """把抓到的正文整理成人能读的段落（见 cuntu/textfmt.py）。

    以前是 `cu.strip_tags()` + 原样落盘：它把**每个标签**都换成换行，
    公众号正文里全是 `<span leaf="">`，于是一句话被切成七八行，
    阅读页一行一段、满屏碎句（用户原话「全是空格啥的」）。
    现在只把块级标签当段落边界，内联标签直接消掉，再把段内硬折的行接回去。
    """
    text = textfmt.html_to_text(raw) if is_html else raw
    return textfmt.tidy_plain(text)[:MAX_BODY]


def _img_bytes(blob: bytes) -> bool:
    """看着像图才算数：微信 cdn 拿不到图时回的是 HTML/text，不能当图存。"""
    b = blob[:16]
    return (b.startswith(b"\xff\xd8\xff") or b.startswith(b"\x89PNG")
            or b.startswith(b"GIF8") or b[:4] == b"RIFF" or b[4:8] == b"ftyp")


def _cdn_shrink(url: str) -> str:
    """把 cdn 链的尺寸段改成 /640：`/0`（原图）拿不到时的退路。

    微信 cdn 只认 `/0` 和 `/640`（`/1280` 一律 404）。少数图原图尺寸不开放，
    `/0` 返回 HTML 错误页，这时退回 640 宽的压缩版总比一张都没有强。
    """
    m = re.search(r"/(\d{1,4})(?=[?#]|$)", url or "")
    if not m or m.group(1) == "640":
        return ""
    return url[:m.start()] + "/640" + url[m.end():]


def _save_inline_images(urls: list[str], dest: Path, cu,
                        referer: str = "https://mp.weixin.qq.com/",
                        ) -> tuple[dict[str, str], list[str]]:
    """正文里的图按出现顺序存成 `images/01.jpg`，返回 ({原地址: 相对路径}, 没下到的地址)。

    公众号文章是「图文并茂」的：一张图一段话交替。以前整篇只留文字，图全丢了，
    存下来的东西和原文差一半。现在图跟着正文一起落盘，正文里按原位置插
    `![](images/01.jpg)`，阅读页照原样排出来。

    - 地址走 `wx_full_size` 换原图（cdn 上的 `/640` 是压缩版，`/0` 才是原图）；
      原图拿不到再退回 640，宁可小一点也别少一张；
    - 同一个地址在正文里出现多次只下一次，几处引用共用一份文件；
    - 下失败的地址要**交回去**：以前只返回成功的，失败被静默吞掉，
      一篇文章的图全没下到，界面上跟「这篇本来没图」一模一样（用户原话
      「图就没有摘录」而没人知道）。
    """
    if not urls:
        return {}, []
    images = dest / "images"
    mapping: dict[str, str] = {}
    failed: list[str] = []
    http = None
    n = 0
    total = 0
    try:
        http = cu.Http()
        for u in urls:
            if u in mapping:
                continue
            if n >= MAX_INLINE_IMAGES or total >= MAX_INLINE_BYTES:
                failed.append(u)
                continue
            cands = []
            for c in (cu.wx_full_size(u), u, _cdn_shrink(u)):
                if c and c not in cands:
                    cands.append(c)
            blob = b""
            for attempt in (0, 1):        # 网络抖动重试一次，别把「超时」当「没有」
                for cand in cands:
                    try:
                        blob = http.get(cand, ua=cu.UA_DESKTOP, referer=referer,
                                        timeout=40).content
                    except Exception:  # noqa: BLE001
                        blob = b""
                    if blob and _img_bytes(blob):
                        break
                    blob = b""
                if blob:
                    break
            if not blob or len(blob) > MAX_IMG_BYTES:
                failed.append(u)
                continue
            ext = cu.sniff_ext(blob)
            if ext in cu.VIDEO_EXT:
                continue
            n += 1
            total += len(blob)
            images.mkdir(parents=True, exist_ok=True)
            name = f"{n:02d}.{ext}"
            (images / name).write_bytes(blob)
            mapping[u] = f"images/{name}"
    except Exception:  # noqa: BLE001
        failed.extend(u for u in urls if u not in mapping and u not in failed)
    finally:
        try:
            if http is not None:
                http.close()
        except Exception:  # noqa: BLE001
            pass
    return mapping, failed


def _rewrite_imgs(body: str, mapping: dict[str, str]) -> str:
    """把正文里的外链图片换成落盘后的相对路径；下失败的整行摘掉。"""
    def repl(m: re.Match) -> str:
        local = mapping.get(m.group(1).strip())
        return f"![]({local})" if local else ""
    out = re.sub(r"!\[[^\]]*\]\(([^)]*)\)", repl, body)
    return re.sub(r"\n{3,}", "\n\n", out).strip()


def save_post_images(post, dest: Path, cu, limit: int = 4) -> int:
    """正文极短的文章（微博长图、图片贴）把图存进文章目录，等 OCR 转文字。

    只存图片，视频跳过——OCR 用不上，留着只是白占空间。
    最多 4 张：长图文一般就 1~3 张，照片贴存多了纯属重复占盘（相册里那份还在 04-存图）。
    返回写入张数；任何异常都吞掉，不能因为存图失败把文章本身也丢了。
    """
    if not getattr(post, "items", None):
        return 0
    http = None
    try:
        http = cu.Http()
        results = cu.download_items(post, http, jobs=4, verbose=False)
    except Exception:  # noqa: BLE001
        return 0
    finally:
        try:
            if http is not None:
                http.close()
        except Exception:  # noqa: BLE001
            pass
    images = dest / "images"
    n = 0
    for _idx, blob, _u, _e, _d in results:
        if not blob or n >= limit:
            continue
        ext = cu.sniff_ext(blob)
        if ext in cu.VIDEO_EXT:
            continue
        images.mkdir(parents=True, exist_ok=True)
        n += 1
        (images / f"{n:02d}.{ext}").write_bytes(blob)
    return n


def load_topics() -> list[str]:
    p = READ_ROOT / TOPICS_FILE
    try:
        d = json.loads(p.read_text(encoding="utf-8"))
        return [t for t in d.get("topics", []) if isinstance(t, str)]
    except (OSError, ValueError):
        return list(DEFAULT_TOPICS)


def save_topics(topics: list[str]) -> None:
    READ_ROOT.mkdir(parents=True, exist_ok=True)
    (READ_ROOT / TOPICS_FILE).write_text(
        json.dumps({"topics": topics, "updated_at": now()}, ensure_ascii=False, indent=1),
        encoding="utf-8")


def article_meta(d: Path) -> dict:
    try:
        return json.loads((d / "article.json").read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}


# ---------------- 抓取入库 ----------------

def _meta(html_text: str, prop: str) -> str:
    try:
        return _cuntu()._meta_content(html_text, prop) or ""
    except Exception:  # noqa: BLE001
        return ""


def _title_from_html(html_text: str, url: str) -> str:
    t = (_meta(html_text, "og:title") or _meta(html_text, "twitter:title")
         or _meta(html_text, "title") or "")
    if not t:
        m = re.search(r"(?is)<title[^>]*>(.*?)</title>", html_text)
        t = _cuntu().decode_html(m.group(1)) if m else ""
    return safe_name(t or urllib.parse.unquote(url).split("/")[-1])


def _article_segment(html_text: str) -> str:
    """从通用网页里切出正文段；找不到就退回整页（再靠 strip_tags 去噪）。"""
    pats = [
        r"(?is)<article[^>]*>(.*?)</article>",
        r'(?is)<div[^>]+(?:id|class)="[^"]*(?:article-body|post-content|entry-content|rich_media_content|content|article|post)[^"]*"[^>]*>(.*?)</div>',
    ]
    for pat in pats:
        m = re.search(pat, html_text)
        if m and len(m.group(1)) > 200:
            return m.group(1)
    return html_text


def fetch_text(text: str, dest: Path | None = None, hint: str = "") -> dict:
    """抓一条链接 → 正文（公众号连图一起，按原位置插在正文里）。

    `dest` 给定时表示「重抓覆盖这一篇」：跳过图集门禁与同链接去重，
    主题/标签/划线/OCR 结果全部保留 —— 以前存的公众号只留了文字、图丢了，
    靠这个把图补回来（`article.py refetch --all`）。

    `hint` 是用户原话（「存图」/「文章」），只用来覆盖内容判定。
    """
    t0 = time.time()
    cu = _cuntu()
    url = extract_url(text)
    if not url:
        return {"ok": False, "error": "没找到链接（要带 http:// 或 https://）"}
    platform, real_url = cu.detect_platform(url)
    real_url = real_url or url
    origin: dict = {}
    post = None          # 微博/小红书会把解析结果留在这里，长图笔记要靠它存图
    wx_items: list[str] = []   # 公众号：正文容器里没图时的兜底图源（见下面的注释）
    wx_layout = ""             # 公众号版面：article 正文文章 / picture 图片消息
    http = cu.Http()
    try:
        if platform == "公众号":
            r = http.get(real_url, ua=cu.UA_DESKTOP, referer="https://mp.weixin.qq.com/")
            r.encoding = r.apparent_encoding or "utf-8"
            h = r.text
            if "该内容已被发布者删除" in h:
                return {"ok": False, "platform": platform, "error": "该内容已被发布者删除"}
            if "环境异常" in h and "验证" in h:
                return {"ok": False, "platform": platform, "error": "公众号返回风控验证页，稍后再试"}
            title = safe_name(cu._tidy_title(
                cu._js_assign(h, "msg_title") or _meta(h, "og:title")))
            author = cu._wx_account_name(h)
            seg = cu._js_content_segment(h) or ""
            raw_body = seg or cu._js_assign(h, "content_noencode") or h
            # 图片消息（没有正文容器、只有 picture_page_info_list）里图才是内容，
            # 配文写几百字也还是照片集 —— 交给 judge 的版面信号，比字数额度准。
            wx_layout = ("article" if seg else
                         ("picture" if "picture_page_info_list:" in h else ""))
            # 2026 年起有一批模板把正文图放进页面脚本的 picture_page_info_list，
            # 正文容器里一张 <img> 都没有；这时只吃 content_noencode 就等于拿到
            # 「纯文字版」——文字全对、图全丢，而且界面上跟「这篇本来没图」一模一样。
            # 借 cuntu 的解析器兜一份图清单（它已做去噪与顺序保证）。
            if "<img" not in raw_body:
                try:
                    wp = cu.fetch_wechat(real_url, http)
                    wx_items = [it.url for it in (wp.items or [])
                                if (it.ext_hint or "").lower() not in cu.VIDEO_EXT]
                    if wx_items and not (wp.body or "").strip():
                        wx_items = []          # 整篇没解出文字，多半是风控壳页
                except Exception:  # noqa: BLE001
                    wx_items = []
            published = ""
            ct = cu._js_assign(h, "ct") or ""
            try:
                published = cu._parse_ts(ct).strftime("%Y-%m-%d %H:%M:%S")
            except (ValueError, AttributeError):
                pass
            origin = cu._detect_reprint(cu.strip_tags(raw_body), h)
        elif platform == "微博":
            post = cu.fetch_weibo(real_url, http)
            title = safe_name(post.title or "微博")
            author = post.author
            raw_body = post.body or ""
            published = post.published.strftime("%Y-%m-%d %H:%M:%S")
            origin = dict(post.origin or {})
        elif platform == "小红书":
            # 优先走 cuntu 的解析：能顺带拿到图片/视频清单，长图笔记才能存图做 OCR。
            # 解析器失败（结构变动/风控）再退回原来的 og:description 纯文本兜底。
            post = None
            try:
                post = cu.fetch_xhs(real_url, http)
                title = safe_name(post.title or "小红书笔记")
                author = post.author
                raw_body = post.body or ""
                published = (post.published.strftime("%Y-%m-%d %H:%M:%S")
                             if post.published else "")
                origin = dict(post.origin or {})
            except Exception:  # noqa: BLE001
                r = http.get(real_url, ua=cu.UA_MOBILE_IOS,
                             referer="https://www.xiaohongshu.com/")
                r.encoding = r.apparent_encoding or "utf-8"
                h = r.text
                title = _title_from_html(h, real_url)
                author = _meta(h, "author")
                desc = (_meta(h, "description") or _meta(h, "og:description") or "")
                raw_body = cu.decode_html(desc) or _article_segment(h)
                published = ""
        else:
            r = http.get(real_url, ua=cu.UA_DESKTOP)
            r.encoding = r.apparent_encoding or "utf-8"
            h = r.text
            if len(h) < 200:
                return {"ok": False, "platform": platform, "error": "页面几乎为空，可能被反爬或需要登录"}
            title = _title_from_html(h, real_url)
            author = _meta(h, "author") or _meta(h, "og:site_name")
            raw_body = _article_segment(h)
            published = _meta(h, "article:published_time") or _meta(h, "date")
            origin = cu._detect_reprint(cu.strip_tags(raw_body), h)
    except Exception as e:  # noqa: BLE001
        return {"ok": False, "platform": platform or "", "error": str(e).splitlines()[0][:200]}
    finally:
        http.close()

    # 公众号风控时不返回 403，而是给一张无正文容器的空壳页：这种页面必须挡掉，
    # 否则会把整页 HTML（<!DOCTYPE html> …）当成正文存进去，重抓一次毁一篇。
    if platform == "公众号" and raw_body is h:
        return {"ok": False, "platform": platform, "title": title,
                "error": "没找到公众号正文容器（可能命中验证页/已被删），正文保持原样"}

    # 公众号要「图文并茂」：正文里小标题、列表、图片都按原文位置留住，
    # 图片先以 markdown 外链形式记着，落盘时再换成 images/01.jpg（见 _save_inline_images）。
    # 微博/小红书的 body 已经是纯文本（换行是作者分段），别再当 HTML 走一遍。
    inline = ""
    if platform == "公众号":
        inline = textfmt.html_to_md(raw_body)
    body = (textfmt.tidy_plain(inline)[:MAX_BODY] if inline
            else body_to_markdown(raw_body, is_html=platform not in ("微博", "小红书")))
    # 图不算字数：一篇 9 张图的文章，字数是 4600 而不是 4600+九个图床地址
    plain = textfmt.strip_images(body) if inline else body
    min_chars = 1 if platform == "微博" else 20
    if len(plain) < min_chars:
        return {"ok": False, "platform": platform or "", "title": title,
                "error": "没抓到正文（可能需要登录或是 JS 动态渲染页）", "chars": len(plain)}

    # ---- 图集门禁：图片是主角、文字只是配文的内容不进阅读 ----
    # 判定交给 judge.decide（跟存图那边同一套），以前只有微博/小红书才拦，
    # 公众号的六张配文照片集就一路进了阅读库 —— 正文几十字，图才是主角。
    # 判成图集时返回 kind=gallery，articlebox 会自动改走存图，不会丢。
    if dest is None:
        if inline:
            n_img = len(set(textfmt._IMG_IN_MD.findall(inline)))
        else:
            n_img = len([i for i in (getattr(post, "items", None) or [])
                         if (i.ext_hint or "").lower() not in cu.VIDEO_EXT])
        # 公众号的「图片消息」正文是 content_noencode（纯文字），一张 <img> 都没有，
        # 图清单在页面脚本里、由上面兜进 wx_items。以前这里只数 inline 里的
        # markdown 图，于是 n_img 恒为 0 —— 九图配四五百字的照片集被当成
        # 「没有图、字够多 → 文章」，一路进阅读库，而且**永远不会触发「问一句」**
        # （judge 的模糊区间要求图 ≥4）。这正是用户说的「还是有的分不清」。
        if platform == "公众号" and wx_items:
            n_img = max(n_img, len(set(wx_items)))
        verdict = judge.decide(len(plain), n_img, platform, hint,
                               wx_layout if platform == "公众号" else "", title)
        if verdict.get("ask"):
            # 两边都像：这次一个目录都不建，让上层挂起问一句再回来。
            # 以前这里会硬判一个方向，用户在两头都碰见过放错的。
            return {"ok": False, "platform": platform, "title": title,
                    "kind": "ask", "chars": len(plain), "images": n_img,
                    "why": verdict.get("why") or "",
                    "error": f"{n_img} 张图、{len(plain)} 字，看着两边都像"}
        if verdict["mode"] == "gallery":
            hit = gallery_hit(real_url) if len(plain) < GALLERY_TEXT_LIMIT else None
            if hit:
                return {"ok": False, "platform": platform, "title": title,
                        "kind": "gallery", "chars": len(plain),
                        "error": f"这条已经在存图里了（{hit['images']} 张图 / {len(plain)} 字），"
                                 f"没重复建阅读条目"}
            return {"ok": False, "platform": platform, "title": title,
                    "kind": "gallery", "chars": len(plain), "images": n_img,
                    "error": f"这更像图集（{n_img} 张图 / {len(plain)} 字：{verdict['why']}），"
                             f"改存图更好"}

    # ---- 重复入库：同一条链接粘第二次不新建一份 ----
    # 之前同一篇「动态Harness」一次性存出 5 份，全是同一个链接。
    # 例外：已存在的那份是没抓到正文的空壳（< 30 字），允许重存来覆盖这次抓取失败。
    prev = read_hit(real_url) if dest is None else None
    if prev and prev["chars"] >= 30:
        return {"ok": False, "platform": platform, "title": title,
                "kind": "dup", "chars": len(plain),
                "error": f"这条已经在阅读里了（{prev['rel']} · {prev['chars']} 字），没重复存"}

    submitted = now()
    if dest is None:
        READ_ROOT.mkdir(parents=True, exist_ok=True)
        dest = unique_dir(READ_ROOT / UNCATEGORIZED, title)
        dest.mkdir(parents=True, exist_ok=True)
    else:
        dest = Path(dest)
        dest.mkdir(parents=True, exist_ok=True)

    # 重抓的兜底：正文明显缩水（风控页 / 只抓到半截）就不覆盖，宁可留旧的。
    # 之前在公众号风控时重抓，一次就把 2957 字的正文换成了 50 字空壳。
    # 注意「原文」要扣掉 OCR 块：那一块是图片里认出来的字，不属于网页正文，
    # 不扣的话带 OCR 的文章每次重抓都会被判成「缩水」（440 vs 2321）而永远补不上图。
    old_p = dest / "正文.md"
    old_ocr = ""
    if old_p.exists():
        try:
            old_raw = old_p.read_text(encoding="utf-8")
            m_ocr = re.search(r"<!--\s*ocr:start\s*-->.*?<!--\s*ocr:end\s*-->", old_raw, re.S)
            if m_ocr:
                old_ocr = m_ocr.group(0)
                old_raw = old_raw.replace(old_ocr, "", 1)
            old_n = len(textfmt.strip_images(old_raw))
        except OSError:
            old_n = 0
        if old_n >= 200 and len(plain) < old_n * 0.6:
            return {"ok": False, "platform": platform or "", "title": title,
                    "error": f"重抓结果异常（{len(plain)} 字 vs 原来 {old_n} 字），正文保持原样"}

    # 确认要写了才清旧的正文图，免得更新的篇数变少后留下孤儿图片
    shutil.rmtree(dest / "images", ignore_errors=True)
    n_inline = 0
    img_failed: list[str] = []
    if inline:
        urls = textfmt._IMG_IN_MD.findall(body)
        if not urls and wx_items:
            # 正文里一张图都没有、但 cuntu 从页面脚本里解出了图清单：
            # 按原顺序接在正文末尾（抽不到正文容器时拿不到图的准确位置，
            # 但「10 张图讲透 X」这种，图本身就是正文）。
            body = body.rstrip() + "\n\n" + "\n\n".join(f"![]({u})" for u in wx_items)
            urls = wx_items
        mapping, img_failed = _save_inline_images(urls, dest, cu)
        n_inline = len(set(mapping.values()))
        body = _rewrite_imgs(body, mapping)
        plain = textfmt.strip_images(body)

    # 微博长图：正文只有一两句，真正的信息在图片里。存下图片交给 OCR，
    # 由口袋的定时任务转成文字回填正文（ocr.py --sweep）。
    n_img, needs_ocr = 0, False
    if platform in ("微博", "小红书") and len(plain) < 30 and post is not None:
        n_img = save_post_images(post, dest, cu)
        needs_ocr = n_img > 0

    # 公众号「图多字少」的也一样：一篇 440 字配 10 张图，真正的干货在图里，
    # 光把图存下来仍是「没转成文字」。每张图平均不到 200 字就交给 OCR，
    # 由定时任务把字接在正文后面（纯照片图会被 ocr.py 判为没字，不硬写）。
    # 图没变就别重跑：上一轮已经转过的（done/empty/skipped）直接沿用。
    if n_inline and not needs_ocr and len(plain) / n_inline < 200:
        prev = article_meta(dest) if (dest / "article.json").exists() else {}
        same_imgs = int(prev.get("inline_images") or 0) == n_inline
        if not (same_imgs and prev.get("ocr_status") in ("done", "empty", "skipped")):
            n_img = n_inline
            needs_ocr = True

    meta = {
        "schema": 2, "title": title, "platform": platform or "网页",
        "source": (platform or "网页") + ((' · ' + author) if author else ''),
        "url": real_url, "canonical": real_url, "author": author,
        "published_at": published, "topic": UNCATEGORIZED, "saved_at": submitted,
        "chars": len(plain), "source_text": (text or "")[:300],
        "tags": [], "origin": origin,
        "milestones": {"submitted_at": submitted, "fetched_at": submitted,
                       "archived_at": submitted},
        "duration_sec": round(time.time() - t0, 1),
    }
    old_meta = article_meta(dest) if (dest / "article.json").exists() else {}
    if old_meta:
        for k in ("topic", "tags", "ocr_text", "ocr_chars", "ocr_at", "ocr_status",
                  "ocr_skipped_images", "ocr_errors", "needs_ocr",
                  "favorite", "summary"):
            if k in old_meta:
                meta[k] = old_meta[k]
        if old_meta.get("saved_at"):
            meta["saved_at"] = old_meta["saved_at"]
        ms = dict(old_meta.get("milestones") or {})
        ms.update(meta["milestones"])
        ms["refetched_at"] = submitted
        meta["milestones"] = ms
        # 重抓补回了以前没拿到（或新多出）的图：旧的 OCR 只转的是老图，
        # 不能在「这张图还没转」的情况下继续显示 ocr_status=done —— 那会让
        # ocr_article() 直接跳过，新图永远转不出来。
        if n_inline and int(old_meta.get("inline_images") or 0) != n_inline:
            for k in ("ocr_text", "ocr_chars", "ocr_status", "needs_ocr"):
                meta.pop(k, None)
    if n_inline:
        # 正文里插着图（images/01.jpg…），阅读页按原位渲染
        meta["inline_images"] = n_inline
    if img_failed:
        # 有图没下到：记下来，界面上能看出「这篇的图不全」，定时任务也会补抓。
        # 以前这里什么都不记，图全丢了跟「本来没图」长得一模一样。
        meta["images_failed"] = len(img_failed)
        meta["images_failed_urls"] = img_failed[:20]
    else:
        meta.pop("images_failed", None)
        meta.pop("images_failed_urls", None)
    if needs_ocr:
        meta.update({"needs_ocr": True, "ocr_status": "pending",
                     "images": n_img})
    # 图没变、这轮也不打算重跑 OCR：把上一轮的图片文字接回去，
    # 免得一次重抓把 OCR 成果冲掉（那一段是「图片里的字」，不在网页正文里）。
    if old_ocr and not needs_ocr and n_inline == int(old_meta.get("inline_images") or 0):
        body = body.rstrip() + "\n\n" + old_ocr.strip() + "\n"
        # 字数口径跟 ocr.py 对齐：网页正文 + 图片里认出来的字数
        meta["chars"] = len(plain) + int(old_meta.get("ocr_chars") or 0)
    (dest / "article.json").write_text(
        json.dumps(meta, ensure_ascii=False, indent=1), encoding="utf-8")
    (dest / "正文.md").write_text(body + "\n", encoding="utf-8")
    topic = meta.get("topic") or UNCATEGORIZED
    return {"ok": True, "platform": meta["platform"], "title": title,
            "chars": len(plain), "images": n_inline,
            "images_failed": len(img_failed), "topic": topic,
            "refetched": bool(old_meta),
            "dir": str(dest),
            "rel": f"{topic}/{dest.name}",
            "ms": int((time.time() - t0) * 1000)}


# ---------------- 4B 分类 ----------------

CLASSIFY_PROMPT = """下面是一篇文章的标题和正文开头。请判断它属于哪个主题，并给出 2-4 个内容标签。

现有主题：{topics}
标签参考（可选用，也可自己起）：人工智能、政治、经济、科技、文化、社会、国际、商业、历史、教育、医疗、法律、环境、军事、体育、娱乐、生活、哲学、心理、能源、汽车、游戏、旅行、美食、摄影

要求输出一行，格式严格为：主题|标签1,标签2,标签3
- 主题：从现有主题里选一个最贴切的；都不合适就新建一个 2-6 个中文字的主题名。
- 标签：2-4 个，每个 2-6 个中文字，反映文章的具体内容（如「大模型」「美联储」「俄乌冲突」），不要和主题重复。
- 只输出这一行，不要解释、不要引号。

标题：{title}
正文：{body}

主题|标签："""


def _remap_state(old_rel: str, new_rel: str) -> None:
    """分类会把文章挪到新主题下，而「已读 / 收藏」记的是 `主题/标题` 这个 rel。

    不跟着改的话，一篇读过的文章分类完就变回未读（实际踩到：`_已读.json` 里
    留着 `未分类/…`，文章已在 `科技/…`）。
    """
    for name in ("_已读.json", "_收藏.json"):
        p = READ_ROOT / name
        if not p.exists():
            continue
        try:
            d = json.loads(p.read_text(encoding="utf-8"))
            rels = d.get("rels") or []
        except (OSError, ValueError):
            continue
        if old_rel not in rels:
            continue
        d["rels"] = sorted({new_rel if r == old_rel else r for r in rels})
        try:
            p.write_text(json.dumps(d, ensure_ascii=False, indent=1), encoding="utf-8")
        except OSError:
            pass


def classify_dir(d: Path) -> dict:
    d = Path(d).expanduser()
    t0 = time.time()
    if not d.is_dir():
        return {"ok": False, "error": "目录不存在"}
    meta = article_meta(d)
    body_path = d / "正文.md"
    body = textfmt.strip_images(
        body_path.read_text(encoding="utf-8"))[:MAX_PROMPT_BODY] if body_path.exists() else ""
    if len(body) < 20:
        # 微博长图这类正文只有一两句的，真正的内容在图里，等 OCR 补完再分。
        # 报「太短」会让人以为是内容问题，其实只是还没轮到 OCR。
        if meta.get("needs_ocr") or meta.get("ocr_status") in ("pending", "failed"):
            return {"ok": False, "error": "正文还在图里，等 OCR 转出文字后再分类"}
        return {"ok": False, "error": "正文太短，没法判断主题"}
    topics = load_topics()
    prompt = CLASSIFY_PROMPT.format(
        topics="、".join(topics) if topics else "（暂无，按文章内容新建一个）",
        title=meta.get("title", d.name), body=body)
    try:
        raw = naming.ask_4b(prompt)
    except Exception as e:  # noqa: BLE001
        return {"ok": False, "error": f"4B 没应答：{str(e)[:80]}"}
    # 兼容老格式（只输出主题名）与新格式（主题|标签1,标签2）
    raw_topic, _, raw_tags = raw.partition("|")
    topic = naming.clean_name(raw_topic)
    if len(topic) < 2 or len(topic) > 8:
        return {"ok": False, "error": f"4B 输出的主题不像话：{raw[:24]!r}"}
    if "：" in topic or ":" in topic:
        topic = topic.split("：")[0].split(":")[0].strip()
    tags = []
    # 只按标点切，不按空格切——「Scaling Laws」这类英文名要当一个标签
    for part in re.split(r"[,，、;；]+", raw_tags):
        t = naming.clean_name(part)
        if 2 <= len(t) <= 12 and t != topic and t not in tags:
            tags.append(t)
        if len(tags) >= 4:
            break

    old_topic = meta.get("topic", UNCATEGORIZED)
    meta["topic"] = topic
    if tags:
        meta["tags"] = tags
    ms = meta.setdefault("milestones", {})
    ms.setdefault("submitted_at", ms.get("fetched_at", ""))
    ms["classified_at"] = now()
    meta["schema"] = max(int(meta.get("schema") or 1), 2)
    if topic != old_topic:
        target_dir = READ_ROOT / topic
        target_dir.mkdir(parents=True, exist_ok=True)
        target = unique_dir(target_dir, d.name)
        try:
            shutil.move(str(d), str(target))
        except OSError as e:
            return {"ok": False, "error": f"移动失败：{e}"}
        _remap_state(f"{old_topic}/{d.name}", f"{topic}/{target.name}")
        d = target
    (d / "article.json").write_text(
        json.dumps(meta, ensure_ascii=False, indent=1), encoding="utf-8")

    created = topic not in topics and topic != UNCATEGORIZED
    if created:
        topics.append(topic)
        save_topics(topics)
    return {"ok": True, "title": meta.get("title", d.name), "topic": topic,
            "tags": meta.get("tags", []), "created": created,
            "rel": f"{topic}/{d.name}",
            "ms": int((time.time() - t0) * 1000)}


def classify_all() -> dict:
    root = READ_ROOT / UNCATEGORIZED
    dirs = [p for p in root.iterdir() if p.is_dir()] if root.exists() else []
    done, failed = [], []
    for d in dirs:
        r = classify_dir(d)
        (done if r.get("ok") else failed).append(r)
    return {"ok": True, "total": len(dirs), "done": done, "failed": failed}


OCR_START = "<!-- ocr:start -->"
OCR_END = "<!-- ocr:end -->"
OCR_HEADING = "## 图片文字（OCR）"
_OCR_RE = re.compile(re.escape(OCR_START) + r"(.*?)" + re.escape(OCR_END), re.S)


def tidy_article(d: Path) -> dict:
    """把一篇已经存坏的正文重排一遍（不重新抓网页，幂等）。

    正文主体走 tidy_plain；OCR 段优先用 article.json 里的 ocr_text 原始识别结果
    重排（而不是对已整理的文本再整理一次，避免信息越洗越少）。
    """
    d = Path(d)
    p = d / "正文.md"
    if not p.exists():
        return {"ok": False, "dir": str(d), "error": "没有 正文.md"}
    raw = p.read_text(encoding="utf-8")
    m = _OCR_RE.search(raw)
    inner = m.group(1) if m else ""
    main_text = ((raw[:m.start()] + raw[m.end():]) if m else raw).replace(OCR_HEADING, "")
    body = textfmt.tidy_plain(main_text)

    meta = article_meta(d)
    ocr_src = str(meta.get("ocr_text") or "").strip()
    if not ocr_src and inner:
        ocr_src = inner.replace(OCR_HEADING, "").strip()
    ocr_clean = textfmt.tidy_ocr_doc(ocr_src) if ocr_src else ""

    out = body.strip()
    if ocr_clean:
        out += "\n\n" + f"{OCR_START}\n{OCR_HEADING}\n\n{ocr_clean}\n{OCR_END}"
    out = out.strip() + "\n"
    changed = out != raw
    if changed:
        p.write_text(out, encoding="utf-8")
        if ocr_clean:
            meta["ocr_text"] = ocr_clean
            meta["ocr_chars"] = len(ocr_clean)
        meta["chars"] = len(textfmt.strip_images(body)) + len(ocr_clean)
        (d / "article.json").write_text(
            json.dumps(meta, ensure_ascii=False, indent=1), encoding="utf-8")
    return {"ok": True, "dir": str(d), "changed": changed,
            "chars": len(textfmt.strip_images(body)) + len(ocr_clean),
            "paragraphs": len([x for x in out.split("\n\n") if x.strip()])}


def tidy_all() -> dict:
    rows = []
    if not READ_ROOT.is_dir():
        return {"ok": False, "error": f"没有数据根 {READ_ROOT}"}
    for topic in sorted(READ_ROOT.iterdir()):
        if not topic.is_dir() or topic.name.startswith(("_", ".")):
            continue
        for d in sorted(topic.iterdir()):
            if not d.is_dir() or d.name.startswith("."):
                continue
            r = tidy_article(d)
            rows.append({"rel": f"{topic.name}/{d.name}",
                         "ok": r.get("ok"), "changed": r.get("changed"),
                         "paragraphs": r.get("paragraphs"), "chars": r.get("chars"),
                         **({"error": r["error"]} if r.get("error") else {})})
    return {"ok": True, "count": len(rows),
            "changed": sum(1 for r in rows if r.get("changed")), "items": rows}


def refetch(d: Path) -> dict:
    """按 article.json 里的原链接重抓这一篇，覆盖正文与图片。

    只用于「以前存下来时丢了东西」的补救：老的公众号正文只有文字、
    图片和版式全丢了，重抓一次就能按原文把图插回原位。
    主题/标签/划线/OCR 结果原样保留（fetch_text 里合并旧 meta）。
    """
    d = Path(d)
    meta = article_meta(d)
    url = str(meta.get("url") or meta.get("canonical") or "")
    if not url.startswith("http"):
        return {"ok": False, "dir": str(d), "error": "这篇文章没记原链接"}
    before = len((d / "正文.md").read_text(encoding="utf-8")) if (d / "正文.md").exists() else 0
    r = fetch_text(url, dest=d)
    r["dir"] = str(d)
    r["before_chars"] = before
    return r


def refetch_all(platform: str = "公众号") -> dict:
    """把所有指定平台的存量文章重抓一遍（默认只补公众号的图文）。"""
    rows = []
    if not READ_ROOT.is_dir():
        return {"ok": False, "error": f"没有数据根 {READ_ROOT}"}
    for topic in sorted(READ_ROOT.iterdir()):
        if not topic.is_dir() or topic.name.startswith(("_", ".")):
            continue
        for d in sorted(topic.iterdir()):
            if not d.is_dir() or d.name.startswith("."):
                continue
            m = article_meta(d)
            if platform and platform not in str(m.get("platform") or ""):
                continue
            r = refetch(d)
            rows.append({"rel": f"{topic.name}/{d.name}", "ok": r.get("ok"),
                         "chars": r.get("chars"), "images": r.get("images"),
                         **({"error": r["error"]} if r.get("error") else {})})
    return {"ok": True, "count": len(rows),
            "done": sum(1 for r in rows if r.get("ok")),
            "images": sum(int(r.get("images") or 0) for r in rows),
            "items": rows}


def main(argv: list[str]) -> int:
    if len(argv) < 2:
        print(__doc__)
        return 2
    cmd = argv[1]
    if cmd == "fetch":
        if len(argv) < 3:
            print(json.dumps({"ok": False, "error": "缺少链接"}, ensure_ascii=False))
            return 2
        print(json.dumps(fetch_text(argv[2], hint=(argv[3] if len(argv) >= 4 else "")),
                         ensure_ascii=False, indent=1))
        return 0
    if cmd == "tidy":
        # 已经把正文存坏了的（碎片行、OCR 一行一段）用这个重排，不重新抓网页
        if len(argv) >= 3 and argv[2] == "--all":
            print(json.dumps(tidy_all(), ensure_ascii=False, indent=1))
        elif len(argv) >= 3:
            print(json.dumps(tidy_article(Path(argv[2])), ensure_ascii=False, indent=1))
        else:
            print(json.dumps({"ok": False, "error": "缺少目录或用 --all"}, ensure_ascii=False))
            return 2
        return 0
    if cmd == "refetch":
        # 以前存下来只留了文字的文章（公众号图文丢了图），按原链接重抓一遍
        if len(argv) >= 3 and argv[2] == "--all":
            plat = argv[3] if len(argv) >= 4 else "公众号"
            print(json.dumps(refetch_all(plat), ensure_ascii=False, indent=1))
        elif len(argv) >= 3:
            print(json.dumps(refetch(Path(argv[2])), ensure_ascii=False, indent=1))
        else:
            print(json.dumps({"ok": False, "error": "缺少目录或用 --all"}, ensure_ascii=False))
            return 2
        return 0
    if cmd == "classify":
        if len(argv) >= 3 and argv[2] == "--all":
            print(json.dumps(classify_all(), ensure_ascii=False, indent=1))
        elif len(argv) >= 3:
            print(json.dumps(classify_dir(Path(argv[2])), ensure_ascii=False, indent=1))
        else:
            print(json.dumps({"ok": False, "error": "缺少目录或用 --all"}, ensure_ascii=False))
            return 2
        return 0
    print(json.dumps({"ok": False, "error": f"未知命令 {cmd}"}, ensure_ascii=False))
    return 2


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
