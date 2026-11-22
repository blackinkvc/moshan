#!/usr/bin/env python3
"""文件层：把 NAS 的那套行为搬到本机。

NAS 好用的地方不在花哨，而在几个「本来就该有」的动作：
  目录层级浏览 / 重命名 / 移动 / 复制 / 删除进回收站 / 回收站可恢复 /
  分享链接（限时、可撤销）/ 打包下载 / 空间用量

这个模块只管文件与路径，不碰 HTTP 也不碰 UI，方便单独测。
"""
from __future__ import annotations

import json
import mimetypes
import os
import shutil
import time
import uuid
import zipfile
from pathlib import Path

ROOTS: list[Path] = []
TRASH: Path = Path("/tmp/pocket-trash")
SHARE_FILE: Path = Path("/tmp/pocket-shares.json")
LOCK_SUFFIX = ".lock"

MIME_EXTRA = {
    "md": "text/markdown", "markdown": "text/markdown", "txt": "text/plain",
    "json": "application/json", "csv": "text/csv", "log": "text/plain",
    "yaml": "text/yaml", "yml": "text/yaml", "py": "text/x-python",
    "sh": "text/x-shellscript", "html": "text/html", "css": "text/css",
    "js": "text/javascript", "ts": "text/javascript", "svg": "image/svg+xml",
    "heic": "image/heic", "heif": "image/heif", "m4a": "audio/mp4",
    "m4v": "video/x-m4v", "mkv": "video/x-matroska", "webm": "video/webm",
    "epub": "application/epub+zip", "pages": "application/octet-stream",
    "numbers": "application/octet-stream", "key": "application/octet-stream",
}
TEXT_EXTS = {"txt", "md", "markdown", "json", "csv", "tsv", "log", "yaml", "yml",
             "toml", "ini", "py", "js", "ts", "jsx", "tsx", "html", "css", "scss",
             "sh", "sql", "xml", "conf", "gitignore", "env"}
PREVIEW_IMAGE = {"jpg", "jpeg", "png", "gif", "webp", "heic", "heif", "bmp", "svg", "ico"}
PREVIEW_VIDEO = {"mp4", "mov", "m4v", "webm", "mkv", "avi"}
PREVIEW_AUDIO = {"mp3", "m4a", "wav", "aac", "flac", "ogg", "opus"}
PREVIEW_PDF = {"pdf"}

_EXTS = {
    "文档": "pdf doc docx txt md markdown rtf pages odt epub mobi azw3",
    "表格": "xls xlsx csv tsv numbers",
    "演示": "ppt pptx key keynote",
    "图片": "jpg jpeg png gif webp heic heif bmp tif tiff svg ico",
    "音视频": "mp3 wav m4a aac flac ogg opus mp4 mov mkv avi webm flv m4v",
    "压缩包": "zip rar 7z tar gz tgz bz2 xz dmg iso",
    "代码": "py js ts jsx tsx json yaml yml toml ini html css scss sh sql",
}
CATEGORY_OF_EXT = {e: c for c, exts in _EXTS.items() for e in exts.split()}


class PathError(Exception):
    """路径不合法或越界。带 HTTP 状态码，方便上层直接映射。"""

    def __init__(self, msg: str, code: int = 400):
        super().__init__(msg)
        self.code = code


def configure(roots: list[str], trash: str, shares: str) -> None:
    global ROOTS, TRASH, SHARE_FILE
    ROOTS = [Path(r).expanduser() for r in roots]
    TRASH = Path(trash).expanduser()
    SHARE_FILE = Path(shares).expanduser()


def safe_path(raw: str, *, must_exist: bool = False) -> Path:
    """只允许 allowed_roots 以内的路径，挡住 ../ 与软链越界。"""
    if not str(raw).strip():
        raise PathError("路径为空")
    p = Path(raw).expanduser()
    try:
        p = p.resolve()
    except (OSError, RuntimeError):
        raise PathError("路径无法解析")
    for root in ROOTS:
        try:
            r = root.resolve()
        except OSError:
            continue
        if p == r or r in p.parents:
            if must_exist and not p.exists():
                raise PathError("文件不存在", 404)
            return p
    raise PathError("这个路径不在允许范围内", 403)


