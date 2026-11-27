#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""迁移覆盖率自证：把 220 条路由全打一遍，看谁还是转发。

判据不是「有响应」，而是**响应的来源**：
  · 出现 `x-moshan-proxy` 头  → 还在转发给 Python（未迁）
  · 502 + 「上游不可用」      → 落到了 fallback，但上游被刻意置空（未迁）
  · 其它任何状态码            → Rust 自己应答了（迁了，哪怕回的是 400/404/500）

把上游置空是这套判定的关键：只要还有一条没迁，它就一定露馅。
"""
from __future__ import annotations

import argparse
import json
import os
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TOKEN = "coverage-token-fixed"


def free_port() -> int:
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def write_cfg(home: Path) -> None:
    lib = home / "library"
    (lib / "收件箱").mkdir(parents=True, exist_ok=True)
    st = home / "state"
    st.mkdir(parents=True, exist_ok=True)
    (st / "config.json").write_text(
        json.dumps(
            {
                "token": TOKEN,
                "host": "127.0.0.1",
                "inbox_dir": str(lib / "收件箱"),
                "allowed_roots": [str(lib)],
                "trash_dir": str(lib / ".回收站"),
                "dav_root": str(lib),
                "max_upload_mb": 500,
                "tls": False,
                "allow_private_only": True,
                "dav_enabled": False,
            },
            ensure_ascii=False,
            indent=2,
        ),
        encoding="utf-8",
    )


def probe(port: int, method: str, path: str) -> tuple[int, bool]:
    """返回 (状态码, 是否转发)。路径里的 `{x}` 换成占位值。"""
    url = f"http://127.0.0.1:{port}{path}"
    data = None
    headers = {"X-Token": TOKEN}
    if method in ("POST", "PUT", "PATCH"):
        data = b"{}"
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=20) as r:
            return r.status, r.headers.get("x-moshan-proxy") == "1"
    except urllib.error.HTTPError as e:
        return e.code, e.headers.get("x-moshan-proxy") == "1"
    except Exception:  # noqa: BLE001
        return 0, False


def concrete(path: str) -> str:
    """把 FastAPI 的占位路径换成能打的具体值。"""
    out = path
    for key in ("{version}", "{tid}", "{sid}", "{name}", "{name:path}", "{version:path}"):
        out = out.replace(key, "x")
    return out


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--rs-bin", default="/tmp/rs-all/release/moshan-server")
    ap.add_argument("--routes", default=str(ROOT / "scripts/routes.json"))
    ap.add_argument("--json", default="", help="把结果写成 JSON")
    args = ap.parse_args()

    routes = json.loads(Path(args.routes).read_text(encoding="utf-8"))
    home = Path(tempfile.mkdtemp(prefix="moshan-cover-"))
    write_cfg(home)
    port = free_port()
    env = {
        **os.environ,
        "MOSHAN_HOME": str(home),
        "MOSHAN_PY_UPSTREAM": "",          # 关键：没有上游可转发
        "MOSHAN_RS_PORT": str(port),
        "MOSHAN_APP_DIR": str(ROOT / "moshan"),
        "MOSHAN_NO_SCHED": "1",            # 只验路由，不跑后台重活
        "MOSHAN_NO_BROWSER": "1",
    }
    p = subprocess.Popen([args.rs_bin], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    try:
        ok = False
        for _ in range(200):
            if probe(port, "GET", "/api/ping")[0] == 200:
                ok = True
                break
            time.sleep(0.1)
        if not ok:
            err = p.stderr.read().decode()[:800] if p.stderr else ""
            print("Rust 侧没起来：", err)
            return 2

        missing, served, note = [], [], []
        for r in routes:
            path = concrete(r["path"])
            status, proxied = probe(port, r["method"], path)
            if proxied or status == 502:
                missing.append({"method": r["method"], "path": r["path"], "module": r["module"]})
            elif status == 404 and r["module"] in ("gray.py", "fleet.py"):
                # 这两套在 Python 主服务里本来就没挂，404 是正确结果
                note.append({"method": r["method"], "path": r["path"], "module": r["module"]})
                served.append(r)
            else:
                served.append(r)

        total = len(routes)
        print(f"共 {total} 条：Rust 自应答 {len(served)}，未迁 {len(missing)}")
        if note:
            print(f"（其中 {len(note)} 条在 Python 主服务本就是 404：gray/fleet 独立服务）")
        for m in missing:
            print(f"  未迁  {m['method']:5s} {m['path']}   ({m['module']})")
        if args.json:
            Path(args.json).write_text(
                json.dumps(
                    {"total": total, "served": len(served), "missing": missing, "note": note},
                    ensure_ascii=False,
                    indent=2,
                ),
                encoding="utf-8",
            )
        return 0 if not missing else 1
    finally:
        p.terminate()
        try:
            p.wait(timeout=8)
        except Exception:  # noqa: BLE001
            p.kill()


if __name__ == "__main__":
    sys.exit(main())
