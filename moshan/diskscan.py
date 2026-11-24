"""磁盘占用分析 —— 巡检页「磁盘文件」卡片的数据层。

为什么要单独一个模块：这件事跟「口袋在传什么文件」无关，它回答的是
「这块盘被谁吃掉了」。三种看法：

  struct  一层一层看目录（点进去），每行带大小和条目数
  apps    应用程序各自占多少（macOS 的 .app 是一整个目录，不展开就看不出来）
  types   按文件类型汇总（图片 / 视频 / 文档 / 代码 …）

设计约束（踩过才知道的）：
  * **别做全盘递归**。一块 1TB 的盘递归一遍几分钟，页面早超时了。
    这里每次只算「当前这一层」的直接子项大小，深一层等用户点进去再算。
    所以首屏是 O(顶层条目数)，不是 O(文件总数)。
  * **必须限时**。外置机械盘 stat 一条要几十毫秒，一层几千个文件就卡死。
    用 deadline 硬切，切了就标 truncated —— 页面显示「只算了前 N 项」，
    比转圈到天荒地老诚实。
  * **权限错误照跳**。macOS 的 ~/Library 下一堆不可读目录，跳过就是，
    不能因为一个目录 403 让整张卡报错。
  * **符号链接不跟**。跟进去就是无穷递归 + 重复计数（自建盘符链接很常见）。

缓存 90 秒：看盘不是看行情，用户点两下不该把同一层算两遍。
"""
from __future__ import annotations

import os
import shutil
import threading
import time
from pathlib import Path

# 一层最多列这么多子项：再多页面也放不下，而且多半是缓存目录
MAX_ENTRIES = 400
# 单次扫描的硬时限（秒）。外置盘 / 网络盘给宽一点也够。
SCAN_SECONDS = 2.5
CACHE_TTL = 90.0

_CACHE: dict[str, object] = {"by": {}}
_LOCK = threading.Lock()

# 认成「一个应用」的东西：macOS 的 .app 是目录，Windows 是 .exe 所在目录
_APP_SUFFIX = {".app", ".appbundle", ".framework"}

# 类型归类：跟口袋的传输分类同一套口径（用户看惯了的六类 + 其他）
_TYPE_OF = {
    "图片": "jpg jpeg png gif webp heic heif bmp tif tiff svg ico raw cr2 nef",
    "视频": "mp4 mov mkv avi webm flv m4v wmv mpg mpeg ts",
    "音频": "mp3 wav m4a aac flac ogg opus aiff wma",
    "文档": "pdf doc docx txt md markdown rtf pages odt epub mobi azw3 xls xlsx csv tsv numbers ppt pptx key keynote",
    "代码": "py js ts jsx tsx json yaml yml toml ini html css scss sh sql go rs java c cpp h swift kt rb php",
    "压缩包": "zip rar 7z tar gz tgz bz2 xz dmg iso pkg deb rpm",
}
_EXT_KIND = {e: k for k, exts in _TYPE_OF.items() for e in exts.split()}


def _cache_get(key: str):
    with _LOCK:
        slot = _CACHE["by"].get(key)          # type: ignore[union-attr]
    if not slot:
        return None
    at, data = slot                             # type: ignore[misc]
    if time.time() - at > CACHE_TTL:
        return None
    return data


def _cache_put(key: str, data) -> None:
    with _LOCK:
        by = _CACHE["by"]                          # type: ignore[assignment]
        by[key] = (time.time(), data)
        if len(by) > 64:                           # 别让逛目录把内存逛大
            for k in sorted(by, key=lambda k: by[k][0])[:24]:
                by.pop(k, None)


# ---------------------------------------------------------------- 卷

def volumes() -> list[dict]:
    """这台机器能看的盘：根卷 + /Volumes 下挂着的。

    外置盘没插时 /Volumes 里那条会消失，所以每次都现扫，不缓存 ——
    扫 /Volumes 只是一次 readdir，比缓存失效逻辑便宜。
    """
    out: list[dict] = []

    def add(mount: str) -> None:
        try:
            u = shutil.disk_usage(mount)
        except OSError:
            return
        p = Path(mount)
        name = "启动磁盘" if mount == "/" else p.name
        out.append({
            "name": name, "mount": mount,
            "total": u.total, "used": u.used, "free": u.free,
            "pct": round(u.used / u.total * 100) if u.total else 0,
            "removable": mount != "/",
        })

    add("/")
    try:
        for p in sorted(Path("/Volumes").iterdir()):
            if p.is_symlink() or not p.is_dir():
                continue
            # 根卷常被 挂到 /Volumes/<名字>，别重复列一遍
            try:
                if os.stat(p).st_dev == os.stat("/").st_dev:
                    continue
            except OSError:
                pass
            add(str(p))
    except OSError:
        pass
    return out


