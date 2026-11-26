#!/usr/bin/env python3
"""w7b 写族对拍：更新状态机 / 传输队列 / 主机推送（落盘也一起比）。

为什么不用 `scripts/rust-parity.py --suite`：那条是按 W3/W4/W6 的步骤清单写死的，
公共脚本不许改。这里照它的形状自带一份，只跑 w7b 的 18 条路由。

三条安全约束（很重要）：
  1. `/api/update/apply` 的成功分支**不跑**：Rust 侧迁移期直接 500 拒绝，
     Python 侧跑下去会改写 `moshan/*.py`（对照组）。
  2. 传输的「网页 → 微信推送」**不触发真发送**：Python 侧从一份临时副本启动，
     副本里的 `wechat_push.py` 被替换成永不发送的桩；Rust 侧本来就没实现推送。
  3. `/api/update/push` 的落盘对比在两份临时程序目录里做，绝不指向仓库。

用法：
    python3 parity_w7b.py            # 两轮：正常仓库 / 不存在的仓库；再做推送落盘
    python3 parity_w7b.py --keep     # 保留临时目录
"""
from __future__ import annotations

import hashlib
import importlib.util
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
import zipfile
from email.utils import formatdate
from pathlib import Path

ROOT = Path(__file__).resolve().parents[5]  # rust/moshan-server/src/routes/w7b → 仓库根
PARITY = ROOT / "scripts/rust-parity.py"
RS_BIN = "/tmp/rs-w7b/release/moshan-server"
PYTHON = os.environ.get("MOSHAN_PY", str(Path.home() / "Library/Caches/moshan-build-venv/bin/python"))


def load_parity():
    spec = importlib.util.spec_from_file_location("rust_parity", PARITY)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


rp = load_parity()
TOKEN = rp.PARITY_TOKEN


# ---------------------------------------------------------------- 归一化

_NORMS = [
    (re.compile(rb"tf\d{12}[0-9a-f]{4}"), b"<TFID>"),
    (re.compile(rb"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:[+-]\d{2}:\d{2})?"), b"<TS>"),
    (re.compile(rb'"checked_at":-?\d+(?:\.\d+)?'), b'"checked_at":<EPOCH>'),
    (re.compile(rb'"(seconds|rate_mb_s)":-?\d+(?:\.\d+)?'), rb'"\1":<N>'),
    (re.compile(rb"\d{8}-\d{6}"), b"<STAMP>"),
    (re.compile(rb"apply-\d+"), b"apply-<TS>"),
    (re.compile(rb"[0-9a-f]{40}"), b"<SHA1>"),
]


def norm(raw: bytes, home: Path, app: Path) -> bytes:
    b = raw.replace(str(home).encode(), b"<HOME>")
    b = b.replace(str(home.resolve()).encode(), b"<HOME>")
    b = b.replace(str(app).encode(), b"<APP>")
    for rx, rep in _NORMS:
        b = rx.sub(rep, b)
    return b


def first_diff(a: bytes, b: bytes) -> str:
    return rp.first_diff(a, b)


# ---------------------------------------------------------------- 起服务

def free_port() -> int:
    for p in range(8900, 8999):
        with socket.socket() as s:
            try:
                s.bind(("127.0.0.1", p))
                return p
            except OSError:
                continue
    raise SystemExit("no port")


STUB_WECHAT = '''"""对拍用桩：绝不真发微信。"""
CHANNEL = "openclaw-weixin"
URL = "ws://127.0.0.1:18789"
LAST_ERROR = "对拍桩：不发微信"

def send(*a, **k):
    return False

def send_media(*a, **k):
    return False
'''


def copy_app(tag: str, stub: bool) -> Path:
    d = Path(tempfile.mkdtemp(prefix=f"w7b-app-{tag}-"))
    shutil.copytree(ROOT / "moshan", d / "app", ignore=shutil.ignore_patterns("__pycache__"))
    if stub:
        (d / "app" / "wechat_push.py").write_text(STUB_WECHAT, encoding="utf-8")
    return d / "app"


