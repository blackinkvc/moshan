#!/usr/bin/env python3
"""第三方插件：把一个现成的开源程序接进中台。

中台自己的服务（看图 / 阅读 / 影音 / 巡检…）走 plugins.json 那套：有端口、有
我们自己的 .command 启动脚本。但用户真正想加的，常常是**别人写的程序** ——
LocalSend 传文件、IINA 放片、Syncthing 同步、Rectangle 摆窗口。这类东西没有
我们的启动脚本，有的是 .app、brew 包、或者自带的网页界面。

所以这里单放一份「配方」(recipe)：一段描述
    · 怎么认它装没装     app 路径 / brew 包 / which
    · 怎么认它在不在跑   进程（最准）/ 本地端口（兜底）
    · 怎么装            brew install [--cask] xxx
    · 怎么开、怎么退     open -a / 命令行 / osascript quit
加一个第三方程序 = 挑一条配方（或者粘一个 .app 路径自动生成配方），其余交给
这里。中台那边只看到一颗普通的插件药丸，跟内置插件长一样。

数据：<数据根>/_工作台/第三方插件.json（只存用户自己加的配方；内置目录在本文件里）。
日志：~/Library/Logs/pocket-ext-<id>.log
"""
from __future__ import annotations

import hashlib
import json
import os
import plistlib
import re
import shutil
import socket
import subprocess
import threading
import time
from datetime import datetime
from pathlib import Path

RECIPE_FILE = "第三方插件.json"
APP_DIRS = ["/Applications", "/System/Applications", "/Applications/Utilities",
            "/System/Applications/Utilities", str(Path.home() / "Applications")]

_LOCK = threading.Lock()
_TASK_LOCK = threading.Lock()
_TASKS: dict[str, dict] = {}          # rid -> {state, lines, started, ended, cmd}
_LOG_TAIL = 40


