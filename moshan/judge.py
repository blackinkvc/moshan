#!/usr/bin/env python3
"""存图还是存文章：看抓到的内容，不看你打了什么字。

判断只有一条主干：**正文和图片谁是主角**。

  · 字数过线 → 文章（公众号 300 字、微博/小红书 500 字）
  · 字数到六成、图又只有 3 张以内 → 文章（图只是配图，不是内容）
  · 一张图都没有、字够 30 → 文章（短公告、纯文字页）
  · 其余 → 图集

「一张图都没有」不能放宽成「图不多」：公众号的图片消息里，单图配 96 字
文案是最常见的一类，那是照片不是文章（2026-11-19 试过一次，多判了 6 条
OOTD 进阅读）。

字数线为什么这么定：公众号 300 字以上基本就是真文章；小红书/微博的笔记
大多是 100 字以内的配文，超过 500 字已经是在写东西了。图多又字中等的是
照片集（穿搭九图、旅行九图），要求字数**全额**过线才算文章。

⚠️ 字数只是**没有版面信息时**的替代信号。2026-11-19 之前不认版面，只看字数，
结果两头都不对：公众号的「图片消息」配文四百多字照样被判成文章（库里堆了 5 条
九图配文在存图里、2 条在阅读里），小红书四百多字的长笔记被 600 的线挡在图集。
现在版面优先，字数只兜底。

两个方向都靠这一套：linkbox 抓到图之后判一次（判成文章就转存阅读），
article.py 抓到正文之后判一次（判成图集就转存图）。

**拿不准就问，不猜**（2026-11-20 加）：字数和张数都落在中间那一段时，
`decide()` 返回 `ask=True` 并附一句问题，调用方（pending.py）把它挂起、
发一句问话，等用户回「存图 / 存文章」再动手。以前这类靠字数硬拍，用户在两头
都碰见过放错的：
  · 公众号「图片消息」九图 + 四百多字配文 → 判成文章，照片堆进阅读库；
  · 小红书几百字长笔记 + 九图 → 被判成图集，正文只剩配图。
"""
from __future__ import annotations

ARTICLE_KEYWORDS = ("文章", "长文", "正文")
GALLERY_KEYWORDS = ("存图", "图集", "存图片")

# 写真类直通：用户原话（2026-11-21）——「写真，不管里面文字写的什么，
# 我都是存了看里面的美女图片的」。写真/人像这类内容，图才是唯一目的，
# 配文再长也只是店家话术和话题标签，不该拿字数去判文章、更不该为此问一句。
# 2026-11-21 又确认《…Cos 人像》也是「显然是存图」，所以从「写真」扩到「人像」。
# 只认标题/原话里明说的这两个词，不往「摄影」「街拍」上再扩散。
VISUAL_KEYWORDS = ("写真", "人像")

# 正文这么多字才算文章。公众号和笔记类差一个量级，分开定。
ARTICLE_TEXT = {"公众号": 300, "微博": 500, "小红书": 500}
DEFAULT_ARTICLE_TEXT = 500
MIN_TEXT = 30          # 不到 30 字只能是图（长图笔记、纯图）
FEW_IMAGES = 3         # 图少于这个数，「字过半」就算文章
FEW_RATIO = 0.6        # 「字过半」的线

# ---- 「两边都像」的区间：命中就别猜，挂到聊天窗里问一句（2026-11-20 用户要求）----
# 用户第一次说：「以后有疑惑的，直接发对话问我好了，我直接回复文字」；
# 第二次又强调一次「还是有的分不清」——说明 0.8~1.5 那条带**太窄**，
# 真正的错案都掉在带外面：
#   · 摄影/OOTD 九图 + 四五百字配文（0.6~1.0 倍线）被判成文章进了阅读库；
#   · 短一些的公众号图文（0.5 倍线上下）被判成图集，正文只剩几张图。
# 现在只要「图够多（≥4）」且「字不算少（≥半个线）」，就问 —— 问错的代价
# 只是多回一句话，判错的代价是东西进错库、以后翻不到。
ASK_TEXT_LO = 0.5      # 字数到线的一半
ASK_TEXT_HI = 3.5      # 超过三倍半就是真长文了（图文教程那种），别问
ASK_MIN_IMG = 4        # 图少于四张没什么可争的
ASK_FEW_LO = 0.6       # 图少（1~3 张）但字卡在线下的，也在争：按文章还是按配图
ASK_FEW_HI = 1.0


def keyword_of(text: str) -> str | None:
    """用户原话里想要哪种。返回 'article' / 'gallery' / None。

    只认链接前后那一段，不认整条消息：转发口令、引用原文里带个「文章」不算。
    """
    body = text or ""
    if not body.strip():
        return None
    # 「存图」优先：「这文章帮我存图」要以存图为准
    if any(k in body for k in GALLERY_KEYWORDS):
        return "gallery"
    if any(k in body for k in ARTICLE_KEYWORDS):
        return "article"
    return None


def limit_for(platform: str) -> int:
    """按平台取字数线。平台名两种写法都认（「公众号」/「微信公众号」）。"""
    p = platform or ""
    for key, val in ARTICLE_TEXT.items():
        if key in p:
            return val
    return DEFAULT_ARTICLE_TEXT


