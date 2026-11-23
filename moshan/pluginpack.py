"""插件包（`.moshanplug`）—— 把「一个能跑的插件」整包装进中台。

为什么单独一份，而不是塞进 dash.py 的 add_plugin
    add_plugin 只登记一行 json，它假设**程序和启动脚本已经在机器上了**。可用户
    真正拿到的是「一个下载下来的包」：别人发来的、从 GitHub 下的、网站上一键下
    的。这类东西需要解包、校验、落盘、补启动脚本、登记，五件事一件不能少。

包是什么
    一个 zip，扩展名 `.moshanplug`（`.zip` 也认），根上必须有 plugin.json：

        plugin.json      必需。id / name / version；其余字段同 plugins.json
        run.command      可选。不给就按 entry 自动生成一个
        run.bat          可选。Windows 用
        <程序文件>        其余随便放

    GitHub 的源码 zip（`仓库名-分支/plugin.json`）也认 —— 顶层只有一个目录时往
    里看一层。`__MACOSX`、`.DS_Store`、`._*` 一律丢掉（exFAT 上天天见）。

装到哪
    <数据根>/_插件/<id>/        程序
    <数据根>/_工作台/plugins.json  登记（只改自己那一行，别人的行原样留着）
    <数据根>/<data_default>     用户数据；已经存在就一个字节都不动

    升级＝新包覆盖同名文件，包里没有的旧文件保留（不做「先删再放」，删错一次
    就是插件再也起不来）。旧版目录另存一份到 `_插件/.history/`，能退回去。

三条硬约束（都踩过坑才写的）
    1. 解包不用 extractall：逐条校验相对路径，`..` / 绝对路径 / 符号链接一律拒绝；
    2. 解压总量封顶：先看 zip 里声明的总大小，超过上限直接拒，不给 zip bomb 机会；
    3. 写完 plugins.json 才算装完：文件落地了但没登记，用户看到的就是「装了个寂寞」。
"""
from __future__ import annotations

import hashlib
import io
import json
import os
import re
import shutil
import socket
import stat
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import zipfile
from concurrent.futures import ThreadPoolExecutor, as_completed
from datetime import datetime
from pathlib import Path

# 交付版有 paths.py（程序/状态/资料库三层目录的唯一来源）；开发版没有这个模块，
# 得自己兜一个。**不能**顶层 `import paths`：开发目录里 sys.path 上挂着 cuntu 的
# paths.py，它没有 state_dir()，导入成功、调用时才 AttributeError，更难看。
try:                                    # noqa: SIM105
    import paths as _paths
except Exception:                       # noqa: BLE001
    _paths = None


def _state_dir() -> Path:
    """缓存、下载包这类「软件自己」的东西。生产路径优先走 paths.py。"""
    fn = getattr(_paths, "state_dir", None) if _paths else None
    if callable(fn):
        return fn()
    if os.environ.get("MOSHAN_HOME"):
        return Path(os.environ["MOSHAN_HOME"]).expanduser() / "state"
    if sys.platform == "darwin":
        return Path.home() / "Library" / "Application Support" / "Moshan"
    if os.name == "nt":
        base = os.environ.get("APPDATA") or (Path.home() / "AppData" / "Roaming")
        return Path(base) / "Moshan"
    return Path(os.environ.get("XDG_STATE_HOME", Path.home() / ".local" / "state")) / "moshan"


def _data_root() -> Path:
    """数据根默认值。生产走 paths.data_root()，开发版就是作者的目录结构。"""
    fn = getattr(_paths, "data_root", None) if _paths else None
    if callable(fn):
        return fn()
    if os.environ.get("MOSHAN_HOME"):
        return Path(os.environ["MOSHAN_HOME"]).expanduser() / "library"
    return Path.home() / "Documents" / "AI项目"

# ---------------------------------------------------------------- 常量

MANIFEST = "plugin.json"
PACK_EXT = (".moshanplug", ".zip")
PLUG_DIR = "_插件"
HISTORY_DIR = ".history"

MAX_FILES = 20000                 # 文件数上限：普通插件几十个，两万已经离谱
MAX_TOTAL = 512 * 1024 * 1024     # 解压总量上限
MAX_ONE = 200 * 1024 * 1024       # 单文件上限
SKIP_DIRS = {"__MACOSX", ".git", ".svn", "node_modules", "__pycache__"}
SKIP_NAMES = {".DS_Store", "Thumbs.db", "desktop.ini"}

ID_RE = re.compile(r"^[a-z][a-z0-9_-]{0,31}$")

# 更新检查缓存：放状态目录（软件自己），不放数据根（用户的）
CACHE_TTL = 6 * 3600.0
UPDATE_TTL = 5.0          # 单个请求的上限：墙外的地址经常「连得上但不回话」
UPDATE_BUDGET = 9.0       # 一个插件查新版的总预算，超了就算查不到

USER_AGENT = "Moshan-PluginPack/1.0"


def _ssl_context():
    """urlopen 用的 CA 上下文。

    为什么不能裸调 urlopen：macOS 上 PyInstaller 的 OpenSSL 找不到系统证书
    （0.2.0 的「检查更新」就因为这一条对所有人失效，交付版的解法是 netca.py）。
    开发机上更直接 —— venv 的 python 同样不认系统 CA，实测报
    `CERTIFICATE_VERIFY_FAILED unable to get local issuer certificate`。
    有 certifi 就用它的证书；没有再退回默认（至少不会因为 import 失败崩掉）。
    """
    try:
        import ssl
        try:
            import certifi
            return ssl.create_default_context(cafile=certifi.where())
        except Exception:                               # noqa: BLE001
            return ssl.create_default_context()
    except Exception:                                   # noqa: BLE001
        return None


_CTX = _ssl_context()