def is_trash(p: Path) -> bool:
    try:
        t = TRASH.resolve()
    except OSError:
        return False
    return p == t or t in p.parents


def _guard_target(p: Path) -> None:
    """禁止对根目录或回收站本身做破坏性操作。"""
    rp = p.resolve()
    for root in ROOTS:
        if rp == root.resolve():
            raise PathError("不能操作根目录", 403)
    if is_trash(rp):
        raise PathError("回收站内的文件请用「恢复」或「清空」", 403)


def human(n: float) -> str:
    if n < 1024:
        return f"{int(n)} B"
    if n < 1048576:
        return f"{n / 1024:.0f} KB"
    if n < 1073741824:
        return f"{n / 1048576:.1f} MB"
    return f"{n / 1073741824:.2f} GB"


def category(name: str) -> str:
    return CATEGORY_OF_EXT.get(Path(name).suffix.lower().lstrip("."), "其他")


def mime_of(p: Path) -> str:
    ext = p.suffix.lower().lstrip(".")
    if ext in MIME_EXTRA:
        return MIME_EXTRA[ext]
    guess, _ = mimetypes.guess_type(p.name)
    return guess or "application/octet-stream"


def preview_kind(p: Path) -> str:
    ext = p.suffix.lower().lstrip(".")
    if ext in PREVIEW_IMAGE:
        return "image"
    if ext in PREVIEW_VIDEO:
        return "video"
    if ext in PREVIEW_AUDIO:
        return "audio"
    if ext in PREVIEW_PDF:
        return "pdf"
    if ext in TEXT_EXTS and p.is_file():
        try:
            if p.stat().st_size <= 2 * 1024 * 1024:
                return "text"
        except OSError:
            pass
    return "none"


def unique_path(directory: Path, name: str) -> Path:
    safe = Path(name).name.strip() or "未命名"
    if safe in (".", ".."):
        safe = "未命名"
    p = directory / safe
    if not p.exists():
        return p
    stem, suf = Path(safe).stem, Path(safe).suffix
    for i in range(2, 1000):
        cand = directory / f"{stem}-{i}{suf}"
        if not cand.exists():
            return cand
    return directory / f"{stem}-{int(time.time())}{suf}"


def entry(p: Path, root: Path | None = None) -> dict:
    try:
        st = p.stat()
    except OSError:
        st = None
    is_dir = p.is_dir()
    item = {
        "name": p.name,
        "path": str(p),
        "is_dir": is_dir,
        "bytes": 0 if is_dir else (st.st_size if st else 0),
        "size": "" if is_dir else human(st.st_size if st else 0),
        "mtime": int(st.st_mtime) if st else 0,
        "kind": "folder" if is_dir else preview_kind(p),
        "category": "文件夹" if is_dir else category(p.name),
    }
    if root is not None:
        try:
            item["rel"] = str(p.relative_to(root))
        except ValueError:
            item["rel"] = p.name
    if is_dir:
        try:
            with os.scandir(p) as it:
                n = 0
                for e in it:
                    if not e.name.startswith("."):
                        n += 1
                    if n >= 999:
                        break
                item["children"] = n
        except OSError:
            item["children"] = 0
    return item


def crumbs(path: Path, root: Path) -> list[dict]:
    """面包屑：从 root 到当前目录，供界面点击跳转。"""
    out = [{"name": root.name, "path": str(root)}]
    try:
        rel = path.relative_to(root)
    except ValueError:
        return out
    acc = root
    for part in rel.parts:
        acc = acc / part
        out.append({"name": part, "path": str(acc)})
    return out


def root_for(path: Path) -> Path:
    """当前路径属于哪个允许根。

    有多个 allowed_roots 时必须按归属判断：写死 ROOTS[0] 会让第二个根下的
    目录算不出面包屑（relative_to 抛错），界面就只剩一个根名。
    """
    try:
        rp = path.resolve()
    except OSError:
        rp = path
    for r in ROOTS:
        try:
            rr = r.resolve()
        except OSError:
            continue
        if rp == rr or rr in rp.parents:
            return rr
    return ROOTS[0].resolve() if ROOTS else rp