# --------------------------------------------------------------------- 目录
# 内置配方：只收「确定存在、装法确定」的开源程序。加新的一条不用改别的代码，
# 前端那张「第三方插件」表会自动多一行。
#
#   kind    app=桌面程序（.app） / cli=命令行程序
#   app     默认安装位置（brew cask 装完都在 /Applications）
#   bundle  bundle id，用来 open -b 和「按 app 名退出」
#   proc    pgrep -f 的匹配串：判「在不在跑」，比端口可靠
#   port    本地端口号（有就顺带探一下，给「服务清单」那种视角）
#   web     自带网页界面的地址（Syncthing 这种）。有值＝药丸点开会在浏览器里开它
#   brew    brew 包名；brew_kind=cask|formula
#   start   没有 .app 时的启动命令（argv list）
CATALOG: list[dict] = [
    {
        "id": "localsend", "name": "LocalSend", "cat": "传输", "glyph": "ext",
        "note": "同一 Wi-Fi 下跨设备传文件",
        "desc": "手机、电脑连同一个局域网就能互传文件／照片，不经过服务器、不压缩画质。",
        "app": "/Applications/LocalSend.app", "bundle": "org.localsend.localsend_app",
        "exe": "LocalSend", "proc": "/LocalSend.app/Contents/MacOS/LocalSend", "port": 53317,
        "brew": "localsend", "brew_kind": "cask",
        "home": "https://localsend.org", "repo": "https://github.com/localsend/localsend",
    },
    {
        "id": "syncthing", "name": "Syncthing", "cat": "同步", "glyph": "ext",
        "note": "多台机器之间同步文件夹",
        "desc": "点对点同步，不经过云盘。自带网页管理界面，加完点开就能配。",
        "cli": "syncthing", "exe": "syncthing", "proc": "syncthing serve", "port": 8384,
        "web": "http://127.0.0.1:8384/",
        "start": ["syncthing", "serve", "--no-browser", "--no-restart"],
        "brew": "syncthing", "brew_kind": "formula",
        "home": "https://syncthing.net", "repo": "https://github.com/syncthing/syncthing",
    },
    {
        "id": "iina", "name": "IINA", "cat": "播放", "glyph": "ext",
        "note": "macOS 上的开源播放器",
        "desc": "能放的格式比 QuickTime 多得多，支持画中画和触控板手势。",
        "app": "/Applications/IINA.app", "bundle": "com.colliderli.iina",
        "exe": "IINA", "proc": "/IINA.app/Contents/MacOS/IINA",
        "brew": "iina", "brew_kind": "cask",
        "home": "https://iina.io", "repo": "https://github.com/iina/iina",
    },
    {
        "id": "rectangle", "name": "Rectangle", "cat": "效率", "glyph": "ext",
        "note": "用快捷键摆窗口",
        "desc": "开源窗口管理：⌃⌥ + 方向键把窗口摆到左半屏／右半屏／最大化。",
        "app": "/Applications/Rectangle.app", "bundle": "com.knollsoft.Rectangle",
        "exe": "Rectangle", "proc": "/Rectangle.app/Contents/MacOS/Rectangle",
        "brew": "rectangle", "brew_kind": "cask",
        "home": "https://rectangleapp.com", "repo": "https://github.com/rxhanson/Rectangle",
    },
    {
        "id": "ollama", "name": "Ollama", "cat": "模型", "glyph": "ext",
        "note": "本机跑开源大模型",
        "desc": "一条命令把开源模型拉下来在本机跑，中台里的本地模型就走它。",
        "cli": "ollama", "exe": "ollama", "proc": "ollama serve", "port": 11434,
        "start": ["ollama", "serve"],
        "brew": "ollama", "brew_kind": "formula",
        "home": "https://ollama.com", "repo": "https://github.com/ollama/ollama",
    },

    # ---- 传输 ----------------------------------------------------------
    {
        "id": "cyberduck", "name": "Cyberduck", "cat": "传输", "glyph": "ext",
        "note": "连服务器 / 云盘",
        "desc": "SFTP、FTP、WebDAV、S3 和常见网盘都能挂，连服务器像逛 Finder。",
        "app": "/Applications/Cyberduck.app",
        "brew": "cyberduck", "brew_kind": "cask",
        "home": "https://cyberduck.io",
    },
    {
        "id": "transmission", "name": "Transmission", "cat": "传输", "glyph": "ext",
        "note": "BT 下载",
        "desc": "轻量 BT 客户端，没广告；下片子、下镜像都稳。",
        "app": "/Applications/Transmission.app",
        "brew": "transmission", "brew_kind": "cask",
        "home": "https://transmissionbt.com",
    },

    # ---- 播放 / 音视频 -------------------------------------------------
    {
        "id": "vlc", "name": "VLC", "cat": "播放", "glyph": "media",
        "note": "万能播放器",
        "desc": "几乎什么格式都能放，没下完的文件、坏掉的文件也能播。",
        "app": "/Applications/VLC.app",
        "brew": "vlc", "brew_kind": "cask",
        "home": "https://www.videolan.org/vlc/",
    },
    {
        "id": "jellyfin-media-player", "name": "Jellyfin", "cat": "播放", "glyph": "media",
        "note": "连家里的 Jellyfin",
        "desc": "官方桌面客户端：连家里盒子的 Jellyfin，原盘直放、不转码。",
        "app": "/Applications/Jellyfin Media Player.app",
        "brew": "jellyfin-media-player", "brew_kind": "cask",
        "home": "https://jellyfin.org",
    },
    {
        "id": "handbrake", "name": "HandBrake", "cat": "音视频", "glyph": "media",
        "note": "视频转码压体积",
        "desc": "把大视频压小、转格式，预设直接选 Apple 设备或网页用。",
        "app": "/Applications/HandBrake.app",
        "brew": "handbrake", "brew_kind": "cask",
        "home": "https://handbrake.fr",
    },
    {
        "id": "obs", "name": "OBS", "cat": "音视频", "glyph": "media",
        "note": "录屏与直播",
        "desc": "录屏幕、录摄像头、多路推流，教程和直播都用它。",
        "app": "/Applications/OBS.app",
        "brew": "obs", "brew_kind": "cask",
        "home": "https://obsproject.com",
    },

    # ---- 图像 ----------------------------------------------------------
    {
        "id": "krita", "name": "Krita", "cat": "图像", "glyph": "gallery",
        "note": "开源绘画",
        "desc": "笔刷、图层、动画都齐的绘画软件，画插画和分镜够用。",
        "app": "/Applications/krita.app",
        "brew": "krita", "brew_kind": "cask",
        "home": "https://krita.org",
    },
    {
        "id": "gimp", "name": "GIMP", "cat": "图像", "glyph": "gallery",
        "note": "开源修图",
        "desc": "接近 Photoshop 的那一套：图层、蒙版、滤镜，修图和抠图都能干。",
        "app": "/Applications/GIMP.app",
        "brew": "gimp", "brew_kind": "cask",
        "home": "https://www.gimp.org",
    },
    {
        "id": "inkscape", "name": "Inkscape", "cat": "图像", "glyph": "gallery",
        "note": "矢量图 / SVG",
        "desc": "画 SVG、图标、示意图，输出能直接塞进网页。",
        "app": "/Applications/Inkscape.app",
        "brew": "inkscape", "brew_kind": "cask",
        "home": "https://inkscape.org",
    },

    # ---- 效率 ----------------------------------------------------------
    {
        "id": "alt-tab", "name": "AltTab", "cat": "效率", "glyph": "devices",
        "note": "按窗口切 App",
        "desc": "⌥+Tab 按窗口切换，而不是按 App；多窗口的活儿快很多。",
        "app": "/Applications/AltTab.app",
        "brew": "alt-tab", "brew_kind": "cask",
        "home": "https://alt-tab.app",
    },
    {
        "id": "maccy", "name": "Maccy", "cat": "效率", "glyph": "devices",
        "note": "剪贴板历史",
        "desc": "⇧⌘C 翻之前复制过的东西，找刚复制的那段文字不用重来。",
        "app": "/Applications/Maccy.app",
        "brew": "maccy", "brew_kind": "cask",
        "home": "https://maccy.app",
    },
    {
        "id": "itsycal", "name": "Itsycal", "cat": "效率", "glyph": "devices",
        "note": "菜单栏日历",
        "desc": "菜单栏显示日期，点开看日程，点某天直接开系统日历。",
        "app": "/Applications/Itsycal.app",
        "brew": "itsycal", "brew_kind": "cask",
        "home": "https://www.mowglii.com/itsycal/",
    },
    {
        "id": "hiddenbar", "name": "Hidden Bar", "cat": "效率", "glyph": "devices",
        "note": "收起菜单栏图标",
        "desc": "把不常用的菜单栏图标折起来，只留想看的。",
        "app": "/Applications/Hidden Bar.app",
        "brew": "hiddenbar", "brew_kind": "cask",
        "home": "https://github.com/dwarvesf/hidden",
    },
    {
        "id": "keepingyouawake", "name": "KeepingYouAwake", "cat": "效率", "glyph": "devices",
        "note": "一键防休眠",
        "desc": "菜单栏一个小咖啡杯，点了就不休眠；跑长任务、下大文件时用。",
        "app": "/Applications/KeepingYouAwake.app",
        "brew": "keepingyouawake", "brew_kind": "cask",
        "home": "https://keepingyouawake.app",
    },
    {
        "id": "espanso", "name": "Espanso", "cat": "效率", "glyph": "devices",
        "note": "打字自动展开",
        "desc": "打一个缩写就展开成整段文字（地址、签名、常用回复），所有 App 都生效。",
        "app": "/Applications/Espanso.app",
        "brew": "espanso", "brew_kind": "cask",
        "home": "https://espanso.org",
    },

    # ---- 系统 / 工具 ---------------------------------------------------
    {
        "id": "stats", "name": "Stats", "cat": "系统", "glyph": "inspect",
        "note": "菜单栏系统监控",
        "desc": "菜单栏看 CPU、内存、温度、网速、磁盘，比活动监视器顺手。",
        "app": "/Applications/Stats.app",
        "brew": "stats", "brew_kind": "cask",
        "home": "https://github.com/exelban/stats",
    },
    {
        "id": "keka", "name": "Keka", "cat": "系统", "glyph": "ext",
        "note": "压缩解压",
        "desc": "7z、rar、tar、zip 都认，比系统自带的归档实用工具全。",
        "app": "/Applications/Keka.app",
        "brew": "keka", "brew_kind": "cask",
        "home": "https://www.keka.io",
    },
    {
        "id": "imageoptim", "name": "ImageOptim", "cat": "系统", "glyph": "gallery",
        "note": "图片瘦身",
        "desc": "把图拖进去就压小，肉眼几乎看不出差别；发网页、发文章前用。",
        "app": "/Applications/ImageOptim.app",
        "brew": "imageoptim", "brew_kind": "cask",
        "home": "https://imageoptim.com/mac",
    },
    {
        "id": "balenaetcher", "name": "Etcher", "cat": "系统", "glyph": "ext",
        "note": "镜像写 U 盘",
        "desc": "把系统镜像写到 U 盘 / SD 卡，给盒子、树莓派刷机用。",
        "app": "/Applications/balenaEtcher.app",
        "brew": "balenaetcher", "brew_kind": "cask",
        "home": "https://etcher.balena.io",
    },
    {
        "id": "keepassxc", "name": "KeePassXC", "cat": "系统", "glyph": "ext",
        "note": "本地密码库",
        "desc": "所有密码存一个加密文件，换机器拷走就能用，不依赖云。",
        "app": "/Applications/KeePassXC.app",
        "brew": "keepassxc", "brew_kind": "cask",
        "home": "https://keepassxc.org",
    },

    # ---- 开发 ----------------------------------------------------------
    {
        "id": "iterm2", "name": "iTerm2", "cat": "开发", "glyph": "github",
        "note": "增强终端",
        "desc": "系统终端的加强版：分屏、搜索、粘贴历史、触发器。",
        "app": "/Applications/iTerm.app",
        "brew": "iterm2", "brew_kind": "cask",
        "home": "https://iterm2.com",
    },
    {
        "id": "vscode", "name": "VS Code", "cat": "开发", "glyph": "github",
        "note": "写代码",
        "desc": "编辑器里插件生态最大的一个，远程开发、Jupyter 都能接。",
        "app": "/Applications/Visual Studio Code.app",
        "brew": "visual-studio-code", "brew_kind": "cask",
        "home": "https://code.visualstudio.com",
    },
    {
        "id": "zed", "name": "Zed", "cat": "开发", "glyph": "github",
        "note": "新编辑器",
        "desc": "Rust 写的新编辑器，开大文件和搜索都很快，内置协作。",
        "app": "/Applications/Zed.app",
        "brew": "zed", "brew_kind": "cask",
        "home": "https://zed.dev",
    },
    {
        "id": "ghostty", "name": "Ghostty", "cat": "开发", "glyph": "github",
        "note": "GPU 终端",
        "desc": "GPU 渲染的终端，启动快、配置简单，原生支持分屏。",
        "app": "/Applications/Ghostty.app",
        "brew": "ghostty", "brew_kind": "cask",
        "home": "https://ghostty.org",
    },
    {
        "id": "bruno", "name": "Bruno", "cat": "开发", "glyph": "github",
        "note": "API 调试",
        "desc": "调接口的客户端，集合存本地文件夹、能进 git，不往云上传。",
        "app": "/Applications/Bruno.app",
        "brew": "bruno", "brew_kind": "cask",
        "home": "https://www.usebruno.com",
    },
    {
        "id": "dbeaver", "name": "DBeaver", "cat": "开发", "glyph": "github",
        "note": "数据库客户端",
        "desc": "连 MySQL、Postgres、SQLite 的图形客户端，SQL 补全和导出都顺手。",
        "app": "/Applications/DBeaver.app",
        "brew": "dbeaver-community", "brew_kind": "cask",
        "home": "https://dbeaver.io",
    },

    # ---- 模型 ----------------------------------------------------------
    {
        "id": "jan", "name": "Jan", "cat": "模型", "glyph": "llmcost",
        "note": "本地模型聊天",
        "desc": "在桌面端跑本地模型的聊天客户端，断网也能用。",
        "app": "/Applications/Jan.app",
        "brew": "jan", "brew_kind": "cask",
        "home": "https://jan.ai",
    },
    {
        "id": "chatbox", "name": "Chatbox", "cat": "模型", "glyph": "llmcost",
        "note": "多家模型客户端",
        "desc": "一个客户端接各家模型的 API，桌面端能存多套对话。",
        "app": "/Applications/Chatbox.app",
        "brew": "chatbox", "brew_kind": "cask",
        "home": "https://chatboxai.app",
    },
]

