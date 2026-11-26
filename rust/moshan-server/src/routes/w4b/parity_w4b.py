#!/usr/bin/env python3
"""w4b 写族对拍：AI 工作计划 / 每日简报 / 笔记 / 待确认。

用法：
    python3 rust/moshan-server/src/routes/w4b/parity_w4b.py --rs-bin /tmp/rs-w4b/release/moshan-server

做法照抄 scripts/rust-parity.py 的 w4_write_suite：
  两边各起一份**干净数据根**，跑同一串请求，比归一后的响应字节；
  再把两边的**数据文件**（计划.json / notes.json / _待定.json / 简报）拉出来 diff ——
  那比响应更能说明「写进去的东西一样」。

几个必须归一的东西（两边必然不同、又不是契约）：
  · 随机 id：pr_/tk_/ib_ 六个十六进制 + 备忘的 m + 十一位；
  · 时间戳、磁盘余量；
  · 临时数据根路径。
id 用「按首次出现顺序编号」的方式归一，这样 id 之间的**引用关系**仍然参与比较。

为什么把 Python 侧产出的 hostwork 缓存拷到 Rust 侧：
  Rust 侧只读 `_本机工作/缓存/会话.json`（写它的是 Python 采集层）。真实部署里
  两边共用同一个数据根，缓存本来就在；对拍为了写隔离用了两份根，所以要补齐这份
  只读输入。心跳快照同理。
"""
from __future__ import annotations

import argparse
import importlib.util
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.parse
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[4]          # /Volumes/AI仓库/Moshan
PARITY = ROOT / "scripts" / "rust-parity.py"

spec = importlib.util.spec_from_file_location("rparity", PARITY)
rp = importlib.util.module_from_spec(spec)
spec.loader.exec_module(rp)

TOKEN = rp.PARITY_TOKEN
NOW = time.time()      # 夹具统一用同一个「现在」，两边写出来的 asked_at 才一致


# ---------------------------------------------------------------- 夹具

def pend_fixture(home: Path) -> None:
    """待确认队列：两条挂起单，一条超龄（该过期）、一条刚问的。"""
    now = NOW
    p = home / "library" / "04-存图" / "_待定.json"
    p.parent.mkdir(parents=True, exist_ok=True)
    items = [
        {"id": "pdead0001", "kind": "link", "job": "j1", "url": "https://example.com/a",
         "title": "窗边的小豆豆", "platform": "微博", "dir_name": "", "text_len": 1200,
         "n_img": 9, "n_video": 0, "suggest": "gallery", "why": "两边都像",
         "agent": "main", "channel": "openclaw-weixin", "conv": "",
         "state": "open", "asked_at": now - 9 * 86400, "answered_at": 0.0,
         "pushed_at": 1.0, "push_tries": 0, "next_try": 0.0},
        {"id": "palive002", "kind": "article", "job": "j2", "url": "https://example.com/b",
         "title": "长文：模型蒸馏", "platform": "知乎", "dir_name": "", "text_len": 8000,
         "n_img": 3, "n_video": 1, "suggest": "article", "why": "正文很长",
         "agent": "main", "channel": "openclaw-weixin", "conv": "",
         "state": "open", "asked_at": now - 60, "answered_at": 0.0,
         "pushed_at": 1.0, "push_tries": 0, "next_try": 0.0},
    ]
    p.write_text(json.dumps({"v": 1, "items": items}, ensure_ascii=False, indent=1),
                 encoding="utf-8")


NOTES_BODY = {
    "todos": [
        {"id": "t1", "text": "把简报推给手机", "done": False, "due": "2026-11-27", "created": "2026-11-20"},
        {"id": "t2", "text": "校对回忆录第三章", "done": True, "due": "2026-11-26", "created": "2026-11-10"},
        {"id": "t3", "text": "没有截止日期的一条", "done": False, "due": "", "created": "2026-11-01"},
    ],
    "events": [
        {"id": "e1", "date": "2026-11-27", "time": "14:30", "title": "牙医", "place": "口腔医院",
         "done": False, "created": "2026-11-20T10:00:00", "to": "城西", "from": "家"},
        {"id": "e2", "date": "2026-12-01", "time": "08:00", "title": "", "from": "北京",
         "to": "上海", "endDate": "2026-12-01", "endTime": "12:20", "flight": "MU5101",
         "fee": "¥1,280", "stops": ["南京"], "delay": "35", "delayNote": "流量控制"},
    ],
    "memo": [
        {"id": "m1", "text": "记得买电池", "at": "2026-11-26T21:00:00"},
        {"id": "m2", "text": "多行\n备忘", "at": "2026-11-05T09:12:00"},
    ],
    "diary": {"2026-11-27": "今天把简报改成五段。"},
}


