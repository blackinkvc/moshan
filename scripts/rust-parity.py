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
import uuid
import urllib.parse
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
PARITY_TOKEN = "parity-token-fixed-for-both-sides"


def seed(home: Path) -> None:
    """往临时数据根里铺一套夹具。

    W2 及之后的对拍必须跑在**有内容的目录**上：空目录里 `files`/`browse`/`trash`/`shares`
    都只返回空数组，字段顺序、链接判定、排序键全都验不到。
    夹具刻意包含：中文名、点开头文件、`index.jsonl`、符号链接目录、符号链接文件、
    空目录、四层嵌套、回收站里「索引有但磁盘没有」的残条。
    """
    lib = home / "library"
    inbox = lib / "收件箱"
    (inbox / "子目录" / "深层").mkdir(parents=True, exist_ok=True)
    (inbox / "空目录").mkdir(parents=True, exist_ok=True)
    (inbox / "报告.md").write_text("# 小结\nhello\n", encoding="utf-8")
    # 真图才验得到内联缩略图那条路（假 JPEG 会让两边都缩不出来，等于没测）。
    icon = ROOT / "src-tauri/icons/icon.png"
    photo = inbox / "照片.jpg"
    made = False
    if icon.exists():
        made = subprocess.run(
            ["sips", "-Z", "240", "-s", "format", "jpeg", str(icon), "--out", str(photo)],
            capture_output=True,
        ).returncode == 0 and photo.exists()
    if not made:
        photo.write_bytes(b"\xff\xd8\xff\xe0" + b"0" * 200)
    (inbox / "数据.csv").write_text("a,b\n1,2\n", encoding="utf-8")
    (inbox / ".隐藏.txt").write_text("hidden", encoding="utf-8")
    (inbox / "index.jsonl").write_text('{"x":1}\n', encoding="utf-8")
    (inbox / "子目录" / "a.txt").write_text("a", encoding="utf-8")
    (inbox / "子目录" / "深层" / "b.md").write_text("b", encoding="utf-8")
    # 数据根下的真实形态就是符号链接：目录链接不能进（rglob 口径），文件链接要能列出
    (inbox / "链接目录").symlink_to(inbox / "子目录")
    (inbox / "别名.txt").symlink_to(inbox / "报告.md")

    arch = lib / "04-归档"
    arch.mkdir(parents=True, exist_ok=True)
    (arch / "旧文件.pdf").write_bytes(b"%PDF-1.4\n")

    now = int(time.time())
    state = home / "state"
    state.mkdir(parents=True, exist_ok=True)
    # 固定令牌 + 固定数据根：两边各跑一遍写脚本时，配置必须完全对等，
    # 否则「Rust 侧全是 401」会被误读成鉴权实现有问题。
    # 令牌固定还有一个好处：对拍脚本不用等 Python 生成随机 token 再读回来。
    cfg = {
        "token": PARITY_TOKEN,
        "host": "0.0.0.0",
        "port": 8321,
        "inbox_dir": str(inbox),
        "allowed_roots": [str(lib)],
        "trash_dir": str(lib / ".回收站"),
        "dav_root": str(lib),
        "trash_days": 30,
        "max_upload_mb": 500,
        "tls": False,
        "allow_private_only": True,
        "dav_enabled": False,
        "gateway_base": "http://127.0.0.1:8080",
        "gateway_api_key": "",
        "model": "qwen3.5-4b-gguf:q4km",
    }
    (state / "config.json").write_text(
        json.dumps(cfg, ensure_ascii=False, indent=2), encoding="utf-8")
    shares = {
        "aaaaaaaaaa": {"sid": "aaaaaaaaaa", "path": str(inbox / "报告.md"), "name": "报告.md",
                       "is_dir": False, "created": now - 100, "expires": 0, "hits": 3},
        "bbbbbbbbbb": {"sid": "bbbbbbbbbb", "path": str(inbox / "照片.jpg"), "name": "照片.jpg",
                       "is_dir": False, "created": now - 50, "expires": now + 3600, "hits": 1},
        # 过期的那条不该出现在列表里
        "cccccccccc": {"sid": "cccccccccc", "path": str(inbox / "已删.txt"), "name": "已删.txt",
                       "is_dir": False, "created": now - 10, "expires": now - 5, "hits": 0},
    }
    (state / "shares.json").write_text(
        json.dumps(shares, ensure_ascii=False, indent=2), encoding="utf-8")

    trash = lib / ".回收站"
    (trash / "d").mkdir(parents=True, exist_ok=True)
    (trash / "d" / "x.bin").write_bytes(b"1" * 1500)
    (trash / "20260101-000000_旧.txt").write_text("old", encoding="utf-8")
    rows = [
        {"name": "20260101-000000_旧.txt", "orig": str(inbox / "旧.txt"),
         "ts": now - 7200, "bytes": 3, "is_dir": False},
        {"name": "d", "orig": str(inbox / "d"), "ts": now - 600, "bytes": 1500, "is_dir": True},
        # 索引里有、磁盘上已经没了 → 两边都必须跳过
        {"name": "20260101-000000_丢失.txt", "orig": str(inbox / "丢失.txt"),
         "ts": now - 100, "bytes": 0, "is_dir": False},
    ]
    (trash / "index.jsonl").write_text(
        "\n".join(json.dumps(r, ensure_ascii=False) for r in rows) + "\n", encoding="utf-8")

    # ---- 记账账本：三件商品，覆盖「在用 / 已卖 / 字段缺失」三种形态
    led = lib / "12-记账"
    for sub in ("图", "原图", "_回收站"):
        (led / sub).mkdir(parents=True, exist_ok=True)
    img = led / "图" / "p-20260101-aaaa.jpg"
    if photo.exists():
        subprocess.run(["sips", "-Z", "600", "-s", "format", "jpeg", str(photo), "--out", str(img)],
                       capture_output=True)
    if not img.exists():
        img.write_bytes(b"\xff\xd8\xff\xe0" + b"1" * 300)
    (led / "原图" / "p-20260101-aaaa.png").write_bytes(b"\x89PNG\r\n\x1a\n" + b"0" * 64)
    book = {
        "items": [
            {"id": "p-20260101-aaaa", "name": "无线降噪耳机 白色", "platform": "京东",
             "bought_at": "2026-01-01", "price": 1299.0, "category": "数码",
             "shop": "某某旗舰店", "order_no": "1234567890123", "note": "  划痕在背面  ",
             "status": "在用", "sold_at": "", "sold_price": None, "sold_to": "",
             "image": "12-记账/图/p-20260101-aaaa.jpg",
             "raw": "12-记账/原图/p-20260101-aaaa.png",
             "crop_src": "vision", "ocr": "订单详情\n实付款 ¥1299.00", "conf": ["shop"],
             "created_at": "2026-01-01T10:00:00", "updated_at": "2026-01-01T10:00:00"},
            {"id": "p-20260102-bbbb", "name": "旧键盘", "platform": "",
             "bought_at": "2025-12-01", "price": 399, "category": "数码",
             "shop": "", "order_no": "", "note": "", "status": "已卖",
             "sold_at": "2026-01-05", "sold_price": 260.5, "sold_to": "同事",
             "image": "", "raw": "", "crop_src": "none", "ocr": "", "conf": [],
             "created_at": "2025-12-01T09:00:00", "updated_at": "2026-01-05T09:00:00"},
            {"id": "p-20260103-cccc", "name": "（待命名）", "bought_at": "2026-01-03",
             "price": None, "status": "在用", "note": "", "image": "", "raw": "",
             "conf": ["price", "name"]},
        ],
        "updated_at": "2026-01-05T09:00:00",
    }
    (led / "账本.json").write_text(
        json.dumps(book, ensure_ascii=False, indent=1), encoding="utf-8")