# 目录展示顺序：按用途分堆，比按添加时间排好找。没列到的分类排在最后。
CAT_ORDER = ["传输", "同步", "播放", "音视频", "图像", "效率", "系统", "开发", "模型"]

# 前端「添加」时只认这几个字段，别把 status 之类的脏东西写进 plugins.json
_ROW_FIELDS = ("id", "name", "note", "glyph", "cat", "web", "port", "brew", "brew_kind")
# 配方能落盘的字段（粘 .app 生成的那份要过一遍，别把前端塞的东西全存下来）
_RECIPE_FIELDS = ("id", "name", "cat", "glyph", "note", "desc", "app", "bundle", "exe", "proc",
                  "cli", "port", "web", "brew", "brew_kind", "start", "home", "repo")
_BUILTIN_IDS = {r["id"] for r in CATALOG}


def _root(cfg: dict) -> Path:
    roots = cfg.get("allowed_roots") or []
    return Path(roots[0]) if roots else Path(cfg.get("inbox_dir", ".")).parent


def recipes_path(cfg: dict) -> Path:
    return _root(cfg) / "_工作台" / RECIPE_FILE


def _load(p: Path, default):
    try:
        return json.loads(p.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return default


def custom_recipes(cfg: dict) -> list[dict]:
    d = _load(recipes_path(cfg), {})
    rows = d.get("recipes") if isinstance(d, dict) else d
    if not isinstance(rows, list):
        return []
    return [r for r in rows if isinstance(r, dict) and r.get("id") and r.get("name")]


def save_recipe(cfg: dict, r: dict) -> dict:
    """存一条用户自己加的配方（.app 粘出来的那种）。同 id 覆盖。"""
    p = recipes_path(cfg)
    rows = custom_recipes(cfg)
    rows = [x for x in rows if x.get("id") != r["id"]] + [r]
    p.parent.mkdir(parents=True, exist_ok=True)
    payload = {"_说明": "用户自己加的第三方插件配方。粘 .app 路径时自动生成，也能手改。",
               "recipes": rows, "updated_at": datetime.now().isoformat(timespec="seconds")}
    tmp = p.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(payload, ensure_ascii=False, indent=1), encoding="utf-8")
    tmp.replace(p)
    return r


def all_recipes(cfg: dict) -> list[dict]:
    """内置目录 + 用户配方（同 id 用户优先，用来改 brew 包名/端口）。"""
    seen, out = set(), []
    for r in custom_recipes(cfg):
        if r["id"] not in seen:
            seen.add(r["id"])
            out.append(r)
    for r in CATALOG:
        if r["id"] not in seen:
            seen.add(r["id"])
            out.append(dict(r))
    return out


def recipe(cfg: dict, rid: str) -> dict | None:
    rid = (rid or "").strip()
    return next((r for r in all_recipes(cfg) if r["id"] == rid), None)


# --------------------------------------------------------------------- 探测
def _path_env() -> str:
    # launchd 起的口袋进程 PATH 很短，brew 不在里面就找不到 —— 显式补两段。
    return os.environ.get("PATH", "") + ":/opt/homebrew/bin:/usr/local/bin"


def which(name: str) -> str:
    if not name:
        return ""
    return shutil.which(name, path=_path_env()) or ""