def plan_steps(state: dict):
    """计划：增删改 / 打钩 / 收件箱归位 / 导入 / 错误分支。"""
    inbox = state["inbox"]
    return [
        ("计划初始", "GET", "/api/plan", None),
        ("建项目", "POST", "/api/plan/act",
         {"action": "proj.add", "name": "个人中台 v2", "note": "把中台搬去 Rust",
          "due": "2026-12-31", "keys": ["中台", "moshan"]}),
        ("建项目重名空名", "POST", "/api/plan/act", {"action": "proj.add", "name": "  "}),
        ("加任务", "POST", "/api/plan/act",
         {"action": "task.add", "proj": "{pid}", "title": "迁 w4b 的 13 条路由",
          "due": "2026-11-28", "note": "逐字节"}), 
        ("加子任务", "POST", "/api/plan/act",
         {"action": "task.addChild", "proj": "{pid}", "parent": "{tid}",
          "title": "先迁计划", "src": "import"}),
        ("打钩", "POST", "/api/plan/act",
         {"action": "task.toggle", "proj": "{pid}", "id": "{tid}"}),
        ("取消打钩", "POST", "/api/plan/act",
         {"action": "task.toggle", "proj": "{pid}", "id": "{tid}", "done": False}),
        ("打钩不存在", "POST", "/api/plan/act",
         {"action": "task.toggle", "proj": "{pid}", "id": "tk_nope"}),
        ("收件箱加一条", "POST", "/api/plan/act",
         {"action": "inbox.add", "text": "研究页本地软件增加新闻摘录功能", "note": "随口记"}),
        ("收件箱看建议", "GET", "/api/plan", None),
        ("代码建议", "POST", "/api/plan/triage", {"ids": []}),
        ("指定 id 建议", "POST", "/api/plan/triage", {"ids": ["{iid}"]}),
        ("归位到项目", "POST", "/api/plan/act",
         {"action": "inbox.assign", "id": "{iid}", "proj": "{pid}",
          "title": "研究页：新闻摘录"}),
        ("收件箱再加一条", "POST", "/api/plan/act",
         {"action": "inbox.add", "text": "给影音室做片源标签词表"}),
        ("归位新建项目", "POST", "/api/plan/act",
         {"action": "inbox.assign", "id": "{iid2}"}),
        ("归位不存在", "POST", "/api/plan/act",
         {"action": "inbox.assign", "id": "ib_nope"}),
        ("收件箱第三条", "POST", "/api/plan/act",
         {"action": "inbox.add", "text": "待批量落库的一条"}),
        ("批量落库", "POST", "/api/plan/assign",
         {"items": [{"id": "{iid3}", "proj": "{pid}", "title": "批量落库任务"},
                    {"id": "ib_nope"}]}),
        ("批量落库空", "POST", "/api/plan/assign", {"items": []}),
        ("改项目名", "POST", "/api/plan/act",
         {"action": "proj.rename", "id": "{pid}", "name": "个人中台 Rust"}),
        ("绑空间", "POST", "/api/plan/act",
         {"action": "proj.link", "id": "{pid}", "spaces": ["space:个人中台"], "keys": ["中台"]}),
        ("改状态", "POST", "/api/plan/act",
         {"action": "proj.status", "id": "{pid}", "status": "paused"}),
        ("改状态非法", "POST", "/api/plan/act",
         {"action": "proj.status", "id": "{pid}", "status": "sleeping"}),
        ("改任务", "POST", "/api/plan/act",
         {"action": "task.rename", "proj": "{pid}", "id": "{kid}",
          "title": "先迁计划（改）", "note": "改过", "link": "space:x"}),
        ("移动任务", "POST", "/api/plan/act",
         {"action": "task.move", "proj": "{pid}", "id": "{kid}", "parent": "{tid}"}),
        ("删任务", "POST", "/api/plan/act",
         {"action": "task.del", "proj": "{pid}", "id": "{kid}"}),
        ("导入预览", "POST", "/api/plan/act",
         {"action": "import", "text": "# 周计划\n\n- [ ] 写周报\n  - [x] 收数据\n1. 开周会", "name": ""}),
        ("导入落库", "POST", "/api/plan/act",
         {"action": "import", "text": "# 周计划\n\n- [ ] 写周报\n  - [x] 收数据\n1. 开周会",
          "apply": True}),
        ("再导一次合并", "POST", "/api/plan/act",
         {"action": "import", "text": "# 周计划\n\n- [ ] 写周报\n- [ ] 新的一条", "apply": True}),
        ("不认识的 action", "POST", "/api/plan/act", {"action": "wat"}),
        ("空 action", "POST", "/api/plan/act", {}),
        ("删项目", "POST", "/api/plan/act", {"action": "proj.del", "id": "{pid2}"}),
        ("删项目不存在", "POST", "/api/plan/act", {"action": "proj.del", "id": "pr_nope"}),
        ("计划末态", "GET", "/api/plan", None),
    ]


