#!/usr/bin/env python3
"""外观（皮肤 + 强调色）—— 唯一真源。

为什么单独立一份、而不是塞进 dash.py
------------------------------------
1. 皮肤要**跨进程**生效：工作台在 8323/8321，插件各自跑在自己的端口
   （巡检 8774 / Token 价格 8776 / GitHub 8777 / 智能设备 8778 / 关注 8780）。
   它们谁也不用认识谁，只认同一个数据根下的两份文件：
       _工作台/appearance.json   设置本身（皮肤 id + 强调色）
       _工作台/appearance.css    由 json 生成的样式（写给人看的是 json）
2. **写的人只有一个**：设置面板 POST 进来 → 这里改 json + 立刻重写 css。
   读的人（本文件 + cuntu/kit 的 pgkit）只管把 css 发出去，不重新生成。
   两边各算一遍迟早分叉（当初五个插件页配色的分叉就是这么来的）。

皮肤为什么能用「一层 token」换掉整个外观
------------------------------------
工作台和五个插件页共用同一批变量名（--bg/--card/--line/--tx/--pri/--r/--sh1…），
插件页的 kit/shell.css 里那些私有名字（--paper/--ink/--gold）已经全部
指向这一批。所以这里只覆盖 token，不改任何一个组件的排版 —— 换皮不动骨。

三代 Apple 语言的取舍（2026-11-22）
----------------------------------
g1 拟物   亚麻底 + 斜面高光 + 1px 深线 + 小圆角。质感靠 inset 高光和白字阴影，
          不靠图片，所以换肤不掉帧、也不增加任何资源。
g2 毛玻璃 色块铺底 + vibrancy。**卡片不挂 backdrop-filter**：一屏三十张卡
          各开一层实时模糊，滚动会明显发涩，而背景本来就是一层淡渐变，
          模糊出来的东西肉眼几乎看不出。只在顶栏/抽屉/弹层挂（它们本来就挂了）。
g3 液态玻璃 大圆角 + 镜面白边 + 字重加重；卡片挂 blur(18px)，因为这一代的
          辨识度全在「透过去」这件事上，去掉就只剩大圆角了。
          代价是每张卡一层合成层，桌面端可接受，移动端宽度下自动降级为不透明。
"""
from __future__ import annotations

import colorsys
import json
import re
from pathlib import Path

SKINS = ("classic", "g1", "g2", "g3")
DEFAULT = {"skin": "classic", "accent": "#0a84ff", "bg": "auto", "v": 2}

# 预设强调色：都按「浅色底上做正文色」调过明度，直接能过 WCAG AA（4.5:1）。
PRESETS = [
    ("系统蓝", "#0a84ff"),
    ("靛紫", "#5451e8"),
    ("青", "#0d8f86"),
    ("绿", "#0f8f56"),
    ("琥珀", "#a86a04"),
    ("朱红", "#cf3a3a"),
    ("玫红", "#cc2e76"),
    ("石墨", "#3b4252"),
]

# 背景壁纸（2026-11-22 晚）
# ---------------------------------
# bg = "auto" 时各皮肤用自己那张默认底；写成某个 key 就是**四套皮肤共用这一张**；
# "custom" 是用户上传的那张。这跟强调色的关系是并列的两种思路：
#   强调色 —— 全局一个值，四套皮肤天生共用（换皮肤颜色跟过去）；
#   背景   —— 默认各用各的（亚麻配暖光、毛玻璃配冷雾、玻璃配折射），
#             想统一气质就挑一张共用的，等于给整套外观换一个"环境"。
# 文件都在 <数据根>/_工作台/wallpapers/<key>.jpg，缺文件就自动退回纯渐变，
# 不留 404、不白屏 —— 这层本来就是可选的。
WALL_DIR = "wallpapers"
SKIN_DEFAULT_BG = {"classic": "", "g1": "dawn", "g2": "mist", "g3": "glass"}
PRESET_BGS = [
    ("dawn", "晨光", "暖米金主光，拟物默认"),
    ("mist", "冷雾", "冷蓝紫雾，毛玻璃默认"),
    ("glass", "玻璃", "冷色折射，液态玻璃默认"),
    ("dusk", "暮色", "深靛紫，暗色模式最好看"),
    ("moss", "苔原", "灰绿米白，低饱和"),
    ("graphite", "石墨", "中性灰，最不抢戏"),
]
BG_KEYS = tuple(k for k, _, _ in PRESET_BGS) + ("custom",)

# 壁纸之上压的一层薄膜：浅色给白纱（照片太抢会把卡片的字压下去），
# 暗色给暗罩。写进 --wall 里跟着图一起换，省掉一套 data-theme 分支。
VEIL_LIGHT = ("linear-gradient(180deg, rgba(255,255,255,.24), "
              "rgba(255,255,255,.10) 52%, rgba(255,255,255,.18))")
# 暗色的罩子别压太狠：压到 .68 时壁纸的冷色全被吃掉，整块变成一片煤灰，
# 玻璃也就没东西可拾了。.40 起跳刚好留得住色相、又压得住亮部。
VEIL_DARK = ("linear-gradient(180deg, rgba(6,10,18,.40), "
             "rgba(6,10,18,.55))")

_RE_HEX = re.compile(r"^#?([0-9a-fA-F]{6})$")


# ---------------------------------------------------------------- 路径

def folder(cfg: dict) -> Path:
    """与 dash.page_path 同一个数据根（allowed_roots[0]/_工作台）。"""
    roots = (cfg or {}).get("allowed_roots") or []
    base = Path(roots[0]) if roots else Path((cfg or {}).get("inbox_dir", ".")).parent
    return base / "_工作台"


def json_path(cfg: dict) -> Path:
    return folder(cfg) / "appearance.json"


def css_path(cfg: dict) -> Path:
    return folder(cfg) / "appearance.css"


def bg_path(cfg: dict) -> Path:
    """旧版单张壁纸的位置（只在 g3 时代用过）。

    留着只为一件事：/__appearance-bg.jpg 这条老路由别断。新代码一律走
    wall_path()，壁纸按 key 分文件存在 wallpapers/ 下。"""
    return folder(cfg) / "appearance-bg.jpg"