def find_app(query: str) -> Path | None:
    """把粘进来的东西找成 .app：完整路径 → 各 Applications 目录 → 模糊两层。"""
    q = (query or "").strip().rstrip("/")
    if not q:
        return None
    p = Path(q).expanduser()
    if q.lower().endswith(".app") and p.is_dir():
        return p
    name = p.name[:-4] if p.name.lower().endswith(".app") else p.name
    if not name:
        return None
    for d in APP_DIRS:
        base = Path(d)
        if not base.is_dir():
            continue
        for cand in (base / (name + ".app"), base / name):
            if cand.is_dir():
                return cand
    ql = name.lower()
    hits: list[Path] = []
    for d in APP_DIRS:
        base = Path(d)
        if not base.is_dir():
            continue
        try:
            kids = list(base.glob("*.app"))
        except OSError:
            continue
        for c in kids:
            nl = c.stem.lower()
            if nl == ql:
                return c
            if ql in nl:
                hits.append(c)
    if hits:
        return sorted(hits, key=lambda x: len(x.stem))[0]
    for d in APP_DIRS:
        base = Path(d)
        if not base.is_dir():
            continue
        try:
            for c in base.glob("*/*.app"):
                if ql in c.stem.lower():
                    return c
        except OSError:
            continue
    return None


_INFO_CACHE: dict[str, tuple[float, dict]] = {}
_INFO_LOCK = threading.Lock()


def app_info(app: Path) -> dict:
    """Info.plist 带 mtime 缓存。探活每 1.8 秒跑一轮，别每次都解剖 plist。"""
    try:
        mtime = (app / "Contents" / "Info.plist").stat().st_mtime
    except OSError:
        return {"bundle": "", "exe": app.stem, "version": ""}
    key = str(app)
    with _INFO_LOCK:
        hit = _INFO_CACHE.get(key)
        if hit and hit[0] == mtime:
            return dict(hit[1])
    info = _app_info(app)
    with _INFO_LOCK:
        _INFO_CACHE[key] = (mtime, info)
    return dict(info)


def _app_info(app: Path) -> dict:
    """读 Info.plist：bundle id / 可执行文件名 / 版本。读不到就给空。"""
    info = {"bundle": "", "exe": app.stem, "version": ""}
    pl = app / "Contents" / "Info.plist"
    try:
        with open(pl, "rb") as f:
            d = plistlib.load(f)
    except (OSError, plistlib.InvalidFileException, ValueError):
        return info
    if isinstance(d, dict):
        info["bundle"] = str(d.get("CFBundleIdentifier") or "")
        info["exe"] = str(d.get("CFBundleExecutable") or app.stem)
        info["version"] = str(d.get("CFBundleShortVersionString") or "")
    return info


def _brew_installed(r: dict) -> bool:
    b = r.get("brew")
    if not b:
        return False
    for prefix in ("/opt/homebrew", "/usr/local"):
        sub = "Caskroom" if r.get("brew_kind") == "cask" else "Cellar"
        if Path(prefix, sub, b).is_dir():
            return True
    return False


def installed(r: dict) -> bool:
    """装没装。.app 优先，其次命令行，最后 brew 目录。"""
    app = r.get("app")
    if app and Path(str(app)).expanduser().is_dir():
        return True
    if not app and r.get("name"):
        found = find_app(r["name"])
        if found:
            return True
    if r.get("cli") and which(r["cli"]):
        return True
    return _brew_installed(r)


def _pgrep_exact(name: str) -> bool:
    """按进程名精确匹配（-x）。

    这里**不能**用 pgrep -f：-f 匹配整条命令行，而这条命令行可能就是
    浏览器发给我们的请求体（JSON 里带着 /Applications/X.app/Contents/MacOS/X），
    于是 curl 自己会被认成「X 正在运行」——实测过一次，加完计算器就显示运行中。
    """
    if not name:
        return False
    try:
        rc = subprocess.run(["/usr/bin/pgrep", "-x", "-q", name],
                            capture_output=True, timeout=4).returncode
    except (OSError, subprocess.SubprocessError):
        return False
    return rc == 0


def _exe_name(r: dict) -> str:
    """进程名从哪来：装了的话直接读 .app 的 CFBundleExecutable（最准），
    其次配方里的 exe，最后 proc 的最后一段。

    为什么优先读 plist：VS Code 这种 Electron 程序的可执行文件叫 Electron，
    配方里手写的进程名十有八九是错的；装完读一次 plist 就自动对上。
    """
    app = str(r.get("app") or "").strip()
    if app:
        p = Path(app).expanduser()
        if p.is_dir():
            got = app_info(p)["exe"]
            if got:
                return got
    exe = str(r.get("exe") or "").strip()
    if exe:
        return exe
    pat = str(r.get("proc") or "").strip()
    if not pat:
        return ""
    return Path(pat).name if "/" in pat else pat.split()[0]


def _bundle_id(r: dict) -> str:
    """bundle id：装了就读 plist，没装用配方里的。"""
    app = str(r.get("app") or "").strip()
    if app:
        p = Path(app).expanduser()
        if p.is_dir():
            got = app_info(p)["bundle"]
            if got:
                return got
    return str(r.get("bundle") or "").strip()


def port_open(port) -> bool:
    try:
        port = int(port)
    except (TypeError, ValueError):
        return False
    try:
        with socket.create_connection(("127.0.0.1", port), timeout=0.35):
            return True
    except OSError:
        return False


def running(r: dict) -> bool:
    """在不在跑。进程名最准（端口可能还没 listen / 只走 UDP），端口兜底。"""
    if _pgrep_exact(_exe_name(r)):
        return True
    return port_open(r.get("port"))


def version(r: dict) -> str:
    app = r.get("app")
    if app and Path(str(app)).expanduser().is_dir():
        return _app_info(Path(str(app)).expanduser())["version"]
    found = find_app(r.get("name", ""))
    if found:
        return _app_info(found)["version"]
    return ""


def probe(r: dict) -> dict:
    inst = installed(r)
    run = running(r) if inst else False
    return {"installed": inst, "running": run, "version": version(r) if inst else "",
            "port": r.get("port"), "port_open": port_open(r.get("port")) if inst else False}


def cat_rank(cat: str) -> int:
    try:
        return CAT_ORDER.index(cat)
    except ValueError:
        return len(CAT_ORDER)


def catalog(cfg: dict) -> list[dict]:
    """目录 + 现况。前端那张表就用这个渲染。

    排序：用户自己加的排最前（改 brew 包名那种要一眼看见），其余按用途分堆，
    前端靠 cat 变化画分类小标题 —— 三十多个程序平铺一列太长了。
    """
    rows = all_recipes(cfg)
    rows.sort(key=lambda r: (0 if r["id"] not in _BUILTIN_IDS else 1,
                             cat_rank(str(r.get("cat") or ""))))
    out = []
    for r in rows:
        item = {k: r.get(k) for k in _ROW_FIELDS if r.get(k) not in (None, "")}
        item.update({
            "desc": r.get("desc", ""), "home": r.get("home", ""), "repo": r.get("repo", ""),
            "custom": r["id"] not in _BUILTIN_IDS,
            "state": probe(r),
            "task": task(r["id"]),
        })
        out.append(item)
    return out


