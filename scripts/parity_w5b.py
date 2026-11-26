#!/usr/bin/env python3
"""w5b 泳道的写族对拍：账单 / 流水 / 工作记录 / 模型探测 / 巡检。

读族用 `scripts/rust-parity.py --paths ... --seed` 就够了；这个脚本补的是
「有副作用」的那一半：两边各起一份干净数据根，按同一脚本跑一遍，逐条比
归一后的响应字节，最后再比两边**落盘文件**（计费账号.json / worklog.json /
流水.json / 勾选.json / 当月镜像 md）的字节。

归一化只抹「两边必然不同」的东西：随机 id、当前时间、临时数据根路径、
巡检里的活计数（内存 GB / 毫秒 / 磁盘 GB）。形状与措辞一个都不抹。

用法：
    python3 scripts/parity_w5b.py --rs-bin /tmp/rs-w5b/release/moshan-server
"""
from __future__ import annotations

import argparse
import importlib.util
import json
import os
import re
import subprocess
import sys
import tempfile
import time
import urllib.parse
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("rp", str(ROOT / "scripts/rust-parity.py"))
rp = importlib.util.module_from_spec(spec)
spec.loader.exec_module(rp)

PARITY_TOKEN = rp.PARITY_TOKEN


def steps(state: dict) -> list[tuple[str, str, str, object]]:
    return [
        # —— 账单 ——
        ("账单空", "GET", "/api/billing", None),
        ("供应商表", "GET", "/api/billing/providers", None),
        ("建账号缺字段", "POST", "/api/billing/save",
         {"provider": "deepseek", "name": "主号"}),
        ("建账号", "POST", "/api/billing/save",
         {"provider": "deepseek", "name": "主号", "fields": {"key": "sk-abcdefghijklmnop"}}),
        ("建完再看", "GET", "/api/billing", None),
        ("重名同名", "POST", "/api/billing/save",
         {"provider": "deepseek", "name": "主号", "fields": {"key": "sk-2"}}),
        ("改名字", "POST", "/api/billing/save",
         {"id": "{acct}", "provider": "deepseek", "name": "改过的名字",
          "note": "备注一下", "fields": {"key": "sk-…cdef"}}),
        ("未打码密钥", "POST", "/api/billing/save",
         {"id": "{acct}", "provider": "deepseek", "fields": {"key": "sk-new-1234567890"}}),
        ("未知供应商", "POST", "/api/billing/save", {"provider": "nope"}),
        ("手动录入", "POST", "/api/billing/save",
         {"provider": "manual", "name": "手录", "fields": {"balance": "12.5", "month": "3"}}),
        ("删联网账号", "POST", "/api/billing/del", {"id": "{acct}"}),
        ("删第二个联网账号", "POST", "/api/billing/del", {"id": "{acct2}"}),
        ("拉手动那档", "POST", "/api/billing/pull", {"ids": ["{manual}"]}),
        ("拉全部", "POST", "/api/billing/pull", None),
        ("拉不存在的", "POST", "/api/billing/pull", {"id": "nope"}),
        ("拉一次无体", "POST", "/api/billing/pull", None),
        ("删账号", "POST", "/api/billing/del", {"id": "{manual}"}),
        ("删完再看", "GET", "/api/billing", None),
        # —— 工作流水 ——
        ("流水空", "GET", "/api/flow", None),
        ("没目录就补（应报错）", "POST", "/api/flow/add",
         {"text": "先来一条", "day": "2026-09-01"}),
        ("建流水目录", "MKDIR", "09-个人工作台/流水", None),
        ("补一条", "POST", "/api/flow/add", {"text": "  写下   一句话  ", "day": "2026-09-01"}),
        ("再补一条", "POST", "/api/flow/add", {"text": "第二句", "day": "2026-09-01"}),
        ("补到另一天", "POST", "/api/flow/add", {"text": "另一天", "day": "2026-09-02"}),
        ("空文本", "POST", "/api/flow/add", {"text": "   "}),
        ("流水无体", "POST", "/api/flow", None),
        ("流水坏 JSON", "POST", "/api/flow", "__RAW__{bad"),
        ("日期不对", "POST", "/api/flow/add", {"text": "x", "day": "2026-13-01"}),
        ("看一眼流水", "GET", "/api/flow?month=all", None),
        ("勾一条", "POST", "/api/flow", {"id": "20260901-1", "done": True}),
        ("取消勾", "POST", "/api/flow", {"id": "20260901-1", "done": False}),
        ("勾不存在的", "POST", "/api/flow", {"id": "nope", "done": True}),
        ("勾完再看", "GET", "/api/flow?month=2026-09", None),
        # —— 工作记录 ——
        ("补记一条", "POST", "/api/worklog",
         {"task": "  改了账单页  ", "detail": "细节", "kind": "both", "who": "a.py+DeepSeek",
          "client": "WorkBuddy", "start": "2026-09-01T10:00:00"}),
        ("补记没任务", "POST", "/api/worklog", {"task": ""}),
        ("补记无体", "POST", "/api/worklog", None),
        ("补记进行中", "POST", "/api/worklog",
         {"task": "跑着", "status": "run", "kind": "code", "who": ""}),
        ("再看工作记录", "GET", "/api/worklog", None),
        # —— 模型探测 ——
        ("本机模型", "GET", "/api/llm", None),
        ("探空地址", "POST", "/api/model/test", {"base": ""}),
        ("探死端口", "POST", "/api/model/test", {"base": "http://127.0.0.1:1"}),
        # —— 巡检 ——
        ("报告没名字", "GET", "/api/inspect/report", None),
        ("报告路径穿越", "GET", "/api/inspect/report?name=../x.md", None),
        ("报告不存在", "GET", "/api/inspect/report?name=nope.md", None),
        ("造一份报告", "MKDIR", "10-系统记录/巡检/报告", None),
        ("写报告正文", "WRITE", "10-系统记录/巡检/报告/2026-01-01_000000.md",
         "# 巡检报告\n\n- 平台：macOS\n"),
        ("读报告", "GET", "/api/inspect/report?name=2026-01-01_000000.md", None),
        ("报告不是 md", "GET", "/api/inspect/report?name=x.txt", None),
        ("报告带斜杠", "GET", "/api/inspect/report?name=a%2Fb.md", None),
        ("巡检状态", "GET", "/api/inspect/state", None),
        ("巡检跑无体", "POST", "/api/inspect/run", None),
        ("巡检跑一轮", "POST", "/api/inspect/run", {"heal": False}),
    ]