def list_dir(path: Path, q: str = "", sort: str = "name") -> dict:
    if not path.exists():
        raise PathError("目录不存在", 404)
    if not path.is_dir():
        raise PathError("这不是一个目录", 400)
    root = root_for(path)
    dirs, files = [], []
    try:
        with os.scandir(path) as it:
            for e in it:
                if e.name.startswith("."):
                    continue
                if e.name == "index.jsonl":
                    continue
                try:
                    p = Path(e.path)
                    if q:
                        # 搜索时允许进入子目录，避免"每层都要点一下"
                        pass
                    item = entry(p, root)
                except OSError:
                    continue
                (dirs if item["is_dir"] else files).append(item)
    except PermissionError:
        raise PathError("没有读取权限", 403)

    if q:
        ql = q.lower()
        files = [f for f in files if ql in f["name"].lower()]
        dirs = [d for d in dirs if ql in d["name"].lower()]
        if not files and not dirs:
            files = _deep_search(path, ql, root, limit=200)

    key = {"name": lambda x: x["name"].lower(),
           "mtime": lambda x: -x["mtime"],
           "size": lambda x: -x["bytes"]}.get(sort, lambda x: x["name"].lower())
    dirs.sort(key=key)
    files.sort(key=key)
    return {"path": str(path), "crumbs": crumbs(path, root),
            "parent": str(path.parent) if path != root else None,
            "dirs": dirs, "files": files,
            "count": len(dirs) + len(files), "usage": usage(path)}


def _deep_search(root: Path, ql: str, allowed_root: Path, limit: int = 200) -> list[dict]:
    """当前层没命中时往下搜四层，NAS 的"全盘搜索"体验。"""
    hits: list[dict] = []
    base_depth = len(root.parts)
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if not d.startswith(".")]
        if len(Path(dirpath).parts) - base_depth >= 4:
            dirnames[:] = []
        for fn in filenames:
            if fn.startswith(".") or fn == "index.jsonl":
                continue
            if ql in fn.lower():
                try:
                    hits.append(entry(Path(dirpath) / fn, allowed_root))
                except OSError:
                    continue
                if len(hits) >= limit:
                    return hits
    return hits


def usage(path: Path) -> dict:
    try:
        u = shutil.disk_usage(str(path))
    except OSError:
        return {}
    return {"total": u.total, "used": u.used, "free": u.free,
            "total_h": human(u.total), "free_h": human(u.free),
            "used_pct": round(u.used / u.total * 100, 1) if u.total else 0}


# ---------------------------------------------------------------- 回收站

def _trash_index() -> Path:
    return TRASH / "index.jsonl"


def _trash_log(rec: dict) -> None:
    TRASH.mkdir(parents=True, exist_ok=True)
    with _trash_index().open("a", encoding="utf-8") as f:
        f.write(json.dumps(rec, ensure_ascii=False) + "\n")


def delete_to_trash(paths: list[Path]) -> list[dict]:
    TRASH.mkdir(parents=True, exist_ok=True)
    out = []
    for p in paths:
        _guard_target(p)
        if not p.exists():
            out.append({"path": str(p), "ok": False, "error": "不存在"})
            continue
        stamp = time.strftime("%Y%m%d-%H%M%S")
        dest = unique_path(TRASH, f"{stamp}_{p.name}")
        try:
            size = p.stat().st_size if p.is_file() else _dir_size(p)
            shutil.move(str(p), str(dest))
            _trash_log({"name": dest.name, "orig": str(p), "ts": int(time.time()),
                        "bytes": size, "is_dir": p.is_dir()})
            out.append({"path": str(p), "ok": True, "trash": dest.name})
        except OSError as e:
            out.append({"path": str(p), "ok": False, "error": str(e)[:120]})
    return out


def _dir_size(p: Path) -> int:
    total = 0
    for dirpath, _dn, fns in os.walk(p):
        for fn in fns:
            try:
                total += os.path.getsize(os.path.join(dirpath, fn))
            except OSError:
                pass
    return total


