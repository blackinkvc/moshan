"""正文整理：把网页 HTML 与截屏 OCR 的碎片行收拾成能读的段落。

两类来源的毛病不一样，所以分两条路：

1) 网页（公众号 / 知乎 / 通用页面）
   原来用 `strip_tags()` 一律「标签换行」，可是公众号正文是一堆
   `<span leaf="">` 套在 `<p>` 里，于是一句话被切成七八行：

       让人意外的是，Anthropic居然在IPO路演中，
       把这张内部神图当成头号王牌公之于众
       。

   `html_to_text()` 只把**块级**标签当段落边界，内联标签直接消掉，
   再由 `reflow()` 把同一段里被硬折的行接回去。

2) 截屏 OCR（微博 / 小红书长图）
   一行就是图上的一行（手机屏宽，十五六个字），还夹着状态栏、
   「9邀请回答」「+关注」「→」这类界面字。
   `tidy_ocr()` 先滤掉界面噪声行，再按中文标点把断行接成句子。

两个函数都是幂等的：对整理过的文本再跑一遍，结果不变。
"""
from __future__ import annotations

import re

# 块级标签：这些是真正的段落/换行边界
_BLOCK = ("p", "div", "section", "article", "main", "header", "footer", "aside",
          "nav", "blockquote", "pre", "figure", "figcaption", "hr",
          "h1", "h2", "h3", "h4", "h5", "h6",
          "ul", "ol", "li", "dl", "dt", "dd", "table", "thead", "tbody",
          "tr", "td", "th", "caption")
# 整块丢掉的（内容不是正文）
_DROP = ("script", "style", "noscript", "iframe", "svg", "video", "audio",
         "head", "form", "select", "button")

_TEXT_BLOCK_RE = re.compile(
    r"(?is)<(/?)\s*(" + "|".join(_BLOCK) + r")\b[^>]*>")
_DROP_RE = re.compile(
    r"(?is)<(" + "|".join(_DROP) + r")\b[^>]*>.*?</\1\s*>")
_BR_RE = re.compile(r"(?is)<br\s*/?>")
_TAG_RE = re.compile(r"(?s)<[^>]+>")

# 句末 / 句中：用来判断这一行是不是话说完了
_SENT_END = "。！？!?…"
_SENT_END_EXTRA = "」』》）】”’\"'"
_SOFT_END = "，、；：,;:—–~～"
_NO_LINE_START = "，。、；：,;:！？!?）】》」』”’…"

_CN = re.compile(r"[\u4e00-\u9fff]")
# 一眼就知道是小标题的行首：「▍全景：」「一、」「2.」「第三章」
_HEADING_START = re.compile(
    r"^(?:[▍■◆●▪▫◦◾◽※]|[一二三四五六七八九十]+[、.．]|\d{1,2}[、.．]\s|第[一二三四五六七八九十]+[章节部分])")


def _decode(s: str) -> str:
    import html as _h
    s = _h.unescape(s or "")
    # 全角空格、不换行空格、零宽字符
    s = s.replace("\u3000", " ").replace("\xa0", " ").replace("\u200b", "")
    return s


def html_to_text(html_text: str) -> str:
    """HTML 片段 → 保留段落结构的纯文本（不合并行，交给 reflow）。"""
    s = _DROP_RE.sub(" ", html_text or "")
    # <br> 是作者的手动折行（软边界），先用普通换行占位
    s = _BR_RE.sub("\n", s)
    s = _TEXT_BLOCK_RE.sub("\n\n", s)
    s = _TAG_RE.sub("", s)
    s = _decode(s)
    out = []
    for ln in s.splitlines():
        ln = re.sub(r"[ \t]+", " ", ln).strip()
        out.append(ln)
    # 连续空行压成一个（也就是段落分隔符）
    text = "\n".join(out)
    return re.sub(r"\n{3,}", "\n\n", text).strip()


# ---------------------------------------------------------------- 保留版式