# ---------------------------------------------------------------- 扫描

def _safe_dir(path: str, roots: list[str]) -> str:
    """把用户给的路径收进「这块盘」的范围里，防止 ../ 穿越去读别处。

    按挂载点**从长到短**匹配：根卷 "/" 能匹配一切，先试它的话外置盘
    永远被当成根卷的一部分（面包屑就变成「启动磁盘 / Volumes / AI仓库」）。
    """
    p = Path(path).expanduser()
    try:
        real = p.resolve()
    except OSError:
        return ""
    for r in sorted(roots, key=len, reverse=True):
        try:
            rr = Path(r).resolve()
        except OSError:
            continue
        if real == rr or rr in real.parents:
            return str(real)
    return ""


def _dir_size(path: Path, deadline: float) -> tuple[int, int, bool]:
    """递归累加一个目录的大小。返回 (字节, 文件数, 是否被时限截断)。

    用栈不用递归：深目录递归会撞 Python 的 1000 层限制（node_modules 轻松超）。
    follow_symlinks=False：跟链接就是无穷递归 + 重复计数。

    **挂载点必须跳过**：macOS 把数据卷挂在 /System/Volumes/Data，把外置盘挂在
    /Volumes 下。不跳的话 /System 会显示 784G —— 那是整块盘的数据卷被算了两遍。
    注意不能用 st_dev 判断：/Users、/Applications 是 firmlink（同一容器不同卷），
    st_dev 跟 / 不一样但它们是正经目录，跳掉就什么都不剩了。
    """
    total = 0
    files = 0
    cut = False
    stack = [path]
    while stack:
        if time.time() > deadline:
            cut = True
            break
        cur = stack.pop()
        try:
            with os.scandir(cur) as it:
                for e in it:
                    try:
                        if e.is_dir(follow_symlinks=False):
                            child = Path(e.path)
                            if os.path.ismount(child):
                                continue           # 别的卷，不算进这个目录
                            stack.append(child)
                        elif e.is_file(follow_symlinks=False):
                            total += e.stat(follow_symlinks=False).st_size
                            files += 1
                    except OSError:
                        continue
        except OSError:
            continue
    return total, files, cut


def _dir_shallow(path: Path) -> tuple[int, int]:
    """这个目录**直接**有多少东西：直接文件的大小合计 + 子项数。

    为什么不在这里递归：根目录下每个子项都递归一遍就是全盘扫描，
    一块装满的盘要几分钟，页面早超时了（第一版就是这么卡的，只剩 usr/home
    两行、还是 0 字节）。文件夹在 Finder 里点进去也是这个道理 ——
    一层一层看，想看总量再单独按「算总量」。
    """
    size = 0
    n = 0
    try:
        with os.scandir(path) as it:
            for e in it:
                n += 1
                try:
                    if e.is_file(follow_symlinks=False):
                        size += e.stat(follow_symlinks=False).st_size
                except OSError:
                    continue
    except OSError:
        pass
    return size, n


def _entry(p: Path, deadline: float) -> dict | None:
    try:
        st = p.stat()
    except OSError:
        return None
    if p.is_dir():
        size, n = _dir_shallow(p)
        row = {"name": p.name, "path": str(p), "dir": True,
               "size": size, "n": n, "shallow": True}
        # 之前按过「算」的目录直接把真值贴回来 —— 否则用户算完一刷新
        # 又变回「直接文件大小」，等于白算（Applications 会显示 32K，看着像空的）。
        hit = _cache_get("deep:" + str(p))
        if hit and hit.get("ok"):
            row["size"] = int(hit.get("size") or 0)
            row["shallow"] = False
        return row
    return {"name": p.name, "path": str(p), "dir": False,
            "size": st.st_size, "n": 1, "shallow": False}