def sanitize_recipe(raw: dict) -> dict | None:
    """前端传来的配方草稿 → 只留认识的字段。缺 id/name 或压根没有启动方式的丢掉。"""
    if not isinstance(raw, dict):
        return None
    r = {}
    for k in _RECIPE_FIELDS:
        v = raw.get(k)
        if v in (None, ""):
            continue
        if k == "port":
            try:
                v = int(v)
            except (TypeError, ValueError):
                continue
        if k == "start":
            if not isinstance(v, (list, tuple)) or not v:
                continue
            v = [str(x) for x in v]
        r[k] = v
    if not r.get("id") or not r.get("name"):
        return None
    if not any(r.get(k) for k in ("app", "bundle", "cli", "start", "brew")):
        return None
    r["id"] = _slug(str(r["id"])) or ("ext-" + hashlib.md5(
        str(r.get("app") or r["name"]).encode("utf-8")).hexdigest()[:6])
    r["name"] = str(r["name"])[:24]
    return r


# --------------------------------------------------------------------- 安装
def _log_path(rid: str) -> Path:
    d = Path.home() / "Library" / "Logs"
    try:
        d.mkdir(parents=True, exist_ok=True)
    except OSError:
        d = Path("/tmp")
    return d / f"pocket-ext-{rid}.log"


def task(rid: str) -> dict | None:
    with _TASK_LOCK:
        t = _TASKS.get(rid)
        return dict(t) if t else None


def _set_task(rid: str, **kw) -> None:
    with _TASK_LOCK:
        t = _TASKS.setdefault(rid, {"state": "idle", "lines": [], "started": ""})
        t.update(kw)
        if len(t["lines"]) > _LOG_TAIL:
            t["lines"] = t["lines"][-_LOG_TAIL:]


def _install_cmd(r: dict) -> list[str]:
    brew = which("brew")
    if not brew or not r.get("brew"):
        return []
    cmd = [brew, "install"]
    if r.get("brew_kind") == "cask":
        cmd.append("--cask")
    cmd.append(str(r["brew"]))
    return cmd


def _do_install(rid: str, cmd: list[str]) -> None:
    log = _log_path(rid)
    rc = -1
    try:
        with open(log, "a", encoding="utf-8") as lf:
            lf.write(f"\n===== {datetime.now().isoformat(timespec='seconds')} {' '.join(cmd)}\n")
            lf.flush()
            proc = subprocess.Popen(cmd, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                    stderr=subprocess.STDOUT, text=True, bufsize=1,
                                    env=dict(os.environ, PATH=_path_env(), HOMEBREW_NO_AUTO_UPDATE="1"))
            assert proc.stdout is not None
            for line in proc.stdout:
                line = line.rstrip("\n")
                lf.write(line + "\n")
                lf.flush()
                with _TASK_LOCK:
                    t = _TASKS.setdefault(rid, {"lines": [], "started": "", "state": "running"})
                    t["lines"] = (t.get("lines") or [])[-_LOG_TAIL:] + [line]
            rc = proc.wait()
    except (OSError, subprocess.SubprocessError) as e:
        _set_task(rid, state="failed", ended=datetime.now().isoformat(timespec="seconds"),
                  error=str(e)[:200])
        return
    _set_task(rid, state="done" if rc == 0 else "failed", rc=rc,
              ended=datetime.now().isoformat(timespec="seconds"),
              log=str(log))


def install(cfg: dict, rid: str) -> dict:
    r = recipe(cfg, rid)
    if not r:
        return {"ok": False, "error": "没有这条配方"}
    if installed(r):
        return {"ok": True, "already": True, "state": probe(r)}
    cmd = _install_cmd(r)
    if not cmd:
        return {"ok": False, "error": f"{r['name']} 没有 brew 配方，得手动装"}
    t = task(rid)
    if t and t.get("state") == "running":
        return {"ok": True, "state": t}
    _set_task(rid, state="running", lines=[], cmd=cmd,
              started=datetime.now().isoformat(timespec="seconds"), ended="", rc=None)
    threading.Thread(target=_do_install, args=(rid, cmd), daemon=True).start()
    return {"ok": True, "state": task(rid)}


# --------------------------------------------------------------------- 开 / 退
def _spawn(cmd: list[str], rid: str) -> None:
    log = _log_path(rid)
    out = open(log, "a", encoding="utf-8")
    subprocess.Popen(cmd, stdin=subprocess.DEVNULL, stdout=out, stderr=subprocess.STDOUT,
                     start_new_session=True, close_fds=True,
                     env=dict(os.environ, PATH=_path_env()))


def start(cfg: dict, rid: str, wait: float = 10.0) -> dict:
    r = recipe(cfg, rid)
    if not r:
        return {"ok": False, "error": "没有这条配方"}
    if not installed(r):
        return {"ok": False, "error": f"{r['name']} 还没装"}
    if running(r):
        return {"ok": True, "started": False, "state": probe(r)}
    try:
        if r.get("start"):
            argv = [which(r["start"][0]) or r["start"][0]] + list(r["start"][1:])
            _spawn(argv, rid)
        elif r.get("app") and Path(str(r["app"])).expanduser().is_dir():
            _spawn(["/usr/bin/open", "-a", str(Path(str(r["app"])).expanduser())], rid)
        elif r.get("bundle"):
            _spawn(["/usr/bin/open", "-b", str(r["bundle"])], rid)
        elif r.get("name"):
            app = find_app(r["name"])
            if not app:
                return {"ok": False, "error": f"找不到 {r['name']}.app"}
            _spawn(["/usr/bin/open", "-a", str(app)], rid)
        else:
            return {"ok": False, "error": "这条配方没写怎么启动"}
    except OSError as e:
        return {"ok": False, "error": str(e)[:200]}
    t0 = time.time()
    while time.time() - t0 < wait:
        if running(r):
            return {"ok": True, "started": True, "seconds": round(time.time() - t0, 1),
                    "state": probe(r)}
        time.sleep(0.4)
    # 桌面程序（无端口、无进程名）起来就是起来了，别硬等成「失败」
    if not r.get("port") and not r.get("proc") and r.get("app"):
        return {"ok": True, "started": True, "state": probe(r)}
    return {"ok": False, "started": True, "state": probe(r),
            "error": f"{r['name']} 起了但没确认到在跑，看日志 ~/Library/Logs/pocket-ext-{rid}.log"}