# 行级 markdown 标记：这些行是「整块」的，不能和上下文接行
_MD_LINE = re.compile(r"^(?:!\[[^\]]*\]\([^)]*\)|#{1,6} |[-*] |> )")
_IMG_IN_MD = re.compile(r"!\[[^\]]*\]\(([^)]*)\)")
_FULL_IMG = re.compile(r"^!\[[^\]]*\]\([^)]*\)$")
_TOKEN_RE = re.compile(
    r"""(?is)<(/?)([a-zA-Z][a-zA-Z0-9]*)((?:"[^"]*"|'[^']*'|[^>"'])*)(/?)>|([^<]+)""")
_HEAD_TAGS = {"h1": 1, "h2": 2, "h3": 3, "h4": 4, "h5": 5, "h6": 6}
# 这些图不是正文：二维码、头像、表情占位
_BAD_IMG = ("res.wx.qq.com", "qrcode", "qr_code", "data:image")


def _img_src(attrs: str) -> str:
    """<img> 的属性串 → 真实图片地址（公众号懒加载把真地址放在 data-src）。"""
    for attr in ("data-src", "data-original", "data-actualsrc", "src"):
        m = re.search(r"""\b%s\s*=\s*["']([^"']+)["']""" % attr, attrs or "", re.I)
        if not m:
            continue
        u = _decode(m.group(1)).replace("\\/", "/").strip()
        if not u.lower().startswith(("http://", "https://")):
            continue
        if any(k in u for k in _BAD_IMG):
            continue
        return u.replace(" ", "%20")
    return ""


def html_to_md(html_text: str, keep_img: bool = True) -> str:
    """HTML 片段 → 保留版式的 markdown（块级标记 + `![](图地址)` 留在原位置）。

    跟 `html_to_text()` 的区别就是「不把格式抹平」：小标题、列表、引用、图片
    都按原文顺序留下来，正文里图片插在它本来出现的地方。

    公众号正文的坑：`<span leaf="">` 把一句话切成七八段、`<section>` 层层嵌套、
    空段落 `<p><br></p>` 满地都是 —— 所以只在**缓冲区非空时**才产出块，
    嵌套的容器标签自然不会产出空行。
    """
    s = re.sub(r"(?is)<!--.*?-->", " ", html_text or "")
    s = _DROP_RE.sub(" ", s)
    out: list[str] = []
    buf: list[str] = []
    kind, level = "p", 0
    strong = 0          # 在 <strong>/<b> 里面的层数
    plain = False       # 这段里有没有「不在加粗里」的字

    def flush() -> None:
        nonlocal buf, kind, level, strong, plain
        t = "".join(buf)
        buf = []
        bold_only = bool(t.strip()) and (not plain) and strong == 0
        k, lvl = kind, level
        kind, level, strong, plain = "p", 0, 0, False
        t = re.sub(r"[ \t\r\f\v]+", " ", t)
        t = re.sub(r" *\n *", "\n", t)
        lines = [x.strip() for x in t.split("\n") if x.strip()]
        if not lines:
            return
        if k == "h":
            out.append("#" * min(6, lvl or 2) + " " + " ".join(lines))
        elif k == "li":
            out.append("- " + " ".join(lines))
        elif k == "q":
            out.append("\n".join("> " + x for x in lines))
        else:
            # 整段都包在 <strong> 里、又短又没句末标点的，是公众号的小标题
            if bold_only and len(" ".join(lines)) <= 40 \
                    and not re.search(r"[。！？；，,：:]", " ".join(lines)):
                out.append("## " + " ".join(lines))
            else:
                # 块内 <br> 只是作者的手动折行：留成软换行，交给 reflow 按标点
                # 决定「接回去」还是「另起一段」（公众号常在半句话处折行）
                out.append("\n".join(lines))

    for m in _TOKEN_RE.finditer(s):
        closing, tag, attrs, selfclose, text = m.groups()
        if text is not None:
            buf.append(_decode(text))
            if strong == 0 and text.strip():
                plain = True
            continue
        tag = (tag or "").lower()
        if tag in ("br",):
            buf.append("\n")
            continue
        if tag == "img":
            flush()
            src = _img_src(attrs)
            if src and keep_img:
                out.append(f"![]({src})")
            continue
        if tag in _HEAD_TAGS:
            if not closing:
                flush()
                kind, level = "h", _HEAD_TAGS[tag]
            else:
                flush()
            continue
        if tag == "li":
            flush()
            if not closing:
                kind = "li"
            continue
        if tag == "blockquote":
            flush()
            if not closing:
                kind = "q"
            continue
        if tag in ("strong", "b"):
            strong += 1 if not closing else -1
            continue
        if tag in _BLOCK:
            # 包裹用的容器（div/section/p）只当分段边界，不写标记
            if not closing and kind == "p":
                flush()
            elif closing:
                flush()
            continue
    flush()
    md = "\n\n".join(x for x in out if x.strip())
    return re.sub(r"\n{3,}", "\n\n", md).strip()