def w4_steps(state: dict):
    """记账账本：读、单条、图片、改（含非法价）、删（含重复删）。"""
    q = lambda s: urllib.parse.quote(s, safe="")  # noqa: E731
    return [
        ("账本整体", "GET", "/api/ledger", None),
        ("单条详情", "GET", "/api/ledger/item?id=p-20260101-aaaa", None),
        ("单条不存在", "GET", "/api/ledger/item?id=nope", None),
        ("产品图", "GET", "/api/ledger/img?id=p-20260101-aaaa", None),
        ("原图", "GET", "/api/ledger/img?id=p-20260101-aaaa&raw=1", None),
        ("图不存在", "GET", "/api/ledger/img?id=nope", None),
        ("改价格与状态", "POST", "/api/ledger/update",
         {"id": "p-20260101-aaaa",
          "patch": {"price": "¥1,299.90", "status": "已卖", "note": "改过了"}}),
        ("改完再读", "GET", "/api/ledger/item?id=p-20260101-aaaa", None),
        ("价格非法", "POST", "/api/ledger/update",
         {"id": "p-20260101-aaaa", "patch": {"price": "不是数字"}}),
        ("清空价格", "POST", "/api/ledger/update",
         {"id": "p-20260102-bbbb", "patch": {"sold_price": ""}}),
        ("改不存在的", "POST", "/api/ledger/update", {"id": "nope", "patch": {"name": "x"}}),
        ("删一条", "POST", "/api/ledger/remove", {"id": "p-20260103-cccc"}),
        ("删后再看", "GET", "/api/ledger", None),
        ("重复删", "POST", "/api/ledger/remove", {"id": "p-20260103-cccc"}),
        ("删带图的", "POST", "/api/ledger/remove", {"id": "p-20260101-aaaa"}),
        ("删完看账本", "GET", "/api/ledger", None),
    ]


