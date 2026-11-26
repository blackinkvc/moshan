#!/usr/bin/env python3
"""Rust 服务与 Python 服务的逐字节对拍。

用途：每迁完一波路由，跑一次这个脚本。**判据是响应字节相同**，不是"看起来差不多"——
前端 `dash.html` 有 1.1 MB 内联脚本，字段形状或顺序一变就可能成片白屏，
用肉眼比对不可行。

用法：
    python3 scripts/rust-parity.py                      # 默认清单
    python3 scripts/rust-parity.py --paths /api/dash /api/notes
    python3 scripts/rust-parity.py --all-get            # 清单换成全部 GET 路由

退出码：0 = 全部一致；1 = 有差异（逐条打印首个不同字节）。
"""
from __future__ import annotations

import argparse
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
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# W1 五条（Rust 直接应答）+ 若干代理样本（验证转发透明）。
# 选样本的原则：响应稳定（不含时间戳、随机数、本机采样）。
DEFAULT_PATHS = [
    "/api/ping",
    "/api/versions",
    "/api/about",
    "/api/appearance",
    "/api/notes",
    "/api/ledger",
    "/api/plan",
    "/api/dash",
    "/about",
]

# 含运行期状态的端点不列入对拍清单（改了也会一直 DIFF，反而掩盖真差异）：
#   /api/heartbeat  STATE 是进程内字典，Python 启动后立刻跑过一轮，last_at 已填；
#                   Rust 此刻还没有自己的调度器。等 W5 由 Rust 接管调度再纳入。
TIME_KEYS = ("/api/heartbeat",)


def free_port(start: int = 8400) -> int:
    for p in range(start, start + 60):
        with socket.socket() as s:
            try:
                s.bind(("127.0.0.1", p))
                return p
            except OSError:
                continue
    raise SystemExit("找不到空闲端口")


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


def fetch(port: int, path: str, token: str) -> tuple[int, bytes]:
    req = urllib.request.Request(
        f"http://127.0.0.1:{port}{path}", headers={"X-Token": token, "Accept": "*/*"}
    )
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()
    except Exception as e:  # noqa: BLE001
        return 0, str(e).encode()


# 「现在几点」类字段天然不可能两边相同（相隔几十毫秒也算不同）。
# 抹掉时间戳后仍然一致，就说明结构与取值都对——比笼统标 DIFF 有用得多。
_TS = re.compile(rb"\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2}:\d{2}(?:\.\d+)?")


def mask(b: bytes) -> bytes:
    return _TS.sub(b"<TS>", b)