def strip_images(md: str) -> str:
    """去掉 `![](…)` 行：送 4B 分类、算字数时不该把图片地址当正文。"""
    return re.sub(r"(?m)^!\[[^\]]*\]\([^)]*\)\s*$", "", md or "").strip()


def _ascii_word_edge(ch: str) -> bool:
    return ch.isascii() and (ch.isalnum() or ch in "'’")


def join_line(a: str, b: str) -> str:
    """接行：中文直接拼；两侧都是 ASCII 单词时补一个空格；英文断词去掉连字符。"""
    if not a:
        return b
    if not b:
        return a
    if a.endswith("-") and _ascii_word_edge(a[-2]) and _ascii_word_edge(b[0]):
        return a[:-1] + b
    if _ascii_word_edge(a[-1]) and _ascii_word_edge(b[0]):
        return a + " " + b
    return a + b


def _is_short_heading(line: str, short_keep: int) -> bool:
    """像栏目名/小标题的短行（「新智元报道」）：别跟下一行粘在一起。

    只对含中文的短行生效 —— OCR 里「AI」这种英文短行通常是长句被折行，
    应该接回去，不该独立成段。
    """
    if len(line) > short_keep:
        return False
    if re.search(r"[，,。；;：:！？!?、]", line):
        return False
    return bool(_CN.search(line))


def reflow(text: str, short_keep: int = 12) -> str:
    """把硬折的行接成段落（空行 = 段落边界）。"""
    paras: list[str] = []
    cur = ""
    blank = True
    pending = ""          # 上一行（判断下一行是不是它的延续）
    for raw in (text or "").splitlines():
        ln = raw.strip()
        if not ln:
            blank = True
            continue
        if blank and cur:
            paras.append(cur)
            cur = ""
        blank = False
        # 图片 / 小标题 / 列表 / 引用：整行自成一类，前后都不接
        if _MD_LINE.match(ln):
            if cur:
                paras.append(cur)
                cur = ""
            paras.append(ln)
            pending = ln
            continue
        if not cur:
            cur = ln
            pending = cur
            continue
        if _HEADING_START.match(cur):         # 上一行是小标题 → 到它为止
            paras.append(cur)
            cur = ln
            pending = ln
            continue
        # 短行后面紧跟引号/书名号开头的一句：那是「图的纵轴叫 / 「算力乘数」」这种排版断行
        quoted_next = ln[0] in "「『“‘《【"
        if ln[0] in _NO_LINE_START:
            cur = join_line(cur, ln)          # 标点开头：肯定是上一行的延续
        elif cur[-1] in _SOFT_END:
            cur = join_line(cur, ln)          # 上一行停在逗号/顿号：话没说完
        elif quoted_next and len(pending) <= short_keep + 4 and _CN.search(pending):
            cur = join_line(cur, ln)
        elif cur[-1] in _SENT_END or cur[-1] in _SENT_END_EXTRA:
            paras.append(cur)                 # 上一行说完了 → 新段
            cur = ln
        elif _is_short_heading(cur, short_keep):
            paras.append(cur)
            cur = ln
        else:
            cur = join_line(cur, ln)
        pending = ln
    if cur:
        paras.append(cur)
    paras = [p for p in paras if p]
    # 孤儿短尾：「……「对数刻度」」换行后单独一个「四个字里。」→ 并回上一段
    merged: list[str] = []
    for p in paras:
        # 注意两处：p 本身不能是块标记，merged[-1] 也不能是（图片行、标题行）
        if (merged and len(p) <= 6 and _CN.search(p) and len(merged[-1]) > 12
                and not p.startswith(("#", "-", "*", "!", ">"))
                and not merged[-1].startswith(("#", "-", "*", "!", ">"))):
            merged[-1] = join_line(merged[-1], p)
        else:
            merged.append(p)
    return "\n\n".join(merged).strip()


