#!/usr/bin/env python3
"""w4c 泳道的写族对拍：文章 / 去重 / 收件箱 / 链接 / 报告 / 记账扩展。

与 `rust-parity.py --suite w4` 同一套办法：**两边各起一份干净数据根**，
按同一份步骤清单打同样的请求，再比归一化后的字节；最后再 diff 两边的数据目录。

用法：
    python3 scripts/parity_w4c.py --rs-bin /tmp/rs-w4c/release/moshan-server
    python3 scripts/parity_w4c.py --rs-bin ... --py-python /usr/bin/python3 --verbose

为什么单开一个脚本：这几条路由里有「进程启动时刻」「随机 id」「耗时毫秒」
这类天然不同的值（与 /api/heartbeat 同类），归一化规则要说清楚，不能混进
已经验收的 w3/w4 基线里。

**不会碰真实数据**：dedupe 的 root 在夹具里改到临时数据根、apply=false；
article/link 的抓取分支不会被触发（不联网）。
"""
from __future__ import annotations

import hashlib
import importlib.util
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import uuid
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TOKEN = "parity-token-fixed-for-both-sides"


def load_rp():
    spec = importlib.util.spec_from_file_location("rust_parity", ROOT / "scripts/rust-parity.py")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


R = load_rp()


# ---------------------------------------------------------------- 夹具