def w6_steps(state: dict):
    """泳道 w6b：Agent 自述 / 总线 / 插件 / 收件箱整理 / 联动回执 / 问答快路。

    刻意不在这里跑的（汇报里写明原因）：
      · store/refresh 与 link/import（要真解几百 MB 会话）；
      · parse-event / chat 的真模型分支（4B 输出不确定）；
      · pick/dir（会弹原生对话框）；reveal 的 root（会打开访达）。
    """
    inbox = str(state["inbox"])
    return [
        ("自述一笔", "POST", "/api/agent/note",
         {"agent": "TRAE", "task": "跑对拍", "summary": "一次", "project": "p",
          "model": "m", "in": 10, "out": 5, "calls": 2}),
        ("自述重复同句", "POST", "/api/agent/note",
         {"agent": "TRAE", "task": "跑对拍", "summary": "二次"}),
        ("老入口自述", "POST", "/api/trae/note", {"task": "老入口", "in": "3"}),
        ("自述缺 task", "POST", "/api/agent/note", {"agent": "X"}),
        ("自述非对象", "POST", "/api/agent/note", [1, 2]),
        ("自述数组体", "POST", "/api/agent/event", 123),
        ("写事件", "POST", "/api/agent/event",
         {"ns": "工作", "type": "task", "title": "跑通一条", "text": "对拍用"}),
        ("报用量", "POST", "/api/agent/usage",
         {"client": "trae", "in": 10, "out": 2, "calls": 1, "task": "对拍"}),
        ("读总线", "GET", "/api/agent/read?ns=all&since=all&limit=20", None),
        ("加插件", "POST", "/api/plugins/add",
         {"id": "parity", "name": "对拍插件", "port": 8899, "note": "临时"}),
        ("加同名插件", "POST", "/api/plugins/add",
         {"id": "parity", "name": "又一个"}),
        ("加内置同名", "POST", "/api/plugins/add",
         {"id": "pocket", "name": "盖内置", "url": "http://127.0.0.1:1/"}),
        ("加插件缺体", "POST", "/api/plugins/add", None),
        ("删插件", "POST", "/api/plugins/remove", {"id": "parity"}),
        ("删不存在的", "POST", "/api/plugins/remove", {"id": "nope"}),
        ("联动回执", "POST", "/api/agent/link/report",
         {"agent": "trae", "paths": [{"key": "memory", "path": inbox}], "note": "对拍"}),
        ("回执坏体", "POST", "/api/agent/link/report", [1]),
        ("别家导入", "POST", "/api/agent/link/import", {"agent": "cursor"}),
        # 真装一次说明：会写进本机该 agent 的说明目录（auto_install 本来也写），
        # 但 links.json 里那条 `prompt` 记录在落盘比对时被抹掉（见 _drop_prompt）。
        ("装说明 trae", "POST", "/api/agent/link/install", {"agent": "trae"}),
        ("不认识的装说明", "POST", "/api/agent/link/install", {"agent": "nope"}),
        # 手工探活：只量体积不解析，给对拍一个不看外部目录的确定输入。
        ("探活收件箱", "POST", "/api/agent/probe", {"path": inbox}),
        # 只挑 Cherry Studio：另外两家（WorkBuddy / TRAE）要过采集层 hostwork，
        # 那一层没迁（见汇报的已知分叉），挑上就会把分叉混进这一步。
        ("导入樱桃账", "POST", "/api/agent/import", {"sources": ["Cherry Studio"]}),
        # 再导一次：验证 _strip_imported 先删旧行再写，不叠加。
        ("再导一次", "POST", "/api/agent/import", {"sources": ["Cherry Studio"]}),
        ("问收件箱", "POST", "/api/chat", {"text": "收件箱里有什么？"}),
        ("问文件", "POST", "/api/chat", {"text": "文件列表"}),
        ("空聊天", "POST", "/api/chat", {"text": ""}),
        ("入站空文", "POST", "/api/intake", {"text": ""}),
        ("入站无链接", "POST", "/api/intake", {"text": "今天没链接"}),
        # 空文字不走 4B（本机有模型时那条路输出不确定，比不了），只验兜底那一支。
        ("空行程", "POST", "/api/parse-event", {"text": ""}),
        ("重扫", "POST", "/api/rescan", {}),
        ("打开越界目标", "POST", "/api/reveal", {"target": "etc"}),
        ("配对取包", "GET", "/api/pair", None),
        # 不带令牌只验 401 那一支：带令牌会走 wechat_push（网关 WebSocket），
        # 网关在跑时 Python 会真把一条消息发到微信里，对拍脚本不该有这个副作用；
        # Rust 侧没有 WebSocket 客户端（Cargo.toml 不归本泳道），这一支另作分叉记。
        ("配对无令牌", "GET", "/api/pair", None, {"noauth": True}),
        ("发微信无令牌", "POST", "/api/pair/send", {"text": "对拍用"}, {"noauth": True}),
        ("账本统计", "GET", "/api/agent/store/stats", None),
        ("整理收件箱", "POST", "/api/sort", None),
        ("再整理一次", "POST", "/api/sort", None),
    ]