def list_trash() -> list[dict]:
    idx = _trash_index()
    rows: list[dict] = []
    if not idx.exists():
        return rows
    try:
        for line in idx.read_text(encoding="utf-8").splitlines():
            if not line.strip():
                continue
            try:
                r = json.loads(line)
            except ValueError:
                continue
            p = TRASH / r["name"]
            if not p.exists():
                continue
            r["bytes"] = _dir_size(p) if p.is_dir() else p.stat().st_size
            r["size"] = human(r["bytes"])
            r["age"] = _age(int(r.get("ts", 0)))
            rows.append(r)
    except OSError:
        return []
    rows.sort(key=lambda x: -int(x.get("ts", 0)))
    return rows


def _age(ts: int) -> str:
    if not ts:
        return ""
    d = max(0, int(time.time() - ts))
    if d < 3600:
        return f"{d // 60} 分钟前"
    if d < 86400:
        return f"{d // 3600} 小时前"
    return f"{d // 86400} 天前"


def restore(names: list[str]) -> list[dict]:
    out = []
    keep = []
    index = {r["name"]: r for r in list_trash()}
    for n in names:
        r = index.get(n)
        if not r:
            out.append({"name": n, "ok": False, "error": "回收站里没有"})
            continue
        src, orig = TRASH / n, Path(r["orig"])
        try:
            orig.parent.mkdir(parents=True, exist_ok=True)
            shutil.move(str(src), str(unique_path(orig.parent, orig.name)))
            out.append({"name": n, "ok": True, "to": str(orig.parent)})
        except OSError as e:
            keep.append(r)
            out.append({"name": n, "ok": False, "error": str(e)[:120]})
    _rewrite_trash_index([r for r in index.values() if r["name"] in keep])
    return out


def purge(names: list[str] | None = None) -> int:
    rows = list_trash()
    target = [r for r in rows if names is None or r["name"] in set(names or [])]
    n = 0
    for r in target:
        p = TRASH / r["name"]
        try:
            shutil.rmtree(p) if p.is_dir() else p.unlink()
            n += 1
        except OSError:
            pass
    rest = [r for r in rows if r not in target]
    _rewrite_trash_index(rest)
    return n


def _rewrite_trash_index(rows: list[dict]) -> None:
    TRASH.mkdir(parents=True, exist_ok=True)
    tmp = _trash_index().with_suffix(".tmp")
    with tmp.open("w", encoding="utf-8") as f:
        for r in rows:
            f.write(json.dumps(r, ensure_ascii=False) + "\n")
    tmp.replace(_trash_index())


def auto_purge(days: int = 30) -> int:
    """回收站保留 30 天，和 NAS 的默认策略一致。"""
    if days <= 0:
        return 0
    cutoff = time.time() - days * 86400
    return purge([r["name"] for r in list_trash() if int(r.get("ts", 0)) < cutoff])


# ---------------------------------------------------------------- 操作

def mkdir(parent: Path, name: str) -> dict:
    name = (name or "").strip()
    if not name or "/" in name or name in (".", ".."):
        raise PathError("目录名不合法")
    if not parent.is_dir():
        raise PathError("上级不是目录")
    dest = parent / name
    if dest.exists():
        raise PathError("同名目录已存在", 409)
    dest.mkdir()
    return entry(dest)


def rename(p: Path, new_name: str) -> dict:
    _guard_target(p)
    new_name = (new_name or "").strip()
    if not new_name or "/" in new_name or new_name in (".", ".."):
        raise PathError("名字不合法")
    dest = p.parent / new_name
    if dest == p:
        return entry(p)
    if dest.exists():
        raise PathError("这个名字已被占用", 409)
    p.rename(dest)
    return entry(dest)