# 两边必然不同的东西：随机 id、当前时间、临时数据根。
_NORM = [
    (re.compile(rb'"id": ?"[0-9a-f]{8}"'), b'"id":"<ID8>"'),
    (re.compile(rb'"id": ?"w[0-9a-f]{10}"'), b'"id":"<WID>"'),
    (re.compile(rb'"w[0-9a-f]{10}"'), b'"<WID>"'),
    (re.compile(rb"\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2}:\d{2}(?:\.\d+)?"), b"<TS>"),
    (re.compile(rb'"at": ?"\d{2}:\d{2}:\d{2}"'), b'"at":"<HMS>"'),
    (re.compile(rb'"at": ?\d{10,}(?:\.\d+)?'), b'"at":<EPOCH>'),
    (re.compile(rb'"ms": ?\d+'), b'"ms":<MS>'),
    (re.compile(rb'"at": ?"\d{4}-\d{2}-\d{2} \d{2}:\d{2}"'), b'"at":"<MIN>"'),
    # 8774 的 repair 段带机器活采样（内存 / 负载）：两次请求之间本来就会变
    (re.compile(rb'"(mem_pct|disk_pct|load1|load5|load15)": ?[0-9.]+'), rb'"\1":<N>'),
    # 巡检报告文件名带生成时刻；8774 每轮都会新出一份
    (re.compile(rb"\d{8}-\d{6}\.md"), b"<REPORT>.md"),
]