def seed_extra(home: Path) -> None:
    """在 rp.seed() 之上补 w4c 需要的夹具：阅读库、去重报告、日聚合、账本。"""
    lib = home / "library"
    inbox = lib / "收件箱"

    # 去重：root 指到临时数据根，apply=false（绝不真删东西）
    cfg_path = home / "state" / "config.json"
    cfg = json.loads(cfg_path.read_text(encoding="utf-8"))
    # 预置一个随机本地端口：默认 8321 起扫，上一轮还没退干净时会被挤到别的号，
    # 而对拍要读 config 才知道最终端口 —— 少一次这种赛跑。
    cfg["local_port"] = R.free_port()
    cfg["dedupe"] = {"root": str(lib / "04-存图"), "apply": False, "hour": 3,
                     "limit": 100, "script": str(Path.home() / "Documents/cuntu/dedupe.py")}
    cfg_path.write_text(json.dumps(cfg, ensure_ascii=False, indent=2), encoding="utf-8")

    # 08-阅读：两个主题 + 一个 _ 开头目录（不该出现在分组里）
    read = lib / "08-阅读"
    arts = {
        "未分类/标题甲": {"title": "标题甲", "chars": 120, "url": "https://example.com/a",
                          "platform": "网页", "author": "作者甲", "saved_at": "2026-11-01T10:00:00+08:00"},
        "科技/标题乙": {"title": "标题乙", "chars": 3000, "url": "https://example.com/b",
                        "platform": "公众号", "author": "", "saved_at": "2026-11-02T10:00:00+08:00"},
        "科技/无正文": {"title": "无正文", "chars": 0, "url": "", "platform": "网页"},
        "_隐藏/不该出现": {"title": "不该出现", "chars": 1},
    }
    base = time.time() - 86400 * 3
    for i, (rel, meta) in enumerate(arts.items()):
        d = read / rel
        d.mkdir(parents=True, exist_ok=True)
        (d / "article.json").write_text(json.dumps(meta, ensure_ascii=False, indent=1), encoding="utf-8")
        if rel != "科技/无正文":
            (d / "正文.md").write_text("# 正文\n\n" + "字" * 60, encoding="utf-8")
        os.utime(d, (base + i, base + i))
    (read / "_主题.json").write_text(
        json.dumps({"topics": ["科技", "经济"], "updated_at": "2026-11-01T10:00:00+08:00"},
                   ensure_ascii=False, indent=1), encoding="utf-8")
    (read / "未分类").mkdir(parents=True, exist_ok=True)

    # 去重报告：latest + 历史 + 一个可还原批次
    dd = lib / "04-存图" / "_去重报告"
    dd.mkdir(parents=True, exist_ok=True)
    (dd / "latest.json").write_text(json.dumps(
        {"ok": True, "groups": 2, "freed": 1024, "scanned": {"dirs": 9}}, ensure_ascii=False,
        indent=1), encoding="utf-8")
    rows = [{"at": "2026-11-20 03:00:01", "groups": 1, "freed": 500},
            {"at": "2026-11-21 03:00:02", "groups": 3, "freed": 900}]
    (dd / "index.jsonl").write_text(
        "\n".join(json.dumps(r, ensure_ascii=False) for r in rows) + "\n", encoding="utf-8")
    trash = lib / "04-存图" / "_回收站" / "去重_20261121-030002"
    trash.mkdir(parents=True, exist_ok=True)
    (trash / "_manifest.json").write_text(json.dumps(
        [{"name": "a", "bytes": 100}, {"name": "b", "bytes": 250}], ensure_ascii=False),
        encoding="utf-8")
    other = lib / "04-存图" / "_回收站" / "别的不算"
    other.mkdir(parents=True, exist_ok=True)

    # 日聚合：跨 2025 年、H1、Q3、上月、上周、本周各铺几天（都不含今天，
    # 免得生成「进行中」那版）。这样六种周期都能被 due() 补出来、正文都能对拍。
    sysdir = lib / "10-系统记录" / "使用报告" / "_数据"
    sysdir.mkdir(parents=True, exist_ok=True)
    days = ["2025-12-03", "2026-02-11", "2026-08-19", "2026-09-02",
            "2026-10-05", "2026-10-06", "2026-11-16", "2026-11-17",
            "2026-11-25", "2026-11-26"]
    lines = []
    for i, d in enumerate(days):
        lines.append(json.dumps({
            "date": d, "n": 320 + i, "full": 12, "awake_min": 300 + i * 17, "span_min": 600 + i,
            "first": "08:0%d" % (i % 10), "last": "23:1%d" % (i % 10), "boots": i % 2,
            "segs": [["08:00", "12:00"], ["14:00", "18:30"]],
            "mem": {"avg": 52.5 + i, "max": 91.0 if i == 2 else 70.0, "min": 33.0,
                    "max_at": "15:0%d" % (i % 10)},
            "load": {"avg": 2.25 + i * 0.1, "max": 9.5 if i == 3 else 4.0,
                     "max_at": "16:0%d" % (i % 10)},
            "disk": {"start": 61.5, "end": 61.2, "min_free": 18.5 if i == 1 else 120.0},
            "uptime_max": 12.5, "cpus": 10,
            "hourly": [0, 0, 0, 0, 0, 0, 0, 0, 30, 60, 45, 20, 0, 0, 25, 40, 30, 0, 0, 0, 0, 0, 0, 0],
            "hourly_mem": [None] * 8 + [50.0, 55.0, 52.0, 51.0, None, None, 48.0, 49.0,
                                        50.0, None, None, None, None, None, None, None],
            "top": [{"n": "WindowServer", "avg": 12.5, "max": 30.0, "hits": 20},
                    {"n": "Safari", "avg": 8.0, "max": 22.5, "hits": 15}],
        }, ensure_ascii=False))
    (sysdir / "日聚合.jsonl").write_text("\n".join(lines) + "\n", encoding="utf-8")

    # 收件箱：一个待起名的空组、一个只有图的组、一个已起过名的组
    g1 = inbox / "组-测试"
    g1.mkdir(parents=True, exist_ok=True)
    g2 = inbox / "组-图片"
    g2.mkdir(parents=True, exist_ok=True)
    photo = inbox / "照片.jpg"
    if photo.exists():
        shutil.copyfile(photo, g2 / "01.jpg")
    else:
        (g2 / "01.jpg").write_bytes(b"\xff\xd8\xff\xe0" + b"0" * 200)
    (inbox / "组-图片" / "备注.txt").write_text("x", encoding="utf-8")
    g3 = inbox / "起过名的"
    g3.mkdir(parents=True, exist_ok=True)

    # 「打开」类接口要调 macOS 的 open —— 无人值守跑对拍不能真弹 Finder。
    # PATH 里放一个立刻退出的同名壳，两边走的是同一条调用链，返回值还能对拍。
    shim = home / "bin"
    shim.mkdir(parents=True, exist_ok=True)
    fake_open = shim / "open"
    fake_open.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
    fake_open.chmod(0o755)

    # 账本图（换图那一步要用真实可读的图）
    led = lib / "12-记账"
    for sub in ("图", "原图", "_回收站"):
        (led / sub).mkdir(parents=True, exist_ok=True)


# ---------------------------------------------------------------- 归一化