def stop(cfg: dict, rid: str) -> dict:
    r = recipe(cfg, rid)
    if not r:
        return {"ok": False, "error": "没有这条配方"}
    if not running(r):
        return {"ok": True, "stopped": False, "state": probe(r)}
    told = False
    # 桌面程序优先走 AppleScript 正常退出 —— 直接 kill 会丢窗口状态，也可能不落盘。
    # bundle id 没有就按应用名退（装了的话 plist 里也能读到 id）。
    bundle = _bundle_id(r)
    appname = str(r.get("name") or "").replace('"', "")
    script = ""
    if bundle:
        script = f'tell application id "{bundle}" to quit'
    elif appname and (r.get("app") or r.get("bundle")):
        script = f'tell application "{appname}" to quit'
    if script:
        try:
            told = subprocess.run(["/usr/bin/osascript", "-e", script],
                                  capture_output=True, timeout=8).returncode == 0
        except (OSError, subprocess.SubprocessError):
            told = False
    if not told:
        exe = _exe_name(r)
        pat = str(r.get("proc") or "").strip()
        try:
            if exe:
                subprocess.run(["/usr/bin/pkill", "-x", exe], capture_output=True, timeout=5)
            elif pat:
                subprocess.run(["/usr/bin/pkill", "-f", pat], capture_output=True, timeout=5)
        except (OSError, subprocess.SubprocessError):
            pass
    t0 = time.time()
    while time.time() - t0 < 6:
        if not running(r):
            return {"ok": True, "stopped": True, "state": probe(r)}
        time.sleep(0.4)
    return {"ok": False, "error": f"{r['name']} 还在跑，可能要手动退出"}


# --------------------------------------------------------------------- 生成配方
def _slug(text: str) -> str:
    s = re.sub(r"[^a-z0-9]+", "-", str(text or "").lower()).strip("-")
    return s[:28]


def draft_from_text(text: str) -> dict:
    """粘一个 .app 路径 / 应用名 → 一条能直接用的配方草稿（还不落盘）。"""
    raw = (text or "").strip()
    if not raw:
        return {"ok": False, "error": "先粘一个 .app 路径或者应用名"}
    app = find_app(raw)
    if not app:
        return {"ok": False, "error": f"没找到这个应用：{raw}"}
    info = _app_info(app)
    name = app.stem
    rid = _slug(name)
    if not rid:
        rid = "ext-" + hashlib.md5(str(app).encode("utf-8")).hexdigest()[:6]
    proc = f"/{app.name}/Contents/MacOS/{info['exe']}"
    return {"ok": True, "app": str(app), "recipe": {
        "id": rid, "name": name, "cat": "自加", "glyph": "ext",
        "note": f"{name}（本机程序）", "desc": "",
        "app": str(app), "bundle": info["bundle"], "exe": info["exe"], "proc": proc,
        "brew": "", "brew_kind": "", "version": info["version"],
    }}


# ------------------------------------------------- 对外接口：配方 spec / prompt
# 目录里的程序是「常用的那批」，用不到的不必等我加 —— 用户拿自己的模型 token
# 就能写一条新配方。所以这里把三样东西固定下来：
#   1) 字段说明（spec）      GET  /api/ext/spec
#   2) 提示词（prompt）      GET  /api/ext/prompt      存成 _工作台/第三方插件-Prompt.md，可改
#   3) 解析器（parse）       POST /api/ext/parse       把模型回答里的 JSON 抠出来校验
# 再留一条可选通道：_工作台/第三方插件-模型.json 里填了自己的模型，POST /api/ext/compose
# 就直接调它生成配方；不填就退回「给你提示词、你自己粘给模型」。
PROMPT_FILE = "第三方插件-Prompt.md"
MODEL_FILE = "第三方插件-模型.json"

FIELD_DOC = [
    {"k": "id", "type": "string", "need": "必填", "doc": "小写连字符，唯一，如 ice、localsend"},
    {"k": "name", "type": "string", "need": "必填", "doc": "显示名，≤24 字符，顶栏药丸就用它"},
    {"k": "cat", "type": "string", "need": "可选", "doc": "传输/同步/播放/音视频/图像/效率/系统/开发/模型"},
    {"k": "note", "type": "string", "need": "可选", "doc": "一句话，≤18 字，列表里显示"},
    {"k": "desc", "type": "string", "need": "可选", "doc": "一到两句说明，悬停时显示"},
    {"k": "app", "type": "string", "need": "三选一", "doc": "装好后的 .app 绝对路径，如 /Applications/Keka.app"},
    {"k": "cli", "type": "string", "need": "三选一", "doc": "命令行可执行名（PATH 里找得到，如 syncthing）"},
    {"k": "start", "type": "string[]", "need": "可选", "doc": "命令行程序怎么起，argv 数组，如 [\"syncthing\",\"serve\"]"},
    {"k": "brew", "type": "string", "need": "建议", "doc": "brew 包名；没有就留空，按钮会变成「手动装」"},
    {"k": "brew_kind", "type": "cask | formula", "need": "有 brew 时必填", "doc": "桌面程序 cask，命令行 formula"},
    {"k": "port", "type": "number", "need": "可选", "doc": "本地网页端口，只有自带网页界面的程序才填"},
    {"k": "web", "type": "string", "need": "可选", "doc": "网页地址，通常 http://127.0.0.1:<port>/"},
    {"k": "exe", "type": "string", "need": "可省略", "doc": "进程名；中台会从 .app 的 Info.plist 读，别猜"},
    {"k": "bundle", "type": "string", "need": "可省略", "doc": "bundle id；同上，装了自动读"},
    {"k": "home", "type": "string", "need": "可选", "doc": "官网"},
    {"k": "repo", "type": "string", "need": "可选", "doc": "源码仓库"},
]

EXAMPLES = [
    {"id": "keka", "name": "Keka", "cat": "系统", "note": "压缩解压",
     "desc": "7z、rar、tar、zip 都认。",
     "app": "/Applications/Keka.app", "brew": "keka", "brew_kind": "cask",
     "home": "https://www.keka.io"},
    {"id": "syncthing", "name": "Syncthing", "cat": "同步", "note": "文件夹同步",
     "cli": "syncthing", "start": ["syncthing", "serve", "--no-browser"],
     "brew": "syncthing", "brew_kind": "formula",
     "port": 8384, "web": "http://127.0.0.1:8384/"},
    {"id": "ice", "name": "Ice", "cat": "效率", "note": "菜单栏图标管理",
     "desc": "把菜单栏图标分组、隐藏、调顺序。",
     "app": "/Applications/Ice.app", "brew": "jordanbaird-ice", "brew_kind": "cask",
     "home": "https://icemenubar.app"},
]