def scan_layer(path: str, roots: list[str]) -> dict:
    """把这一层所有目录一次性算到底（「全部算」按钮）。

    和 tree() 的区别就是那个递归：tree 只给直接文件大小，这个给总量。
    所以要限时 —— 一层十几个大目录，全算是十几秒级。做法：
      · 总预算 12 秒，每个目录最多 3 秒（先算到的先返回，别让一个巨型目录拖死）；
      · 算过的走 90 秒缓存，所以第二次点「全部算」只会补上次没算完的那几个，
        点两下就齐了；
      · 只剩「这一层的文件」也要算进去，否则文件夹和文件的比例对不上。
    """
    real = _safe_dir(path, roots)
    if not real:
        return {"ok": False, "error": "这个位置不在这块盘的范围内"}
    base = Path(real)
    deadline = time.time() + SCAN_SECONDS * 5
    rows: list[dict] = []
    cut = False
    try:
        with os.scandir(base) as it:
            entries = [e for e in it if not e.name.startswith(".")]
    except OSError as e:
        return {"ok": False, "error": f"读不了这个目录：{e.strerror or e}"}
    if len(entries) > MAX_ENTRIES:
        entries = entries[:MAX_ENTRIES]
        cut = True

    for e in entries:
        if time.time() > deadline:
            cut = True
            break
        try:
            is_dir = e.is_dir(follow_symlinks=False)
        except OSError:
            continue
        if not is_dir:
            try:
                st = e.stat(follow_symlinks=False)
            except OSError:
                continue
            rows.append({"name": e.name, "path": e.path, "dir": False,
                         "size": st.st_size, "n": 1, "shallow": False})
            continue
        hit = _cache_get("deep:" + str(Path(e.path)))
        if hit and hit.get("ok"):
            rows.append({"name": e.name, "path": e.path, "dir": True,
                         "size": int(hit.get("size") or 0), "n": int(hit.get("n") or 0),
                         "shallow": False, "cut": bool(hit.get("cut"))})
            continue
        budget = min(deadline, time.time() + 3.0)
        size, n, c = _dir_size(Path(e.path), budget)
        _cache_put("deep:" + str(Path(e.path)),
                   {"ok": True, "path": e.path, "size": size, "n": n, "cut": c,
                    "at": time.strftime("%H:%M:%S")})
        rows.append({"name": e.name, "path": e.path, "dir": True,
                     "size": size, "n": n, "shallow": False, "cut": c})

    rows.sort(key=lambda r: (not r["dir"], -r["size"]))
    return {"ok": True, "path": str(base), "entries": rows,
            "total": sum(r["size"] for r in rows), "truncated": cut,
            "crumbs": _crumbs(base, roots), "at": time.strftime("%H:%M:%S")}


def tree(path: str, roots: list[str]) -> dict:
    """某一层的直接子项（目录带递归大小，文件带自身大小）。"""
    real = _safe_dir(path, roots)
    if not real:
        return {"ok": False, "error": "这个位置不在这块盘的范围内"}
    key = "tree:" + real
    hit = _cache_get(key)
    if hit:
        return hit

    base = Path(real)
    deadline = time.time() + SCAN_SECONDS
    rows: list[dict] = []
    try:
        with os.scandir(base) as it:
            names = [e.name for e in it]
    except OSError as e:
        return {"ok": False, "error": f"读不了这个目录：{e.strerror or e}"}

    # 先按名字挑出来，再算大小：算大小才是最贵的那步，条目太多可以砍
    truncated = len(names) > MAX_ENTRIES
    for nm in names[:MAX_ENTRIES]:
        if nm.startswith(".") and nm not in (".Trash",):
            continue                                  # 隐藏目录不列，免得全是 .xxx
        if time.time() > deadline:
            truncated = True
            break
        e = _entry(base / nm, deadline)
        if e is not None:
            rows.append(e)

    rows.sort(key=lambda r: (not r["dir"], -r["size"]))
    parent = "" if str(base) in [str(Path(r).resolve()) for r in roots] else str(base.parent)
    data = {
        "ok": True, "path": str(base), "parent": parent,
        "crumbs": _crumbs(base, roots),
        "entries": rows,
        "total": sum(r["size"] for r in rows),
        "truncated": truncated,
        "at": time.strftime("%H:%M:%S"),
    }
    _cache_put(key, data)
    return data


def deep(path: str, roots: list[str]) -> dict:
    """递归算这个目录到底多大 —— 用户点「算总量」才走这条。

    限时 8 秒（比列一层宽得多，因为这一次真的要把子树走完），
    走不完就返回已算到的部分并置 cut，页面写「≥ X GB，还没算完」。
    结果缓存 90 秒：算一次就够了，来回点不该重算。
    """
    real = _safe_dir(path, roots)
    if not real:
        return {"ok": False, "error": "这个位置不在这块盘的范围内"}
    key = "deep:" + real
    hit = _cache_get(key)
    if hit:
        return hit
    size, n, cut = _dir_size(Path(real), time.time() + SCAN_SECONDS * 3.2)
    data = {"ok": True, "path": real, "size": size, "n": n, "cut": cut,
            "at": time.strftime("%H:%M:%S")}
    _cache_put(key, data)
    return data