def _open(req: urllib.request.Request, timeout: float):
    """统一出口：把 CA 上下文带上（None 表示用默认）。"""
    if _CTX is not None:
        return urllib.request.urlopen(req, timeout=timeout, context=_CTX)
    return urllib.request.urlopen(req, timeout=timeout)


class PackError(Exception):
    """可预期的失败：包坏了、id 不对、写不进去。文案直接给用户看。"""


# ---------------------------------------------------------------- 目录

def _root(cfg: dict) -> Path:
    """数据根。跟 dash._root 同一套判断，但这里不 import dash（那边五千行）。"""
    roots = cfg.get("allowed_roots") or []
    if roots:
        return Path(roots[0]).expanduser()
    return Path(cfg.get("inbox_dir", ".")).expanduser().parent


def plugins_dir(cfg: dict) -> Path:
    return _root(cfg) / PLUG_DIR


def plugins_json(cfg: dict) -> Path:
    return _root(cfg) / "_工作台" / "plugins.json"


def _cache_file() -> Path:
    return _state_dir() / "plugin-updates.json"


def ensure_plugins_dir(cfg: dict) -> Path:
    d = plugins_dir(cfg)
    d.mkdir(parents=True, exist_ok=True)
    return d


# ---------------------------------------------------------------- plugins.json
# 这里**不**复用 dash.write_plugins：那份按白名单重写，会把用户手写的未知字段
# （比如 note 以外的自定义项）悄悄抹掉。插件包只改自己那一行，别的行原样带回去。

