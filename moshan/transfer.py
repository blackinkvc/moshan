"""文件传输助手：自己给自己发的东西，都落在这里。

照着微信那个「文件传输助手」做的，规矩也一样：

  · 只有一个方向 —— 你发，它收。它不回话，也不问你要不要；
  · 手机微信发来的文字/链接/图片/文件，和网页这边发的，进同一条时间线；
  · 微信里它是「chat」，这里就是一张卡片 + 一个输入框，往上翻是历史。

目录（<数据根>/14-传输/）：
    消息.jsonl        真源，一行一条，**只追加**（两个页面同时开着也不会互相冲掉）
    文件/YYYY-MM/     收到的文件，名字前面带条目 id，重名不会互相盖
    _回收站/          删掉的条目：jsonl 里留一行墓碑，文件挪到这里

为什么不放进 07-收件箱：那是「待处理」，会被文件卡扫、被去重扫；传输助手是
流水，发过就算，不该出现在任何待办清单里。单开一个目录，谁也不打扰谁。

删除是软删：jsonl 追加 {"id":..,"deleted":true}，读的时候按最后一次出现算。
这样删错还能从 _回收站 + jsonl 里捞回来，也不用整份重写。
"""

from __future__ import annotations

import json
import mimetypes
import os
import re
import shutil
import time
import uuid
from datetime import datetime
from pathlib import Path

DIR_NAME = "14-传输"
PUSH_LOG = "推送失败.jsonl"     # 「网页 → 手机微信」每次推送的成败账本，一行一次尝试
PUSH_KEEP = 500                 # 只留最近 500 行：这是排障用的，不是业务数据
import paths  # noqa: E402  （账本必须落可写目录：写进 .app 内部会让签名 seal 失效）
JOURNAL = paths.base_dir() / "pocket.jsonl"   # 工作台「任务」页的账

IMG_EXT = {"jpg", "jpeg", "png", "gif", "webp", "heic", "heif", "bmp", "tiff", "tif"}
VID_EXT = {"mp4", "mov", "m4v", "webm", "avi", "mkv"}
AUD_EXT = {"m4a", "mp3", "wav", "aac", "amr", "silk", "ogg", "flac"}

_LINK_RE = re.compile(r"https?://[^\s，。、；）)\]】\"'<>]+", re.I)
_SAFE_RE = re.compile(r"[\\/:*?\"<>|\x00-\x1f]")


# ------------------------------------------------------------------ 路径

def _root(cfg: dict) -> Path:
    """和 dash._root 同一套判断：数据根就是 allowed_roots 的第一个。"""
    try:
        import dash as _d
        return _d._root(cfg)
    except Exception:
        roots = cfg.get("allowed_roots") or []
        return Path(roots[0]) if roots else Path(cfg.get("inbox_dir", ".")).parent


def base_dir(cfg: dict) -> Path:
    return _root(cfg) / DIR_NAME


def store_path(cfg: dict) -> Path:
    return base_dir(cfg) / "消息.jsonl"


def files_dir(cfg: dict, at: datetime | None = None) -> Path:
    at = at or datetime.now()
    return base_dir(cfg) / "文件" / at.strftime("%Y-%m")


def trash_dir(cfg: dict) -> Path:
    return base_dir(cfg) / "_回收站"


# ------------------------------------------------------------------ 小工具

def _now() -> str:
    return datetime.now().isoformat(timespec="seconds")


def _new_id(at: datetime | None = None) -> str:
    at = at or datetime.now()
    return "tf" + at.strftime("%y%m%d%H%M%S") + uuid.uuid4().hex[:4]


def _human(n: int) -> str:
    n = int(n or 0)
    for unit, size in (("GB", 1 << 30), ("MB", 1 << 20), ("KB", 1 << 10)):
        if n >= size:
            v = n / size
            return (f"{v:.1f}".rstrip("0").rstrip(".")) + " " + unit
    return f"{n} B"


def _safe_name(name: str, fallback: str = "未命名") -> str:
    name = _SAFE_RE.sub("_", str(name or "").strip()) or fallback
    name = name.lstrip(".")
    if len(name) > 90:
        stem, dot, ext = name.rpartition(".")
        name = (stem[:80] + dot + ext) if dot and len(ext) <= 8 else name[:90]
    return name