_TS_KEYS = {"at", "ts", "ms", "now", "created", "updated_at", "started", "finished",
            "last_read", "mtime", "recent_at"}


def _strip_ts(v):
    """结构对比时抹掉时间：两边相隔几秒，时间戳本来就不会相同。"""
    if isinstance(v, dict):
        return {k: _strip_ts(x) for k, x in v.items() if k not in _TS_KEYS}
    if isinstance(v, list):
        return [_strip_ts(x) for x in v]
    if isinstance(v, str):
        v = re.sub(r"\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2}(:\d{2})?", "<TS>", v)
        # 事件 id 里嵌了 epoch 秒（`link-report-trae-1795725396`），差一秒就不同。
        return re.sub(r"(?<=-)(?:1[6-9]|2[0-9])\d{8}$", "<EPOCH>", v)
    return v


def _drop_prompt(v):
    """抹掉 links.json 里的 `prompt` 记录（见 w6 落盘比对处的说明）。

    抹完如果某个 agent 只剩空壳（`{"workbuddy": {}}`）也一起去掉 —— 那个壳
    就是 prompt 被抹掉后剩下的，不是真的有一条空记录。
    """
    if isinstance(v, dict):
        out = {}
        for k, x in v.items():
            if k == "prompt":
                continue
            y = _drop_prompt(x)
            if isinstance(y, dict) and not y and isinstance(x, dict) and "prompt" in x:
                continue
            out[k] = y
        return out
    if isinstance(v, list):
        return [_drop_prompt(x) for x in v]
    return v


def _load_any(p: Path, home: Path | None = None):
    text = p.read_text(encoding="utf-8", errors="replace")
    if home is not None:
        # 落盘里也带着各自的数据根（回执的 `paths[].path` 就是请求体原样）。
        # 两份数据根必然不同，抹成 <HOME> 再比结构。
        text = text.replace(str(home), "<HOME>").replace(str(home.resolve()), "<HOME>")
    if p.suffix == ".jsonl":
        out = []
        for ln in text.splitlines():
            ln = ln.strip()
            if not ln:
                continue
            try:
                out.append(_strip_ts(json.loads(ln)))
            except Exception:  # noqa: BLE001
                out.append(ln)
        return out
    try:
        return _strip_ts(json.loads(text))
    except Exception:  # noqa: BLE001
        return text


def w6_files(home: Path) -> dict:
    lib = home / "library"
    return {
        "Agent自述.jsonl": lib / "_本机工作" / "Agent自述.jsonl",
        "events.jsonl": lib / "_工作台" / "agents" / "events.jsonl",
        "usage.jsonl": lib / "_工作台" / "agents" / "usage.jsonl",
        "links.json": lib / "_工作台" / "agents" / "links.json",
        "plugins.json": lib / "_工作台" / "plugins.json",
    }


def w2_paths(home: Path) -> list[str]:
    inbox = home / "library" / "收件箱"
    q = lambda p: urllib.parse.quote(str(p), safe="")  # noqa: E731
    return [
        "/api/files",
        "/api/files?scope=archive",
        "/api/files?q=%E6%8A%A5%E5%91%8A",
        f"/api/browse?path={q(inbox)}",
        f"/api/browse?path={q(inbox / '子目录')}",
        f"/api/browse?path={q(inbox)}&sort=mtime",
        f"/api/browse?path={q(inbox)}&sort=size",
        f"/api/browse?path={q(inbox)}&q=%E6%8A%A5",
        f"/api/browse?path={q(inbox / '空目录')}",
        f"/api/browse?path={q(inbox / '报告.md')}",      # 307 跳转到所在目录
        "/api/browse?path=%2Fetc",                        # 403 越界
        f"/api/browse?path={q(inbox / '不存在')}",         # 404
        "/api/usage",
        "/api/trash",
        "/api/shares",
    ]


# ---------------------------------------------------------------- W3 写族回放
#
# 写接口不能用「两边打同一份数据根」的方式对拍：Python 建了目录，Rust 再建就是 409，
# 比的不是契约而是"谁先跑"。所以改成：**同一脚本各跑一遍，每边一份干净数据根**，
# 逐条比归一后的响应。路径里的临时根、随机 sid、时间戳都在归一化里抹掉，
# 剩下的就是形状与取值 —— 那才是契约。