def norm(b: bytes, home: Path) -> bytes:
    b = b.replace(str(home).encode(), b"<HOME>")
    b = b.replace(str(home.resolve()).encode(), b"<HOME>")
    for rx, rep in _NORM:
        b = rx.sub(rep, b)
    return b


def call(port: int, token: str, method: str, path: str, body) -> tuple[int, bytes]:
    return rp.call(port, token, method, path, body)


def _kill_tree(p: subprocess.Popen) -> None:
    """Python 侧的 uvicorn 是 launcher 的子进程，只 terminate 父进程会留下孤儿占端口。"""
    import signal
    try:
        os.killpg(os.getpgid(p.pid), signal.SIGTERM)
    except Exception:  # noqa: BLE001
        try:
            p.terminate()
        except Exception:  # noqa: BLE001
            pass
    try:
        p.wait(timeout=8)
    except Exception:  # noqa: BLE001
        try:
            os.killpg(os.getpgid(p.pid), signal.SIGKILL)
        except Exception:  # noqa: BLE001
            p.kill()


def _mk(home: Path, rel: str) -> None:
    (home / "library" / rel).mkdir(parents=True, exist_ok=True)


def _identity_ok(port: int, token: str, home: Path) -> bool:
    """确认应答的就是「这个数据根」起的那个实例。

    并行施工时另一条泳道可能在同一秒选到同一个端口，谁先绑上谁应答 ——
    不核对就会拿别人的数据根去比，差异全是假的。
    """
    try:
        st, raw = call(port, token, "GET", "/api/billing", None)
        if st != 200:
            return False
        store = json.loads(raw).get("store", "")
        return store.startswith(str(home)) or store.startswith(str(home.resolve()))
    except Exception:  # noqa: BLE001
        return False