# ---------------------------------------------------------------- OCR 专用

# 整行就是界面元素（不是正文）
_NOISE_EXACT = re.compile(
    r"^(?:邀请回答|写回答|回答问题|分享|收藏|评论|点赞|在看|"
    r"打开APP|打开App|下载APP|阅读原文|查看图片|展开全文|收起|"
    r"关注|已关注|互相关注|\+关注|举报|返回|首页|更多|搜索|"
    r"知乎|微博|小红书|抖音|微信)$")
# 行里带这些字眼的多半是界面计数条（「679个回答・1567个关注」「1万人赞同」）
_NOISE_ANY = re.compile(
    r"(?:个回答|个关注|人赞同|人听过|人收藏|次播放|万粉丝|邀请回答|写回答)")
_CLOCK = re.compile(r"^\d{1,2}[:：]\d{2}$")
# 信号/电量/运营商被识成一串符号数字，如「Q * 486001 080」
_STATBAR = re.compile(r"^[A-Za-z]?[\s\d*%·•.\-]{5,16}$")
# 纯符号行：只剩「→」「，」「。」这类
_ONLY_SYMBOL = re.compile(r"^[^\w\u4e00-\u9fff]{1,4}$")
# 版面装饰符（常单独占一行，是下面那行小标题的「标记」）：别丢，挂到下一行去
_DECO = re.compile(r"^[▍■◆●▪▫◦◾◽※]{1,3}$")
# OCR 常把行末的 emoji 认成「口」，接在句号前面（「个规律。口」「听说过口。」）。
# 但「人口。」「门口。」是真的字，所以只在「口」前面不是常见词尾时删。
_KOU_KEEP = "人出进门口户家路伤港窗瓶感碑径粮水服语才气子岸号袋腔红白胃头牙合袖"
_KOU_BEFORE_PUNCT = re.compile(r"(?<![%s])口(?=[。，、；：！？])" % _KOU_KEEP)
# 长图切片在边界被切半行时，OCR 会吐出「A + 乱码 + A」这种回声，掐掉前一份
_ECHO = re.compile(r"([\u4e00-\u9fff][\u4e00-\u9fff，、；：？！“”「」]{7,39}).{0,30}?\1")


def dedupe_echo(line: str) -> str:
    prev = None
    cur = line
    for _ in range(4):                      # 最多叠几层，防止死循环
        prev, cur = cur, _ECHO.sub(r"\1", cur)
        if cur == prev:
            break
    return cur


def _is_noise(line: str) -> bool:
    s = line.strip()
    if not s:
        return True
    if _ONLY_SYMBOL.match(s) or _CLOCK.match(s):
        return True
    if _NOISE_EXACT.match(s) or _NOISE_ANY.search(s):
        return True
    # 状态栏那一串：只在几乎没有中文时才判为噪声（正文里的数字不能误杀）
    if len(_CN.findall(s)) <= 1 and _STATBAR.match(s):
        return True
    return False