def notes_steps(state: dict):
    return [
        ("笔记初始", "GET", "/api/notes", None),
        ("整份写", "POST", "/api/notes", NOTES_BODY),
        ("整份读回", "GET", "/api/notes", None),
        ("追加待办", "POST", "/api/notes/add",
         {"kind": "todo", "text": "洗碗机洗完了", "key": "device:dish", "src": "设备"}),
        ("重复追加", "POST", "/api/notes/add",
         {"kind": "todo", "text": "洗碗机又洗完了", "key": "device:dish"}),
        ("追加备忘", "POST", "/api/notes/add", {"kind": "memo", "text": "月度备忘一条"}),
        ("空文本", "POST", "/api/notes/add", {"kind": "todo", "text": "   "}),
        ("超长截断", "POST", "/api/notes/add", {"text": "长" * 260}),
        ("笔记末态", "GET", "/api/notes", None),
    ]


def pending_steps(state: dict):
    return [
        ("待确认初始", "GET", "/api/pending", None),
        ("认不出的回答", "POST", "/api/pending/answer", {"text": "这篇文章写得真好"}),
        ("空白回答", "POST", "/api/pending/answer", {"text": "   "}),
        ("算了", "POST", "/api/pending/answer", {"text": "算了"}),
        ("等后台落定", "SLEEP", "", {"secs": 1.0}),
        ("待确认再读", "GET", "/api/pending", None),
        ("resolve 模式非法", "POST", "/api/pending/resolve", {"id": "palive002", "mode": "x"}),
        ("resolve 不存在", "POST", "/api/pending/resolve", {"id": "pnope", "mode": "drop"}),
        ("resolve 缺 id", "POST", "/api/pending/resolve", {"mode": "drop"}),
        ("resolve 存文章", "POST", "/api/pending/resolve", {"id": "palive002", "mode": "article"}),
        ("再次整份归位", "POST", "/api/pending/answer", {"text": "都存图"}),
        ("待确认末态", "GET", "/api/pending", None),
    ]


def brief_steps(state: dict):
    return [
        ("简报读", "GET", "/api/brief", None),
        ("简报重建", "POST", "/api/brief/run", {}),
        ("简报重建再读", "GET", "/api/brief", None),
        ("指定日期重建", "POST", "/api/brief/run", {"day": "2026-11-26"}),
        ("force 查询参数", "GET", "/api/brief?force=1", None),
    ]


def contract_steps(state: dict):
    """FastAPI 契约：body 缺失 / 非对象 / 查询参数非法。"""
    return [
        ("act 无 body", "POST", "/api/plan/act", None),
        ("act body 非对象", "POST", "/api/plan/act", [1, 2]),
        ("triage 无 body", "POST", "/api/plan/triage", None),
        ("notes 无 body", "POST", "/api/notes", None),
        ("notes body 非对象", "POST", "/api/notes", ["x"]),
        ("notes/add 无 body", "POST", "/api/notes/add", None),
        ("pending/answer 无 body", "POST", "/api/pending/answer", None),
        ("pending/resolve 无 body", "POST", "/api/pending/resolve", None),
        ("brief 非法日期", "GET", "/api/brief?day=2026-13-40", None),
    ]


# ---------------------------------------------------------------- 归一