_EXTRA = [
    (re.compile(rb'"id":"[0-9a-f]{12}"'), b'"id":"<JID>"'),
    (re.compile(rb'"id": "[0-9a-f]{12}"'), b'"id": "<JID>"'),
    (re.compile(rb'p-\d{8}-[0-9a-f]{4}'), b"p-<DATE>-<HEX>"),
    (re.compile(rb"\xe7\xbb\x84-\d{6}"), b"<GROUP>"),
    (re.compile(rb'"ms":\d+'), b'"ms":<MS>'),
    (re.compile(rb'"ms": \d+'), b'"ms": <MS>'),
    (re.compile(rb'"duration_sec":[\d.]+'), b'"duration_sec":<N>'),
    (re.compile(rb'"started":[\d.]+'), b'"started":<EPOCH>'),
    (re.compile(rb'"started": [\d.]+'), b'"started": <EPOCH>'),
    (re.compile(rb'"seconds":[\d.]+'), b'"seconds":<N>'),
    (re.compile(rb'"at":"[^"]*"'), b'"at":"<AT>"'),
    (re.compile(rb'"at": "[^"]*"'), b'"at": "<AT>"'),
    (re.compile(rb'"last_sample": "[^"]*"'), b'"last_sample": "<AT>"'),
    (re.compile(rb'"last_run":[\d.]+'), b'"last_run":<EPOCH>'),
    (re.compile(rb'"last_run": [\d.]+'), b'"last_run": <EPOCH>'),
]


# Python 进程里 ocr_runner 线程会在启动后给「公众号 + 正文一张图都没有」的文章补写
# img_retry_count / img_retry_at（见 moshan/ocr_runner.py: refetch_missing）。
# Rust 泳道不启动调度线程，因此这两个键只可能出现在 py 侧；只剥这一块，
# 其余字节仍逐字比。归为「已知运行期分叉」，不算路由差异。
_IMG_RETRY = [
    (re.compile(rb',\n "img_retry_count": \d+,\n "img_retry_at": "[^"]*"(\n?\})'), rb"\1"),
    (re.compile(rb',\n "img_retry_count": \d+(\n?\})'), rb"\1"),
]


def norm(b: bytes, home: Path) -> bytes:
    # 先跑自己这几条：R.norm 里 `\b[0-9a-f]{10}\b` 会把 epoch 的前 10 位当 SID 抹掉，
    # 留下 `.080176` 这种尾巴，反而更难对齐。
    for rx, rep in _EXTRA:
        b = rx.sub(rep, b)
    b = R.norm(b, home, coarse=True)
    for rx, rep in _EXTRA:
        b = rx.sub(rep, b)
    b = re.sub(rb'"started":<SID>\.\d+', b'"started":<EPOCH>', b)
    for rx, rep in _IMG_RETRY:
        b = rx.sub(rep, b)
    return b


# ---------------------------------------------------------------- 步骤