def tidy_ocr(text: str) -> str:
    """截屏 OCR 文本 → 段落化正文（滤界面噪声 + 接断行）。"""
    raw = text or ""
    # 「口」出现在行尾或标点前，是表情/图钉被认错；偶尔一两次可能是真的字（人口。），
    # 满篇都是就一定是噪声 —— 按频次自适应，别硬编码。
    kou_tail = re.compile(r"口(?=\s*[。，、；]?\s*$)", re.M)
    if len(kou_tail.findall(raw)) >= 3:
        raw = kou_tail.sub("", raw)

    kept: list[str] = []
    head_zone = True          # 还没进正文：截图顶部是标题栏/作者/关注按钮
    deco = ""                 # 上一行是「▍」这种装饰符，等下挂到下一行开头
    for ln in raw.splitlines():
        ln = re.sub(r"[ \t]{2,}", " ", _decode(ln)).strip()
        if _DECO.match(ln):
            deco = ln
            continue
        if deco:
            ln, deco = deco + ln, ""
        ln = re.sub(r"^([A-Za-z])(?=[\u4e00-\u9fff])", "", ln)   # 行首被认出来的单个字母
        ln = _KOU_BEFORE_PUNCT.sub("", ln)
        ln = re.sub(r"(?<=[。，、；：！？])口\s*$", "", ln)      # 行尾「。口」
        ln = dedupe_echo(ln)
        if _is_noise(ln):
            continue
        # 作者名 / 签名这类短标签：只在前几行里清（真正文开头常有短句，不能一直删）
        if head_zone and len(kept) < 10 and len(ln) <= 8 \
                and not re.search(r"[，,。；;：:！？!?、]", ln):
            continue
        if len(ln) >= 20 and re.search(r"[，,。；;：:！？!?、]", ln):
            head_zone = False
        kept.append(ln)
    body = reflow("\n".join(kept), short_keep=0)
    # 回声可能跨行（半行在上一行、重复句在下一行），合并成段之后再掐一次
    body = "\n\n".join(dedupe_echo(p) for p in body.split("\n\n"))
    body = re.sub(r"。{2,}", "。", body)
    body = re.sub(r"[，、；：]\s*。", "。", body)      # 「脸面，。」这种
    return body.strip()


def split_img_inline(text: str) -> str:
    """图片标记和文字挤在同一行的，拆开让图片单独成段。

    `html_to_md` 本来就按块输出，问题是老版本 reflow 会把紧跟图片的短句
    并回图片行（`![](images/08.png)总结`），页面就只能显示这串字符。
    """
    out: list[str] = []
    for ln in (text or "").split("\n"):
        if _IMG_IN_MD.search(ln) and not _FULL_IMG.match(ln.strip()):
            ln = _IMG_IN_MD.sub(lambda m: "\n\n" + m.group(0) + "\n\n", ln)
        out.append(ln)
    return "\n".join(out)


def tidy_plain(text: str) -> str:
    """已经是纯文本（微博正文、已有 正文.md）：拆开挤在文字里的图，再接断行。"""
    return reflow(split_img_inline(_decode(text)), short_keep=12)


def tidy_ocr_doc(text: str) -> str:
    """一篇里有多张图时，OCR 文本按「### 01」分了段：逐段整理，标题原样留着。"""
    out: list[str] = []
    for part in re.split(r"(?m)^(### .*)$", text or ""):
        if part.startswith("### "):
            out.append(part.strip())
            continue
        t = tidy_ocr(part)
        if t:
            out.append(t)
    return "\n\n".join(out).strip()


__all__ = ["html_to_text", "html_to_md", "strip_images", "split_img_inline",
           "reflow", "join_line",
           "dedupe_echo", "tidy_ocr", "tidy_ocr_doc", "tidy_plain"]