def make_home(tag: str, extra_cfg: dict | None = None, with_pkg: bool = False) -> Path:
    home = Path(tempfile.mkdtemp(prefix=f"w7b-home-{tag}-"))
    rp.seed(home)
    cfg_path = home / "state/config.json"
    cfg = json.loads(cfg_path.read_text(encoding="utf-8"))
    if extra_cfg:
        cfg.update(extra_cfg)
    cfg_path.write_text(json.dumps(cfg, ensure_ascii=False, indent=2), encoding="utf-8")
    if with_pkg:
        d = home / "state/updates"
        d.mkdir(parents=True, exist_ok=True)
        (d / "墨山-0.3.14.dmg").write_bytes(b"not a real dmg")
    return home


def start_py(home: Path, app: Path) -> tuple[subprocess.Popen, int]:
    env = {**os.environ, "MOSHAN_HOME": str(home), "MOSHAN_NO_BROWSER": "1"}
    p = subprocess.Popen([PYTHON, str(app / "launcher.py")], env=env, cwd=str(app),
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    cfg_path = home / "state/config.json"
    port = 0
    for _ in range(160):
        try:
            cfg = json.loads(cfg_path.read_text(encoding="utf-8"))
        except Exception:  # noqa: BLE001
            cfg = {}
        port = cfg.get("local_port") or 0
        if port and cfg.get("token"):
            break
        time.sleep(0.25)
    return p, port


def start_rs(home: Path, app: Path) -> tuple[subprocess.Popen, int]:
    port = free_port()
    env = {**os.environ, "MOSHAN_HOME": str(home), "MOSHAN_RS_PORT": str(port),
           "MOSHAN_PY_UPSTREAM": "", "MOSHAN_APP_DIR": str(app)}
    p = subprocess.Popen([RS_BIN], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    return p, port


def wait_ping(port: int, timeout: float = 90.0) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/api/ping", timeout=2) as r:
                if r.status == 200:
                    return True
        except Exception:  # noqa: BLE001
            pass
        time.sleep(0.4)
    return False


def call(port: int, method: str, path: str, body=None, timeout: float = 90) -> tuple[int, bytes]:
    if isinstance(body, dict) and "__files__" in body:
        data, ctype = rp._multipart(body["__files__"])  # noqa: SLF001
    else:
        data = None if body is None else json.dumps(body, ensure_ascii=False).encode()
        ctype = "application/json"
    req = urllib.request.Request(
        f"http://127.0.0.1:{port}{path}", data=data, method=method,
        headers={"X-Token": TOKEN, "Content-Type": ctype},
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()
    except Exception as e:  # noqa: BLE001
        return 0, str(e).encode()


# ---------------------------------------------------------------- 步骤

def update_steps() -> list:
    return [
        ("状态快照(初始)", "GET", "/api/update/state", None),
        ("没新版就下载", "POST", "/api/update/download", None),
        ("未校验包直接装", "POST", "/api/update/apply", None),
        ("强制但仍没查过", "POST", "/api/update/apply?force=1", None),
        ("切回 github", "POST", "/api/update/mode", {"mode": "github"}),
        ("检查更新(真网)", "POST", "/api/update/check", None),
        ("检查后的状态", "GET", "/api/update/state", None),
        ("切成 push", "POST", "/api/update/mode", {"mode": "push"}),
        ("push 模式再检查", "POST", "/api/update/check", None),
        ("push 模式状态", "GET", "/api/update/state", None),
        ("非法 mode", "POST", "/api/update/mode", {"mode": "bad"}),
        ("空 body 的 mode", "POST", "/api/update/mode", {}),
        ("没 body 的 mode", "POST", "/api/update/mode", None),
        ("非对象的 mode", "POST", "/api/update/mode", [1]),
        ("push 模式装包", "POST", "/api/update/apply", None),
        ("更新总账", "GET", "/api/updates", None),
        ("推送缺 version", "POST", "/api/update/push", {"version": ""}),
        ("推送缺 sha", "POST", "/api/update/push", {"version": "1.0.0"}),
        ("推送 sha 不匹配", "POST", "/api/update/push",
         {"version": "1.0.0", "sha256": "0" * 64, "b64": "aGk="}),
        ("推送非对象", "POST", "/api/update/push", [1]),
        ("推送空 body", "POST", "/api/update/push", None),
        ("推送状态", "GET", "/api/update/status", None),
        ("回滚(无备份)", "POST", "/api/update/rollback", {}),
        ("回滚 restart=0", "POST", "/api/update/rollback", {"restart": False}),
        ("最终状态", "GET", "/api/update/state", None),
    ]


def transfer_steps(state: dict) -> list:
    return [
        ("传输空队列", "GET", "/api/transfer", None),
        ("加链接", "POST", "/api/transfer/add",
         {"text": "看这个 https://example.com/a?b=1 ，不错", "push": False}),
        ("微信来源文本", "POST", "/api/transfer/add",
         {"text": "微信发来的", "src": "wechat", "conv": "文件传输助手"}),
        ("加文件", "POST", "/api/transfer/add",
         {"path": "{target}", "name": "报告 2026.txt", "mime": "text/plain",
          "push": False, "src": "web"}),
        ("空消息", "POST", "/api/transfer/add", {"push": False}),
        ("文件不存在", "POST", "/api/transfer/add", {"path": "{inbox}/没有这个"}),
        ("非对象 body", "POST", "/api/transfer/add", [1]),
        ("没 body", "POST", "/api/transfer/add", None),
        ("队列一眼", "GET", "/api/transfer", None),
        ("只看 2 条", "GET", "/api/transfer?limit=2", None),
        ("搜报告", "GET", "/api/transfer?q=%E6%8A%A5%E5%91%8A", None),
        ("只看链接", "GET", "/api/transfer?kind=link", None),
        ("limit 非法", "GET", "/api/transfer?limit=x", None),
        ("传两个文件", "MULTIPART", "/api/transfer/upload",
         {"__files__": [("a.txt", b"hello"), ("照片.png", b"\x89PNG\r\n\x1a\n" + b"1" * 64)]}),
        ("下载原文件", "GET", "/api/transfer/file?id={tf_file}&dl=1", None),
        ("缩略图", "GET", "/api/transfer/file?id={tf_img}&w=120", None),
        ("文件缺 id", "GET", "/api/transfer/file", None),
        ("文件空 id", "GET", "/api/transfer/file?id=", None),
        ("文件没见过", "GET", "/api/transfer/file?id=tf000", None),
        ("删一条", "POST", "/api/transfer/del", {"id": "{tf_file}"}),
        ("删不存在的", "POST", "/api/transfer/del", {"id": "tf000"}),
        ("重推两条", "POST", "/api/transfer/repush",
         {"ids": ["{tf_file}", "{tf_img}", "tf000"]}),
        ("删后队列", "GET", "/api/transfer", None),
    ]


def _get_headers(port: int, path: str, extra: dict | None = None) -> tuple[int, dict, bytes]:
    req = urllib.request.Request(
        f"http://127.0.0.1:{port}{path}", method="GET",
        headers={"X-Token": TOKEN, **(extra or {})},
    )
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return r.status, {k.lower(): v for k, v in r.headers.items()}, r.read()
    except urllib.error.HTTPError as e:
        return e.code, {k.lower(): v for k, v in e.headers.items()}, e.read()
    except Exception as e:  # noqa: BLE001
        return 0, {}, str(e).encode()


def _stat_file(home: Path, iid: str):
    # 落盘在 `14-传输/文件/<YYYY-MM>/<id>_<原名>`
    for p in (home / "library/14-传输/文件").glob(f"*/{iid}_*"):
        return p
    return None


def compare_file_headers(sides: dict, states: dict) -> int:
    """`/api/transfer/file` 的响应头。

    Python 侧是 Starlette 0.36.3 的 `FileResponse`，实测三处细节必须照抄：
    `text/*` 补 `; charset=utf-8`、**没有** `accept-ranges`、带
    `last-modified` / `etag`，且 `Range` 请求**不生效**（还是 200 全量）。
    etag 是「文件自身 mtime+size」的 md5，两侧文件是各自落盘的（mtime 必然
    不同），所以只能各自对着自己的 stat 校验公式，不能横向比字符串。
    """
    keys = ("content-type", "content-disposition", "cache-control",
            "content-length", "accept-ranges")
    probes = [
        ("下载txt头", "tf_txt", "&dl=1", None),
        ("下载图片头", "tf_img", "&dl=1", None),
        ("Range 不生效", "tf_txt", "&dl=1", {"Range": "bytes=0-1"}),
    ]
    got = {}
    for side in ("py", "rs"):
        port, _home, _app = sides[side]
        st = states[side]
        got[side] = [
            _get_headers(port, f"/api/transfer/file?id={st.get(key, '')}{tail}", extra)
            for _n, key, tail, extra in probes
        ]
    bad = 0
    for i, (note, _k, _t, _e) in enumerate(probes):
        ca, ha, ba = got["py"][i]
        cb, hb, bb = got["rs"][i]
        va = (ca, {k: ha.get(k, "<缺>") for k in keys}, ba)
        vb = (cb, {k: hb.get(k, "<缺>") for k in keys}, bb)
        if va == vb:
            print(f"{note:22s} {ca:4d} {cb:4d}  一致")
        else:
            bad += 1
            print(f"{note:22s} {ca:4d} {cb:4d}  差异")
            print(f"    py={va[1]} len={len(ba)}\n    rs={vb[1]} len={len(bb)}")
    for side in ("py", "rs"):
        port, home, _app = sides[side]
        iid = states[side].get("tf_txt", "")
        p = _stat_file(home, iid)
        _code, hdrs, _body = _get_headers(port, f"/api/transfer/file?id={iid}&dl=1")
        if p is None:
            bad += 1
            print(f"{'etag(' + side + ')':22s} 差异：找不到 {iid} 的文件")
            continue
        s = p.stat()
        want_etag = '"' + hashlib.md5(f"{s.st_mtime}-{s.st_size}".encode()).hexdigest() + '"'
        want_lm = formatdate(s.st_mtime, usegmt=True)
        ok = hdrs.get("etag") == want_etag and hdrs.get("last-modified") == want_lm
        if not ok:
            bad += 1
        print(f"{'etag/时间(' + side + ')':22s} {'':4s} {'':4s}  {'一致' if ok else '差异'}"
              + ("" if ok else f"\n    得到 etag={hdrs.get('etag')} lm={hdrs.get('last-modified')}"
                                f"\n    应为 etag={want_etag} lm={want_lm}"))
    return bad


def _drop_push(b: bytes):
    try:
        v = json.loads(b)
    except Exception:  # noqa: BLE001
        return None
    if isinstance(v, dict):
        v.pop("push", None)
    return json.dumps(v, ensure_ascii=False, sort_keys=True)


def _subst_str(v: str, state: dict) -> str:
    return (v.replace("{tf_file}", state.get("tf_file", ""))
             .replace("{tf_img}", state.get("tf_img", ""))
             .replace("{target}", str(state.get("target", "")))
             .replace("{inbox}", str(state.get("inbox", ""))))


def _subst_val(v, state: dict):
    if isinstance(v, str):
        return _subst_str(v, state)
    if isinstance(v, dict):
        return {k: _subst_val(x, state) for k, x in v.items()}
    if isinstance(v, list):
        return [_subst_val(x, state) for x in v]
    return v


def run_round(name: str, extra_cfg: dict | None, with_pkg: bool, keep: bool) -> int:
    print(f"\n===== {name} =====")
    py_app = copy_app(f"{name}-py", stub=True)
    rs_app = copy_app(f"{name}-rs", stub=True)
    py_home = make_home(f"{name}-py", extra_cfg, with_pkg)
    rs_home = make_home(f"{name}-rs", extra_cfg, with_pkg)
    target = py_home / "library/收件箱/报告.md"
    procs = []
    try:
        py, py_port = start_py(py_home, py_app)
        procs.append(py)
        rs, rs_port = start_rs(rs_home, rs_app)
        procs.append(rs)
        if not py_port or not wait_ping(py_port):
            print("Python 侧没起来")
            return 2
        if not wait_ping(rs_port):
            print("Rust 侧没起来", (rs.stderr.read().decode()[:400] if rs.stderr else ""))
            return 2

        # 每个条目 id 是各自进程随机生成的，不能把 py 的 id 发给 rs；
        # 所以两边各维护一份 state，替换各自的值，再比归一后的响应。
        states = {
            "py": {"inbox": py_home / "library/收件箱", "target": target},
            "rs": {"inbox": rs_home / "library/收件箱", "target": rs_home / "library/收件箱/报告.md"},
        }
        sides = {"py": (py_port, py_home, py_app), "rs": (rs_port, rs_home, rs_app)}
        steps = list(update_steps())
        if name.startswith("A"):
            steps += transfer_steps(states["py"])
        bad = 0
        print(f"{'步骤':22s} {'py':>4s} {'rs':>4s}  结果")
        for step in steps:
            note, method, path, body = step[:4]
            http_method = "POST" if method == "MULTIPART" else method
            got = {}
            for side in ("py", "rs"):
                port, _h, _a = sides[side]
                pth = _subst_str(path, states[side])
                b = _subst_val(body, states[side])
                got[side] = (port, http_method, pth, b)
            sa, ra = call(*got["py"])
            sb, rb = call(*got["rs"])
            ra_n = norm(ra, py_home, py_app)
            rb_n = norm(rb, rs_home, rs_app)
            ok = sa == sb and ra_n == rb_n
            note_extra = ""
            if not ok and path.startswith("/api/transfer"):
                # `push` 是「网页 → 微信」的推送账本：Python 侧真去连网关（本轮用桩），
                # Rust 侧没有 WebSocket 客户端。这是已知分叉，不把它算成迁移差异。
                ja = _drop_push(ra_n)
                jb = _drop_push(rb_n)
                if ja is not None and ja == jb:
                    ok = True
                    note_extra = "（push 账本差异=已知分叉）"
            if not ok:
                bad += 1
            print(f"{note:22s} {sa:4d} {sb:4d}  {'一致' if ok else '差异'}{note_extra}")
            if not ok:
                print("    " + first_diff(ra_n, rb_n))
            for side, raw in (("py", ra), ("rs", rb)):
                try:
                    js = json.loads(raw)
                except Exception:  # noqa: BLE001
                    js = {}
                if note == "加文件" and isinstance(js, dict):
                    states[side]["tf_file"] = (js.get("item") or {}).get("id", "")
                if note == "传两个文件" and isinstance(js, dict):
                    for it in js.get("items") or []:
                        if it.get("kind") == "image":
                            states[side]["tf_img"] = it.get("id", "")
                        elif it.get("kind") == "file":
                            states[side]["tf_file"] = it.get("id", "")
                            states[side]["tf_txt"] = it.get("id", "")
            # 文件头要在「删一条」之前查；此时 txt 与图片都还在。
            if note == "缩略图" and name.startswith("A"):
                bad += compare_file_headers(sides, states)

        if name.startswith("A"):
            bad += compare_transfer_disk(py_home, rs_home)
        bad += compare_config(py_home, rs_home)
        print(f"\n{name}：实质差异 {bad}")
        return 0 if bad == 0 else 1
    finally:
        for p in procs:
            p.terminate()
        for p in procs:
            try:
                p.wait(timeout=10)
            except Exception:  # noqa: BLE001
                p.kill()
        if not keep:
            for d in (py_app.parent, rs_app.parent, py_home, rs_home):
                shutil.rmtree(d, ignore_errors=True)


def _load_jsonl(p: Path) -> list:
    if not p.exists():
        return []
    out = []
    for ln in p.read_text(encoding="utf-8").splitlines():
        ln = ln.strip()
        if not ln:
            continue
        v = json.loads(ln)
        out.append(_mask_obj(v))
    return out


def _mask_obj(v):
    if isinstance(v, dict):
        return {k: ("<TS>" if k == "at" else "<TFID>" if k == "id" and str(x).startswith("tf")
                    else "<PATH>" if k == "path" else _mask_obj(x))
                for k, x in v.items()}
    if isinstance(v, list):
        return [_mask_obj(x) for x in v]
    return v


def compare_transfer_disk(py_home: Path, rs_home: Path) -> int:
    """消息.jsonl + 收到的文件（不含推送账本：Python 侧有后台重试行，Rust 不推）。"""
    if os.environ.get("W7B_DEBUG"):
        for h in (py_home, rs_home):
            p = h / "library/14-传输/消息.jsonl"
            print("DBG", h, p.exists(), p.stat().st_size if p.exists() else -1)
    a = _load_jsonl(py_home / "library/14-传输/消息.jsonl")
    b = _load_jsonl(rs_home / "library/14-传输/消息.jsonl")
    bad = 0
    if a == b:
        print(f"{'消息.jsonl':22s} 一致")
    else:
        bad += 1
        print(f"{'消息.jsonl':22s} 差异")
        print("    " + first_diff(
            json.dumps(a, ensure_ascii=False, sort_keys=True).encode(),
            json.dumps(b, ensure_ascii=False, sort_keys=True).encode()))
    fa = _file_map(py_home)
    fb = _file_map(rs_home)
    if fa == fb:
        print(f"{'14-传输/文件':22s} 一致")
    else:
        bad += 1
        print(f"{'14-传输/文件':22s} 差异\n    py={fa}\n    rs={fb}")
    return bad


def _file_map(home: Path) -> dict:
    d = home / "library/14-传输/文件"
    if not d.is_dir():
        return {}
    out = {}
    rx = re.compile(r"tf\d{12}[0-9a-f]{4}_")
    for p in sorted(d.rglob("*")):
        if p.is_file():
            rel = str(p.relative_to(d))
            rel = rx.sub("<TFID>_", rel)
            out[rel] = (p.stat().st_size, hashlib.sha256(p.read_bytes()).hexdigest())
    return out


def compare_config(py_home: Path, rs_home: Path) -> int:
    """只比 config 的 `update` 子树。

    `port`/`local_port` 是 launcher 启动时补的（Python 有、Rust 侧没有），
    那是启动器职责，不是 `/api/update/mode` 的契约；`update.mode` 才是这条
    路由写的东西，必须逐字节一致。
    """
    try:
        ca = json.loads((py_home / "state/config.json").read_text(encoding="utf-8")).get("update")
    except Exception:  # noqa: BLE001
        ca = None
    try:
        cb = json.loads((rs_home / "state/config.json").read_text(encoding="utf-8")).get("update")
    except Exception:  # noqa: BLE001
        cb = None
    a = json.dumps(ca, ensure_ascii=False, indent=1).encode()
    b = json.dumps(cb, ensure_ascii=False, indent=1).encode()
    if a == b:
        print(f"{'config.update':22s} 一致")
        return 0
    print(f"{'config.update':22s} 差异")
    print("    " + first_diff(a, b))
    return 1


# ---------------------------------------------------------------- 推送落盘

def make_zip(path: Path) -> bytes:
    buf = path
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr("server.py", "print('hi')\n")
        z.writestr("version.py", 'VERSION = "9.9.9"\n')
        z.writestr("VERSION", "9.9.9\n")
        z.writestr("ui.html", "<html>ok</html>")
        z.writestr("pkg/mod.py", "x = 1\n")
        z.writestr("evil.exe", b"MZ")
        z.writestr("../逃逸.txt", b"bad")
    return buf.read_bytes()


def fake_app(tag: str) -> Path:
    d = Path(tempfile.mkdtemp(prefix=f"w7b-push-{tag}-"))
    (d / "app").mkdir()
    (d / "app/server.py").write_text("print('old')\n", encoding="utf-8")
    (d / "app/version.py").write_text('VERSION = "0.3.13"\n', encoding="utf-8")
    (d / "app/ui.html").write_text("<html>old</html>", encoding="utf-8")
    (d / "app/__pycache__").mkdir()
    (d / "app/__pycache__/x.pyc").write_bytes(b"junk")
    (d / "app/old.log").write_text("log", encoding="utf-8")
    return d / "app"


def run_push_round(keep: bool) -> int:
    print("\n===== C 主机推送落盘 =====")
    _fd, _tmpzip = tempfile.mkstemp(suffix=".zip")
    os.close(_fd)
    zip_bytes = make_zip(Path(_tmpzip))
    sha = hashlib.sha256(zip_bytes).hexdigest()
    body = {"version": "v9.9.9", "sha256": sha,
            "b64": __import__("base64").b64encode(zip_bytes).decode(),
            "name": "moshan-9.9.9.zip", "restart": False}
    py_app = copy_app("push-py", stub=True)
    rs_app = copy_app("push-rs", stub=True)
    for a in (py_app, rs_app):
        (a / "server.py").write_text("print('old')\n", encoding="utf-8")
        (a / "version.py").write_text('VERSION = "0.3.13"\n', encoding="utf-8")
        (a / "ui.html").write_text("<html>old</html>", encoding="utf-8")
        (a / "__pycache__").mkdir(exist_ok=True)
        (a / "__pycache__/x.pyc").write_bytes(b"junk")
        (a / "old.log").write_text("log", encoding="utf-8")
    py_home = Path(tempfile.mkdtemp(prefix="w7b-pushhome-py-"))
    rs_home = Path(tempfile.mkdtemp(prefix="w7b-pushhome-rs-"))
    (py_home / "state").mkdir(parents=True)
    (rs_home / "state").mkdir(parents=True)
    bad = 0
    # Python：直接调 pushupd.push（app_dir = 临时副本，安全）
    code = (
        "import json,sys,pushupd;"
        "b=json.load(open(sys.argv[1],encoding='utf-8'));"
        "print(json.dumps(pushupd.push(b),ensure_ascii=False,sort_keys=True))"
    )
    (py_home / "body.json").write_text(json.dumps(body), encoding="utf-8")
    env = {**os.environ, "MOSHAN_HOME": str(py_home)}
    r = subprocess.run([PYTHON, "-c", code, str(py_home / "body.json")],
                       cwd=str(py_app), env=env, capture_output=True, text=True)
    if r.returncode != 0:
        print("Python push 失败：", r.stderr[:600])
        return 2
    py_out = json.loads(r.stdout)

    # Rust：起服务走 HTTP（app_dir 同样是临时副本）
    port = free_port()
    proc = subprocess.Popen([RS_BIN], env={**os.environ, "MOSHAN_HOME": str(rs_home),
                                           "MOSHAN_RS_PORT": str(port), "MOSHAN_PY_UPSTREAM": "",
                                           "MOSHAN_APP_DIR": str(rs_app)},
                            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    try:
        if not wait_ping(port):
            print("Rust 侧没起来")
            return 2
        st, raw = call(port, "POST", "/api/update/push", body)
        rs_out = json.loads(raw)
        a = json.dumps(py_out, ensure_ascii=False, sort_keys=True)
        b = json.dumps(rs_out, ensure_ascii=False, sort_keys=True)
        a = a.replace(str(py_app), "<APP>").replace(str(py_home), "<HOME>")
        b = b.replace(str(rs_app), "<APP>").replace(str(rs_home), "<HOME>")
        a = re.sub(r"\d{8}-\d{6}", "<STAMP>", a)
        b = re.sub(r"\d{8}-\d{6}", "<STAMP>", b)
        if a == b:
            print(f"{'push 响应':22s} 一致（{st}）")
        else:
            bad += 1
            print(f"{'push 响应':22s} 差异（{st}）")
            print("    " + first_diff(a.encode(), b.encode()))

        # current.json
        ca = (py_home / "state/updates/current.json").read_text(encoding="utf-8")
        cb = (rs_home / "state/updates/current.json").read_text(encoding="utf-8")
        ca = ca.replace(str(py_app), "<APP>").replace(str(py_home), "<HOME>")
        cb = cb.replace(str(rs_app), "<APP>").replace(str(rs_home), "<HOME>")
        ca = re.sub(r"\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}", "<TS>", ca)
        cb = re.sub(r"\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}", "<TS>", cb)
        ca = re.sub(r"\d{8}-\d{6}", "<STAMP>", ca)
        cb = re.sub(r"\d{8}-\d{6}", "<STAMP>", cb)
        if ca == cb:
            print(f"{'current.json':22s} 一致")
        else:
            bad += 1
            print(f"{'current.json':22s} 差异")
            print("    " + first_diff(ca.encode(), cb.encode()))

        # 解包后的程序目录
        ta = dir_map(py_app)
        tb = dir_map(rs_app)
        if ta == tb:
            print(f"{'解包目录':22s} 一致（{len(ta)} 个文件）")
        else:
            bad += 1
            print(f"{'解包目录':22s} 差异")
            print("    py-rs:", set(ta.items()) ^ set(tb.items()))

        # 回滚：两边都回到 0.3.13
        rs2 = call(port, "POST", "/api/update/rollback", {"restart": False})[1]
        r = subprocess.run([PYTHON, "-c",
                            "import json,pushupd;print(json.dumps(pushupd.rollback(restart=False),ensure_ascii=False,sort_keys=True))"],
                           cwd=str(py_app), env=env, capture_output=True, text=True)
        py2 = json.loads(r.stdout)
        rs2v = json.loads(rs2)
        a = json.dumps(py2, ensure_ascii=False, sort_keys=True).replace(str(py_app), "<APP>").replace(str(py_home), "<HOME>")
        b = json.dumps(rs2v, ensure_ascii=False, sort_keys=True).replace(str(rs_app), "<APP>").replace(str(rs_home), "<HOME>")
        if a == b:
            print(f"{'rollback 响应':22s} 一致")
        else:
            bad += 1
            print(f"{'rollback 响应':22s} 差异")
            print("    " + first_diff(a.encode(), b.encode()))
        if (py_app / "version.py").read_text() == (rs_app / "version.py").read_text():
            print(f"{'回滚后 version.py':22s} 一致")
        else:
            bad += 1
            print(f"{'回滚后 version.py':22s} 差异")
        print(f"\nC：实质差异 {bad}")
        return 0 if bad == 0 else 1
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except Exception:  # noqa: BLE001
            proc.kill()
        if not keep:
            for d in (py_app.parent, rs_app.parent, py_home, rs_home):
                shutil.rmtree(d, ignore_errors=True)


def dir_map(app: Path) -> dict:
    out = {}
    for p in sorted(app.rglob("*")):
        if p.is_file():
            rel = str(p.relative_to(app))
            if rel.startswith("__pycache__") or rel.endswith(".pyc") or rel.endswith(".log"):
                continue
            out[rel] = hashlib.sha256(p.read_bytes()).hexdigest()
    return out


def main() -> int:
    keep = "--keep" in sys.argv
    bad = 0
    bad += run_round("A 正常仓库", {}, with_pkg=True, keep=keep)
    bad += run_round("B 发布方不存在", {"update": {"repo": "blackinkvc/moshan-not-here-xyz"}},
                     with_pkg=False, keep=keep)
    bad += run_push_round(keep)
    print(f"\n合计实质差异 {bad}")
    return 0 if bad == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