def steps(state: dict):
    inbox = state["inbox"]
    lib = inbox.parent
    return [
        ("建组-指定名", "POST", "/api/inbox/group", {"name": "组-测试2"}),
        ("建组-重名", "POST", "/api/inbox/group", {"name": "组-测试2"}),
        ("建组-带斜杠", "POST", "/api/inbox/group", {"name": "a/b"}),
        ("起名-不存在", "POST", "/api/inbox/name", {"path": str(inbox / "根本没有")}),
        ("起名-不是目录", "POST", "/api/inbox/name", {"path": str(inbox / "报告.md")}),
        ("起名-越界", "POST", "/api/inbox/name", {"path": str(lib / "04-归档")}),
        ("起名-空组", "POST", "/api/inbox/name", {"path": str(inbox / "组-测试")}),
        ("起名-有图", "POST", "/api/inbox/name", {"path": str(inbox / "组-图片")}),
        ("起名-已有名字", "POST", "/api/inbox/name", {"path": str(inbox / "起过名的")}),
        ("打开收件箱", "GET", "/api/inbox/open", None),
        ("去重状态", "GET", "/api/dedupe", None),
        ("去重跑一轮", "POST", "/api/dedupe/run?apply=false", None),
        ("还原-缺批次", "POST", "/api/dedupe/restore", None),
        ("还原-批次名不对", "POST", "/api/dedupe/restore?batch=x", None),
        ("文章-空链接", "POST", "/api/article/fetch", {"text": "   "}),
        # 「任务已受理」这一段只验**立即返回的形状**：链接故意用 .invalid（DNS 必然失败），
        # 后台线程只会走错误分支，不联网、不落正文。真抓取/真识别留给人工验收。
        #
        # 步骤顺序是按「槽位可预期」排的，不是随手写的：
        #   · 分类用不存在的目录 —— article.py 立刻回「目录不存在」，不碰 4B、不挪目录
        #   · 单任务都放在各自队列的第一个 —— 槽位必空，状态必是 running
        #   · 并发上限只在**同一次请求的 multi** 里验（a b a c d）：
        #     前两条抢到 Semaphore(2) → running，重复的 a → dup，后两条 → queued。
        #     同一次请求内几个任务在微秒内建完，子进程还没退出，所以这个组合是确定的；
        #     跨请求去等 queued/running 就成了赌子进程跑多快，不做。
        ("分类-单篇", "POST", "/api/article/classify", {"rel": "科技/根本没有"}),
        ("文章-单条", "POST", "/api/article/fetch",
         {"text": "https://no-such-host.invalid/c"}),
        ("分类-越界rel", "POST", "/api/article/classify", {"rel": "../etc"}),
        ("链接-空", "POST", "/api/link", {"text": "  "}),
        ("链接-五条", "POST", "/api/link",
         {"text": " ".join(f"https://no-such-host.invalid/{c}" for c in ("a", "b", "a", "c", "d"))}),
        ("文章列表", "GET", "/api/article/list", None),
        ("文章主题", "GET", "/api/article/topics", None),
        ("文章状态-不存在", "GET", "/api/article/status?id=deadbeef0000", None),
        ("链接状态-不存在", "GET", "/api/link/status?id=deadbeef0000", None),
        ("记账上传-空文件", "MULTIPART", "/api/ledger/upload",
         {"__files__": [("empty.png", b"")]}),
        ("记账上传-无文件", "MULTIPART", "/api/ledger/upload", {"__files__": []}),
        ("记账上传-假图", "MULTIPART", "/api/ledger/upload",
         {"__files__": [("shot.png", b"\x89PNG\r\n\x1a\n" + b"0" * 64)]}),
        ("换图-没有记录", "MULTIPART", "/api/ledger/image?id=nope",
         {"__files__": [("a.png", b"\x89PNG\r\n\x1a\n" + b"0" * 32)], "__field__": "file"}),
        ("换图-无文件", "MULTIPART", "/api/ledger/image?id=nope",
         {"__files__": [], "__field__": "file"}),
        # 成功分支：换一张「能读但不是真 jpeg」的图 —— sips 会失败、兜底直接拷贝，
        # 两边跑的是同一条兜底，落盘与账本改名都能量到。
        ("换图-成功", "MULTIPART", "/api/ledger/image?id=p-20260101-aaaa",
         {"__files__": [("new.jpg", b"\xff\xd8\xff\xe0" + b"0" * 100)],
          "__field__": "file"}),
        ("换图后读", "GET", "/api/ledger/item?id=p-20260101-aaaa", None),
        ("打开账本", "POST", "/api/ledger/open", None),
        ("报告-没有这份", "POST", "/api/report/open", {"kind": "day", "key": "1999-01-01"}),
        ("报告-坏kind", "GET", "/api/report?kind=nope", None),
        ("报告生成", "POST", "/api/report/run", None),
        ("报告清单", "GET", "/api/report/list", None),
        ("月报", "GET", "/api/report?kind=month", None),
        ("季报", "GET", "/api/report?kind=quarter", None),
        ("半年报", "GET", "/api/report?kind=half", None),
        ("年报", "GET", "/api/report?kind=year", None),
        ("周报", "GET", "/api/report?kind=week", None),
        ("打开月报", "POST", "/api/report/open", {"kind": "month"}),
        ("最新日报", "GET", "/api/report?kind=day", None),
        ("推送后再跑", "POST", "/api/report/run?force=true", None),
        ("日报不存在", "GET", "/api/report?kind=day&key=1999-01-01", None),
        ("生成后清单", "GET", "/api/report/list", None),
    ]


def call_mp(port: int, token: str, path: str, files: list,
            field: str = "files") -> tuple[int, bytes]:
    """手搓 multipart 并发送。

    `R.call` 的 `_multipart` 把字段名写死成 `files`（/api/ledger/upload 的形状），
    但 `/api/ledger/image` 收的是单数 `file` —— 字段名错了 pydantic 会先回 422，
    根本走不到业务分支。这里补一个能指定字段名的版本。
    """
    boundary = "----parity" + uuid.uuid4().hex[:12]
    data = bytearray()
    for name, blob in files:
        data += (f'--{boundary}\r\n'
                 f'Content-Disposition: form-data; name="{field}"; filename="{name}"\r\n'
                 f"Content-Type: application/octet-stream\r\n\r\n").encode("utf-8")
        data += blob
        data += b"\r\n"
    data += f"--{boundary}--\r\n".encode()
    req = urllib.request.Request(
        f"http://127.0.0.1:{port}{path}", data=bytes(data), method="POST",
        headers={"X-Token": token,
                 "Content-Type": f"multipart/form-data; boundary={boundary}"})
    try:
        with urllib.request.urlopen(req, timeout=120) as r:
            return r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()
    except Exception as e:  # noqa: BLE001
        return 0, str(e).encode()