def _kind_of(path: Path, mime: str = "") -> str:
    ext = path.suffix.lower().lstrip(".")
    if ext in IMG_EXT or mime.startswith("image/"):
        return "image"
    if ext in VID_EXT or mime.startswith("video/"):
        return "video"
    if ext in AUD_EXT or mime.startswith("audio/"):
        return "voice"
    return "file"


def split_links(text: str) -> list[str]:
    out = []
    for m in _LINK_RE.finditer(text or ""):
        u = m.group(0).rstrip(".,;:!?)]}")
        if u and u not in out:
            out.append(u)
    return out


def is_image(p: Path) -> bool:
    return p.suffix.lower().lstrip(".") in IMG_EXT


# ------------------------------------------------------------------ 读写

def _append(cfg: dict, row: dict) -> None:
    p = store_path(cfg)
    p.parent.mkdir(parents=True, exist_ok=True)
    line = json.dumps(row, ensure_ascii=False, separators=(",", ":"))
    with p.open("a", encoding="utf-8") as f:
        f.write(line + "\n")


def _journal(entry: dict) -> None:
    """往 pocket.jsonl 记一笔（工作台「任务」页的账）。

    为什么不 import server.log：server 反过来 import 本模块，会循环依赖；
    这里只是追加一行 JSON，没必要为它绕一层。写失败就算了 —— 记账不能
    反过来把「东西已经存进传输助手」这件事变成失败。
    """
    rec = {"ts": time.strftime("%Y-%m-%dT%H:%M:%S%z"), **entry}
    try:
        JOURNAL.parent.mkdir(parents=True, exist_ok=True)
        with JOURNAL.open("a", encoding="utf-8") as f:
            f.write(json.dumps(rec, ensure_ascii=False) + "\n")
    except OSError:
        pass


def _load(cfg: dict) -> list[dict]:
    """按 id 合并：后出现的同 id 行覆盖前一行；墓碑行把条目剔掉。"""
    p = store_path(cfg)
    order: list[str] = []
    by_id: dict[str, dict] = {}
    try:
        with p.open("r", encoding="utf-8") as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    row = json.loads(line)
                except ValueError:
                    continue
                if not isinstance(row, dict):
                    continue
                iid = str(row.get("id") or "")
                if not iid:
                    continue
                if row.get("deleted"):
                    by_id.pop(iid, None)
                    continue
                if iid not in by_id:
                    order.append(iid)
                by_id[iid] = row
    except OSError:
        return []
    return [by_id[i] for i in order if i in by_id]


# ------------------------------------------------------------------ 写入

def add(cfg: dict, text: str = "", path: str | Path = "", name: str = "",
        src: str = "web", conv: str = "", mime: str = "",
        at: str = "", move: bool = False) -> dict:
    """记一条。text 和 path 至少给一个。

    path 是这台机器上的一个文件（微信插件抓下来的图/文件就走这条路）：
    复制进 文件/YYYY-MM/，之后跟条目一起管。move=True 时改成移动（省一份拷贝）。
    """
    text = str(text or "").strip()
    src_path = Path(path) if path else None
    if src_path and not src_path.is_file():
        return {"ok": False, "error": "找不到这个文件"}
    if not text and not src_path:
        return {"ok": False, "error": "空消息"}

    when = datetime.now()
    if at:
        try:
            when = datetime.fromisoformat(str(at).replace("Z", "+00:00")).astimezone()
        except ValueError:
            pass
    item = {"id": _new_id(when), "at": when.isoformat(timespec="seconds"),
            "src": src or "web", "conv": conv or ""}

    if src_path:
        name = _safe_name(name or src_path.name)
        mime = mime or (mimetypes.guess_type(name)[0] or "application/octet-stream")
        d = files_dir(cfg, when)
        d.mkdir(parents=True, exist_ok=True)
        dest = d / f"{item['id']}_{name}"
        try:
            if move:
                shutil.move(str(src_path), str(dest))
            else:
                shutil.copy2(src_path, dest)
            size = dest.stat().st_size
        except OSError as e:
            return {"ok": False, "error": f"存文件失败：{e}"}
        kind = _kind_of(dest, mime)
        item.update({
            "kind": kind, "name": name, "size": size, "mime": mime,
            "path": str(dest.relative_to(base_dir(cfg))),
            # 文件附带的文字当说明留着（图片/语音的说明没意义，丢掉）
            "text": text if kind == "file" else "",
        })
    else:
        links = split_links(text)
        item.update({"kind": "link" if links and not _LINK_RE.sub("", text).strip(" \n,，。、")
                     else "text", "text": text, "links": links})

    _append(cfg, item)
    # 微信来的才算「收到」；网页自己加的那条由推送链记 tf_push，这里再记一遍就重了
    if str(src or "web") != "web":
        _journal({"event": "tf_in", "kind": item.get("kind"),
                  "name": item.get("name") or "",
                  "size": int(item.get("size") or 0),
                  "text": str(item.get("text") or "")[:80],
                  "from": str(src)})
    return {"ok": True, "item": item}