def run_side(args, side: str) -> tuple[list, Path]:
    home = Path(tempfile.mkdtemp(prefix=f"moshan-w5b-{side}-"))
    rp.seed(home)
    env = {**os.environ, "MOSHAN_HOME": str(home), "MOSHAN_NO_BROWSER": "1"}
    if side == "py":
        p = subprocess.Popen(
            [args.py_python, str(ROOT / "moshan/launcher.py")],
            env=env, cwd=str(ROOT / "moshan"), start_new_session=True,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        cfg = rp._wait_cfg(home)
        port, token = cfg.get("local_port"), cfg.get("token", "")
    else:
        port = rp.free_port()
        p = subprocess.Popen(
            [args.rs_bin],
            env={**env, "MOSHAN_PY_UPSTREAM": "", "MOSHAN_RS_PORT": str(port),
                 "MOSHAN_APP_DIR": str(ROOT / "moshan")},
            start_new_session=True,
            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        token = PARITY_TOKEN
    if not port or not rp.wait_ping(port) or not _identity_ok(port, token, home):
        err = ""
        if side == "rs" and p.stderr:
            err = p.stderr.read().decode()[:600]
        _kill_tree(p)
        raise SystemExit(f"{side} 侧没起来（端口 {port}）{err}")
    out = []
    try:
        state: dict = {"acct": "", "acct2": "", "manual": ""}
        for note, method, path, body in steps(state):
            if isinstance(body, str) and body.startswith("__RAW__"):
                body = ("__RAW__", body[len("__RAW__"):])
            if body is not None and not isinstance(body, tuple) and "{acct}" in json.dumps(body):
                body = json.loads(json.dumps(body).replace("{acct}", state["acct"]))
            if body is not None and not isinstance(body, tuple) and "{manual}" in json.dumps(body):
                body = json.loads(json.dumps(body).replace("{manual}", state["manual"]))
            if body is not None and not isinstance(body, tuple) and "{acct2}" in json.dumps(body):
                body = json.loads(json.dumps(body).replace("{acct2}", state.get("acct2", "")))
            if method == "MKDIR":
                _mk(home, path)
                out.append((note, 200, b""))
                continue
            if method == "WRITE":
                f = home / "library" / path
                f.parent.mkdir(parents=True, exist_ok=True)
                f.write_text(body, encoding="utf-8")
                out.append((note, 200, b""))
                continue
            if isinstance(body, tuple):
                import urllib.request
                req = urllib.request.Request(
                    f"http://127.0.0.1:{port}{path}", data=body[1].encode(),
                    method=method, headers={"X-Token": token,
                                            "Content-Type": "application/json"})
                try:
                    with urllib.request.urlopen(req, timeout=30) as r:
                        status, raw = r.status, r.read()
                except urllib.error.HTTPError as e:
                    status, raw = e.code, e.read()
                except Exception as e:  # noqa: BLE001
                    status, raw = 0, str(e).encode()
                body = None
            else:
                status, raw = call(port, token, method, path, body)
            out.append((note, status, norm(raw, home)))
            try:
                js = json.loads(raw)
            except Exception:  # noqa: BLE001
                js = {}
            if note == "建账号" and isinstance(js, dict) and js.get("account"):
                state["acct"] = js["account"].get("id", "")
            if note == "重名同名" and isinstance(js, dict) and js.get("account"):
                state["acct2"] = js["account"].get("id", "")
            if note == "手动录入" and isinstance(js, dict) and js.get("account"):
                state["manual"] = js["account"].get("id", "")
    finally:
        _kill_tree(p)
    return out, home


DATA_FILES = [
    "_本机工作/计费账号.json",
    "_工作台/worklog.json",
    "09-个人工作台/流水/流水.json",
    "09-个人工作台/流水/勾选.json",
    "09-个人工作台/流水/2026-09.md",
]


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--rs-bin", default="/tmp/rs-w5b/release/moshan-server")
    ap.add_argument("--py-python",
                    default=os.environ.get(
                        "MOSHAN_PY",
                        str(Path.home() / "Library/Caches/moshan-build-venv/bin/python")))
    ap.add_argument("--keep", action="store_true")
    args = ap.parse_args()

    results = {}
    homes = {}
    for side in ("py", "rs"):
        results[side], homes[side] = run_side(args, side)

    bad = 0
    print(f"{'步骤':16s} {'py':>4s} {'rs':>4s}  结果")
    for (note, sa, ra), (_, sb, rb) in zip(results["py"], results["rs"]):
        ok = sa == sb and ra == rb
        if not ok:
            bad += 1
        print(f"{note:16s} {sa:4d} {sb:4d}  {'一致' if ok else '差异'}")
        if not ok:
            print("    " + rp.first_diff(ra, rb))

    print("\n—— 落盘文件 ——")
    file_bad = 0
    for rel in DATA_FILES:
        a = homes["py"] / "library" / rel
        b = homes["rs"] / "library" / rel
        ea, eb = a.exists(), b.exists()
        if not ea and not eb:
            print(f"{rel:40s} 两边都没有")
            continue
        ba = norm(a.read_bytes(), homes["py"]) if ea else b"<MISSING>"
        bb = norm(b.read_bytes(), homes["rs"]) if eb else b"<MISSING>"
        same = ba == bb
        if not same:
            file_bad += 1
        print(f"{rel:40s} {'一致' if same else '差异'}")
        if not same:
            print("    " + rp.first_diff(ba, bb))

    print(f"\n共 {len(results['py'])} 步，响应差异 {bad}，落盘差异 {file_bad}")
    if args.keep:
        print("py home:", homes["py"])
        print("rs home:", homes["rs"])
    return 0 if bad == 0 and file_bad == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