def preflight(port: int, token: str, tries: int = 20) -> bool:
    """确认端口上就是**这一轮**起的服务。

    只 ping 不够（`/api/ping` 不鉴权），只看到 200 也不够（上一轮残留的进程
    认同一个固定 token、读的却是旧数据根）。所以要求 `/api/article/topics`
    能返回夹具里的主题「科技」—— 这条同时证明了「是我们的进程」和「读的是新数据根」。
    """
    for _ in range(tries):
        try:
            st, body = R.call(port, token, "GET", "/api/article/topics", None)
            if st == 200 and "科技".encode() in body:
                return True
        except Exception:
            pass
        time.sleep(0.25)
    return False


def replay(port: int, token: str, home: Path, state: dict):
    out = []
    for note, method, path, body in steps(state):
        if method == "MULTIPART" and isinstance(body, dict) and body.get("__field__"):
            status, raw = call_mp(port, token, path, body["__files__"], body["__field__"])
        else:
            http_method = "POST" if method == "MULTIPART" else method
            status, raw = R.call(port, token, http_method, path, body)
        out.append((note, status, norm(raw, home)))
    return out


# ---------------------------------------------------------------- 目录 diff

# 不参与比较的路径：
#   state/                 两边各自的 config/证书
#   04-存图/_去重报告      dedupe 子进程的输出（含跑批时刻）
#   04-存图/_回收站        同上（批次目录名带时间戳）
#   10-系统记录/心跳|磁盘   Python 的心跳/磁盘采集线程写的，属别的泳道
#   _工作台 / _本机工作     Python 启动时的后台线程产物，属别的泳道
_SKIP_DIRS = {
    "state", "04-存图/_去重报告", "04-存图/_回收站",
    "10-系统记录/心跳", "10-系统记录/磁盘", "_工作台", "_本机工作",
}
_SKIP_FILES = {".回收站/index.jsonl"}
_PID_RE = re.compile(r"p-\d{8}-[0-9a-f]{4}")

def tree(home: Path) -> dict[str, str]:
    out: dict[str, str] = {}
    root = home / "library"
    for p in sorted(root.rglob("*")):
        raw_rel = str(p.relative_to(root))
        rel = _PID_RE.sub("p-<DATE>-<HEX>", raw_rel)
        # 跳过判定要在记目录之前，否则空目录本身仍会进 diffs
        if any(raw_rel == s or raw_rel.startswith(s + "/") for s in _SKIP_DIRS):
            continue
        if p.is_dir():
            out[rel + "/"] = "dir"
            continue
        if raw_rel in _SKIP_FILES:
            continue
        if rel.endswith("index.jsonl") and "收件箱" in rel:
            continue
        raw = p.read_bytes()
        if rel.endswith((".json", ".jsonl", ".md", ".txt", ".csv")):
            b = norm(raw, home)
            out[rel] = hashlib.sha1(b).hexdigest()
        else:
            out[rel] = f"bin:{len(raw)}"
    return out


def diff_tree(a: dict[str, str], b: dict[str, str]) -> list[str]:
    msgs = []
    for k in sorted(set(a) | set(b)):
        if k not in a:
            msgs.append(f"  rs 多出：{k}")
        elif k not in b:
            msgs.append(f"  py 多出：{k}")
        elif a[k] != b[k]:
            msgs.append(f"  内容不同：{k}")
    return msgs


# ---------------------------------------------------------------- 主流程