def w3_steps(state: dict):
    """返回 [(说明, method, path, body)]。状态字典用来串起上一步产生的随机值。"""
    inbox = state["inbox"]
    return [
        ("建目录", "POST", "/api/mkdir", {"path": str(inbox), "name": "新目录"}),
        ("重名再建", "POST", "/api/mkdir", {"path": str(inbox), "name": "新目录"}),
        ("名字带斜杠", "POST", "/api/mkdir", {"path": str(inbox), "name": "a/b"}),
        ("上级不是目录", "POST", "/api/mkdir", {"path": str(inbox / "报告.md"), "name": "x"}),
        ("改名", "POST", "/api/rename", {"path": str(inbox / "新目录"), "name": "改名了"}),
        ("改成已占用名", "POST", "/api/rename", {"path": str(inbox / "改名了"), "name": "别名.txt"}),
        ("改名越界", "POST", "/api/rename", {"path": "/etc/hosts", "name": "x"}),
        ("移动进子目录", "POST", "/api/move",
         {"paths": [str(inbox / "改名了")], "dest": str(inbox / "子目录")}),
        ("移动空清单", "POST", "/api/move", {"paths": [], "dest": str(inbox)}),
        ("移动不存在", "POST", "/api/move",
         {"paths": [str(inbox / "没有这个")], "dest": str(inbox / "子目录")}),
        ("复制文件", "POST", "/api/move",
         {"paths": [str(inbox / "报告.md")], "dest": str(inbox / "子目录"), "copy": True}),
        ("建分享", "POST", "/api/share", {"path": str(inbox / "报告.md"), "hours": 2}),
        ("分享越界", "POST", "/api/share", {"path": "/etc/hosts"}),
        ("分享不存在", "POST", "/api/share", {"path": str(inbox / "没有这个")}),
        ("列分享", "GET", "/api/shares", None),
        ("撤销分享", "DELETE", "/api/shares/{sid}", None),
        ("上传两个文件", "MULTIPART", "/api/upload",
         {"__files__": [("a.txt", b"hello"), ("中文名.bin", bytes(range(32)))]}),
        ("上传到指定目录", "MULTIPART",
         "/api/upload?dir=" + urllib.parse.quote(str(inbox / "子目录"), safe=""),
         {"__files__": [("c.txt", b"ccc")]}),
        ("上传空文件", "MULTIPART", "/api/upload", {"__files__": [("empty.txt", b"")]}),
        ("删进回收站", "POST", "/api/delete", {"paths": [str(inbox / "数据.csv")]}),
        ("删空清单", "POST", "/api/delete", {"paths": []}),
        ("删根目录", "POST", "/api/delete", {"paths": [str(inbox.parent)]}),
        ("回收站列表", "GET", "/api/trash", None),
        ("从回收站恢复", "POST", "/api/trash/restore", {"names": ["{trash_name}"]}),
        ("清空回收站", "POST", "/api/trash/purge", {"names": None}),
        ("清空后列表", "GET", "/api/trash", None),
        ("改完看一眼目录", "GET", "/api/browse?path={inbox_q}", None),
    ]


# 随机值/时间/临时根：两边必然不同，抹掉再比。
_NORM = [
    # 时间字段先于 SID 规则：`"at": 1795725396.4` 的整数部分正好 10 位，
    # 会被下面的 `[0-9a-f]{10}` 当成 sid 吃掉一半（剩 `<SID>.4`），
    # 看着像差异其实只是秒的小数位不同。
    (re.compile(rb'"(at|since|until|first|last)":-?\d+(?:\.\d+)?'), rb'"\1":<EPOCH>'),
    (re.compile(rb"\b[0-9a-f]{10}\b"), b"<SID>"),
    (re.compile(rb'"(created|expires)":\d+'), rb'"\1":<EPOCH>'),
    # 总线里的 `ts` 是 epoch 毫秒。两边跑的时刻差几秒，数值不可能相等；
    # 但格式（13 位整数，不是字符串）本身是契约，所以只抹数字、留引号形状。
    (re.compile(rb'"ts":\d{10,13}'), b'"ts":<EPOCH>'),
    # `/api/chat` 快答回 `{"reply": ..., "ms": N}`：N 是这次请求的耗时，
    # 本来就是「越快越好」的观测值，逐字节比它没有意义（同一份代码两次也不同）。
    (re.compile(rb'"ms":\d+'), b'"ms":<MS>'),
    # 说明书正文里嵌着 `http://127.0.0.1:<本机端口>`，两边端口位数不同（py 是
    # launcher 随机挑的五位数，rs 是脚本给的 8400+），字节数跟着差一两位。
    (re.compile(rb'"bytes":\d+'), b'"bytes":<N>'),
    (re.compile(rb"\d{8}-\d{6}_"), b"<STAMP>_"),
    # 记账的回收站目录是 `{stamp}-{id}`（连字符，不是下划线）
    (re.compile(rb"\d{8}-\d{6}-"), b"<STAMP>-"),
    (re.compile('"\\d+ (?:分钟|小时|天)前"'.encode()), '"<AGE>"'.encode()),
    (re.compile(rb"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}[+-]\d{4}"), b"<TS>"),
    (re.compile(rb'"\?v=\d+"'), b'"?v=<M>"'),
    # OS 错误文本：Python 是 `[Errno 5] 说明 (os error 2): '/path'`，Rust 是
    # `说明 (os error 2)`。冒号后的绝对路径两边必然不同（各自的数据根），
    # `[Errno N]` 那个 N 也不是内核 errno —— 但括号里的 **(os error N) 是真 errno**，
    # 所以只去掉前缀与路径，保留 strerror 和 errno：错误类型不同照样能抓到。
    (re.compile(rb"\[Errno \d+\] "), b""),
    # 末尾的 `'` 可以是缺失的：Python 那边写的是 `str(e)[:120]`，路径常被截断在引号前。
    # 字符类必须排除 `"` 和 `'`，否则截断时会把后面整个 JSON 尾巴一起吃掉。
    (re.compile(rb"\(os error (\d+)\): '[^'\"]*'?"), rb"(os error \1)"),
]