def ambiguous(text_len: int, n_img: int, platform: str = "", layout: str = "") -> bool:
    """这条是不是「两边都像」。是就别自己拍板，交给用户一句话定。

    两段窄带（不是所有拿不准的都问）：
      · 图多（≥4 张）且字数在线的 0.5~3.5 倍 —— 典型是九图配四五百字；
      · 图少（1~3 张）但字数卡在线的 0.6~1.0 倍 —— 短文章和长配文在这撞车。
    一张图配三千字（真文章）、九图配四十字（纯图集）都不问。
    版面已经明说是正文文章（layout=="article"）也不问 —— 那是文章，图只是插图。
    """
    limit = limit_for(platform)
    if layout == "article":
        return False
    r = float(max(0, int(text_len or 0))) / float(limit)
    if n_img >= ASK_MIN_IMG:
        return ASK_TEXT_LO <= r <= ASK_TEXT_HI
    if n_img >= 1:
        return ASK_FEW_LO <= r <= ASK_FEW_HI
    return False


def question(text_len: int, n_img: int, platform: str = "", title: str = "") -> str:
    """问用户的那一句。短、带数字、给两个能照抄的答案。"""
    name = ("《%s》" % str(title).strip()) if str(title or "").strip() else "这条"
    return (f"{name}{n_img} 张图、{text_len} 字，看着两边都像。"
            f"回「存图」还是「存文章」？")


def decide(text_len: int, n_img: int, platform: str = "",
           hint: str = "", layout: str = "", title: str = "",
           n_video: int = 0) -> dict:
    """返回 {'mode': 'article'|'gallery', 'why': 一句人话}。

    text_len 是去掉图床地址之后的纯正文字数，n_img 是这篇的图片张数。
    n_video 是这条里的视频段数（抖音/微博）：有视频就是视频，不参与图/文之争。
    layout 是抓取器给的版面类型（公众号才有）：
      "article" = 正文文章，字才是内容
      "picture" = 图片消息，图才是内容（配文几百字也还是照片集）
    版面明确时不再猜字数——字数只是没版面信息时的替代信号。

    另外两种可能的键（调用方要认，不然「问一句」这个能力就白白浪费了）：
      ask=True  → 两边都像，别动手：挂起来问用户，q 就是要发出去的那句话。
      forced=True → 用户原话里已经说了存图/文章，这不是判断结果，别再问回去。
    """
    text_len = max(0, int(text_len or 0))
    n_img = max(0, int(n_img or 0))
    n_video = max(0, int(n_video or 0))
    kw = keyword_of(hint)
    if kw == "article":
        return {"mode": "article", "why": "你说了「文章」", "forced": True}
    if kw == "gallery":
        return {"mode": "gallery", "why": "你说了「存图」", "forced": True}
    # 视频直通：视频作品的「配文」是文案不是文章，视频也没法塞进阅读库。
    # 必须在 ask 之前——抖音几百字的文案正好落在问话带里，问「存图还是存文章」
    # 对一个视频毫无意义（2026-11-27 加）。
    if n_video >= 1:
        return {"mode": "gallery", "why": f"视频作品（{n_video} 段），不按文章转存"}
    # 写真直通：标题或用户原话里带「写真」，有图就存图，字数完全不看。
    # 必须排在 ask 之前——写真笔记的几百字话术正好落在问话带里。
    # 用户明确说了「文章」时上面已强制返回，不会被这条盖掉。
    who = f"{str(title or '')} {str(hint or '')}"
    if any(k in who for k in VISUAL_KEYWORDS) and n_img >= 1:
        return {"mode": "gallery", "why": "写真：图才是内容，配文不算数"}
    # 两边都像：先给一个「倾向」，但打上 ask，让调用方挂起来问一句再动手。
    # 注意必须排在版面/字数判定**之前**——图片消息的版面信号本来就不可靠。
    if ambiguous(text_len, n_img, platform, layout):
        limit = limit_for(platform)
        lean = "article" if text_len >= limit else "gallery"
        return {"mode": lean, "ask": True,
                "why": f"{n_img} 张图、{text_len} 字，两边都像",
                "q": question(text_len, n_img, platform, title)}
    if layout == "picture":
        limit = limit_for(platform)
        # 「版面=图片消息」这条信号本身会误报：2026 年的新版公众号模板把正文
        # 放进 js_content_container，抓不到正文容器时也会被判成图片消息
        # （2026-11-19 实测：一篇 438 字、6 张插图的文章被这条规则打成图集）。
        # 所以图片消息只在「图多（满九宫格）或字少」时成立；字够多、图又只有
        # 五六张的，那是文章里的配图，不是照片集。
        if text_len >= limit and n_img < 9:
            return {"mode": "article",
                    "why": f"正文 {text_len} 字、{n_img} 张图，是文章配图"}
        return {"mode": "gallery",
                "why": f"图片消息（{n_img} 张图、配文 {text_len} 字），图才是内容"}
    if layout == "article":
        return {"mode": "article", "why": f"正文文章（{text_len} 字）"}
    limit = limit_for(platform)
    if text_len >= limit:
        return {"mode": "article", "why": f"正文 {text_len} 字，过 {limit} 字线"}
    if n_img == 0 and text_len >= MIN_TEXT:
        return {"mode": "article", "why": f"没有图、正文 {text_len} 字"}
    if n_img <= FEW_IMAGES and text_len >= limit * FEW_RATIO:
        return {"mode": "article",
                "why": f"正文 {text_len} 字（够 {int(limit * FEW_RATIO)}）、图才 {n_img} 张"}
    return {"mode": "gallery", "why": f"正文 {text_len} 字 / {n_img} 张图"}