ENDPOINTS = [
    {"method": "GET", "path": "/api/ext/list", "doc": "目录 + 现况（装没装 / 跑没跑 / 安装进度）"},
    {"method": "GET", "path": "/api/ext/spec", "doc": "这份说明（JSON）"},
    {"method": "GET", "path": "/api/ext/prompt?goal=", "doc": "提示词纯文本，可直接喂给任意模型"},
    {"method": "POST", "path": "/api/ext/parse", "doc": "body {text, verify}：从模型回答里抠配方，不落盘"},
    {"method": "POST", "path": "/api/ext/add", "doc": "body {recipe} 或 {id}：落配方并加进中台"},
    {"method": "POST", "path": "/api/ext/compose", "doc": "body {goal}：配了模型就直接生成，没配返回提示词"},
    {"method": "POST", "path": "/api/ext/install", "doc": "body {id}：brew 后台安装"},
    {"method": "POST", "path": "/api/ext/start", "doc": "body {id}：启动"},
    {"method": "POST", "path": "/api/ext/stop", "doc": "body {id}：退出"},
]

DEFAULT_PROMPT = """你是 macOS「个人中台」的插件配方助手。中台用一条 JSON 配方管一个第三方程序：
装没装、在不在跑、怎么装、怎么开、怎么退。

## 输出
只输出**一个 JSON 对象**。不要解释、不要 markdown 代码围栏、不要注释。

## 字段
| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| id | string | 是 | 小写连字符，唯一，如 ice |
| name | string | 是 | 显示名，≤24 字 |
| cat | string | 否 | 传输/同步/播放/音视频/图像/效率/系统/开发/模型 |
| note | string | 否 | 一句话，≤18 字 |
| desc | string | 否 | 一到两句说明 |
| app | string | 三选一 | 装好后的 .app 绝对路径，如 /Applications/Keka.app |
| cli | string | 三选一 | 命令行可执行名，如 syncthing |
| start | string[] | 否 | 命令行启动 argv，如 ["syncthing","serve"] |
| brew | string | 建议 | brew 包名 |
| brew_kind | cask / formula | 有 brew 必填 | 桌面程序 cask，命令行 formula |
| port | number | 否 | 本地网页端口，只有自带网页界面才填 |
| web | string | 否 | 通常 http://127.0.0.1:<port>/ |
| home | string | 否 | 官网 |

## 硬约束
1. app / cli / brew 至少给一个，否则这条配方没用。
2. brew 包名必须真实存在：桌面程序是 `brew install --cask <名>`，命令行是 `brew install <名>`；
   不确定就**别写 brew**（按钮会变成「手动装」，用户自己装完照样能用），不要编一个名字。
3. 桌面程序：brew_kind=cask + app=/Applications/<App>.app。
4. 有网页界面的（Syncthing、code-server 这类）才填 port 和 web；端口不确定就不要猜。
5. exe / bundle 不用写 —— 中台会从 .app 的 Info.plist 读。
6. 只有网页、没有桌面程序的，不要写 app。
7. id 全小写连字符；同一个程序不要起两个 id。

## 例子
{"id":"keka","name":"Keka","cat":"系统","note":"压缩解压","app":"/Applications/Keka.app","brew":"keka","brew_kind":"cask"}
{"id":"syncthing","name":"Syncthing","cat":"同步","note":"文件夹同步","cli":"syncthing","start":["syncthing","serve","--no-browser"],"brew":"syncthing","brew_kind":"formula","port":8384,"web":"http://127.0.0.1:8384/"}
{"id":"ice","name":"Ice","cat":"效率","note":"菜单栏图标管理","app":"/Applications/Ice.app","brew":"jordanbaird-ice","brew_kind":"cask"}

## 这台机器
已有 id（别重复）：{{IDS}}
这次要加：{{GOAL}}

现在输出 JSON。
"""


def _data_dir(cfg: dict) -> Path:
    return _root(cfg) / "_工作台"


def prompt_path(cfg: dict) -> Path:
    return _data_dir(cfg) / PROMPT_FILE


def model_path(cfg: dict) -> Path:
    return _data_dir(cfg) / MODEL_FILE


def ensure_prompt(cfg: dict) -> Path:
    """提示词落成文件：用户想改语气 / 换语言 / 加自己的规则，直接改这个 md。"""
    p = prompt_path(cfg)
    if p.exists():
        return p
    try:
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(
            "<!-- 中台「第三方插件」的配方提示词。改完立刻生效（下一轮截图/复制就用新的）。\n"
            "     {{IDS}} 会替换成已有 id，{{GOAL}} 替换成这次要加的东西。 -->\n\n" + DEFAULT_PROMPT,
            encoding="utf-8")
    except OSError:
        pass
    return p


def prompt_text(cfg: dict, goal: str = "") -> str:
    p = ensure_prompt(cfg)
    try:
        base = p.read_text(encoding="utf-8")
    except OSError:
        base = DEFAULT_PROMPT
    base = re.sub(r"<!--.*?-->\s*", "", base, flags=re.S)
    ids = ", ".join(r["id"] for r in all_recipes(cfg))
    goal = (goal or "").strip() or "按你的判断挑一个常用、开源、brew 能装的程序"
    out = base.replace("{{IDS}}", ids).replace("{{GOAL}}", goal)
    if "已有 id" not in out:      # 用户自己改过模板、把占位符删了
        out += f"\n\n已有 id：{ids}\n这次要加：{goal}\n"
    return out


def spec(cfg: dict | None = None, goal: str = "") -> dict:
    """给外部 agent / 页面看的一页说明书。字段说明和 prompt 只有这一份。"""
    d = {
        "v": 1,
        "what": "第三方插件 = 一条 JSON 配方（怎么认装没装 / 怎么装 / 怎么开退），"
                "加进来就变成中台顶栏的一颗药丸。",
        "recipe_fields": FIELD_DOC,
        "endpoints": ENDPOINTS,
        "examples": EXAMPLES,
        "prompt": prompt_text(cfg, goal) if cfg else DEFAULT_PROMPT,
        "files": {"prompt": PROMPT_FILE, "model": MODEL_FILE, "recipes": RECIPE_FILE},
    }
    if cfg:
        d["ids"] = [r["id"] for r in all_recipes(cfg)]
        d["paths"] = {"prompt": str(prompt_path(cfg)), "model": str(model_path(cfg)),
                      "recipes": str(recipes_path(cfg))}
        d["model_configured"] = bool(model_config(cfg).get("api_key"))
    return d


_FENCE = re.compile(r"```(?:json|JSON)?\s*([\s\S]*?)```")