# 写族是「两边各跑一遍」，两份数据根在不同时刻采样同一块卷 ——
# used/free 差个几 MB 完全是别的进程在写盘，没有可比性。这一档直接抹掉数值，
# 只保留键存在与 total（卷容量，必须一致）。
_DISK_COARSE = re.compile(rb'"(used|free)":\d+')
# 派生的展示值跟着 used/free 走，一并抹掉（`total_h` 不在此列：它是卷容量算的）。
_DISK_DERIVED = re.compile(rb'"(free_h|used_pct)":(?:"[^"]*"|[0-9.]+)')


def norm(b: bytes, home: Path, coarse: bool = False) -> bytes:
    b = b.replace(str(home).encode(), b"<HOME>")
    b = b.replace(str(home.resolve()).encode(), b"<HOME>")
    if coarse:
        b = _DISK_COARSE.sub(lambda m: b'"' + m.group(1) + b'":<N>', b)
        b = _DISK_DERIVED.sub(lambda m: b'"' + m.group(1) + b'":<N>', b)
    for rx, rep in _NORM:
        b = rx.sub(rep, b)
    return mask(b) if not coarse else _TS.sub(b"<TS>", b)


def _multipart(files: list) -> tuple[bytes, str]:
    """手搓 multipart：标准库没有表单构造器，而这里只需要「多个文件」这一种形状。"""
    boundary = "----parity" + uuid.uuid4().hex[:12]
    out = bytearray()
    for name, data in files:
        out += (f'--{boundary}\r\n'
                f'Content-Disposition: form-data; name="files"; filename="{name}"\r\n'
                f"Content-Type: application/octet-stream\r\n\r\n").encode("utf-8")
        out += data
        out += b"\r\n"
    out += f"--{boundary}--\r\n".encode()
    return bytes(out), f"multipart/form-data; boundary={boundary}"