def wall_dir(cfg: dict) -> Path:
    return folder(cfg) / WALL_DIR


def wall_path(cfg: dict, key: str) -> Path:
    """壁纸文件。key 只认预设名和 custom —— 路由会把它拼进 URL，
    这里再挡一次，免得哪天从 json 里读出个 ../ 把别的文件送出去。"""
    k = (key or "").strip()
    if k not in BG_KEYS:
        return wall_dir(cfg) / "_none_.jpg"
    return wall_dir(cfg) / f"{k}.jpg"


def has_wall(cfg: dict, key: str) -> bool:
    try:
        return wall_path(cfg, key).is_file()
    except OSError:
        return False


def wall_ver(cfg: dict, key: str) -> str:
    """custom 那张是替换式的（同名覆盖），浏览器按 max-age 会缓存一天 ——
    换了图还是旧图。给它带上 mtime 当查询串，改一次 URL 变一次，缓存自然失效。
    预设图是「一张 key 一张文件」，不需要。"""
    if key != "custom":
        return ""
    try:
        return f"?v={int(wall_path(cfg, key).stat().st_mtime)}"
    except OSError:
        return ""


def effective_bg(cfg: dict) -> str:
    """当前**实际**用哪张：auto 时按皮肤解析成具体 key，可能为空。"""
    a = load(cfg)
    key = SKIN_DEFAULT_BG.get(a["skin"], "") if a["bg"] == "auto" else a["bg"]
    return key if has_wall(cfg, key) else ""


# ---------------------------------------------------------------- 读写

def _norm_hex(s: str) -> str:
    m = _RE_HEX.match((s or "").strip())
    return ("#" + m.group(1).lower()) if m else ""