def _crumbs(base: Path, roots: list[str]) -> list[dict]:
    """从这块盘的根到当前目录，给面包屑用。同样从长到短匹配挂载点。"""
    for r in sorted(roots, key=len, reverse=True):
        try:
            rr = Path(r).resolve()
        except OSError:
            continue
        if base == rr or rr in base.parents:
            out = [{"name": "启动磁盘" if str(rr) == "/" else rr.name, "path": str(rr)}]
            rel = base.relative_to(rr)
            cur = rr
            for part in rel.parts:
                cur = cur / part
                out.append({"name": part, "path": str(cur)})
            return out
    return []


def apps(mount: str, roots: list[str]) -> dict:
    """这块盘上装的应用各占多少。

    只看三个约定位置：<盘>/Applications、<盘>/System/Applications、
    ~/Applications（家目录那份只有根卷才有意义）。要全盘找 .app 的话，
    一块盘上几千个 .app 副本（备份、旧版本、Xcode 模拟器）会把页面淹掉，
    而且那也不是「装了什么」。
    """
    real = _safe_dir(mount, roots)
    if not real:
        return {"ok": False, "error": "这块盘不在范围内"}
    key = "apps:" + real
    hit = _cache_get(key)
    if hit:
        return hit

    dirs = [Path(real) / "Applications", Path(real) / "System" / "Applications"]
    if real == "/":
        dirs.append(Path.home() / "Applications")
    deadline = time.time() + SCAN_SECONDS * 2      # 应用目录深，给多一点
    rows: list[dict] = []
    seen: set[str] = set()
    for d in dirs:
        try:
            with os.scandir(d) as it:
                entries = [e for e in it]
        except OSError:
            continue
        for e in entries:
            if time.time() > deadline:
                break
            if not e.name.endswith(tuple(_APP_SUFFIX)):
                continue
            try:
                if not e.is_dir(follow_symlinks=False):
                    continue
            except OSError:
                continue
            real_p = str(Path(e.path).resolve())
            if real_p in seen:
                continue
            seen.add(real_p)
            size, n, cut = _dir_size(Path(e.path), deadline)
            rows.append({"name": e.name[:-4] if e.name.endswith(".app") else e.name,
                         "path": e.path, "size": size, "n": n, "cut": cut,
                         "system": "/System/" in e.path})
    rows.sort(key=lambda r: -r["size"])
    total = sum(r["size"] for r in rows)
    data = {"ok": True, "path": real, "entries": rows, "total": total,
            "at": time.strftime("%H:%M:%S")}
    _cache_put(key, data)
    return data


def types(path: str, roots: list[str]) -> dict:
    """按文件类型汇总这一层以下的占用（递归，但只统计大小不建索引）。"""
    real = _safe_dir(path, roots)
    if not real:
        return {"ok": False, "error": "这个位置不在这块盘的范围内"}
    key = "types:" + real
    hit = _cache_get(key)
    if hit:
        return hit

    deadline = time.time() + SCAN_SECONDS * 2
    agg: dict[str, dict] = {}
    stack = [Path(real)]
    cut = False
    while stack:
        if time.time() > deadline:
            cut = True
            break
        cur = stack.pop()
        try:
            with os.scandir(cur) as it:
                for e in it:
                    try:
                        if e.is_dir(follow_symlinks=False):
                            child = Path(e.path)
                            if os.path.ismount(child):
                                continue           # 别的卷不算进来（见 _dir_size）
                            stack.append(child)
                            continue
                        if not e.is_file(follow_symlinks=False):
                            continue
                        ext = os.path.splitext(e.name)[1].lower().lstrip(".")
                        kind = _EXT_KIND.get(ext) or ("其他" if ext else "无扩展名")
                        sz = e.stat(follow_symlinks=False).st_size
                    except OSError:
                        continue
                    b = agg.setdefault(kind, {"kind": kind, "size": 0, "n": 0})
                    b["size"] += sz
                    b["n"] += 1
        except OSError:
            continue
    rows = sorted(agg.values(), key=lambda r: -r["size"])
    data = {"ok": True, "path": real, "entries": rows,
            "total": sum(r["size"] for r in rows), "truncated": cut,
            "at": time.strftime("%H:%M:%S")}
    _cache_put(key, data)
    return data