_IDS = re.compile(r"\b(?:pr|tk|ib)_[0-9a-f]{6}\b")
_MID = re.compile(r"\bm[0-9a-f]{11}\b")
_ISO = re.compile(r"\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2}:\d{2}(?:\.\d+)?")
_HMS = re.compile(r'"(\d{2}):(\d{2}):(\d{2})"')
_DATE_ONLY = re.compile(r'"(2026-1[12]-\d{2}|2026-0\d-\d{2})"')
# 挂起单的挂钟戳：回答落定时刻差几秒只是因为后台收尾线程先跑后跑，不是契约
_EPOCH = re.compile(r'"(asked_at|answered_at|pushed_at|next_try)":\s*-?[0-9.]+')


def normalize(text: str, home: Path, idmap: dict, coarse: bool) -> str:
    text = text.replace(str(home), "<HOME>").replace(str(home.resolve()), "<HOME>")
    text = _ISO.sub("<TS>", text)
    text = _HMS.sub('"<HMS>"', text)
    text = _EPOCH.sub(lambda m: f'"{m.group(1)}":<TS>', text)

    def sub_id(m):
        k = m.group(0)
        if k not in idmap:
            idmap[k] = f"<ID{len(idmap) + 1}>"
        return idmap[k]

    text = _IDS.sub(sub_id, text)
    text = _MID.sub(sub_id, text)
    if coarse:
        text = re.sub(r'"(used|free|used_pct|free_h|used_h)":(?:"[^"]*"|[0-9.]+)', r'"\1":<N>', text)
        text = re.sub(r'"at":"\d{2}:\d{2}:\d{2}"', '"at":"<HMS>"', text)
    return text


def call(port: int, method: str, path: str, body):
    status, raw = rp.call(port, TOKEN, method, path, body, timeout=180)
    return status, raw


# ---------------------------------------------------------------- 跑一侧