def main() -> int:
    import argparse
    ap = argparse.ArgumentParser()
    ap.add_argument("--rs-bin", required=True)
    ap.add_argument(
        "--py-python",
        default=os.environ.get("MOSHAN_PY", str(Path.home() / "Library/Caches/moshan-build-venv/bin/python")),
    )
    ap.add_argument("--verbose", action="store_true")
    args = ap.parse_args()

    results: dict[str, list] = {}
    trees: dict[str, dict] = {}
    procs = []
    homes: dict[str, Path] = {}
    try:
        for side in ("py", "rs"):
            home = Path(tempfile.mkdtemp(prefix=f"moshan-w4c-{side}-"))
            homes[side] = home
            R.seed(home)
            seed_extra(home)
            env = {**os.environ, "MOSHAN_HOME": str(home), "MOSHAN_NO_BROWSER": "1",
                   # 先命中夹具里的假 open（见 seed_extra），别真开 Finder
                   "PATH": f"{home / 'bin'}:{os.environ.get('PATH', '')}"}
            if side == "py":
                # 自成进程组：launcher 会再起 uvicorn 子进程，只 kill 父进程会把
                # 监听 local_port 的子进程留下，下一轮就打到上一轮的数据根上，
                # 表现成「43 步全不一致」这种假红。收尾统一 killpg。
                p = subprocess.Popen(
                    [args.py_python, str(ROOT / "moshan/launcher.py")],
                    env=env, cwd=str(ROOT / "moshan"), start_new_session=True,
                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                procs.append(p)
                # launcher 发现首选端口被占会改号并重写 config，所以不能只读一次：
                # 反复读 config、拿里面的端口做预检，直到打到的就是这一轮的服务。
                port, token = None, ""
                deadline = time.time() + 45
                while time.time() < deadline:
                    cfg = R._wait_cfg(home) or {}
                    cand = cfg.get("local_port")
                    tk = cfg.get("token", "")
                    if cand and preflight(cand, tk, tries=1):
                        port, token = cand, tk
                        break
                    time.sleep(0.3)
            else:
                port = R.free_port()
                p = subprocess.Popen(
                    [args.rs_bin],
                    env={**env, "MOSHAN_PY_UPSTREAM": "", "MOSHAN_RS_PORT": str(port),
                         "MOSHAN_APP_DIR": str(ROOT / "moshan")},
                    # 同样自成进程组：否则它的组就是我们自己，killpg 会把脚本一起收掉
                    start_new_session=True,
                    stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
                procs.append(p)
                token = TOKEN
            if side == "rs":
                if not port or not R.wait_ping(port):
                    err = p.stderr.read().decode()[:800] if p.stderr else ""
                    print(f"rs 侧没起来（端口 {port}）{err}")
                    return 2
                # ping 不校验 token，端口上完全可能是别的进程；用一条要鉴权的
                # 只读接口确认「这台是我们刚起的、读的是新数据根」。
                if not preflight(port, TOKEN):
                    print(f"rs 侧预检失败：{port} 上的服务不认这次的数据根")
                    return 2
            elif not port:
                print("py 侧没起来：等不到一个认这次数据根的 local_port")
                return 2
            state = {"inbox": home / "library" / "收件箱"}
            results[side] = replay(port, TOKEN if side == "rs" else token, home, state)
            time.sleep(0.6)
            trees[side] = tree(home)
    finally:
        for p in procs:
            try:
                os.killpg(os.getpgid(p.pid), signal.SIGTERM)
            except Exception:
                p.terminate()
        for p in procs:
            try:
                p.wait(timeout=8)
            except Exception:
                try:
                    os.killpg(os.getpgid(p.pid), signal.SIGKILL)
                except Exception:
                    p.kill()

    print(f"{'步骤':16s} {'py':>4s} {'rs':>4s}  结果")
    bad = 0
    for (note, sa, ra), (_, sb, rb) in zip(results["py"], results["rs"]):
        ok = sa == sb and ra == rb
        if not ok:
            bad += 1
        print(f"{note:16s} {sa:4d} {sb:4d}  {'一致' if ok else '差异'}")
        if not ok:
            print("    " + R.first_diff(ra, rb))
            if args.verbose:
                print(f"    py={ra[:400]!r}\n    rs={rb[:400]!r}")

    print("\n数据目录：")
    d = diff_tree(trees["py"], trees["rs"])
    if d:
        print("\n".join(d))
    else:
        print("  一致")
    print(f"\n共 {len(results['py'])} 步，实质差异 {bad}；目录差异 {len(d)} 处")
    if bad or d:
        print(f"留档：py={homes['py']}\n      rs={homes['rs']}")
    else:
        for h in homes.values():
            shutil.rmtree(h, ignore_errors=True)
    return 0 if (bad == 0 and not d) else 1


if __name__ == "__main__":
    raise SystemExit(main())