def remove(cfg: dict, iid: str) -> dict:
    iid = str(iid or "").strip()
    if not iid:
        return {"ok": False, "error": "没给 id"}
    hit = next((x for x in _load(cfg) if x["id"] == iid), None)
    if not hit:
        return {"ok": False, "error": "这条已经不在了"}
    rel = str(hit.get("path") or "")
    if rel:
        src = base_dir(cfg) / rel
        if src.is_file():
            t = trash_dir(cfg)
            t.mkdir(parents=True, exist_ok=True)
            try:
                shutil.move(str(src), str(t / src.name))
            except OSError:
                pass
    _append(cfg, {"id": iid, "deleted": True, "at": _now()})
    _journal({"event": "tf_del", "kind": hit.get("kind"),
              "name": hit.get("name") or "",
              "text": str(hit.get("text") or "")[:60]})
    return {"ok": True, "id": iid}


# ------------------------------------------------------------------ 读出

def file_of(cfg: dict, iid: str) -> Path | None:
    hit = next((x for x in _load(cfg) if x["id"] == str(iid or "")), None)
    if not hit:
        return None
    rel = str(hit.get("path") or "")
    if not rel:
        return None
    p = base_dir(cfg) / rel
    return p if p.is_file() else None


def items(cfg: dict, limit: int = 60, q: str = "", kind: str = "") -> dict:
    rows = _load(cfg)
    if q:
        needle = q.lower()
        rows = [r for r in rows
                if needle in str(r.get("text") or "").lower()
                or needle in str(r.get("name") or "").lower()
                or any(needle in u.lower() for u in (r.get("links") or []))]
    if kind and kind != "all":
        rows = [r for r in rows if r.get("kind") == kind]
    total = len(rows)
    out = []
    for r in reversed(rows[-max(1, int(limit)):]):
        x = dict(r)
        x["size_h"] = _human(x.get("size") or 0) if x.get("size") else ""
        out.append(x)
    return {"ok": True, "items": out, "total": total, "at": _now()}


def stats(cfg: dict) -> dict:
    rows = _load(cfg)
    today = datetime.now().strftime("%Y-%m-%d")
    sizes = sum(int(r.get("size") or 0) for r in rows)
    return {
        "total": len(rows),
        "today": sum(1 for r in rows if str(r.get("at") or "").startswith(today)),
        "files": sum(1 for r in rows if r.get("kind") in ("file", "image", "video", "voice")),
        "images": sum(1 for r in rows if r.get("kind") == "image"),
        "wechat": sum(1 for r in rows if r.get("src") == "wechat"),
        "bytes_h": _human(sizes) if sizes else "0 B",
    }


def payload(cfg: dict, limit: int = 40, q: str = "", kind: str = "") -> dict:
    d = items(cfg, limit=limit, q=q, kind=kind)
    d["stats"] = stats(cfg)
    d["push"] = push_summary(cfg)
    return d


# ------------------------------------------------ 推送账本（网页 → 手机微信）
#
# 为什么单独记一本账：网页发的东西会回推一份到手机微信，这条推送是**后台线程**做的，
# 失败了页面上什么也看不见 —— 2026-11-21 就是这样：图片全推不出去（微信回
# sendMessage ret=-2 prepare failed），但卡片照常显示「发送成功」，人以为助手挂了。
# 现在每次尝试都记一行，页面和巡检都有据可查。