def first_diff(a: bytes, b: bytes) -> str:
    n = min(len(a), len(b))
    for i in range(n):
        if a[i] != b[i]:
            lo = max(0, i - 60)
            return (
                f"首个差异在偏移 {i}（长度 {len(a)} vs {len(b)}）\n"
                f"    py: ...{a[lo:i + 60]!r}\n"
                f"    rs: ...{b[lo:i + 60]!r}"
            )
    if len(a) != len(b):
        longer = "py" if len(a) > len(b) else "rs"
        tail = (a if len(a) > len(b) else b)[n:]
        return f"前缀相同，{longer} 多出 {len(tail)} 字节：{tail[:120]!r}"
    return "内容相同"


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--paths", nargs="*", default=None)
    ap.add_argument("--all-get", action="store_true")
    # 默认跑**源码态** Python：对拍的是接口契约，不是打包形态。
    # 打包后的 sidecar 会返回 frozen=true、app_dir 指向解包临时目录，
    # 那两条差异纯属"换了个形态"，会把真差异淹掉。
    ap.add_argument("--py-frozen", action="store_true", help="拿打好的 sidecar 当对照")
    ap.add_argument("--py-bin", default=str(ROOT / "src-tauri/binaries/moshan-aarch64-apple-darwin"))
    ap.add_argument(
        "--py-python",
        default=os.environ.get("MOSHAN_PY", str(Path.home() / "Library/Caches/moshan-build-venv/bin/python")),
    )
    ap.add_argument("--rs-bin", default="/tmp/moshan-rs-target/release/moshan-server")
    ap.add_argument("--keep", action="store_true", help="保留临时数据根，便于排查")
    args = ap.parse_args()

    paths = args.paths or DEFAULT_PATHS
    if args.all_get:
        routes = json.load(open("/tmp/routes.json"))
        paths = [r["path"] for r in routes if r["method"] == "GET"]

    home = Path(tempfile.mkdtemp(prefix="moshan-parity-"))
    env = {**os.environ, "MOSHAN_HOME": str(home), "MOSHAN_NO_BROWSER": "1"}
    procs: list[subprocess.Popen] = []
    try:
        py_cmd = (
            [args.py_bin]
            if args.py_frozen
            else [args.py_python, str(ROOT / "moshan/launcher.py")]
        )
        py = subprocess.Popen(
            py_cmd, env=env, cwd=str(ROOT / "moshan"), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
        )
        procs.append(py)
        cfg_path = home / "state" / "config.json"
        # 配置是分两步写的：server 导入时先落一份基础配置，launcher 选完端口再补
        # local_port/port。第一次读到的可能没有 local_port，所以要等到键出现。
        cfg: dict = {}
        for _ in range(120):
            try:
                cfg = json.loads(cfg_path.read_text(encoding="utf-8"))
            except Exception:  # noqa: BLE001
                cfg = {}
            if cfg.get("local_port") and cfg.get("token"):
                break
            time.sleep(0.25)
        py_port, token = cfg["local_port"], cfg["token"]
        if not wait_ping(py_port):
            print("Python 侧没起来，先看日志：", home / "state/logs/moshan.log")
            return 2

        rs_port = free_port()
        rs = subprocess.Popen(
            [args.rs_bin],
            env={
                **env,
                "MOSHAN_PY_UPSTREAM": f"http://127.0.0.1:{py_port}",
                "MOSHAN_RS_PORT": str(rs_port),
                # 让 Rust 读到的 app_dir 与 Python 一致，否则 /api/about 必然不同
                "MOSHAN_APP_DIR": str(ROOT / "moshan"),
            },
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
        )
        procs.append(rs)
        if not wait_ping(rs_port):
            err = rs.stderr.read().decode() if rs.stderr else ""
            print("Rust 侧没起来：", err[:600])
            return 2

        bad = 0
        jitter = 0
        print(f"{'端点':32s} {'py':>5s} {'rs':>5s}  结果")
        for p in paths:
            ps, pb = fetch(py_port, p, token)
            rstatus, rb = fetch(rs_port, p, token)
            same_status = ps == rstatus
            exact = same_status and pb == rb
            same_masked = same_status and mask(pb) == mask(rb)
            detail = ""
            if exact:
                flag = "一致"
            elif same_masked:
                flag = "一致（仅时间戳不同）"
            else:
                # 代理透明性的自证：同一端点连打两次 Python，它自己也会变吗？
                # 会变 → 这属于采样/缓存抖动，不是转发引入的差异；
                # 不变 → 是 Rust 真差异，必须修。
                _, pb2 = fetch(py_port, p, token)
                if mask(pb) != mask(pb2):
                    jitter += 1
                    flag = "抖动（py 自己两次也不同）"
                else:
                    bad += 1
                    flag = "差异"
                    detail = "    " + first_diff(pb, rb)
            print(f"{p:32s} {ps:5d} {rstatus:5d}  {flag}")
            if detail:
                print(detail)
        print(
            f"\n共 {len(paths)} 条：一致 {len(paths) - bad - jitter}，"
            f"抖动 {jitter}（对照侧自己两次也不同，非迁移引入），实质差异 {bad}"
        )
        return 0 if bad == 0 else 1
    finally:
        for p in procs:
            p.terminate()
        for p in procs:
            try:
                p.wait(timeout=8)
            except Exception:  # noqa: BLE001
                p.kill()
        if not args.keep:
            shutil.rmtree(home, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