def move(paths: list[Path], dest_dir: Path, *, copy: bool = False) -> list[dict]:
    if not dest_dir.is_dir():
        raise PathError("目标不是目录")
    _guard_target(dest_dir)
    out = []
    for p in paths:
        _guard_target(p)
        if dest_dir == p.parent and not copy:
            out.append({"path": str(p), "ok": True, "note": "已在目标目录"})
            continue
        if p.is_dir() and (dest_dir == p or p in dest_dir.parents):
            out.append({"path": str(p), "ok": False, "error": "不能移进自己"})
            continue
        try:
            target = unique_path(dest_dir, p.name)
            if copy:
                shutil.copytree(p, target) if p.is_dir() else shutil.copy2(p, target)
            else:
                shutil.move(str(p), str(target))
            out.append({"path": str(p), "ok": True, "to": str(target)})
        except OSError as e:
            out.append({"path": str(p), "ok": False, "error": str(e)[:120]})
    return out


# ---------------------------------------------------------------- 打包

def make_zip(paths: list[Path], tmp_dir: Path, name: str = "打包下载") -> Path:
    """把多个文件/目录打成一个 zip。单文件超 4G 时自动改用存储模式。"""
    tmp_dir.mkdir(parents=True, exist_ok=True)
    out = tmp_dir / f"{name}-{time.strftime('%Y%m%d-%H%M%S')}-{uuid.uuid4().hex[:4]}.zip"
    with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED, allowZip64=True) as z:
        for p in paths:
            if p.is_dir():
                base = p.parent
                for dirpath, dirnames, filenames in os.walk(p):
                    dirnames[:] = [d for d in dirnames if not d.startswith(".")]
                    for fn in filenames:
                        if fn.startswith("."):
                            continue
                        fp = Path(dirpath) / fn
                        try:
                            z.write(fp, str(fp.relative_to(base)))
                        except OSError:
                            continue
            elif p.is_file():
                try:
                    z.write(p, p.name)
                except OSError:
                    continue
    return out


# ---------------------------------------------------------------- 分享

def _shares() -> dict:
    if not SHARE_FILE.exists():
        return {}
    try:
        return json.loads(SHARE_FILE.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}


def _save_shares(d: dict) -> None:
    tmp = SHARE_FILE.with_suffix(".tmp")
    SHARE_FILE.parent.mkdir(parents=True, exist_ok=True)
    tmp.write_text(json.dumps(d, ensure_ascii=False, indent=2), encoding="utf-8")
    tmp.replace(SHARE_FILE)
    try:
        os.chmod(SHARE_FILE, 0o600)
    except OSError:
        pass


def share_create(p: Path, hours: float = 24) -> dict:
    """生成一条只读下载链接，可设过期时间，随时撤销。"""
    _guard_target(p) if p.is_dir() else None
    if not p.exists():
        raise PathError("文件不存在", 404)
    sid = uuid.uuid4().hex[:10]
    now = int(time.time())
    rec = {"sid": sid, "path": str(p), "name": p.name, "is_dir": p.is_dir(),
           "created": now,
           "expires": now + int(hours * 3600) if hours and hours > 0 else 0,
           "hits": 0}
    d = _shares()
    d[sid] = rec
    _save_shares(d)
    return rec


def share_get(sid: str, *, touch: bool = True) -> dict | None:
    d = _shares()
    rec = d.get(sid)
    if not rec:
        return None
    if rec.get("expires") and int(rec["expires"]) < time.time():
        d.pop(sid, None)
        _save_shares(d)
        return None
    if touch:
        rec["hits"] = int(rec.get("hits", 0)) + 1
        rec["last"] = int(time.time())
        d[sid] = rec
        _save_shares(d)
    return rec


def share_list() -> list[dict]:
    d = _shares()
    now = time.time()
    live = []
    for rec in d.values():
        if rec.get("expires") and int(rec["expires"]) < now:
            continue
        p = Path(rec["path"])
        rec = dict(rec)
        rec["exists"] = p.exists()
        rec["expires_h"] = (time.strftime("%m-%d %H:%M", time.localtime(rec["expires"]))
                            if rec.get("expires") else "永久")
        live.append(rec)
    live.sort(key=lambda x: -int(x.get("created", 0)))
    return live


def share_revoke(sid: str) -> bool:
    d = _shares()
    if sid in d:
        d.pop(sid)
        _save_shares(d)
        return True
    return False