def read_rows(cfg: dict) -> list[dict]:
    p = plugins_json(cfg)
    if not p.exists():
        return []
    try:
        d = json.loads(p.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return []
    rows = d.get("plugins") if isinstance(d, dict) else d
    return [r for r in (rows or []) if isinstance(r, dict)]


def write_rows(cfg: dict, rows: list[dict]) -> None:
    p = plugins_json(cfg)
    p.parent.mkdir(parents=True, exist_ok=True)
    old = {}
    if p.exists():
        try:
            old = json.loads(p.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            old = {}
    head = old.get("_说明") if isinstance(old, dict) else None
    payload = {}
    if head:
        payload["_说明"] = head
    payload["plugins"] = rows
    payload["updated_at"] = datetime.now().isoformat(timespec="seconds")
    tmp = p.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(payload, ensure_ascii=False, indent=1), encoding="utf-8")
    tmp.replace(p)


def find_row(cfg: dict, pid: str) -> dict | None:
    for r in read_rows(cfg):
        if str(r.get("id") or "") == pid:
            return r
    return None


def upsert_row(cfg: dict, row: dict) -> dict:
    """装/升级后登记。同 id 覆盖（保留用户改过的 note/glyph/group）。"""
    pid = row["id"]
    rows = read_rows(cfg)
    for i, r in enumerate(rows):
        if str(r.get("id") or "") != pid:
            continue
        merged = dict(r)
        # 这几项以包为准（程序换了，地址和启动脚本跟着换）
        for k in ("name", "url", "port", "cmd", "svc_name"):
            if row.get(k) not in (None, ""):
                merged[k] = row[k]
        # 这几项包里没说就不动（用户自己挑的图标、归的档、写的说明）
        for k in ("glyph", "note", "group"):
            if row.get(k) and not merged.get(k):
                merged[k] = row[k]
        # 版本与来源永远以包为准，否则升级检查会一直拿旧版本比
        for k in ("version", "repo", "pack", "update", "data_env", "data_default",
                  "entry", "ping", "platforms"):
            if k in row:
                merged[k] = row[k]
        rows[i] = merged
        write_rows(cfg, rows)
        return merged
    rows.append(row)
    write_rows(cfg, rows)
    return row


def drop_row(cfg: dict, pid: str) -> bool:
    rows = read_rows(cfg)
    keep = [r for r in rows if str(r.get("id") or "") != pid]
    if len(keep) == len(rows):
        return False
    write_rows(cfg, keep)
    return True


# ---------------------------------------------------------------- 读包

def _safe_rel(name: str) -> str:
    """zip 里的名字 → 安全的相对路径。不合法就抛，别静默跳过。

    静默跳过是最坏的选择：包少一个文件，装完起不来，用户完全不知道为什么。
    """
    n = name.replace("\\", "/").strip()
    while n.startswith("./"):
        n = n[2:]
    if not n or n.endswith("/"):
        return ""                                   # 目录条目，由写入时按需建
    if n.startswith("/") or re.match(r"^[A-Za-z]:", n):
        raise PackError(f"包里有个绝对路径，拒绝：{name}")
    parts = [p for p in n.split("/") if p not in ("", ".")]
    if any(p == ".." for p in parts):
        raise PackError(f"包里有个往上跳的路径，拒绝：{name}")
    return "/".join(parts)


def _skip_member(name: str) -> bool:
    parts = name.split("/")
    if any(p in SKIP_DIRS for p in parts):
        return True
    leaf = parts[-1] if parts else ""
    return leaf in SKIP_NAMES or leaf.startswith("._")


def _is_symlink(info: zipfile.ZipInfo) -> bool:
    try:
        mode = (info.external_attr >> 16) & 0xFFFF
    except (AttributeError, TypeError):
        return False
    return stat.S_ISLNK(mode)


def _manifest_of(root: Path) -> tuple[Path, dict, Path]:
    """在一棵目录树里找 manifest，返回 (包根, manifest dict, manifest 路径)。

    认三种布局：根上有 / 只有一个顶层目录（GitHub zip）/ 深度两层的唯一目录。
    """
    direct = root / MANIFEST
    if direct.is_file():
        return root, _read_manifest(direct), direct
    tops = [p for p in root.iterdir()
            if p.is_dir() and p.name not in SKIP_DIRS and not p.name.startswith((".", "_"))]
    if len(tops) == 1:
        cand = tops[0] / MANIFEST
        if cand.is_file():
            return tops[0], _read_manifest(cand), cand
        subs = [p for p in tops[0].iterdir()
                if p.is_dir() and not p.name.startswith((".", "_"))]
        if len(subs) == 1 and (subs[0] / MANIFEST).is_file():
            return subs[0], _read_manifest(subs[0] / MANIFEST), subs[0] / MANIFEST
    deep = sorted(root.glob("*/" + MANIFEST)) + sorted(root.glob("*/*/" + MANIFEST))
    if deep:
        p = deep[0]
        return p.parent, _read_manifest(p), p
    raise PackError("包里没有 plugin.json —— 这不像一个墨山插件包")


def _read_manifest(p: Path) -> dict:
    try:
        d = json.loads(p.read_text(encoding="utf-8"))
    except json.JSONDecodeError as exc:
        raise PackError(f"plugin.json 不是合法 JSON：{exc}") from exc
    except OSError as exc:
        raise PackError(f"plugin.json 读不出来：{exc}") from exc
    if not isinstance(d, dict):
        raise PackError("plugin.json 要是一个对象")
    return d


def norm_manifest(m: dict) -> dict:
    """manifest → 统一形状。缺 id 用名字兜，缺版本当 0.0.0（不阻断安装）。"""
    pid = str(m.get("id") or "").strip().lower()
    name = str(m.get("name") or m.get("displayName") or "").strip()
    if not pid:
        pid = re.sub(r"[^a-z0-9]+", "-", name.lower()).strip("-")[:24]
    if not pid or not ID_RE.match(pid):
        raise PackError("plugin.json 里的 id 要么没写，要么不合规（小写字母开头，只能小写字母/数字/-/_，32 位以内）")
    if not name:
        raise PackError("plugin.json 里没写 name（顶栏上要显示的名字）")
    ver = str(m.get("version") or "0.0.0").strip().lstrip("vV") or "0.0.0"

    out = {
        "id": pid,
        "name": name[:24],
        "version": ver,
        "note": str(m.get("note") or m.get("summary") or "").strip()[:60],
        "glyph": str(m.get("glyph") or "plugin").strip() or "plugin",
        "group": str(m.get("group") or "other").strip().lower(),
        "port": m.get("port"),
        "url": str(m.get("url") or "").strip(),
        "entry": str(m.get("entry") or "").strip(),
        "args": [str(a) for a in (m.get("args") or []) if str(a).strip()],
        "ping": str(m.get("ping") or "").strip(),
        "data_env": str(m.get("data_env") or "").strip(),
        "data_default": str(m.get("data_default") or "").strip(),
        "repo": str(m.get("repo") or "").strip(),
        "platforms": [str(x).lower() for x in (m.get("platforms") or []) if str(x).strip()],
        "icon": str(m.get("icon") or "").strip(),
        "requires": m.get("requires") if isinstance(m.get("requires"), dict) else {},
        "update": m.get("update") if isinstance(m.get("update"), dict) else {},
    }
    try:
        out["port"] = int(out["port"]) if out["port"] not in (None, "", 0, "0") else None
    except (TypeError, ValueError):
        out["port"] = None
    if out["group"] not in ("tool", "plugin", "other"):
        out["group"] = "other"
    return out


def platform_ok(m: dict) -> tuple[bool, str]:
    plats = m.get("platforms") or []
    if not plats:
        return True, ""
    me = "windows" if os.name == "nt" else ("macos" if sys.platform == "darwin" else "linux")
    if me in plats:
        return True, ""
    return False, f"这个包只给 {'、'.join(plats)} 用，当前是 {me}"


# ---------------------------------------------------------------- 解包

def unpack_zip(src: Path, dest: Path) -> tuple[Path, dict, int, int]:
    """解到 dest，返回 (包根, manifest, 文件数, 总字节)。不落 io 之外的东西。"""
    try:
        zf = zipfile.ZipFile(src)
    except (zipfile.BadZipFile, OSError) as exc:
        raise PackError(f"不是有效的 zip：{exc}") from exc

    with zf:
        infos = zf.infolist()
        if len(infos) > MAX_FILES:
            raise PackError(f"包里 {len(infos)} 个文件，超过上限 {MAX_FILES}")
        declared = sum(max(i.file_size, 0) for i in infos if not i.is_dir())
        if declared > MAX_TOTAL:
            raise PackError(f"解压后约 {declared / 1048576:.0f} MB，超过上限 "
                            f"{MAX_TOTAL // 1048576} MB")

        dest.mkdir(parents=True, exist_ok=True)
        base = dest.resolve()
        n_files, n_bytes = 0, 0
        for info in infos:
            if info.is_dir():
                continue
            rel = _safe_rel(info.filename)
            if not rel or _skip_member(rel):
                continue
            if _is_symlink(info):
                raise PackError(f"包里有个符号链接，拒绝：{info.filename}")
            if info.file_size > MAX_ONE:
                raise PackError(f"{rel} 单个 {info.file_size / 1048576:.0f} MB，太大了")
            target = (base / rel).resolve()
            if not str(target).startswith(str(base) + os.sep) and target != base:
                raise PackError(f"包里的路径会写到外面去，拒绝：{info.filename}")
            target.parent.mkdir(parents=True, exist_ok=True)
            try:
                with zf.open(info) as fin, target.open("wb") as fout:
                    shutil.copyfileobj(fin, fout, 256 * 1024)
            except (OSError, zipfile.BadZipFile) as exc:
                raise PackError(f"解包 {rel} 失败：{exc}") from exc
            n_files += 1
            n_bytes += target.stat().st_size
            if n_bytes > MAX_TOTAL:
                raise PackError("解出来的体积超过上限，中止")

    root, m, mpath = _manifest_of(dest)
    return root, m, n_files, n_bytes


def unpack_bytes(raw: bytes, dest: Path) -> tuple[Path, dict, int, int]:
    tmp = dest.parent / (dest.name + ".zip")
    tmp.write_bytes(raw)
    try:
        return unpack_zip(tmp, dest)
    finally:
        tmp.unlink(missing_ok=True)


def collect_files(pairs: list[tuple[str, bytes]], dest: Path) -> tuple[Path, dict, int, int]:
    """把「一堆 (相对路径, bytes)」写成目录树。

    浏览器拖文件夹时拿不到真实路径（安全限制），只能 webkitRelativePath，
    所以这条路必须支持。同样校验，别让人用文件名往上游目录写。
    """
    dest.mkdir(parents=True, exist_ok=True)
    base = dest.resolve()
    n, total = 0, 0
    for rel, raw in pairs:
        name = _safe_rel(rel)
        if not name or _skip_member(name):
            continue
        if len(raw) > MAX_ONE:
            raise PackError(f"{name} 单个 {len(raw) / 1048576:.0f} MB，太大了")
        target = (base / name).resolve()
        if not str(target).startswith(str(base) + os.sep):
            raise PackError(f"路径会写到外面去，拒绝：{rel}")
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(raw)
        n += 1
        total += len(raw)
        if total > MAX_TOTAL:
            raise PackError("内容超过上限，中止")
    if n > MAX_FILES:
        raise PackError(f"{n} 个文件，超过上限 {MAX_FILES}")
    root, m, _ = _manifest_of(dest)
    return root, m, n, total


# ---------------------------------------------------------------- 启动脚本

def _run_script(m: dict) -> tuple[str, str]:
    """给一份 manifest 生成启动脚本 (文件名, 内容)。没有 entry 就返回空。

    为什么替用户生成：绝大多数插件就是「一个 python 文件 + 起个服务」。让每个
    作者都手写 .command，实际上就是谁写谁忘 chmod +x，装完点一下一片空白。
    """
    entry = m.get("entry") or ""
    if not entry:
        return "", ""
    if os.name == "nt":
        body = [
            "@echo off",
            'cd /d "%~dp0"',
            f'set "{m["data_env"]}=%{m["data_env"]}%"' if m.get("data_env") else "",
            f'python "{entry}" ' + " ".join(m.get("args") or []),
        ]
        return "run.bat", "\r\n".join(x for x in body if x) + "\r\n"
    args = " ".join("'" + a.replace("'", "'\\''") + "'" for a in (m.get("args") or []))
    lines = [
        "#!/bin/bash",
        "# 墨山中台自动生成：双击就能起这个插件。",
        'cd "$(dirname "$0")" || exit 1',
    ]
    if m.get("data_env") and m.get("data_default"):
        # 双引号里的 ~ 不会展开（shell 只在无引号时展），这里换成 $HOME。
        # 外层是 "${VAR:-…}"，所以这里绝不能再套一层双引号（bash 3.2 会算错边界）。
        dd = str(m["data_default"]).replace("\\", "\\\\").replace('"', '\\"')
        if dd.startswith("~"):
            dd = "$HOME" + dd[1:]
        lines.append('export %s="${%s:-%s}"' % (m["data_env"], m["data_env"], dd))
    lines.append('PY="$(command -v python3 || command -v python)"')
    lines.append('[ -n "$PY" ] || { echo "需要 python3"; exit 1; }')
    lines.append(f'exec "$PY" "{entry}" {args}'.rstrip())
    return "run.command", "\n".join(lines) + "\n"


def _find_launcher(root: Path) -> str:
    for name in ("run.command", "start.command", "run.sh", "run.bat", "start.bat"):
        if (root / name).is_file():
            return name
    return ""


def _chmod_x(p: Path) -> None:
    try:
        mode = p.stat().st_mode
        p.chmod(mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    except OSError:
        pass


# ---------------------------------------------------------------- 安装

def _version_key(v: str) -> tuple:
    core = str(v or "0").strip().lstrip("vV").split("-")[0]
    out = []
    for seg in core.split("."):
        try:
            out.append(int(seg))
        except ValueError:
            out.append(0)
    while len(out) < 3:
        out.append(0)
    return tuple(out)


def _port_busy(port: int) -> bool:
    if not port:
        return False
    try:
        with socket.create_connection(("127.0.0.1", port), timeout=0.3):
            return True
    except OSError:
        return False


def _install_tree(cfg: dict, pack_root: Path, m: dict, *, source: str,
                  files: int = 0, size: int = 0) -> dict:
    """把已经落到临时目录的包树装进 _插件/<id>/，登记，返回结果。"""
    pid = m["id"]
    ok, why = platform_ok(m)
    if not ok:
        raise PackError(why)

    dest_root = ensure_plugins_dir(cfg)
    dest = dest_root / pid
    warnings: list[str] = []

    old_ver = ""
    keep_git = False
    if dest.exists():
        try:
            old = json.loads((dest / MANIFEST).read_text(encoding="utf-8"))
            old_ver = str(old.get("version") or "")
        except (OSError, json.JSONDecodeError):
            old_ver = ""
        # 用 git clone 装的插件（比如 llmcost）升级后还得能 git pull：
        # 版本库跟着插件目录走，不是程序文件，挪到 history 里就再也 pull 不动了。
        keep_git = (dest / ".git").is_dir()
        # 旧版留一份，能退回；只留最近 3 份，不无限长
        hist = dest_root / HISTORY_DIR
        hist.mkdir(parents=True, exist_ok=True)
        stamp = datetime.now().strftime("%Y%m%d-%H%M%S")
        try:
            shutil.move(str(dest), str(hist / f"{pid}-{(old_ver or 'old')}-{stamp}"))
        except OSError as exc:
            raise PackError(f"旧版挪不开（插件正在运行？）：{exc}") from exc
        _prune_history(dest_root, pid)

    # 拷进去：整棵树，跳过伴生垃圾
    dest.mkdir(parents=True, exist_ok=True)
    for item in pack_root.iterdir():
        if item.name in SKIP_DIRS or item.name in SKIP_NAMES or item.name.startswith("._"):
            continue
        target = dest / item.name
        if item.is_dir():
            shutil.copytree(item, target, dirs_exist_ok=True,
                            ignore=shutil.ignore_patterns(*SKIP_DIRS, "._*", ".DS_Store"))
        elif item.is_file():
            shutil.copy2(item, target)

    # 启动脚本：包里有就用包里的，没有就按 entry 生成
    launcher = _find_launcher(dest)
    if not launcher:
        fname, body = _run_script(m)
        if fname and body:
            (dest / fname).write_text(body, encoding="utf-8", newline="")
            launcher = fname
            warnings.append("包里没带启动脚本，按 entry 生成了一个")
    if launcher:
        _chmod_x(dest / launcher)

    # 把 .git 找回来（见上面 keep_git）。新包里自带 .git 就不动它。
    if keep_git and not (dest / ".git").exists():
        hist = dest_root / HISTORY_DIR
        donors = sorted((p for p in hist.iterdir() if p.name.startswith(pid + "-")),
                        key=lambda p: p.stat().st_mtime, reverse=True)
        for d in donors:
            src = d / ".git"
            if src.is_dir():
                try:
                    shutil.move(str(src), str(dest / ".git"))
                    warnings.append("保留了原来的 .git，升级后还能 git pull")
                except OSError:
                    pass
                break

    # 顶层点开头的文件是「这台机器上的配置」，不是程序：.llmcost-dir（数据目录指针）、
    # .env、.secret 这类。升级只该换程序，把这些一起挪走等于把用户设置清了。
    if old_ver:
        hist = dest_root / HISTORY_DIR
        donors = sorted((p for p in hist.iterdir() if p.name.startswith(pid + "-")),
                        key=lambda p: p.stat().st_mtime, reverse=True)
        if donors:
            for item in donors[0].iterdir():
                if not item.name.startswith(".") or item.name in (".git", ".DS_Store"):
                    continue
                target = dest / item.name
                if target.exists():
                    continue
                try:
                    shutil.move(str(item), str(target))
                except OSError:
                    pass

    # manifest 里的 version 回写到插件目录，升级检查读这里（plugins.json 可能被手改）
    try:
        (dest / MANIFEST).write_text(
            json.dumps({**m, "installed_at": datetime.now().isoformat(timespec="seconds"),
                        "source": source}, ensure_ascii=False, indent=1) + "\n",
            encoding="utf-8")
    except OSError:
        pass

    # 数据目录：给了就建出来，已经有了不动
    data_dir = ""
    if m.get("data_default"):
        raw = str(m["data_default"]).replace("~", str(Path.home()))
        dd = Path(raw)
        if not dd.is_absolute():
            dd = _root(cfg) / raw
        try:
            dd.mkdir(parents=True, exist_ok=True)
            data_dir = str(dd)
        except OSError as exc:
            warnings.append(f"数据目录建不出来：{exc}")

    # 图标：包内 png 拷到 _工作台/_图标/<id>.png（前端按 id 找）
    if m.get("icon"):
        src = dest / m["icon"]
        if src.is_file() and src.suffix.lower() in (".png", ".jpg", ".jpeg", ".svg"):
            ic = _root(cfg) / "_工作台" / "_图标"
            ic.mkdir(parents=True, exist_ok=True)
            try:
                shutil.copy2(src, ic / (pid + src.suffix.lower()))
            except OSError:
                pass
        else:
            warnings.append(f"图标 {m['icon']} 在包里找不到，用默认图标")

    # 端口冲突：不阻断（用户可能就是要接管正在跑的那个），但要说
    port = m.get("port")
    if port and _port_busy(int(port)):
        warnings.append(f"端口 {port} 现在有东西在跑 —— 如果就是它自己，正常；不是的话换一个")

    row = {
        "id": pid,
        "name": m["name"],
        "version": m["version"],
        "url": m.get("url") or (f"http://127.0.0.1:{port}/" if port else ""),
        "port": port,
        "cmd": f"{PLUG_DIR}/{pid}/{launcher}" if launcher else "",
        "glyph": m.get("glyph") or "plugin",
        "note": m.get("note") or "",
        "group": m.get("group") or "other",
        "svc_name": m["name"],
        "pack": True,
        "repo": m.get("repo") or "",
        "data_env": m.get("data_env") or "",
        "data_default": m.get("data_default") or "",
        "entry": m.get("entry") or "",
        "ping": m.get("ping") or "",
        "platforms": m.get("platforms") or [],
        "update": m.get("update") or {},
        "installed_at": datetime.now().isoformat(timespec="seconds"),
    }
    if not row["url"] and not port:
        # 纯卡片插件：没有地址也允许，但必须说清楚顶栏点它只弹一张空卡
        warnings.append("包里没写 port / url —— 装上了，但顶栏点它没有页面")
    row = {k: v for k, v in row.items() if v not in ("", [], {}, None)}
    try:
        upsert_row(cfg, row)
    except OSError as exc:
        raise PackError(f"插件文件装好了，但 plugins.json 写不进去：{exc}") from exc

    return {
        "ok": True,
        "plugin": row,
        "upgraded": bool(old_ver),
        "old_version": old_ver,
        "files": files,
        "size": size,
        "dir": str(dest),
        "data_dir": data_dir,
        "launcher": launcher,
        "warnings": warnings,
    }


def _prune_history(root: Path, pid: str, keep: int = 3) -> None:
    hist = root / HISTORY_DIR
    if not hist.is_dir():
        return
    items = sorted((p for p in hist.iterdir() if p.name.startswith(pid + "-")),
                   key=lambda p: p.stat().st_mtime, reverse=True)
    for p in items[keep:]:
        shutil.rmtree(p, ignore_errors=True)


# ---------------------------------------------------------------- 对外：预览

def inspect_zip(path: Path) -> dict:
    """只看不装：让用户在确认框里看清「这是谁的包、多少文件」。"""
    tmp = Path(tempfile.mkdtemp(prefix="mpk-"))
    try:
        root, m, n, size = unpack_zip(path, tmp / "pack")
        nm = norm_manifest(m)
        ok, why = platform_ok(nm)
        return {
            "ok": True, "manifest": nm, "files": n, "size": size,
            "platform_ok": ok, "platform_note": why,
            "has_launcher": bool(_find_launcher(root)),
            "preview": _tree_preview(root),
        }
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def _tree_preview(root: Path, limit: int = 40) -> list[str]:
    out = []
    for p in sorted(root.rglob("*")):
        if p.is_dir():
            continue
        if any(part in SKIP_DIRS for part in p.parts):
            continue
        out.append(str(p.relative_to(root)))
        if len(out) >= limit:
            out.append("…")
            break
    return out


# ---------------------------------------------------------------- 对外：安装

def install_zip(cfg: dict, src: Path, *, source: str = "upload") -> dict:
    tmp = Path(tempfile.mkdtemp(prefix="mpk-"))
    try:
        root, m, n, size = unpack_zip(src, tmp / "pack")
        nm = norm_manifest(m)
        return _install_tree(cfg, root, nm, source=source, files=n, size=size)
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def install_raw(cfg: dict, raw: bytes, name: str = "") -> dict:
    """上传的一段字节：是 zip 就按包解，否则报清楚。"""
    if not name.lower().endswith(PACK_EXT) and not zipfile.is_zipfile(io.BytesIO(raw)):
        raise PackError("这个文件不是插件包（.moshanplug / .zip）")
    tmp = Path(tempfile.mkdtemp(prefix="mpk-"))
    try:
        f = tmp / (name or "pack.moshanplug")
        f.write_bytes(raw)
        root, m, n, size = unpack_zip(f, tmp / "pack")
        nm = norm_manifest(m)
        return _install_tree(cfg, root, nm, source="upload", files=n, size=size)
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def install_files(cfg: dict, pairs: list[tuple[str, bytes]], *, source: str = "folder") -> dict:
    """拖文件夹 / 选文件夹：一堆相对路径 + 内容。"""
    tmp = Path(tempfile.mkdtemp(prefix="mpk-"))
    try:
        root, m, n, size = collect_files(pairs, tmp / "pack")
        nm = norm_manifest(m)
        return _install_tree(cfg, root, nm, source=source, files=n, size=size)
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def install_dir(cfg: dict, path: Path) -> dict:
    """装一个本机目录（用户已经把仓库 clone 下来了）。"""
    p = Path(path).expanduser()
    if not p.exists():
        raise PackError(f"这个路径不存在：{p}")
    if p.is_file():
        return install_zip(cfg, p, source="path")
    root, m, _ = _manifest_of(p)
    nm = norm_manifest(m)
    pairs = []
    for f in sorted(root.rglob("*")):
        if not f.is_file():
            continue
        rel = str(f.relative_to(root))
        if _skip_member(rel):
            continue
        try:
            pairs.append((rel, f.read_bytes()))
        except OSError:
            continue
    return install_files(cfg, pairs, source="path")


def install_url(cfg: dict, url: str) -> dict:
    """让中台自己下。路径写清楚：下载 → 校验 → 解包 → 装，每一步都可能失败。"""
    u = str(url or "").strip()
    if not re.match(r"^https?://", u, re.I):
        raise PackError("要给一个 http(s) 地址")
    # GitHub 的 releases/latest 是「去 release 页」，不是文件地址 —— 直接下会拿到
    # 一坨 HTML。与其猜资产名，不如把正确形状告诉用户。
    if re.search(r"github\.com/[^/]+/[^/]+/releases/latest/?$", u):
        raise PackError("这是 Release 页面地址，不是包地址。用 "
                        "…/releases/latest/download/<包名>.moshanplug")
    req = urllib.request.Request(u, headers={"User-Agent": USER_AGENT})
    try:
        with _open(req, 60) as resp:
            raw = resp.read(MAX_TOTAL + 1)
            ctype = (resp.headers.get("Content-Type") or "").lower()
    except urllib.error.HTTPError as exc:
        raise PackError(f"下载失败：HTTP {exc.code}") from exc
    except (urllib.error.URLError, TimeoutError, OSError) as exc:
        raise PackError(f"下载失败：{exc}") from exc
    if len(raw) > MAX_TOTAL:
        raise PackError("下载超过上限，中止")
    if "text/html" in ctype and not zipfile.is_zipfile(io.BytesIO(raw)):
        raise PackError("这个地址返回的是网页，不是插件包（是不是给了项目主页而不是文件地址？）")
    name = Path(urllib.parse.urlparse(u).path).name
    return install_raw(cfg, raw, name or "pack.moshanplug")


def uninstall(cfg: dict, pid: str, *, remove_data: bool = False) -> dict:
    """删插件。数据目录默认留着 —— 「删插件」不该顺手删掉用户攒了一年的东西。"""
    if not ID_RE.match(pid):
        raise PackError("id 不合规")
    dest = plugins_dir(cfg) / pid
    had_row = drop_row(cfg, pid)
    moved = ""
    if dest.exists():
        hist = ensure_plugins_dir(cfg) / HISTORY_DIR
        hist.mkdir(parents=True, exist_ok=True)
        stamp = datetime.now().strftime("%Y%m%d-%H%M%S")
        target = hist / f"{pid}-uninstalled-{stamp}"
        try:
            shutil.move(str(dest), str(target))
            moved = str(target)
        except OSError as exc:
            raise PackError(f"删不掉（可能正在运行）：{exc}") from exc
    if not had_row and not moved:
        raise PackError("清单里没有这个插件，目录里也没有")
    data_kept = ""
    row = find_row(cfg, pid) or {}
    if row.get("data_default"):
        data_kept = str(row["data_default"])
    return {"ok": True, "moved_to": moved, "data_kept": data_kept,
            "note": "程序已移走，数据目录没动" if data_kept else ""}


# ---------------------------------------------------------------- 更新检查

def _http_json(url: str, timeout: float = UPDATE_TTL) -> dict:
    req = urllib.request.Request(url, headers={
        "User-Agent": USER_AGENT, "Accept": "application/vnd.github+json"})
    with _open(req, timeout) as resp:
        return json.loads(resp.read().decode("utf-8"))


def _fetch_ver(url: str, kind: str) -> tuple[str, str, str]:
    """取一个候选地址的版本号。拿不到就抛，让调用方当成「这条路不通」。"""
    d = _http_json(url)
    ver = str(d.get("version") or "").lstrip("vV").strip()
    if not ver and kind == "release":                   # release 用 tag 当版本
        ver = str(d.get("tag_name") or "").lstrip("vV").strip()
    notes = str(d.get("body") or d.get("notes") or d.get("summary") or "")[:2000]
    return ver, notes, url


def _latest_online(row: dict) -> tuple[str, str, str]:
    """查一个插件的最新版本，返回 (version, notes, url)。

    三条路并发跑，谁先给出合法版本就用谁 —— **串行是不行的**：实测国内
    raw.githubusercontent.com 是「连得上但不回话」，一条挂 20 秒，串四条就是
    一分多钟，前端轮询直接超时。并发之后总耗时约等于最慢的那一条 ≈ 5 秒。

      1. update.manifest  —— 作者自己挂的 json，最省事也最快（国内也能放）
      2. repo 的 release  —— 标准做法，能拿到更新说明
      3. GitHub Pages 的 plugin.json —— 国内实测比 raw 稳
      4. repo 的 plugin.json（raw，main/master）—— 没发 release 的仓库兜底
    """
    upd = row.get("update")
    if not isinstance(upd, dict):
        upd = {}
    manifest = str(upd.get("manifest") or "").strip()
    if manifest:
        ver, notes, _ = _fetch_ver(manifest, "manifest")
        if not ver:
            raise PackError("update.manifest 里没有 version")
        return ver, notes, manifest

    repo = str(upd.get("repo") or row.get("repo") or "").strip()
    repo = re.sub(r"^https?://github\.com/", "", repo).strip("/")
    if repo.endswith(".git"):
        repo = repo[:-4]
    if not repo or "/" not in repo:
        raise PackError("这个插件没写 repo / update.manifest，查不了新版")

    owner, name = repo.split("/", 1)
    cands = [
        (f"https://api.github.com/repos/{repo}/releases/latest", "release"),
        (f"https://{owner.lower()}.github.io/{name}/{MANIFEST}", "pages"),
        (f"https://raw.githubusercontent.com/{repo}/main/{MANIFEST}", "raw"),
        (f"https://raw.githubusercontent.com/{repo}/master/{MANIFEST}", "raw"),
    ]
    last_err = ""
    ex = ThreadPoolExecutor(max_workers=len(cands))
    futs = [ex.submit(_fetch_ver, u, k) for u, k in cands]
    try:
        for f in as_completed(futs, timeout=UPDATE_BUDGET):
            try:
                ver, notes, url = f.result()
            except Exception as exc:                    # noqa: BLE001
                last_err = last_err or str(exc)[:120]
                continue
            if ver:
                ex.shutdown(wait=False, cancel_futures=True)
                return ver, notes, url
    except Exception as exc:                            # noqa: BLE001  整体超时
        last_err = last_err or f"都超时了（{UPDATE_BUDGET:.0f} 秒）"
    finally:
        ex.shutdown(wait=False, cancel_futures=True)
    raise PackError(f"查不到新版：{last_err or '地址里都没有 version'}")


def load_cache() -> dict:
    p = _cache_file()
    if not p.exists():
        return {}
    try:
        return json.loads(p.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return {}


def save_cache(d: dict) -> None:
    p = _cache_file()
    try:
        p.parent.mkdir(parents=True, exist_ok=True)
        tmp = p.with_suffix(".json.tmp")
        tmp.write_text(json.dumps(d, ensure_ascii=False, indent=1) + "\n", encoding="utf-8")
        tmp.replace(p)
    except OSError:
        pass


def installed_packs(cfg: dict) -> list[dict]:
    """已安装的插件包：以 _插件/<id>/plugin.json 为准（这才是真装上的）。"""
    d = plugins_dir(cfg)
    if not d.is_dir():
        return []
    rows = {str(r.get("id") or ""): r for r in read_rows(cfg)}
    out = []
    for p in sorted(d.iterdir()):
        if not p.is_dir() or p.name.startswith("."):
            continue
        mf = p / MANIFEST
        if not mf.is_file():
            continue
        try:
            m = norm_manifest(_read_manifest(mf))
        except PackError:
            continue
        row = rows.get(m["id"]) or {}
        out.append({
            "id": m["id"], "name": m["name"], "version": m["version"],
            "repo": m.get("repo") or row.get("repo") or "",
            "note": m.get("note") or row.get("note") or "",
            "dir": str(p),
            "registered": m["id"] in rows,
            "data_default": m.get("data_default") or row.get("data_default") or "",
            "port": m.get("port") or row.get("port"),
        })
    return out


def check_updates(cfg: dict, *, force: bool = False) -> dict:
    """插件 + 本体的更新总账。前端只要这一份。

    本体那条不在这里查（about.py 有节流的后台线程），这里只读它当前的状态，
    免得工作台每次轮询都去打一次 GitHub。
    """
    cache = load_cache()
    now = time.time()
    items = []
    for pk in installed_packs(cfg):
        rec = cache.get(pk["id"]) or {}
        fresh = bool(rec) and (now - float(rec.get("checked_at") or 0) < CACHE_TTL)
        if force or not fresh:
            mm = find_manifest_dict(plugins_dir(cfg) / pk["id"] / MANIFEST)
            try:
                ver, notes, url = _latest_online({**mm, **pk})
                rec = {"checked_at": now, "latest": ver, "notes": notes, "url": url,
                       "error": ""}
            except Exception as exc:                    # noqa: BLE001
                rec = {**rec, "checked_at": now, "error": str(exc)[:160]}
        cache[pk["id"]] = rec
        latest = str(rec.get("latest") or "")
        has = bool(latest) and _version_key(latest) > _version_key(pk["version"])
        items.append({
            **pk,
            "latest": latest,
            "has_update": has,
            "notes": rec.get("notes") or "",
            "page": rec.get("url") or "",
            "error": rec.get("error") or "",
            "checked_at": rec.get("checked_at") or 0,
        })
    save_cache(cache)
    return {
        "ok": True,
        "checked_at": now,
        "plugins": items,
        "updates": sum(1 for x in items if x["has_update"]),
    }


def find_manifest_dict(p: Path) -> dict:
    try:
        return _read_manifest(p)
    except PackError:
        return {}


def upgrade(cfg: dict, pid: str) -> dict:
    """按清单里记的来源重装一遍（版本从远端下）。

    三条取包地址的路，按「掌握的信息多少」排：

      1. update.pack / update.zip —— 作者写清楚了，直接用
      2. 从 update.manifest 推断 —— manifest 和包通常放在同一个目录
         （站点根上摆 plugin.json + <id>.moshanplug 是最省事的发布方式）。
         **这条是为老版本准备的**：1.0.0 的 plugin.json 里只有 `"update": "git pull"`，
         没有 pack 字段，不给它一条推断路径，第一批用户永远升不上来。
      3. repo 的 Release 资产 —— 作者按约定挂 <id>.moshanplug
    """
    row = find_row(cfg, pid)
    if not row:
        raise PackError("清单里没有这个插件")
    upd = row.get("update")
    if not isinstance(upd, dict):
        upd = {}
    pack_url = str(upd.get("pack") or upd.get("zip") or "").strip()

    if not pack_url:
        man = str(upd.get("manifest") or "").strip()
        if not man:                                     # 上手时还没查过新版，读一次缓存
            man = str((load_cache().get(pid) or {}).get("url") or "").strip()
        if man and re.search(r"/[^/]+\.json$", man):
            pack_url = man.rsplit("/", 1)[0] + f"/{pid}.moshanplug"

    if not pack_url:
        repo = re.sub(r"^https?://github\.com/", "",
                      str(upd.get("repo") or row.get("repo") or "")).strip("/")
        if repo:
            pack_url = f"https://github.com/{repo}/releases/latest/download/{pid}.moshanplug"

    if not pack_url:
        raise PackError("这个版本里没写更新地址，自己下不了新版 —— 把新包拖进中台就行")
    try:
        res = install_url(cfg, pack_url)
    except PackError as exc:
        raise PackError(f"{exc}（下载地址：{pack_url}）") from exc
    res["upgraded"] = True
    return res


# ---------------------------------------------------------------- 命令行
#
#   python moshan/pluginpack.py inspect a.moshanplug
#   python moshan/pluginpack.py install a.moshanplug --root ~/Documents/AI项目
#   python moshan/pluginpack.py list --root …
#   python moshan/pluginpack.py updates --root …
# 界面走的是同一套函数；这个壳是给发版后自测、或没有图形界面时用的。

def _main(argv: list[str] | None = None) -> int:
    import argparse

    ap = argparse.ArgumentParser(prog="pluginpack", description="墨山插件包：看 / 装 / 列 / 查更新")
    ap.add_argument("cmd", choices=["inspect", "install", "list", "updates", "uninstall"])
    ap.add_argument("target", nargs="?", default="", help="包路径 / id")
    ap.add_argument("--root", default=str(_data_root()), help="数据根")
    ap.add_argument("--url", default="", help="install 时给网址，让中台自己去下")
    args = ap.parse_args(argv)
    cfg = {"allowed_roots": [args.root]}

    try:
        if args.cmd == "inspect":
            print(json.dumps(inspect_zip(Path(args.target)), ensure_ascii=False, indent=1))
        elif args.cmd == "install":
            r = install_url(cfg, args.url) if args.url else install_dir(cfg, Path(args.target))
            print(json.dumps(r, ensure_ascii=False, indent=1))
        elif args.cmd == "list":
            print(json.dumps(installed_packs(cfg), ensure_ascii=False, indent=1))
        elif args.cmd == "updates":
            print(json.dumps(check_updates(cfg, force=True), ensure_ascii=False, indent=1))
        else:
            print(json.dumps(uninstall(cfg, args.target), ensure_ascii=False, indent=1))
    except PackError as exc:
        print(f"失败：{exc}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(_main())