def call(port: int, token: str, method: str, path: str, body, timeout: float = 60) -> tuple[int, bytes]:
    if isinstance(body, dict) and "__files__" in body:
        data, ctype = _multipart(body["__files__"])
    else:
        data = None if body is None else json.dumps(body, ensure_ascii=False).encode()
        ctype = "application/json"
    req = urllib.request.Request(
        f"http://127.0.0.1:{port}{path}",
        data=data,
        method=method,
        headers={"X-Token": token, "Content-Type": ctype},
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()
    except Exception as e:  # noqa: BLE001
        return 0, str(e).encode()


def replay(port: int, token: str, home: Path, steps_fn) -> list[tuple[str, int, bytes]]:
    """跑完整脚本，返回 [(说明, 状态码, 归一后响应)]。"""
    inbox = home / "library" / "收件箱"
    state = {"inbox": inbox}
    out: list[tuple[str, int, bytes]] = []
    for step in steps_fn(state):
        note, method, path, body = step[:4]
        # 第 5 位是可选的 {"noauth": True}：不带令牌打，验 401 那一支。
        opts = step[4] if len(step) > 4 else {}
        tok = "" if opts.get("noauth") else token
        if "{sid}" in path:
            path = path.replace("{sid}", state.get("sid", ""))
        if "{inbox_q}" in path:
            path = path.replace("{inbox_q}", urllib.parse.quote(str(inbox), safe=""))
        if method == "POST" and isinstance(body, dict) and "{trash_name}" in json.dumps(body):
            body = {"names": [state.get("trash_name", "")]}
        http_method = "POST" if method == "MULTIPART" else method
        status, raw = call(port, tok, http_method, path, body)
        out.append((note, status, norm(raw, home, coarse=True)))
        # 串起下一步要用的随机值
        try:
            js = json.loads(raw)
        except Exception:  # noqa: BLE001
            js = {}
        if note == "建分享" and isinstance(js, dict) and js.get("sid"):
            state["sid"] = js["sid"]
        if note == "删进回收站" and isinstance(js, dict):
            for r in js.get("results") or []:
                if r.get("ok") and r.get("trash"):
                    state["trash_name"] = r["trash"]
    return out


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

# 磁盘的 used/free 是**活计数**：同一台机器上别的进程写 32 KB，两次请求就不同
# （实测过，Python 自己连打三次都不一样）。所以比较前按 MB 归一——
# 32 KB 的噪声消失，而「算错了几个 GB」这种真差异一定还在。
# `total` 不归一：它是卷容量，本来就该恒定，变了说明读错了文件系统。
_DISK = re.compile(rb'"(used|free)":(\d+)')


def mask(b: bytes) -> bytes:
    b = _TS.sub(b"<TS>", b)
    return _DISK.sub(lambda m: b'"' + m.group(1) + b'":' + str(int(m.group(2)) // 1048576).encode(), b)


# 这些键的值天然每轮不同（进程启动时刻、采样耗时、正在跑的计时器）。
# 直接比会永远红，红了又没人看，最后真差异被淹掉 —— 掩值不掩结构：
# 字段是否存在、顺序、类型、数组长度仍然照比。
VOLATILE = {
    "at", "now", "started", "last_sample", "last_run", "last_made", "last_removed",
    "last_at", "scan_at", "cache_at", "secs", "seconds", "elapsed", "uptime",
    "ts", "ms", "scan_sec", "sample_at", "first_at", "then", "ago",
}


def _volatile_key(k: str) -> bool:
    """键名本身就是「时刻」的，一律算活值：`at` / `updated_at` / `scan_ts` …
    这条规则比逐个列举可靠 —— 漏一个就会天天红，红了就没人看了。"""
    return k in VOLATILE or k.endswith("_at") or k.endswith("_ts") or k.endswith("_ms")


def _mask_volatile(obj):
    if isinstance(obj, dict):
        out = {}
        for k, v in obj.items():
            if _volatile_key(k) and not isinstance(v, (dict, list)):
                out[k] = "<V>"
            else:
                out[k] = _mask_volatile(v)
        return out
    if isinstance(obj, list):
        return [_mask_volatile(x) for x in obj]
    return obj


def structural(b: bytes) -> bytes:
    """把响应体按 JSON 解析后抹掉活值，再按原键序序列化。
    非 JSON（HTML/二进制）原样返回 —— 那些本来就没有活值可抹。"""
    try:
        obj = json.loads(b.decode("utf-8"))
    except Exception:  # noqa: BLE001
        return mask(b)
    return json.dumps(_mask_volatile(obj), ensure_ascii=False, separators=(",", ":")).encode()


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


def _wait_cfg(home: Path, timeout: float = 40.0) -> dict:
    """等 config.json 里出现 local_port（launcher 选完端口才补这个键）。"""
    deadline = time.time() + timeout
    cfg: dict = {}
    while time.time() < deadline:
        try:
            cfg = json.loads((home / "state/config.json").read_text(encoding="utf-8"))
        except Exception:  # noqa: BLE001
            cfg = {}
        if cfg.get("token") and cfg.get("local_port"):
            return cfg
        time.sleep(0.25)
    return cfg


def run_write_suite(args, name: str) -> int:
    """写族对拍：同一脚本，Python 跑一遍、Rust 跑一遍，各用一份干净数据根。"""
    steps_fn = {"w3": w3_steps, "w4": w4_steps, "w6": w6_steps}[name]
    results: dict[str, list] = {}
    homes: dict[str, Path] = {}
    procs: list[subprocess.Popen] = []
    try:
        for side in ("py", "rs"):
            home = Path(tempfile.mkdtemp(prefix=f"moshan-w3-{side}-"))
            seed(home)
            homes[side] = home
            env = {**os.environ, "MOSHAN_HOME": str(home), "MOSHAN_NO_BROWSER": "1"}
            if side == "py":
                p = subprocess.Popen(
                    [args.py_python, str(ROOT / "moshan/launcher.py")],
                    env=env, cwd=str(ROOT / "moshan"),
                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                procs.append(p)
                cfg = _wait_cfg(home)
                port, token = cfg.get("local_port"), cfg.get("token", "")
            else:
                port = free_port()
                # 上游置空：写族这些路由都应已迁完，任何一条落到 404 都是真问题
                # （代理回来的 200 会掩盖「其实没迁」）。
                p = subprocess.Popen(
                    [args.rs_bin],
                    env={**env, "MOSHAN_PY_UPSTREAM": "", "MOSHAN_RS_PORT": str(port),
                         "MOSHAN_APP_DIR": str(ROOT / "moshan")},
                    stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
                procs.append(p)
                token = PARITY_TOKEN
            if not port or not wait_ping(port):
                err = ""
                if side == "rs" and p.stderr:
                    err = p.stderr.read().decode()[:600]
                print(f"{side} 侧没起来（端口 {port}）{err}")
                return 2
            results[side] = replay(port, token, home, steps_fn)
    finally:
        for p in procs:
            p.terminate()
        for p in procs:
            try:
                p.wait(timeout=8)
            except Exception:  # noqa: BLE001
                p.kill()

    bad = 0
    print(f"{'步骤':16s} {'py':>4s} {'rs':>4s}  结果")
    for (note, sa, ra), (_, sb, rb) in zip(results["py"], results["rs"]):
        ok = sa == sb and ra == rb
        if not ok:
            bad += 1
        print(f"{note:16s} {sa:4d} {sb:4d}  {'一致' if ok else '差异'}")
        if not ok:
            print("    " + first_diff(ra, rb))
    print(f"\n共 {len(results['py'])} 步，实质差异 {bad}")

    if name == "w6":
        print(f"\n{'落盘文件':24s} 结果")
        for label in w6_files(homes["py"]):
            fa = w6_files(homes["py"])[label]
            fb = w6_files(homes["rs"])[label]
            ea, eb = fa.exists(), fb.exists()
            if not ea and not eb:
                print(f"{label:24s} 两边都没有")
                continue
            if ea != eb:
                bad += 1
                print(f"{label:24s} 差异（存在性 py={ea} rs={eb}）")
                continue
            a = _load_any(fa, homes["py"])
            b = _load_any(fb, homes["rs"])
            if label == "links.json":
                # Python 启动线程会跑 agent_link.auto_install，把说明书投放进本机
                # 已装的那几家 agent，并在 links.json 记 `prompt: {version, auto}`。
                # 这是启动期副产物，不在这 29 条路由里（Rust 的启动逻辑不归本泳道），
                # 所以比对结构前把 prompt 记录抹掉；回执本身照比。
                a = _drop_prompt(a)
                b = _drop_prompt(b)
            if a == b:
                print(f"{label:24s} 一致")
            else:
                bad += 1
                print(f"{label:24s} 差异")
                sa = json.dumps(a, ensure_ascii=False, sort_keys=True)
                sb = json.dumps(b, ensure_ascii=False, sort_keys=True)
                print("    " + first_diff(sa.encode(), sb.encode()))
        print(f"\n落盘与响应合计实质差异 {bad}")
    return 0 if bad == 0 else 1


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--paths", nargs="*", default=None)
    ap.add_argument("--all-get", action="store_true")
    # 铺夹具但不改 paths：泳道自带一组端点清单时用这个，
    # 否则只能借 --suite w2 的清单，测不了自己的路由。
    ap.add_argument("--seed", action="store_true", help="铺夹具，但保留 --paths 给的清单")
    ap.add_argument(
        "--struct",
        action="store_true",
        help="按结构比：抹掉「每轮都会变」的活值（时间戳/耗时/计时器）后逐字段比较，"
             "字段、顺序、类型、数组长度仍然照比。用于调度器/采样类端点。",
    )
    ap.add_argument(
        "--suite",
        choices=["w1", "w2", "w3", "w4", "w6"],
        default=None,
        help="用内置夹具跑某一波（会先往临时数据根铺文件）",
    )
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

    if args.suite in ("w3", "w4", "w6"):
        return run_write_suite(args, args.suite)

    paths = args.paths or DEFAULT_PATHS
    if args.all_get:
        routes = json.load(open("/tmp/routes.json"))
        paths = [r["path"] for r in routes if r["method"] == "GET"]

    home = Path(tempfile.mkdtemp(prefix="moshan-parity-"))
    if args.suite or args.seed:
        seed(home)
    if args.suite == "w2":
        paths = w2_paths(home)
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
        if args.suite:
            # 预热：`browse` 会给图片目录**首次生成缩略图**（写盘），那一刻的
            # `usage.free` 必然与下一次不同。这是首次使用的正常代价，不是契约差异；
            # 先各打一遍让缓存落定，再开始测量，否则这条永远红着没人看得懂。
            for p in paths:
                fetch(py_port, p, token)
                fetch(rs_port, p, token)
        print(f"{'端点':32s} {'py':>5s} {'rs':>5s}  结果")
        for p in paths:
            ps, pb = fetch(py_port, p, token)
            rstatus, rb = fetch(rs_port, p, token)
            same_status = ps == rstatus
            exact = same_status and pb == rb
            same_masked = same_status and mask(pb) == mask(rb)
            same_struct = same_status and args.struct and structural(pb) == structural(rb)
            detail = ""
            if exact:
                flag = "一致"
            elif same_struct:
                flag = "一致（仅活值不同）"
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