def load(cfg: dict) -> dict:
    out = dict(DEFAULT)
    try:
        data = json.loads(json_path(cfg).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        data = {}
    if isinstance(data, dict):
        skin = str(data.get("skin") or "")
        out["skin"] = skin if skin in SKINS else DEFAULT["skin"]
        out["accent"] = _norm_hex(data.get("accent", "")) or DEFAULT["accent"]
        bg = str(data.get("bg") or "")
        if bg == "auto" or bg in BG_KEYS:
            out["bg"] = bg
    return out


def save(cfg: dict, patch: dict) -> dict:
    cur = load(cfg)
    if isinstance(patch, dict):
        skin = str(patch.get("skin") or "").strip()
        if skin in SKINS:
            cur["skin"] = skin
        accent = _norm_hex(patch.get("accent", ""))
        if accent:
            # 存的是**调过明度**的值：保证 --pri 当正文色时对比度够。
            # 浏览器里取色器给什么就调什么，幂等（再调一次不变）。
            cur["accent"] = _tune(accent, False)
        if "bg" in patch:
            bg = str(patch.get("bg") or "").strip()
            # custom 要文件真在才算数，否则选了就是一片纯渐变（用户会以为没生效）
            if bg == "auto" or (bg in BG_KEYS and (bg != "custom" or has_wall(cfg, bg))):
                cur["bg"] = bg
    p = json_path(cfg)
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(json.dumps(cur, ensure_ascii=False, indent=1), encoding="utf-8")
    sync(cfg)
    return cur


# ---------------------------------------------------------------- 强调色

def _rgb(h: str) -> tuple[float, float, float]:
    h = h.lstrip("#")
    return tuple(int(h[i:i + 2], 16) / 255.0 for i in (0, 2, 4))  # type: ignore


def _hex(rgb) -> str:
    return "#" + "".join(f"{max(0, min(255, round(v * 255))):02x}" for v in rgb)


def _lum(h: str) -> float:
    """WCAG 相对亮度。"""
    def f(c):
        return c / 12.92 if c <= 0.03928 else ((c + 0.055) / 1.055) ** 2.4
    r, g, b = (f(c) for c in _rgb(h))
    return 0.2126 * r + 0.7152 * g + 0.0722 * b


def _contrast(a: str, b: str) -> float:
    la, lb = _lum(a), _lum(b)
    hi, lo = max(la, lb), min(la, lb)
    return (hi + 0.05) / (lo + 0.05)


def _tune(h: str, dark: bool) -> str:
    """把强调色收进「当正文/图标色也读得清」的明度区间，色相饱和度不动。

    按 HSL 的 L 收是不行的：纯绿 #00ff00 的 L 只有 .5，收到 .46 还是亮得刺眼，
    因为 HSL 的 L 跟人眼亮度不是一回事。这里改成**按 WCAG 相对亮度反解**：
    对白底（浅色）/ 深底（暗色）做一次二分，找刚好达到 3.4:1 的那一档。

    为什么是 3.4 而不是 AA 正文的 4.5：--pri 实际只用在图标、链接、选中态、
    小号加粗标签上，这些按 WCAG 属于「非文本 / 大文本」档（3:1）。苹果自己的
    系统蓝 #007AFF 在白底上就是 3.6:1 —— 按 4.5 收会把系统蓝压成灰蓝，
    一眼就不是 Apple 了。所以取 3.4，留一点余量。
    """
    r, g, b = _rgb(h)
    hh, ll, ss = colorsys.rgb_to_hls(r, g, b)
    bg = "#0c1016" if dark else "#ffffff"
    target = 3.4

    def at(level: float) -> str:
        return _hex(colorsys.hls_to_rgb(hh, level, ss))

    if _contrast(at(ll), bg) >= target:
        return at(ll)                      # 本来就行，一点不动

    lo, hi = 0.0, 1.0
    best = _tune_floor(hh, ss, bg, lo, hi, target, dark)
    return best


def _tune_floor(hh, ss, bg, lo, hi, target, dark) -> str:
    """二分找边界档。

    浅色底：对比度随 L 上升而下降 → 找**最亮**的合格档（保住颜色鲜度）。
    暗色底：反过来 → 找**最暗**的合格档。
    18 次足够收敛到 1/262144，肉眼无差。
    """
    best = None
    for _ in range(18):
        mid = (lo + hi) / 2
        cand = _hex(colorsys.hls_to_rgb(hh, mid, ss))
        ok = _contrast(cand, bg) >= target
        if ok:
            best = cand
            if dark:
                hi = mid        # 暗色：合格就再暗一点试试
            else:
                lo = mid        # 浅色：合格就再亮一点试试
        else:
            if dark:
                lo = mid
            else:
                hi = mid
    return best or _hex(colorsys.hls_to_rgb(hh, 0.45, ss))


# ---------------------------------------------------------------- 生成 CSS

def _skin_g2() -> str:
    return """
/* ===================== g2 · 扁平毛玻璃（iOS 7–18 / macOS Sonoma） =====================
   色块铺底、1px 发丝线、vibrancy。卡片本身不挂实时模糊（见文件头），
   靠半透明 + 内高光 + 大圆角模拟「浮起来的一层」。 */
:root[data-skin="g2"]{
  color-scheme:light;
  --bg:#d9e2ef;  --card:#ffffff;  --card2:#f5f8fd;
  --line:rgba(16,24,40,.055);  --line2:rgba(16,24,40,.04);
  --tx:#0e1116;  --tx2:#4b5262;  --tx3:#7b8291;
  --chip:rgba(16,24,40,.055);  --ring:rgba(16,24,40,.09);
  --ok:#0b9a68;   --ok-soft:rgba(11,154,104,.10);
  --warn:#b0760a; --warn-soft:rgba(176,118,10,.10);
  --bad:#d63a3a;  --bad-soft:rgba(214,58,58,.09);
  --r-lg:22px; --r:18px; --r-sm:12px; --r-xs:9px;
  --sh1:0 1px 2px rgba(16,24,40,.06), 0 10px 24px -18px rgba(16,24,40,.35);
  --sh2:0 2px 6px rgba(16,24,40,.07), 0 18px 34px -18px rgba(16,24,40,.42);
}
:root[data-skin="g2"][data-theme="dark"]{
  color-scheme:dark;
  --bg:#0b0e14;  --card:rgba(28,33,42,.86);  --card2:rgba(36,42,52,.7);
  --line:rgba(255,255,255,.10);  --line2:rgba(255,255,255,.06);
  --tx:#e9edf4;  --tx2:#a6aebd;  --tx3:#7c8496;
  --chip:rgba(255,255,255,.07);  --ring:rgba(255,255,255,.10);
  --ok:#3ecf99;   --ok-soft:rgba(62,207,153,.12);
  --warn:#e8b060; --warn-soft:rgba(232,176,96,.12);
  --bad:#f97066;  --bad-soft:rgba(249,112,102,.12);
  --sh1:inset 0 1px 0 rgba(255,255,255,.06), 0 1px 2px rgba(0,0,0,.5);
  --sh2:inset 0 1px 0 rgba(255,255,255,.08), 0 14px 34px -16px rgba(0,0,0,.7);
}
/* 这一代的辨识点在「冷色画布 + 纯白浮卡」，而不是把壁纸染成彩色（那是 g3 的活）。
   经典皮肤是「近白底 + 发丝描边卡」，两者一眼能分开：这边的底明显偏冷、偏深一档，
   卡片是纯白无边框，靠投影浮起来 —— Big Sur 之后 macOS 就是这个手感。 */
/* 画布：原先是「冷色画布 + 两块色斑」。现在最上面那层交给 --wall（见文件末尾
   的「背景」段）：有壁纸时是「白纱 + 图」，没壁纸时是 none，下面这串渐变照旧兜底。
   注意顺序 —— background-image 的第一层贴在最上面，所以 --wall 必须写在最前，
   否则图会被底色盖死（旧版 g3 就是反过来写的，靠图不透明才碰巧对）。 */
:root[data-skin="g2"] body{
  background-image:
    var(--wall, none),
    radial-gradient(820px 480px at 12% -6%, #eaf1fd, transparent 62%),
    radial-gradient(760px 440px at 92% 2%, #eae4fb, transparent 58%),
    linear-gradient(180deg,#e3ebf7 0%,#d7e1ef 52%,#dee5f1 100%);
  background-size:cover;
  background-position:center;
  background-attachment:fixed;
}
:root[data-skin="g2"] body::before{
  background:
    radial-gradient(900px 420px at 12% -16%, rgba(10,132,255,.16), transparent 70%),
    radial-gradient(720px 380px at 92% -10%, rgba(124,58,237,.13), transparent 70%);
}
:root[data-skin="g2"][data-theme="dark"] body{
  background-image:
    var(--wall, none),
    radial-gradient(820px 480px at 12% -6%, #16233a, transparent 62%),
    radial-gradient(760px 440px at 92% 2%, #201a38, transparent 58%),
    linear-gradient(180deg,#0f141c 0%,#0a0e15 52%,#0b1017 100%);
  background-size:cover;
  background-position:center;
  background-attachment:fixed;
}
:root[data-skin="g2"][data-theme="dark"] body::before{
  background:
    radial-gradient(900px 420px at 12% -16%, rgba(10,132,255,.16), transparent 70%),
    radial-gradient(720px 380px at 92% -10%, rgba(124,58,237,.13), transparent 70%);
}
:root[data-skin="g2"] .card-h,
:root[data-skin="g2"] section[data-card]>.card-h{border-bottom-color:var(--line2)}
/* 无边框靠投影浮起来：边框收到几乎看不见，投影放大一档，
   悬停时再抬一次（这一代就是靠 elevaton 表达层级，不靠描边） */
:root[data-skin="g2"] .card,
:root[data-skin="g2"] section[data-card]{border-color:transparent}
:root[data-skin="g2"] .card:hover,
:root[data-skin="g2"] section[data-card]:hover{
  border-color:transparent;
  box-shadow:0 3px 8px rgba(16,24,40,.08), 0 24px 44px -20px rgba(16,24,40,.45);
}
"""


def _skin_g1() -> str:
    return """
/* ===================== g1 · 拟物（Aqua / iOS 6） =====================
   质感全部由「渐变 + inset 高光 + 1px 深线」三件事堆出来，不用任何图片：
   亚麻底、斜面卡片、玻璃按钮、白字阴影。圆角刻意小，跟现在的圆润风拉开。 */
:root[data-skin="g1"]{
  color-scheme:light;
  --bg:#ded9c9;  --card:linear-gradient(#fdfcf8,#f0ebdc 58%,#e4dfcd);
  --card2:#f2eddd;
  --line:#8f8773;  --line2:#c9c2ad;
  --tx:#2b2721;  --tx2:#5b5546;  --tx3:#8a8272;
  --chip:#e9e4d3;  --ring:#cfc8b4;
  --ok:#2f7f5f;   --ok-soft:#dfeee5;
  --warn:#96700f; --warn-soft:#f6efd8;
  --bad:#a8332f;  --bad-soft:#f6e2df;
  --r-lg:14px; --r:12px; --r-sm:9px; --r-xs:7px;
  --sh1:inset 0 1px 0 #fff, inset 0 -1px 0 rgba(0,0,0,.10), 0 1px 2px rgba(0,0,0,.16), 0 8px 18px -14px rgba(0,0,0,.45);
  --sh2:inset 0 1px 0 #fff, inset 0 -1px 0 rgba(0,0,0,.10), 0 3px 9px rgba(0,0,0,.26);
}
:root[data-skin="g1"][data-theme="dark"]{
  color-scheme:dark;
  --bg:#1a1713;  --card:linear-gradient(#332e26,#282420 62%,#211e1a);
  --card2:#2e2822;
  --line:#5a5245;  --line2:#3d372e;
  --tx:#ece5d5;  --tx2:#b5ab97;  --tx3:#8b8271;
  --chip:#2e2922;  --ring:#433c32;
  --ok:#6fc79b;   --ok-soft:#1a2b23;
  --warn:#e0b168; --warn-soft:#2b2418;
  --bad:#f08a80;  --bad-soft:#2e1c1a;
  --sh1:inset 0 1px 0 rgba(255,255,255,.13), inset 0 -1px 0 rgba(0,0,0,.5), 0 2px 6px rgba(0,0,0,.6);
  --sh2:inset 0 1px 0 rgba(255,255,255,.16), 0 6px 16px rgba(0,0,0,.55);
}
/* 拟物皮的质感是「亚麻布」，两张 repeating 皮纹必须留在最上层 ——
   所以这一套的顺序跟别的皮肤相反：皮纹 > 壁纸 > 底色渐变。
   壁纸铺在皮纹底下，等于给亚麻衬了一层纸，质感还在、但不再是纯色。 */
:root[data-skin="g1"] body{
  background-image:
    repeating-linear-gradient(90deg, rgba(0,0,0,.020) 0 1px, transparent 1px 3px),
    repeating-linear-gradient(0deg, rgba(255,255,255,.22) 0 1px, transparent 1px 4px),
    var(--wall, none),
    radial-gradient(120% 90% at 50% -20%, #efeade, #ded9c9 62%, #cfc9b6 100%);
  background-size:cover;
  background-position:center;
  background-attachment:fixed;
}
:root[data-skin="g1"] body::before{
  background:
    radial-gradient(900px 420px at 14% -14%, rgba(255,255,255,.55), transparent 70%),
    radial-gradient(700px 380px at 90% -6%, rgba(0,0,0,.05), transparent 70%);
}
:root[data-skin="g1"][data-theme="dark"] body{
  background-image:
    repeating-linear-gradient(90deg, rgba(255,255,255,.014) 0 1px, transparent 1px 3px),
    var(--wall, none),
    radial-gradient(120% 90% at 50% -20%, #2a251f, #1a1713 60%, #12100d 100%);
  background-size:cover;
  background-position:center;
  background-attachment:fixed;
}
:root[data-skin="g1"][data-theme="dark"] body::before{
  background:radial-gradient(900px 420px at 14% -14%, rgba(255,255,255,.07), transparent 70%);
}
/* 卡片：顶栏一条浅色皮纹 + 深色收边，靠的是渐变而不是边框图 */
:root[data-skin="g1"] .card,
:root[data-skin="g1"] section[data-card]{border-color:var(--line)}
:root[data-skin="g1"] .card-h,
:root[data-skin="g1"] section[data-card]>.card-h{
  background:linear-gradient(#fdfbf3,#efe9d8 70%,#e6e0cd);
  border-bottom:1px solid rgba(0,0,0,.16);
  text-shadow:0 1px 0 rgba(255,255,255,.85);
}
:root[data-skin="g1"][data-theme="dark"] .card-h,
:root[data-skin="g1"][data-theme="dark"] section[data-card]>.card-h{
  background:linear-gradient(#3a342b,#2a251f);
  border-bottom-color:rgba(0,0,0,.55);
  text-shadow:0 -1px 0 rgba(0,0,0,.6);
}
/* 玻璃按钮 / 胶囊 / 图标按钮：Aqua 的招牌就是那道上亮下暗的高光 */
:root[data-skin="g1"] .pill,
:root[data-skin="g1"] .icon,
:root[data-skin="g1"] .btn-line{
  background:linear-gradient(#fff,#efeadd 55%,#e0dac8);
  border:1px solid #8f8773;
  box-shadow:inset 0 1px 0 #fff, inset 0 -1px 0 rgba(0,0,0,.10), 0 1px 2px rgba(0,0,0,.14);
  text-shadow:0 1px 0 rgba(255,255,255,.9);
}
:root[data-skin="g1"][data-theme="dark"] .pill,
:root[data-skin="g1"][data-theme="dark"] .icon,
:root[data-skin="g1"][data-theme="dark"] .btn-line{
  background:linear-gradient(#3a342b,#282320 60%,#211d19);
  border-color:#5a5245;
  text-shadow:0 -1px 0 rgba(0,0,0,.7);
}
:root[data-skin="g1"] .btn-line:hover{background:linear-gradient(#fff,#e9f1fd 55%,#d3e2fa);color:var(--pri)}
:root[data-skin="g1"] .seg{
  background:linear-gradient(#e2ddcc,#f2eee2);
  border:1px solid #a9a18c;
  box-shadow:inset 0 1px 2px rgba(0,0,0,.16);
}
:root[data-skin="g1"] .seg button.on{
  background:linear-gradient(#fff,#e8edf7);
  box-shadow:inset 0 1px 0 #fff, 0 1px 2px rgba(0,0,0,.18);
  border:1px solid #a9a18c;
}
:root[data-skin="g1"] .logo{
  background:linear-gradient(#9fc4f0,#2f6fd0 55%,#24559b);
  border:1px solid #1d4a86;
  box-shadow:inset 0 1px 0 rgba(255,255,255,.7), 0 2px 6px rgba(0,0,0,.35);
}
:root[data-skin="g1"] .top{
  background:linear-gradient(#f7f4ea,#e6e1d0);
  border-bottom:1px solid #8f8773;
  box-shadow:0 1px 0 #fff inset, 0 2px 6px rgba(0,0,0,.14);
  -webkit-backdrop-filter:none;backdrop-filter:none;
}
:root[data-skin="g1"][data-theme="dark"] .top{
  background:linear-gradient(#332e26,#211e1a);
  border-bottom-color:#000;
}
"""


def _skin_g3() -> str:
    out = """
/* ===================== g3 · 液态玻璃（macOS 26 / iOS 26） =====================
   上一版的毛病：白 56% + 大模糊 + 一条均匀亮边 —— 那是磨砂塑料，玻璃的三个
   特征一个都不在：
     1) 薄膜有厚度：上沿堆光、中间薄、下沿回光（两层渐变 + 四边 inset 叠出来）；
     2) 边缘折射：1.2px 锥形渐变环，光沿着弧面游走，不会是一条等宽白边；
     3) 会拾色：saturate/brightness 拉起来，背后的冷色真的透上来，卡片才不是灰白。
   画布从「四块高饱和色斑」换成一张低频壁纸：一个主光源（左上）+ 冷色沉降。
   色斑太跳时玻璃再准也像糖果，反而假。 */
:root[data-skin="g3"]{
  color-scheme:light;
  --bg:#dde5f2;
  /* 卡片本体不是一个色值，是「一层膜」：见下面 .card 的三层 background。
     这里给的是等效底色，供 chip / 小件 color-mix 兜底。 */
  --card:rgba(255,255,255,.42);  --card2:rgba(255,255,255,.26);
  --line:rgba(255,255,255,.56);  --line2:rgba(255,255,255,.32);
  --tx:#0a0e16;  --tx2:#2f3646;  --tx3:#565e6d;
  --chip:rgba(255,255,255,.42);  --ring:rgba(255,255,255,.55);
  --ok:#0a8a60;   --ok-soft:rgba(10,138,96,.13);
  --warn:#a2660a; --warn-soft:rgba(162,102,10,.13);
  --bad:#c92f36;  --bad-soft:rgba(201,47,54,.12);
  --r-lg:28px; --r:24px; --r-sm:16px; --r-xs:12px;
  /* 四边 inset 是「玻璃厚度」：上沿 1px 纯镜面、下沿 1px 回光、左右两条 0.5 档，
     再压一层 24px 的内发光当折射带。少了内发光，边框就退化成描边。 */
  --sh1:
    inset 0 1px 0 rgba(255,255,255,.96),
    inset 0 -1px 0 rgba(255,255,255,.40),
    inset 1px 0 0 rgba(255,255,255,.44),
    inset -1px 0 0 rgba(255,255,255,.44),
    inset 0 0 24px rgba(255,255,255,.22),
    0 0 0 .5px rgba(8,18,36,.10),
    0 1px 2px rgba(10,20,40,.06),
    0 16px 38px -24px rgba(10,20,40,.42);
  --sh2:
    inset 0 1px 0 rgba(255,255,255,1),
    inset 0 -1px 0 rgba(255,255,255,.48),
    inset 1px 0 0 rgba(255,255,255,.52),
    inset -1px 0 0 rgba(255,255,255,.52),
    inset 0 0 30px rgba(255,255,255,.3),
    0 0 0 .5px rgba(8,18,36,.12),
    0 2px 4px rgba(10,20,40,.07),
    0 24px 50px -24px rgba(10,20,40,.5);
}
:root[data-skin="g3"][data-theme="dark"]{
  color-scheme:dark;
  --bg:#080d15;
  --card:rgba(26,33,46,.54);  --card2:rgba(32,40,55,.42);
  --line:rgba(255,255,255,.17);  --line2:rgba(255,255,255,.09);
  --tx:#eef3fb;  --tx2:#aeb7c7;  --tx3:#848fa1;
  --chip:rgba(255,255,255,.08);  --ring:rgba(255,255,255,.15);
  --ok:#3fd09b;   --ok-soft:rgba(63,208,155,.15);
  --warn:#f0bb69; --warn-soft:rgba(240,187,105,.15);
  --bad:#ff7a70;  --bad-soft:rgba(255,122,112,.15);
  /* 暗色玻璃不是「白边压暗」，是只留上沿一线冷光 + 四周极淡的折射带 */
  --sh1:
    inset 0 1px 0 rgba(255,255,255,.34),
    inset 0 -1px 0 rgba(255,255,255,.10),
    inset 1px 0 0 rgba(255,255,255,.12),
    inset -1px 0 0 rgba(255,255,255,.12),
    inset 0 0 28px rgba(150,190,255,.06),
    0 0 0 .5px rgba(0,0,0,.55),
    0 2px 4px rgba(0,0,0,.4),
    0 22px 48px -26px rgba(0,0,0,.9);
  --sh2:
    inset 0 1px 0 rgba(255,255,255,.32),
    inset 0 -1px 0 rgba(255,255,255,.1),
    inset 1px 0 0 rgba(255,255,255,.16),
    inset -1px 0 0 rgba(255,255,255,.16),
    inset 0 0 34px rgba(170,205,255,.09),
    0 3px 6px rgba(0,0,0,.45),
    0 30px 60px -26px rgba(0,0,0,.95);
}
/* 画布：低频壁纸。左上打光、右下冷沉降、底部一层青，色相只走冷-紫这一小段；
   高饱和小色斑在玻璃底下会变成脏点，这一版刻意压掉。
   最上面那层是 --wall（白纱/暗罩 + 图），没有壁纸时是 none，下面纯粹兜底。 */
:root[data-skin="g3"] body{
  background-image:
    var(--wall, none),
    radial-gradient(560px 460px at 26% 18%, rgba(150,185,240,.30), transparent 64%),
    radial-gradient(600px 500px at 74% 30%, rgba(186,166,236,.24), transparent 66%),
    radial-gradient(660px 520px at 40% 82%, rgba(140,205,196,.22), transparent 66%),
    radial-gradient(520px 420px at 88% 74%, rgba(198,176,232,.20), transparent 68%),
    radial-gradient(1100px 720px at 12% -14%, rgba(255,255,255,.92), transparent 62%),
    linear-gradient(152deg,#e6ebf5 0%,#dde3f0 52%,#e3deee 100%);
  background-size:cover;
  background-position:center;
  background-attachment:fixed;
}
:root[data-skin="g3"] body::before{
  background:
    radial-gradient(1300px 560px at 18% -12%, rgba(255,255,255,.82), transparent 72%),
    radial-gradient(900px 620px at 102% 104%, rgba(110,140,215,.24), transparent 72%);
}
:root[data-skin="g3"][data-theme="dark"] body{
  background-image:
    var(--wall, none),
    radial-gradient(560px 460px at 26% 18%, rgba(38,84,168,.42), transparent 64%),
    radial-gradient(600px 500px at 74% 30%, rgba(89,58,150,.36), transparent 66%),
    radial-gradient(660px 520px at 40% 82%, rgba(24,104,100,.34), transparent 66%),
    radial-gradient(520px 420px at 88% 74%, rgba(96,54,110,.30), transparent 68%),
    radial-gradient(1100px 720px at 12% -12%, #16305a, transparent 60%),
    linear-gradient(152deg,#0b1220 0%,#090e17 54%,#0b0f18 100%);
  background-size:cover;
  background-position:center;
  background-attachment:fixed;
}
:root[data-skin="g3"][data-theme="dark"] body::before{
  background:
    radial-gradient(1200px 520px at 16% -14%, rgba(120,170,255,.18), transparent 72%),
    radial-gradient(900px 600px at 100% 100%, rgba(150,120,255,.14), transparent 72%);
}
/* 卡片：三层背景 —— 极淡的颗粒（玻璃的微观质感）+ 顶部堆光 + 斜向薄膜。
   模糊别只给 blur：saturate 1.9 才让背后的冷色真的渗上来，brightness 补回
   模糊吃掉的那点亮度。 */
:root[data-skin="g3"] .card,
:root[data-skin="g3"] section[data-card]{
  position:relative;
  /* 关键：--card 是 .42 的白，.card 的 background 简写会把它铺在渐变**下面**，
     等于白膜叠白膜 —— 这正是上一版怎么调都像白卡的原因。这里把它摘掉。 */
  background-color:rgba(255,255,255,.14);
  background-image:
    url("data:image/svg+xml;utf8,<svg xmlns='http://www.w3.org/2000/svg' width='140' height='140'><filter id='n'><feTurbulence type='fractalNoise' baseFrequency='.85' numOctaves='3'/><feColorMatrix type='saturate' values='0'/></filter><rect width='140' height='140' filter='url(%23n)' opacity='.03'/></svg>"),
    linear-gradient(118deg, rgba(255,255,255,.22) 0%, rgba(255,255,255,.03) 20%, transparent 38%),
    linear-gradient(180deg, rgba(255,255,255,.22), rgba(255,255,255,0) 46%),
    linear-gradient(156deg, rgba(255,255,255,.28), rgba(255,255,255,.12) 54%, rgba(255,255,255,.20));
  background-size:140px 140px, auto, auto;
  background-repeat:repeat, no-repeat, no-repeat;
  -webkit-backdrop-filter:blur(28px) saturate(2.2) contrast(1.05) brightness(1.04);
  backdrop-filter:blur(28px) saturate(2.2) contrast(1.05) brightness(1.04);
  border-color:rgba(255,255,255,.62);
}
/* 折射带：玻璃有厚度，光在边框内侧被"聚"起来 —— 表现为一圈比中间更冷、
   更饱和的窄带。少了它，卡片看着像一张平贴的膜。::before 空着，给它用。 */
:root[data-skin="g3"] .card::before,
:root[data-skin="g3"] section[data-card]::before{
  content:""; position:absolute; inset:0; border-radius:inherit; pointer-events:none;
  box-shadow:inset 0 0 14px 1px rgba(108,140,200,.14),
             inset 0 0 0 1px rgba(255,255,255,.16);
}
:root[data-skin="g3"][data-theme="dark"] .card::before,
:root[data-skin="g3"][data-theme="dark"] section[data-card]::before{
  box-shadow:inset 0 0 18px 1px rgba(120,170,255,.10),
             inset 0 0 0 1px rgba(255,255,255,.08);
}
/* 折射环：锥形渐变 + content-box 挖空 = 1.4px 的弧面反光。
   少了这一层，四边 inset 只是「亮边」；有了它，光才像绕着实体的弧走。 */
:root[data-skin="g3"] .card::after,
:root[data-skin="g3"] section[data-card]::after{
  content:""; position:absolute; inset:0; border-radius:inherit; pointer-events:none;
  padding:1.4px;
  background:conic-gradient(from 212deg at 50% 26%,
    rgba(255,255,255,.98) 0deg, rgba(120,150,200,0) 40deg, rgba(255,255,255,.62) 96deg,
    rgba(20,40,80,.06) 150deg, rgba(255,255,255,.16) 200deg, rgba(255,255,255,.90) 274deg,
    rgba(255,255,255,.98) 360deg);
  -webkit-mask:linear-gradient(#fff 0 0) content-box, linear-gradient(#fff 0 0);
  -webkit-mask-composite:xor;
  mask:linear-gradient(#fff 0 0) content-box, linear-gradient(#fff 0 0);
  mask-composite:exclude;
}
:root[data-skin="g3"][data-theme="dark"] .card,
:root[data-skin="g3"][data-theme="dark"] section[data-card]{
  background-color:rgba(8,12,20,.26);
  background-image:
    url("data:image/svg+xml;utf8,<svg xmlns='http://www.w3.org/2000/svg' width='140' height='140'><filter id='n'><feTurbulence type='fractalNoise' baseFrequency='.85' numOctaves='3'/><feColorMatrix type='saturate' values='0'/></filter><rect width='140' height='140' filter='url(%23n)' opacity='.06'/></svg>"),
    linear-gradient(118deg, rgba(255,255,255,.07) 0%, rgba(255,255,255,.01) 20%, transparent 38%),
    linear-gradient(180deg, rgba(255,255,255,.09), rgba(255,255,255,0) 44%),
    linear-gradient(156deg, rgba(34,43,60,.52), rgba(20,26,38,.40) 54%, rgba(28,36,50,.46));
  border-color:rgba(255,255,255,.14);
}
:root[data-skin="g3"][data-theme="dark"] .card::after,
:root[data-skin="g3"][data-theme="dark"] section[data-card]::after{
  background:conic-gradient(from 212deg at 50% 26%,
    rgba(255,255,255,.5) 0deg, rgba(255,255,255,.04) 44deg, rgba(255,255,255,.26) 96deg,
    rgba(255,255,255,.02) 152deg, rgba(255,255,255,.10) 202deg, rgba(255,255,255,.4) 274deg,
    rgba(255,255,255,.5) 360deg);
}
/* 小件（胶囊 / 图标钮 / 次级按钮）按 Apple 的 regular glass 处理：比大面更实，
   否则玻璃太清、上面的字读不出来。 */
:root[data-skin="g3"] .pill,
:root[data-skin="g3"] .icon,
:root[data-skin="g3"] .btn-line,
:root[data-skin="g3"] .ev-panel{
  background:linear-gradient(160deg, rgba(255,255,255,.66), rgba(255,255,255,.44));
  -webkit-backdrop-filter:blur(18px) saturate(1.7);
  backdrop-filter:blur(18px) saturate(1.7);
  box-shadow:inset 0 1px 0 rgba(255,255,255,.92), inset 0 0 0 1px rgba(255,255,255,.46),
             0 2px 8px -5px rgba(12,24,48,.3);
}
/* 抽屉要长时间读：透太多字打架，给到 .9 再加大模糊，边缘仍透一点 */
:root[data-skin="g3"] .drawer{
  -webkit-backdrop-filter:blur(34px) saturate(1.8);
  backdrop-filter:blur(34px) saturate(1.8);
  background:rgba(250,252,255,.90);
  box-shadow:inset 0 1px 0 rgba(255,255,255,.95), -30px 0 60px -30px rgba(10,20,40,.4);
}
:root[data-skin="g3"][data-theme="dark"] .drawer{
  background:rgba(18,24,34,.88);
  box-shadow:inset 0 1px 0 rgba(255,255,255,.18), -30px 0 60px -30px rgba(0,0,0,.9);
}
/* 暗色下这几个玻璃小件必须反过来：浅色那版给的是白 66%，
   照搬到暗色上就是一块白板、图标直接看不见（第一版栽在这） */
:root[data-skin="g3"][data-theme="dark"] .pill,
:root[data-skin="g3"][data-theme="dark"] .icon,
:root[data-skin="g3"][data-theme="dark"] .btn-line,
:root[data-skin="g3"][data-theme="dark"] .ev-panel{
  background:linear-gradient(160deg, rgba(255,255,255,.14), rgba(255,255,255,.07));
  box-shadow:inset 0 1px 0 rgba(255,255,255,.2), inset 0 0 0 1px rgba(255,255,255,.08),
             0 3px 12px -8px rgba(0,0,0,.9);
}
@media (max-width:900px){
  :root[data-skin="g3"] .card,
  :root[data-skin="g3"] section[data-card]{
    -webkit-backdrop-filter:blur(14px) saturate(1.6);
    backdrop-filter:blur(14px) saturate(1.6);
    background-image:
      linear-gradient(180deg, rgba(255,255,255,.34), rgba(255,255,255,0) 46%),
      linear-gradient(156deg, rgba(255,255,255,.82), rgba(255,255,255,.68));
  }
  :root[data-skin="g3"][data-theme="dark"] .card,
  :root[data-skin="g3"][data-theme="dark"] section[data-card]{
    background-image:
      linear-gradient(180deg, rgba(255,255,255,.08), rgba(255,255,255,0) 44%),
      linear-gradient(156deg, rgba(30,38,52,.92), rgba(22,28,40,.88));
  }
  :root[data-skin="g3"] .card::after,
  :root[data-skin="g3"] section[data-card]::after{display:none}
}
/* 字重加重是这一代的排版特征：同样的字号，看着更「实」 */
:root[data-skin="g3"] .card-h,
:root[data-skin="g3"] section[data-card]>.card-h{font-weight:720}
:root[data-skin="g3"] .brand-tx b{font-weight:740;letter-spacing:-.012em}
:root[data-skin="g3"] .seg button.on{box-shadow:inset 0 1px 0 rgba(255,255,255,.92), 0 1px 2px rgba(12,24,48,.1)}
/* 顶栏也是一块玻璃：贴顶，只留一道发丝分界 + 上沿镜面 */
:root[data-skin="g3"] .top{
  background:linear-gradient(180deg, rgba(255,255,255,.56), rgba(255,255,255,.30));
  border-bottom:1px solid rgba(255,255,255,.5);
  box-shadow:inset 0 1px 0 rgba(255,255,255,.9), 0 8px 24px -20px rgba(10,20,40,.5);
  -webkit-backdrop-filter:blur(28px) saturate(1.9);backdrop-filter:blur(28px) saturate(1.9);
}
:root[data-skin="g3"][data-theme="dark"] .top{
  background:linear-gradient(180deg, rgba(30,38,54,.62), rgba(18,24,34,.46));
  border-bottom-color:rgba(255,255,255,.12);
  box-shadow:inset 0 1px 0 rgba(255,255,255,.16), 0 10px 28px -22px rgba(0,0,0,.9);
}
:root[data-skin="g3"] .logo{
  box-shadow:inset 0 1px 0 rgba(255,255,255,.8), 0 8px 20px -10px color-mix(in srgb,var(--pri) 70%,transparent);
}
"""
    return out


def _wall_block(cfg: dict) -> str:
    """背景壁纸：只生成 --wall 这一批变量，具体怎么铺由各皮肤的 body 规则决定。

    为什么不在这里直接写 body{background-image:...}：
    四个皮肤对「壁纸该压在哪一层」的要求不一样 —— 拟物的亚麻皮纹必须盖在图上，
    玻璃的渐变要垫在图下面。所以统一只发一个 --wall（值里已经含「白纱/暗罩 + 图」，
    用逗号连成一个列表，展开后正好是两层），谁用谁决定插在第几层。

    --wall 的兜底是 none，跟 background-image 的 none 关键字同义：该层不画，
    下面那串渐变照常出现。所以「没选壁纸」和「文件丢了」两种情况都不会花屏。
    """
    a = load(cfg)
    head = ("/* ===================== 背景 =====================\n"
            "   强调色是全局一个值（四套皮肤天然共用）；背景反过来 ——\n"
            "   auto 时各皮肤用自己的默认底，选了某一张则是四套共用那一张。 */\n"
            ":root{--wall:none}\n"
            ":root[data-theme=\"dark\"]{--wall:none}\n")
    # custom 那张是替换式的（同一个文件名），浏览器会按 max-age 缓存一天，
    # 换了图还是旧图。所以给它带上 mtime：改一次变一次 URL，缓存自然失效。
    ver = wall_ver(cfg, "custom")
    light = VEIL_LIGHT + ', url("/__wall/{k}.jpg{v}")'
    dark = VEIL_DARK + ', url("/__wall/{k}.jpg{v}")'

    def L(k):
        return light.format(k=k, v=ver if k == "custom" else "")

    def D(k):
        return dark.format(k=k, v=ver if k == "custom" else "")

    lines = []
    if a["bg"] == "auto":
        for skin, key in SKIN_DEFAULT_BG.items():
            if not key or not has_wall(cfg, key):
                continue
            # 浅色一条、暗色紧跟一条：同特异度，后写的胜 —— 顺序不能调
            lines.append(f':root[data-skin="{skin}"]{{--wall:{L(key)}}}')
            lines.append(f':root[data-skin="{skin}"][data-theme="dark"]{{--wall:{D(key)}}}')
    elif has_wall(cfg, a["bg"]):
        k = a["bg"]
        lines.append(f':root{{--wall:{L(k)}}}')
        lines.append(f':root[data-theme="dark"]{{--wall:{D(k)}}}')
    # 老皮肤原本只有一句 background:var(--bg)（纯色）。有壁纸时往它上面加一层图，
    # 底色仍然留给 --bg，没选壁纸就是 var(--wall)=none，等于什么都没变。
    lines.append(
        ':root[data-skin="classic"] body{\n'
        '  background-image:var(--wall, none);\n'
        '  background-color:var(--bg);\n'
        '  background-size:cover;background-position:center;background-attachment:fixed;\n'
        '}')
    return head + "\n".join(lines) + "\n"


def _accent_block(accent_light: str, accent_dark: str) -> str:
    """强调色只作用于三代新皮肤（classic 保留原靛紫，用户明确这么要求的）。

    --pri-soft / --pri-line 用 color-mix 从 --pri 和 --card 现算，
    这样同一份强调色在四套皮肤、明暗两态下都能自动贴合底色，
    不用为每种组合手写一遍。"""
    return f"""
/* ===================== 强调色（只作用于 g1/g2/g3） ===================== */
:root:not([data-skin="classic"]):not([data-theme="dark"]){{
  --pri:{accent_light};
  --pri-hi:color-mix(in srgb, var(--pri) 62%, #fff);
  --pri-solid:var(--pri);
  --pri-soft:color-mix(in srgb, var(--pri) 11%, var(--card));
  --pri-line:color-mix(in srgb, var(--pri) 30%, var(--card));
  --sh-pri:0 6px 18px -8px color-mix(in srgb, var(--pri) 55%, transparent);
}}
:root:not([data-skin="classic"])[data-theme="dark"]{{
  --pri:{accent_dark};
  --pri-hi:color-mix(in srgb, var(--pri) 58%, #fff);
  --pri-solid:color-mix(in srgb, var(--pri) 82%, #000);
  --pri-soft:color-mix(in srgb, var(--pri) 22%, var(--card));
  --pri-line:color-mix(in srgb, var(--pri) 42%, var(--card));
  --sh-pri:0 6px 20px -8px color-mix(in srgb, var(--pri) 65%, transparent);
}}
"""


def css(cfg: dict) -> str:
    a = load(cfg)
    eff = effective_bg(cfg)
    head = (
        "/* 由 pocket/appearance.py 生成 —— 别手改，改 json 或设置面板。\n"
        f"   皮肤 {a['skin']} · 强调色 {a['accent']} · 背景 {a['bg']}"
        f"{'（' + eff + '）' if eff else ''} · "
        f"生成于 {__import__('datetime').datetime.now():%Y-%m-%d %H:%M:%S} */\n"
    )
    body = "".join([_skin_g1(), _skin_g2(), _skin_g3()])
    return (head + body + _wall_block(cfg)
            + _accent_block(_tune(a["accent"], False), _tune(a["accent"], True)))


def sync(cfg: dict) -> str:
    """把生成结果写进 _工作台/appearance.css，返回内容。

    谁需要谁调：设置面板保存时调（立刻生效）、工作台渲染时调（json 被手改也能跟上）。
    pgkit 侧只读文件，不调这个 —— 那边不该认识生成逻辑。
    """
    text = css(cfg)
    p = css_path(cfg)
    try:
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text, encoding="utf-8")
    except OSError:
        pass
    return text


def bootstrap(cfg: dict, href: str = "__appearance.css") -> str:
    """插在 </head> 前的两行：样式 + 把皮肤/强调色挂到 <html> 上。

    必须在样式之前跑完，否则暗色和玻璃底会闪一下（跟 dash.html 里那段
    主题脚本同一个理由）。所以是内联同步脚本，不是 fetch。"""
    a = load(cfg)
    # 只挂 data-skin：--pri 交给上面那段按 [data-theme] 写的规则，
    # 别写内联样式 —— 内联优先级最高，用户运行时切成深色会卡在浅色那个值上。
    return (
        f'<link rel="stylesheet" href="{href}">\n'
        "<script>(function(){document.documentElement.dataset.skin="
        + json.dumps(a["skin"]) + ";})();</script>\n"
    )