def run_side(side: str, rs_bin: str, home: Path, fake_home: Path) -> dict:
    global TOKEN
    env = {**os.environ, "MOSHAN_HOME": str(home), "MOSHAN_NO_BROWSER": "1",
           "HOME": str(fake_home)}
    procs = []
    try:
        if side == "py":
            p = subprocess.Popen(
                [os.environ.get(
                    "MOSHAN_PY",
                    str(Path.home() / "Library/Caches/moshan-build-venv/bin/python")),
                 str(ROOT / "moshan/launcher.py")],
                env=env, cwd=str(ROOT / "moshan"),
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            procs.append(p)
            cfg = rp._wait_cfg(home, timeout=90)
            port = cfg.get("local_port")
            token = cfg.get("token", "")
        else:
            port = rp.free_port()
            p = subprocess.Popen(
                [rs_bin],
                env={**env, "MOSHAN_PY_UPSTREAM": "", "MOSHAN_RS_PORT": str(port),
                     "MOSHAN_APP_DIR": str(ROOT / "moshan")},
                stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
            procs.append(p)
            token = TOKEN
        if not port or not rp.wait_ping(port, timeout=120):
            err = ""
            if side == "rs" and p.stderr:
                err = p.stderr.read().decode()[:800]
            raise SystemExit(f"{side} 侧没起来（端口 {port}）{err}")
        TOKEN = token
        return {"port": port, "procs": procs, "responses": run_steps(port, home)}
    finally:
        pass


def run_steps(port: int, home: Path) -> dict:
    """跑全部步骤，返回 {分组: [(说明, 状态码, 归一响应正文)]}。"""
    idmap: dict = {}
    out: dict = {}
    state = {"inbox": home / "library" / "收件箱"}
    groups = [("plan", plan_steps), ("notes", notes_steps),
              ("pending", pending_steps), ("brief", brief_steps),
              ("contract", contract_steps)]
    for gname, fn in groups:
        rows = []
        for note, method, path, body in fn(state):
            if method == "SLEEP":
                time.sleep((body or {}).get("secs", 1.0))
                continue
            path = path.replace("{pid}", state.get("pid", ""))
            path = path.replace("{pid2}", state.get("pid2", ""))
            path = path.replace("{tid}", state.get("tid", ""))
            path = path.replace("{kid}", state.get("kid", ""))
            path = path.replace("{iid}", state.get("iid", ""))
            path = path.replace("{iid2}", state.get("iid2", ""))
            path = path.replace("{iid3}", state.get("iid3", ""))
            if isinstance(body, dict):
                body = json.loads(json.dumps(body, ensure_ascii=False)
                                  .replace("{pid}", state.get("pid", ""))
                                  .replace("{tid}", state.get("tid", ""))
                                  .replace("{iid}", state.get("iid", ""))
                                  .replace("{iid2}", state.get("iid2", ""))
                                  .replace("{iid3}", state.get("iid3", "")))
            status, raw = call(port, method, path, body)
            text = raw.decode("utf-8", "replace")
            rows.append((note, status, normalize(text, home, idmap, True)))
            # 串下一步要用的 id（从原始响应里取，未经归一）
            if note in ("计划末态",):
                continue
            try:
                js = json.loads(raw)
            except Exception:  # noqa: BLE001
                js = {}
            if note == "建项目" and js.get("id"):
                state["pid"] = js["id"]
            if note == "加任务" and js.get("id"):
                state["tid"] = js["id"]
            if note == "加子任务" and js.get("id"):
                state["kid"] = js["id"]
            if note == "收件箱加一条" and js.get("id"):
                state["iid"] = js["id"]
            if note == "收件箱再加一条" and js.get("id"):
                state["iid2"] = js["id"]
            if note == "收件箱第三条" and js.get("id"):
                state["iid3"] = js["id"]
            if note == "归位新建项目" and js.get("proj"):
                state["pid2"] = js["proj"]
        out[gname] = rows
    return out


# ---------------------------------------------------------------- 数据文件

DATA_FILES = [
    "19-AI工作计划/计划.json",
    "19-AI工作计划/计划.md",
    "09-个人工作台/notes.json",
    "09-个人工作台/待办.md",
    "04-存图/_待定.json",
]


def snapshot_data(home: Path) -> dict:
    """把数据文件读成 {相对路径: 归一后文本}（简报按日期单独收集）。"""
    lib = home / "library"
    out = {}
    idmap: dict = {}
    for rel in DATA_FILES:
        p = lib / rel
        if p.exists():
            out[rel] = normalize(p.read_text(encoding="utf-8"), home, idmap, False)
        else:
            out[rel] = "<缺失>"
    bd = lib / "10-系统记录" / "简报"
    if bd.is_dir():
        for p in sorted(bd.iterdir()):
            if p.suffix in (".json", ".md"):
                out[f"10-系统记录/简报/{p.name}"] = normalize(
                    p.read_text(encoding="utf-8"), home, idmap, False)
    # 笔记的 md 镜像：待办/README 在根上，行程/日记/备忘各按日期或月份一个文件
    nd = lib / "09-个人工作台"
    if nd.is_dir():
        for p in sorted(nd.rglob("*.md")):
            rel = str(p.relative_to(lib))
            out[rel] = normalize(p.read_text(encoding="utf-8"), home, idmap, False)
    return out


def real_data_root() -> Path:
    """开发机真实数据根：config.json 的 library_dir → ~/Documents/Moshan → ~/Documents/AI项目。

    只用来取一份**冻住的** hostwork 缓存当夹具，不往真数据目录写任何东西。
    """
    cfg = Path.home() / "Library" / "Application Support" / "Moshan" / "config.json"
    cands = []
    try:
        v = str(json.loads(cfg.read_text(encoding="utf-8")).get("library_dir") or "")
        if v:
            cands.append(Path(v).expanduser())
    except Exception:  # noqa: BLE001
        pass
    cands += [Path.home() / "Documents" / "Moshan", Path.home() / "Documents" / "AI项目"]
    for p in cands:
        if (p / "_本机工作" / "缓存" / "会话.json").exists():
            return p
    return cands[0] if cands else Path.home() / "Documents" / "Moshan"


def stage_inputs() -> Path:
    """把只读输入先抄到**一份**暂存目录里，两边再从暂存目录拷。

    直接各拷一次真数据根不行：心跳文件每几分钟就被外部巡检重写一次，
    Python 侧那轮拷到 `ok:false / 03:50`，Rust 侧那轮拷到 `ok:true / 03:48`，
    简报里就会凭空差出一整行「异常，待修 3 项」。
    """
    stage = Path(tempfile.mkdtemp(prefix="moshan-w4b-in-"))
    real = real_data_root()
    lib = stage / "library"
    for rel in ("_本机工作/缓存/会话.json", "_本机工作/折算.json",
                "10-系统记录/心跳/最新.json"):
        src = real / rel
        dst = lib / rel
        dst.parent.mkdir(parents=True, exist_ok=True)
        if src.exists():
            shutil.copy2(src, dst)
        elif rel.endswith("会话.json"):
            dst.write_text(json.dumps({"v": 17, "files": {}}, ensure_ascii=False),
                           encoding="utf-8")
    return stage


def frozen_inputs(home: Path, stage: Path) -> None:
    """把暂存好的只读输入铺进这一侧的数据根。

    给两边一个**没有 `.workbuddy/projects` 的假 HOME**：`scan_sessions()` 在
    `WB_ROOT` 不存在时直接返回，缓存就冻住了（`ensure_sessions_loaded` 只装盘上一次），
    否则 Python 侧后台扫描线程会在运行途中把 `_SESS` 越扫越全
    （上一轮：打钩时 2.1B、跑完 5.6B），tok 快照自然对不上。
    """
    src = stage / "library"
    lib = home / "library"
    for p in sorted(src.rglob("*")):
        if not p.is_file():
            continue
        rel = p.relative_to(src)
        dst = lib / rel
        dst.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(p, dst)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--rs-bin", default="/tmp/rs-w4b/release/moshan-server")
    ap.add_argument("--keep", action="store_true")
    args = ap.parse_args()

    results = {}
    homes = {}
    procs = []
    stage = None
    # 假 HOME：放一份 workbuddy.db（sqlite 归属查询要用），但**不放 projects/**，
    # 于是 hostwork 的扫描线程没事可做，缓存不会被后台改掉。
    fake_home = Path(tempfile.mkdtemp(prefix="moshan-w4b-home-"))
    wb = fake_home / ".workbuddy"
    wb.mkdir(parents=True, exist_ok=True)
    real_db = Path.home() / ".workbuddy" / "workbuddy.db"
    if real_db.exists():
        (wb / "workbuddy.db").symlink_to(real_db)
    try:
        stage = stage_inputs()
        for side in ("py", "rs"):
            home = Path(tempfile.mkdtemp(prefix=f"moshan-w4b-{side}-"))
            homes[side] = home
            rp.seed(home)
            pend_fixture(home)
            frozen_inputs(home, stage)
            r = run_side(side, args.rs_bin, home, fake_home)
            procs.extend(r["procs"])
            results[side] = r["responses"]
            if side == "py":
                # Python 的巡检线程会在自己那轮里重写心跳文件（真实进程检查，
                # ok/checks 都会变）。Rust 只读这份文件 —— 把 Python 落定的
                # 最后一份抄给下一侧，两边才是在同一份输入上比。
                hb = home / "library" / "10-系统记录" / "心跳" / "最新.json"
                if hb.exists():
                    dst = stage / "library" / "10-系统记录" / "心跳" / "最新.json"
                    dst.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copy2(hb, dst)
    finally:
        for p in procs:
            p.terminate()
        for p in procs:
            try:
                p.wait(timeout=10)
            except Exception:  # noqa: BLE001
                p.kill()

    bad = 0
    print(f"{'步骤':22s} {'py':>4s} {'rs':>4s}  结果")
    for gname in ("plan", "notes", "pending", "brief", "contract"):
        a = results["py"][gname]
        b = results["rs"][gname]
        if len(a) != len(b):
            print(f"[{gname}] 步数不同：{len(a)} vs {len(b)}")
            bad += 1
            continue
        for (note, sa, ra), (_, sb, rb) in zip(a, b):
            ok = sa == sb and ra == rb
            if not ok:
                bad += 1
            print(f"{note:22s} {sa:4d} {sb:4d}  {'一致' if ok else '差异'}")
            if not ok:
                print("    " + rp.first_diff(ra.encode(), rb.encode()))

    print("\n—— 数据文件 ——")
    da = snapshot_data(homes["py"])
    db = snapshot_data(homes["rs"])
    for k in sorted(set(da) | set(db)):
        va = da.get(k)
        vb = db.get(k)
        if va == vb:
            n = len(va or "")
            print(f"{k:34s} 一致（{n} 字节）")
        else:
            bad += 1
            print(f"{k:34s} 差异")
            print("    " + rp.first_diff((va or "").encode(), (vb or "").encode()))

    print(f"\n实质差异 {bad}")
    if not args.keep:
        for h in [x for x in list(homes.values()) + [fake_home, stage] if x]:
            shutil.rmtree(h, ignore_errors=True)
    else:
        print("保留数据根：", ", ".join(str(h) for h in homes.values()),
              "，假 HOME：", fake_home, "，只读输入：", stage)
    return 0 if bad == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