def push_log_path(cfg: dict) -> Path:
    return base_dir(cfg) / PUSH_LOG


def log_push(cfg: dict, item: dict, ok: bool, error: str = "",
             attempts: int = 1) -> None:
    """记一次推送尝试。**每次尝试都记**，不是只记失败 ——
    巡检要靠「同一个 id 的最后一行」判断后来有没有推成功（恢复）。"""
    row = {"at": _now(), "id": str(item.get("id") or ""),
           "name": str(item.get("name") or ""), "kind": str(item.get("kind") or "text"),
           "ok": bool(ok), "attempts": int(attempts), "error": (error or "")[:200]}
    p = push_log_path(cfg)
    try:
        p.parent.mkdir(parents=True, exist_ok=True)
        with p.open("a", encoding="utf-8") as f:
            f.write(json.dumps(row, ensure_ascii=False) + "\n")
        # 超过上限就只留尾巴：整份读一遍在这点体量（几十 KB）上可以接受，
        # 而且不用处理并发截断的坑（两边同时写也最多多留几十行）
        if p.stat().st_size > 200_000:
            lines = p.read_text(encoding="utf-8", errors="replace").splitlines()
            if len(lines) > PUSH_KEEP:
                tmp = p.with_suffix(".jsonl.tmp")
                tmp.write_text("\n".join(lines[-PUSH_KEEP:]) + "\n", encoding="utf-8")
                tmp.replace(p)
    except OSError:
        pass


def _push_rows_at(p: Path) -> list[dict]:
    if not p.exists():
        return []
    out = []
    try:
        for ln in p.read_text(encoding="utf-8", errors="replace").splitlines():
            ln = ln.strip()
            if not ln:
                continue
            try:
                d = json.loads(ln)
            except ValueError:
                continue
            if isinstance(d, dict) and d.get("at"):
                out.append(d)
    except OSError:
        return []
    return out


def push_summary_at(path: Path, hours: int = 6) -> dict:
    """按**文件路径**算（巡检从 8774 那边只拿得到路径，不想为此 import dash）。"""
    rows = _push_rows_at(Path(path))
    if not rows:
        return {"hours": hours, "total": 0, "failed": 0, "ok": 0,
                "error": "", "at": "", "since": "", "names": [], "ids": []}
    cut = time.time() - max(1, int(hours)) * 3600
    last: dict = {}
    first: dict = {}                       # 同一个 id 第一次出现的时刻（「从什么时候开始没到」）
    for r in rows:
        try:
            ts = datetime.fromisoformat(str(r["at"])).timestamp()
        except (ValueError, TypeError):
            continue
        if ts < cut:
            continue
        key = str(r.get("id") or r.get("at"))
        last[key] = r                      # 后面的覆盖前面的（文件本来就是时间序）
        first.setdefault(key, str(r["at"]))
    bad = [r for r in last.values() if not r.get("ok")]
    good = [r for r in last.values() if r.get("ok")]
    bad.sort(key=lambda r: str(r.get("at")))
    since = min((first.get(str(r.get("id") or r.get("at")), "") for r in bad), default="")
    return {
        "hours": hours,
        "total": len(last),
        "failed": len(bad),
        "ok": len(good),
        "error": str(bad[-1].get("error") or "") if bad else "",
        "at": str(bad[-1].get("at") or "") if bad else "",
        # 起点要按「这条第一次失败」算：重推会让同一个 id 又记一行新时刻，
        # 拿最后一行的 at 当起点会把「15:40 就开始没到」写成「刚刚」。
        "since": since if bad else "",
        "names": [str(r.get("name") or "") for r in bad[-3:]],
        # 失败的 id（给「重推」用；只给前 20 条，一次推几十个只会把手机刷爆）
        "ids": [str(r.get("id") or "") for r in bad[:20]],
    }


def push_summary(cfg: dict, hours: int = 6) -> dict:
    """最近 hours 小时里，哪些条目**最后仍然没推到手机**。

    一个 id 可能重试过几行，只认最后一行 —— 重试成功的那条不算失败。
    传输卡的红条和巡检的 transfer_push 都读这里，口径只有这一处。
    """
    return push_summary_at(push_log_path(cfg), hours)