def extract_json(text: str):
    """从模型回答里抠一个 JSON：整段 → ```json 围栏 → 第一个平衡的 {...}。

    模型很少老实只输出 JSON：常见是围栏包着、或者前面扯两句。这里不苛求。
    """
    s = (text or "").strip()
    if not s:
        return None
    cands = []
    m = _FENCE.search(s)
    if m:
        cands.append(m.group(1))
    cands.append(s)
    for c in cands:
        try:
            return json.loads(c.strip())
        except ValueError:
            continue
    # 扫第一个括号配平的片段，字符串里的括号不算
    for i, ch in enumerate(s):
        if ch != "{":
            continue
        depth, instr, esc = 0, False, False
        for j in range(i, len(s)):
            cj = s[j]
            if instr:
                if esc:
                    esc = False
                elif cj == "\\":
                    esc = True
                elif cj == '"':
                    instr = False
                continue
            if cj == '"':
                instr = True
            elif cj == "{":
                depth += 1
            elif cj == "}":
                depth -= 1
                if depth == 0:
                    try:
                        return json.loads(s[i:j + 1])
                    except ValueError:
                        break
        break
    return None


_BREW_OK: dict[tuple[str, str], bool] = {}


def brew_has(name: str, kind: str = "") -> bool:
    """brew 里到底有没有这个包。查不到（没装 brew / 断网）就当没有，只提示不拦。"""
    name = (name or "").strip()
    if not name:
        return False
    key = (name, kind or "")
    if key in _BREW_OK:
        return _BREW_OK[key]
    brew = which("brew")
    if not brew:
        return False
    ok = False
    tries = ([["--cask"], []] if kind == "cask" else [[], ["--cask"]])
    for extra in tries:
        try:
            rc = subprocess.run([brew, "info"] + extra + ["--json=v2", name],
                                capture_output=True, timeout=20,
                                env=dict(os.environ, PATH=_path_env(),
                                         HOMEBREW_NO_AUTO_UPDATE="1")).returncode
        except (OSError, subprocess.SubprocessError):
            continue
        if rc == 0:
            ok = True
            break
    with _LOCK:
        _BREW_OK[key] = ok
    return ok


def parse_recipe_text(cfg: dict, text: str, verify: bool = True) -> dict:
    """模型回答 → 能直接用的配方草稿。不落盘，先让用户看一眼。"""
    d = extract_json(text)
    if not isinstance(d, dict):
        return {"ok": False, "error": "没从这段文字里找到 JSON 对象"}
    if isinstance(d.get("recipe"), dict):
        d = d["recipe"]
    elif isinstance(d.get("recipes"), list) and d["recipes"]:
        d = d["recipes"][0]
    if not isinstance(d, dict):
        return {"ok": False, "error": "JSON 里没有配方对象"}
    r = sanitize_recipe(d)
    if not r:
        return {"ok": False, "error": "配方缺东西：至少要有 id、name，以及 app / cli / start / brew 之一"}
    warns: list[str] = []
    if r.get("app") and not Path(str(r["app"])).expanduser().is_dir():
        warns.append(f"{r['app']} 不在本机，装完才会出现（没装就正常）")
    if r.get("brew") and verify and not brew_has(str(r["brew"]), str(r.get("brew_kind") or "")):
        warns.append(f"brew 里没查到 {r['brew']}：包名可能写错，或者得手动装")
    if r.get("port") and not r.get("web"):
        r["web"] = f"http://127.0.0.1:{int(r['port'])}/"
    if r["id"] in _BUILTIN_IDS:
        warns.append(f"{r['id']} 是目录里已有的内置程序，加进来会覆盖内置那条")
    return {"ok": True, "recipe": r, "warnings": warns,
            "state": probe(r) if installed(r) else None}


# --------------------------------------------------- 可选：用用户自己的模型生成
def model_config(cfg: dict) -> dict:
    """_工作台/第三方插件-模型.json：

    {"base_url": "https://api.openai.com/v1", "model": "gpt-4o-mini",
     "api_key": "sk-…"}            # 或 api_key_env: "OPENAI_API_KEY"

    任何 OpenAI 兼容的 /chat/completions 都能用（本地 Ollama、vLLM、各家云）。
    """
    d = _load(model_path(cfg), {})
    if not isinstance(d, dict):
        return {}
    key = str(d.get("api_key") or "").strip()
    if not key:
        env = str(d.get("api_key_env") or "").strip()
        key = os.environ.get(env, "") if env else ""
    if not key:
        return {}
    return {"base_url": str(d.get("base_url") or "https://api.openai.com/v1").rstrip("/"),
            "model": str(d.get("model") or "gpt-4o-mini"), "api_key": key}


def llm_compose(cfg: dict, goal: str) -> dict:
    """用户填了模型就替他跑一遍：prompt → 模型 → 配方草稿。"""
    import urllib.error
    import urllib.request
    mc = model_config(cfg)
    if not mc:
        return {"ok": False, "need": "token", "prompt": prompt_text(cfg, goal),
                "hint": f"没配模型。想要一键生成，就写 {model_path(cfg)}（见 /api/ext/spec）；"
                        "否则把 prompt 复制给任意模型，答案粘回 /api/ext/parse。"}
    body = json.dumps({
        "model": mc["model"], "temperature": 0.2,
        "messages": [{"role": "user", "content": prompt_text(cfg, goal)}],
    }).encode("utf-8")
    req = urllib.request.Request(
        mc["base_url"] + "/chat/completions", data=body, method="POST",
        headers={"Content-Type": "application/json",
                 "Authorization": f"Bearer {mc['api_key']}"})
    try:
        with urllib.request.urlopen(req, timeout=90) as resp:
            d = json.loads(resp.read().decode("utf-8"))
        content = str((d.get("choices") or [{}])[0].get("message", {}).get("content") or "")
    except (urllib.error.URLError, OSError, ValueError, KeyError) as e:
        return {"ok": False, "via": "model", "need": "token",
                "error": f"模型没调通：{str(e)[:160]}", "prompt": prompt_text(cfg, goal)}
    got = parse_recipe_text(cfg, content)
    got.update({"via": "model", "raw": content[:2000]})
    return got


def compose(cfg: dict, goal: str = "") -> dict:
    """有模型就走模型，没有就给提示词。前端只认这两种回答。"""
    return llm_compose(cfg, goal)


def plugin_row(r: dict) -> dict:
    """配方 → 写进 plugins.json 的那一行。带 ext 字段，dash 层据此走 ext 通道。"""
    row = {
        "id": r["id"], "name": r["name"], "glyph": r.get("glyph") or "ext",
        "note": r.get("note") or "", "url": r.get("web") or "",
        "port": r.get("port"), "group": "tool", "ext": r["id"],
    }
    return row
