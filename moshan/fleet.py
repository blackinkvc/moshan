#!/usr/bin/env python3
"""多机监控：各机 agent 上报，主端汇总。

⚠️ 2026-11-21 起这份是**备份**。真正的服务已经搬到
   <数据根>/16-多机/nest_srv.py（独立进程、独立端口、独立配置），
   server.py 里的挂载也已注释掉。留着它只为回滚：把 server.py 那三行放开、
   停掉 nest 服务即可 —— 但两边共用同一个 fleet.db，绝不能同时挂载，
   否则自采线程会重复写同一条设备记录。

下面是拆分前的原始说明，保留不动。

2026-11-20 第一版，只做「看」，不做任何指令下发。

几条刻意的取舍：

1. **独立令牌**。agent 用 `fleet_token`，主端用 `token`，两条链路互不通用。
   否则一台被偷的笔记本就能拿 agent 令牌去调删文件接口。

2. **SQLite 而不是 JSONL**。一台机器 30 秒一条，四台一天一万多条；
   JSONL 要按设备查、按时间切片都得全表扫，而这份数据天生是「带索引的表」。

3. **写多读少，所以 WAL**。上报是高频小写，页面是低频读，WAL 下互不阻塞。

4. **保留 7 天，且 24 小时外的快照把进程表清空**。进程表占空间的大头，
   历史趋势只需要 cpu/内存/磁盘这几列，没必要拖着几万个进程名。

数据落 `<数据根>/16-多机/fleet.db`，页面落同目录 `fleet_ui.html`（改完刷新即生效）。
"""
from __future__ import annotations

import hashlib
import json
import re
import secrets
import shutil
import sqlite3
import threading
import time
from datetime import datetime, timedelta
from pathlib import Path

# 必须在模块级导入：本文件开了 `from __future__ import annotations`，
# 注解会变成字符串，FastAPI 靠 get_type_hints 在**模块全局**找类型，
# 写在函数内部等于找不到，`Request` 就会被当成一个必填的查询参数（422）。
from fastapi import Request
from fastapi.responses import JSONResponse

# 在线判定：agent 默认 30 秒一报，超过 3 个周期没动静就算掉线
ONLINE_WINDOW = 90
KEEP_DAYS = 7
PROCS_KEEP_HOURS = 24

# ---- 上报侧的上限：防「一台被控机把主端磁盘写满」 ----
# 背景：agent 令牌只写不读，所以就算它落到别人手里也看不到数据。
# 但它能往库里塞东西——伪造十万台设备、每台每秒报一次，就能把盘写爆，
# 顺带拖死这台 Mac 上跑着的所有服务。这三个数就是这条路的刹车。
MAX_DEVICES = 200          # 设备总数上限。正常一个人用，几台到几十台
MAX_REPORT_BYTES = 1024 * 1024   # 单条上报 1MB。正常一条几十 KB，60000 字进程表也才 60KB
REPORT_MIN_GAP = 3         # 同一台设备 3 秒内重复上报只认第一条

_LOCK = threading.Lock()
# device_id -> 上次入库时间（monotonic）。只用来节流，不落盘
_LAST_REPORT: dict[str, float] = {}


# ------------------------------------------------------------------ 路径


def _root(cfg: dict) -> Path:
    """数据根。三种口径，优先级从高到低：

    1. data_root —— 独立部署（虫巢思维自己的 nest.json）显式写死，最直白。
    2. inbox_dir 的父目录 —— 跟 metrics.py 同一个口径，挂在口袋进程里时用这个。
    3. 默认 ~/Documents/AI项目 —— 什么都没配时的兜底。
    """
    dr = cfg.get("data_root")
    if dr:
        return Path(str(dr)).expanduser()
    inbox = cfg.get("inbox_dir")
    if inbox:
        return Path(str(inbox)).expanduser().parent
    return Path.home() / "Documents" / "AI项目"


def data_dir(cfg: dict) -> Path:
    d = _root(cfg) / "16-多机"
    try:
        d.mkdir(parents=True, exist_ok=True)
    except OSError:
        pass
    return d


def db_path(cfg: dict) -> Path:
    return data_dir(cfg) / "fleet.db"


def page_path(cfg: dict) -> Path:
    return data_dir(cfg) / "fleet_ui.html"


# ------------------------------------------------------------------ 库


SCHEMA = """
CREATE TABLE IF NOT EXISTS devices (
    id          TEXT PRIMARY KEY,
    name        TEXT,
    os          TEXT,
    host        TEXT,
    ip          TEXT,
    agent_ver   TEXT,
    access      TEXT DEFAULT 'control',
    fp          TEXT DEFAULT '',
    note        TEXT DEFAULT '',
    first_seen  TEXT,
    last_seen   TEXT
);

CREATE TABLE IF NOT EXISTS snapshots (
    device_id   TEXT NOT NULL,
    ts          TEXT NOT NULL,
    cpu         REAL,
    mem_used    INTEGER,
    mem_total   INTEGER,
    disk_used   INTEGER,
    disk_total  INTEGER,
    uptime      INTEGER,
    procs       TEXT,
    PRIMARY KEY (device_id, ts)
);

CREATE INDEX IF NOT EXISTS ix_snap_ts ON snapshots(ts);

-- 指令队列（2026-11-20 加）。
-- 为什么走队列而不是「主端直连 agent」：办公室那台在 NAT 后面、没有公网端口，
-- 主端根本连不进去。改成 agent 定时来取，方向反过来，就不需要任何入站端口。
-- 顺带好处是：主端宕机时指令留在表里，agent 恢复后照常执行，不会丢。
CREATE TABLE IF NOT EXISTS commands (
    id          TEXT PRIMARY KEY,
    device_id   TEXT NOT NULL,
    action      TEXT NOT NULL,
    state       TEXT DEFAULT 'pending',   -- pending | taken | done | failed | expired
    result      TEXT DEFAULT '',
    created_at  TEXT,
    taken_at    TEXT,
    done_at     TEXT,
    issued_from TEXT DEFAULT ''
);

CREATE INDEX IF NOT EXISTS ix_cmd_dev ON commands(device_id, state);

-- ---------------------------------------------------------------- 软件清单
-- 整体替换式：每次上报都是这台机器的全量，先删后插，不做增量合并。
-- 软件列表没有"历史"语义，留着旧行只会让已经卸载的软件一直挂在页面上。
-- 不存卸载命令串：那东西一旦从主端下发就等于拿到了半条任意命令通道。
-- 卸载时 agent 自己回本地注册表按 app_id 查，主端只传 id。
CREATE TABLE IF NOT EXISTS software (
    device_id    TEXT NOT NULL,
    app_id       TEXT NOT NULL,
    name         TEXT,
    version      TEXT,
    publisher    TEXT,
    install_date TEXT,
    size_kb      INTEGER DEFAULT 0,
    source       TEXT DEFAULT 'registry',   -- registry | winget | msix | app
    bits         TEXT DEFAULT '',           -- 64 / 32（注册表视图）
    updated_at   TEXT,
    PRIMARY KEY (device_id, app_id)
);

-- winget 报出来的可升级项。没有 winget 的机器这张表就是空的，属正常。
CREATE TABLE IF NOT EXISTS upgrades (
    device_id   TEXT NOT NULL,
    app_id      TEXT NOT NULL,
    name        TEXT,
    current     TEXT,
    available   TEXT,
    updated_at  TEXT,
    PRIMARY KEY (device_id, app_id)
);

-- ---------------------------------------------------------------- 长任务
-- 和 commands 分开，因为超时语义根本不同：
--   指令是秒级（锁屏、重启），300 秒没取就作废；
--   任务要下载几百 MB、跑 MSI，可能十几分钟，还要能报进度。
-- 塞进一张表会让 TTL 变成两不管：要么误杀长任务，要么让死指令赖着不走。
CREATE TABLE IF NOT EXISTS tasks (
    id          TEXT PRIMARY KEY,
    device_id   TEXT NOT NULL,
    kind        TEXT NOT NULL,     -- install | uninstall | upgrade | self_update | refresh
    target      TEXT DEFAULT '',   -- winget PackageId 或 software.app_id
    args_json   TEXT DEFAULT '{}', -- {url, sha256, silent}
    state       TEXT DEFAULT 'pending',  -- pending|running|done|failed|expired
    progress    INTEGER DEFAULT 0,
    log         TEXT DEFAULT '',
    created_at  TEXT,
    taken_at    TEXT,
    done_at     TEXT
);

CREATE INDEX IF NOT EXISTS ix_task_dev ON tasks(device_id, state);

-- ------------------------------------------------------------ agent 版本
-- 主端只放"发布过的版本"，agent 自己来问最新版是多少。
-- 自升级本质上是让主端能给被控机送一份可执行代码，所以：
-- 发布必须用 view_token（人操作），下载必须用 agent_token，且哈希要对得上。
CREATE TABLE IF NOT EXISTS agent_release (
    version      TEXT PRIMARY KEY,
    file_path    TEXT,
    sha256       TEXT,
    size         INTEGER DEFAULT 0,
    notes        TEXT DEFAULT '',
    published_at TEXT,
    is_current   INTEGER DEFAULT 0
);
"""


def connect(cfg: dict) -> sqlite3.Connection:
    c = sqlite3.connect(str(db_path(cfg)), timeout=5.0)
    c.execute("PRAGMA journal_mode=WAL")
    c.execute("PRAGMA synchronous=NORMAL")
    return c


def init(cfg: dict) -> None:
    with _LOCK:
        c = connect(cfg)
        try:
            c.executescript(SCHEMA)
            # 老库补列：CREATE TABLE IF NOT EXISTS 不会给已存在的表加字段。
            # fp = 设备指纹，绑定后用来挡「冒用别人的 device_id」。
            cols = {r[1] for r in c.execute("PRAGMA table_info(devices)")}
            if "fp" not in cols:
                c.execute("ALTER TABLE devices ADD COLUMN fp TEXT DEFAULT ''")
            c.commit()
        finally:
            c.close()


def _now() -> str:
    return datetime.now().isoformat(timespec="seconds")


# ------------------------------------------------------------------ 令牌


def ensure_token(cfg: dict, cfg_file: Path) -> str:
    """agent 专用令牌。缺了就生成并写回 config.json。

    只写这一个键，其余原样保留（不能整份覆盖，会把用户的配置抹掉）。
    """
    tok = str(cfg.get("fleet_token") or "")
    if tok:
        return tok
    tok = secrets.token_urlsafe(24)
    try:
        raw = json.loads(cfg_file.read_text(encoding="utf-8"))
        if not isinstance(raw, dict):
            raw = {}
        raw["fleet_token"] = tok
        cfg_file.write_text(json.dumps(raw, ensure_ascii=False, indent=2), encoding="utf-8")
    except (OSError, ValueError):
        pass
    cfg["fleet_token"] = tok
    return tok


def token_ok(cfg: dict, supplied: str) -> bool:
    want = str(cfg.get("fleet_token") or "")
    return bool(want) and secrets.compare_digest(want, supplied or "")


def ensure_view_token(cfg: dict, cfg_file: Path) -> str:
    """查看令牌，和 agent 令牌**刻意分成两把**。

    原来的设计里 fleet_token 既是「agent 上报凭证」又是「页面 ?k=」，
    这在单机时没毛病，但多机就漏了：agent 的配置必须落到每台被监控的机器上，
    谁打开那个文件，谁就能拿它调 /overview 看全部设备的数据。

    NUCBOX_M8 放在办公室，这条必须堵死。所以：
      · fleet_token —— agent 专用，只能 report / poll / result，读不到任何数据
      · view_token  —— 只在你手里的浏览器用，能看全部、能发指令
    两把都只写进 config.json，不动其他键。
    """
    tok = str(cfg.get("view_token") or "")
    if tok:
        return tok
    tok = secrets.token_urlsafe(24)
    try:
        raw = json.loads(cfg_file.read_text(encoding="utf-8"))
        if not isinstance(raw, dict):
            raw = {}
        raw["view_token"] = tok
        cfg_file.write_text(json.dumps(raw, ensure_ascii=False, indent=2), encoding="utf-8")
    except (OSError, ValueError):
        pass
    cfg["view_token"] = tok
    return tok


def view_ok(cfg: dict, supplied: str) -> bool:
    want = str(cfg.get("view_token") or "")
    return bool(want) and secrets.compare_digest(want, supplied or "")


# ------------------------------------------------------------------ 写入


def report(cfg: dict, body: dict, ip: str = "", fp: str = "") -> dict:
    """收一条上报。字段缺了就按 0 处理，不让脏数据把整条丢掉。"""
    dev = str(body.get("device_id") or "").strip()[:64]
    if not dev:
        return {"ok": False, "error": "device_id 必填"}
    # 指纹是「这台机器是这个 id」的唯一凭据。device_id 是主机名，谁都猜得到，
    # 光靠它挡不住持有 fleet_token 的人冒用别人的身份。所以没有指纹一律不收。
    fp = (fp or "").strip()[:64]
    if not fp:
        return {"ok": False,
                "error": "缺少设备指纹：agent 版本过旧，请装 0.5.0 以上"}

    def num(k, cast=float):
        v = body.get(k)
        try:
            return cast(v) if v is not None else 0
        except (TypeError, ValueError):
            return 0

    ts = str(body.get("at") or "")[:19] or _now()
    row = (
        dev, ts,
        round(num("cpu"), 1),
        num("mem_used", int), num("mem_total", int),
        num("disk_used", int), num("disk_total", int),
        num("uptime", int),
        json.dumps(body.get("procs") or [], ensure_ascii=False)[:60000],
    )

    now_m = time.monotonic()
    with _LOCK:
        # 节流：正常 agent 30 秒一报。3 秒内来两次，只能是脚本在刷盘。
        # 刷盘的目的不是改数据，是把 snapshots 灌到磁盘满，所以这里直接不写。
        prev = _LAST_REPORT.get(dev)
        if prev is not None and now_m - prev < REPORT_MIN_GAP:
            return {"ok": True, "ts": ts, "skipped": "too_frequent"}
        if len(_LAST_REPORT) > 5000:
            cut = now_m - 60
            for k in [k for k, v in _LAST_REPORT.items() if v < cut]:
                _LAST_REPORT.pop(k, None)

        c = connect(cfg)
        try:
            known = c.execute("SELECT fp FROM devices WHERE id=?", (dev,)).fetchone()
            if known is None:
                # 设备数上限：只拦新设备。已有设备照常更新，不然连正常的都会断。
                n = c.execute("SELECT COUNT(*) FROM devices").fetchone()[0]
                if n >= MAX_DEVICES:
                    return {"ok": False,
                            "error": f"设备数已达上限 {MAX_DEVICES}，拒绝新设备 {dev[:32]}"}
            else:
                have = str(known[0] or "").strip()
                if have and have != fp:
                    return {"ok": False,
                            "error": f"设备指纹不匹配：{dev[:32]} 这个 id 已被另一台机器绑定"}
            c.execute(
                "INSERT OR REPLACE INTO snapshots"
                "(device_id,ts,cpu,mem_used,mem_total,disk_used,disk_total,uptime,procs)"
                " VALUES (?,?,?,?,?,?,?,?,?)", row)
            c.execute(
                "INSERT INTO devices(id,name,os,host,ip,agent_ver,first_seen,last_seen,fp)"
                " VALUES (?,?,?,?,?,?,?,?,?)"
                " ON CONFLICT(id) DO UPDATE SET"
                "   name=excluded.name, os=excluded.os, host=excluded.host,"
                "   ip=excluded.ip, agent_ver=excluded.agent_ver,"
                "   last_seen=excluded.last_seen,"
                # fp 只在原值为空时补写。冲突时若跟着 excluded 走，
                # 冒名者一次上报就能把指纹换成自己的，绑定等于白做。
                "   fp=CASE WHEN devices.fp IS NULL OR devices.fp=''"
                "           THEN excluded.fp ELSE devices.fp END",
                (dev, str(body.get("name") or dev)[:64], str(body.get("os") or "")[:32],
                 str(body.get("host") or "")[:64], ip[:48],
                 str(body.get("agent_ver") or "")[:16], ts, ts, fp))
            c.commit()
            _LAST_REPORT[dev] = now_m
        finally:
            c.close()
    return {"ok": True, "ts": ts}


def fp_ok(cfg: dict, dev: str, fp: str | None) -> tuple[bool, str]:
    """这个指纹能不能代表这台设备。

    空指纹一律不放行——那是 0.4.0 以前的 agent。新版的意义就在于带上它，
    所以宁可让老 agent 连不上、逼着升级，也不能留「不带指纹就放行」的口子，
    那正好是冒名要走的门。

    真机首次上报时这里还是 Unknown，返回 new 让它把指纹绑上去；
    report() 里绑定，且绑定后不允许被改写。
    """
    fp = (fp or "").strip()[:64]
    if not dev:
        return False, "device_id 必填"
    if not fp:
        return False, "缺少设备指纹：agent 版本过旧，请装 0.5.0 以上"
    with _LOCK:
        c = connect(cfg)
        try:
            row = c.execute("SELECT fp FROM devices WHERE id=?", (dev,)).fetchone()
        finally:
            c.close()
    if row is None:
        return True, "new"          # 还没上报过，先让它报一次
    have = str(row[0] or "").strip()
    if not have:
        return True, "unbound"      # 老记录没指纹，第一次带上来就绑
    if have != fp:
        return False, f"设备指纹不匹配：{dev[:32]} 已被另一台机器绑定"
    return True, "ok"


def device_of(cfg: dict, table: str, row_id: str) -> str:
    """反查一条指令/任务属于哪台设备。表名只吃内部常量，不接受外部输入。"""
    if table not in ("commands", "tasks"):
        return ""
    with _LOCK:
        c = connect(cfg)
        try:
            row = c.execute(f"SELECT device_id FROM {table} WHERE id=?",
                            (row_id,)).fetchone()
        finally:
            c.close()
    return str(row[0] or "") if row else ""


def prune(cfg: dict) -> dict:
    """7 天前的删掉；24 小时外的把进程表清空（省空间，趋势不受影响）。"""
    now = datetime.now()
    cut_all = (now - timedelta(days=KEEP_DAYS)).isoformat(timespec="seconds")
    cut_proc = (now - timedelta(hours=PROCS_KEEP_HOURS)).isoformat(timespec="seconds")
    with _LOCK:
        c = connect(cfg)
        try:
            n1 = c.execute("DELETE FROM snapshots WHERE ts < ?", (cut_all,)).rowcount
            n2 = c.execute(
                "UPDATE snapshots SET procs='[]' WHERE ts < ? AND procs<>'[]'",
                (cut_proc,)).rowcount
            c.commit()
        finally:
            c.close()
    return {"deleted": n1, "trimmed": n2}


# ------------------------------------------------------------------ 读取


def overview(cfg: dict, limit: int = 30) -> dict:
    """一屏所需的全部东西：设备清单 + 各自最后一条快照。

    每台设备独立成条，页面一张卡——不做「混在一起的平均值」，
    那样反而看不出是哪台机器出了问题。
    """
    now = time.time()
    with _LOCK:
        c = connect(cfg)
        try:
            c.row_factory = sqlite3.Row
            devs = [dict(r) for r in c.execute(
                "SELECT * FROM devices ORDER BY last_seen DESC LIMIT ?", (limit,))]
            out = []
            for d in devs:
                snap = c.execute(
                    "SELECT * FROM snapshots WHERE device_id=? ORDER BY ts DESC LIMIT 1",
                    (d["id"],)).fetchone()
                s = dict(snap) if snap else {}
                try:
                    procs = json.loads(s.get("procs") or "[]")
                except ValueError:
                    procs = []
                # 最近 20 分钟的 cpu 点，够页面画一条细曲线
                spark = [r["cpu"] for r in c.execute(
                    "SELECT cpu FROM snapshots WHERE device_id=? ORDER BY ts DESC LIMIT 40",
                    (d["id"],))]
                # 软件数与可升级数：卡片上要显示「有 3 项可升级」这种提示
                soft_n = c.execute("SELECT COUNT(*) FROM software WHERE device_id=?",
                                   (d["id"],)).fetchone()[0]
                up_n = c.execute("SELECT COUNT(*) FROM upgrades WHERE device_id=?",
                                 (d["id"],)).fetchone()[0]
                age = _age_sec(d.get("last_seen") or "")
                out.append({
                    "id": d["id"], "name": d["name"] or d["id"], "os": d["os"],
                    "host": d["host"], "ip": d["ip"], "access": d["access"],
                    "agent_ver": d["agent_ver"], "note": d["note"],
                    "last_seen": d["last_seen"],
                    "age": age,
                    "online": age is not None and age <= ONLINE_WINDOW,
                    "cpu": s.get("cpu") or 0,
                    "mem_used": s.get("mem_used") or 0,
                    "mem_total": s.get("mem_total") or 0,
                    "disk_used": s.get("disk_used") or 0,
                    "disk_total": s.get("disk_total") or 0,
                    "uptime": s.get("uptime") or 0,
                    "procs": procs[:8],
                    "spark": list(reversed(spark)),
                    "soft_count": soft_n,
                    "upgrade_count": up_n,
                })
        finally:
            c.close()
    online = sum(1 for d in out if d["online"])
    return {"ok": True, "now": _now(), "count": len(out), "online": online, "devices": out}


def _age_sec(ts: str):
    try:
        return max(0, int(time.time() - datetime.fromisoformat(ts).timestamp()))
    except (TypeError, ValueError):
        return None


def set_access(cfg: dict, device_id: str, access: str) -> dict:
    """改权限档：control / read / self。目前只落库，接口校验在 P2 接。"""
    if access not in ("control", "read", "self"):
        return {"ok": False, "error": "档位只能是 control / read / self"}
    with _LOCK:
        c = connect(cfg)
        try:
            n = c.execute("UPDATE devices SET access=? WHERE id=?", (access, device_id)).rowcount
            c.commit()
        finally:
            c.close()
    return {"ok": bool(n), "updated": n}


def remove(cfg: dict, device_id: str) -> dict:
    """踢掉一台设备：设备记录、历史、指令一起删。

    指令记录不删的话，那台机器重装回来会立刻领到一条几天前的旧指令，
    开机就被锁屏/重启，很难查。
    """
    with _LOCK:
        c = connect(cfg)
        try:
            c.execute("DELETE FROM snapshots WHERE device_id=?", (device_id,))
            c.execute("DELETE FROM commands WHERE device_id=?", (device_id,))
            c.execute("DELETE FROM software WHERE device_id=?", (device_id,))
            c.execute("DELETE FROM upgrades WHERE device_id=?", (device_id,))
            c.execute("DELETE FROM tasks WHERE device_id=?", (device_id,))
            n = c.execute("DELETE FROM devices WHERE id=?", (device_id,)).rowcount
            c.commit()
        finally:
            c.close()
    return {"ok": bool(n)}


# ------------------------------------------------------------------ 指令队列（2026-11-20）
#
# 只给三个动作，**没有 run_command**。
#
# 这不是保守，是算过账的：agent 一旦能执行任意 shell，那把令牌就等于整台机器。
# 办公室里一台常开的机器，被拿到令牌的概率不低（配置文件就在硬盘上），
# 那时候「能重启」和「能执行任何命令」是两件性质完全不同的事。
# 要自动化就再加固定动作，别开口子。

ACTIONS = {
    "lock": "锁定屏幕（回登录界面，程序继续跑）",
    "reboot": "重启系统",
    "logout": "注销当前用户",
}

# pending 超过这个秒数就作废——办公室那台可能关机了，堆着没意义
CMD_TTL = 300
# taken 表示 agent 已经拿走但没回报结果，多半是执行时进程挂了，放回去重试
CMD_TAKEN_TTL = 180


def _expire(c: sqlite3.Connection) -> int:
    now = datetime.now()
    a = (now - timedelta(seconds=CMD_TTL)).isoformat(timespec="seconds")
    b = (now - timedelta(seconds=CMD_TAKEN_TTL)).isoformat(timespec="seconds")
    n1 = c.execute(
        "UPDATE commands SET state='expired' WHERE state='pending' AND created_at < ?", (a,)).rowcount
    # taken 超时：普通动作退回 pending 重试一次（用 result='retry' 防无限重试）。
    # reboot 例外——直接置 done，绝不重发。
    #
    # 准确的回执现在由 agent 侧的意图日志负责（pocket_agent.flush_intent）：
    # 它在重启前把「要做什么」落盘，开机后补报真实结果，这里只是兜底 ——
    # 用于 agent 彻底失联（被卸载、硬盘拔了）时，不让页面永远停在「执行中」。
    # 两道保险不冲突：补报到达时会覆盖这里的 result，说的更准确。
    n2 = c.execute(
        "UPDATE commands SET state='pending', taken_at=NULL, result='retry'"
        " WHERE state='taken' AND taken_at < ? AND result<>'retry' AND action<>'reboot'",
        (b,)).rowcount
    c.execute(
        "UPDATE commands SET state='done', result='超时未见回执（agent 可能已失联）', done_at=?"
        " WHERE state='taken' AND taken_at < ? AND action='reboot'",
        (_now(), b))
    return n1 + n2


def enqueue(cfg: dict, device_id: str, action: str, issued_from: str = "") -> dict:
    """入队一条指令。5 分钟内的同设备同动作不重复入队，防手抖连点。"""
    if action not in ACTIONS:
        return {"ok": False, "error": f"不支持的动作：{action}"}

    with _LOCK:
        c = connect(cfg)
        try:
            c.row_factory = sqlite3.Row      # 不设这里，下面 dup["id"] 会 TypeError
            row = c.execute("SELECT id FROM devices WHERE id=?", (device_id,)).fetchone()
            if not row:
                return {"ok": False, "error": f"没有这台设备：{device_id}"}
            dup = c.execute(
                "SELECT id FROM commands WHERE device_id=? AND action=?"
                " AND state IN ('pending','taken') AND created_at > ?",
                (device_id, action,
                 (datetime.now() - timedelta(seconds=60)).isoformat(timespec="seconds"))).fetchone()
            if dup:
                return {"ok": True, "id": dup["id"], "duplicate": True}
            cid = "c" + secrets.token_hex(6)
            c.execute(
                "INSERT INTO commands(id,device_id,action,created_at,issued_from)"
                " VALUES (?,?,?,?,?)",
                (cid, device_id, action, _now(), issued_from[:64]))
            c.commit()
        finally:
            c.close()
    return {"ok": True, "id": cid, "action": action, "device_id": device_id}


def take(cfg: dict, device_id: str) -> dict:
    """agent 来取一条待办。一次只给一条，执行完再来拿——避免并发把机器搞出意外状态。"""
    with _LOCK:
        c = connect(cfg)
        try:
            c.row_factory = sqlite3.Row
            _expire(c)
            r = c.execute(
                "SELECT id,action FROM commands WHERE device_id=? AND state='pending'"
                " ORDER BY created_at LIMIT 1", (device_id,)).fetchone()
            if not r:
                c.commit()
                return {"ok": True, "command": None}
            c.execute("UPDATE commands SET state='taken', taken_at=? WHERE id=?",
                      (_now(), r["id"]))
            c.commit()
            return {"ok": True, "command": {"id": r["id"], "action": r["action"]}}
        finally:
            c.close()


def finish(cfg: dict, cmd_id: str, ok: bool, output: str = "") -> dict:
    with _LOCK:
        c = connect(cfg)
        try:
            n = c.execute(
                "UPDATE commands SET state=?, result=?, done_at=? WHERE id=?",
                ("done" if ok else "failed", str(output)[:500], _now(), cmd_id)).rowcount
            c.commit()
        finally:
            c.close()
    return {"ok": bool(n)}


def recent_commands(cfg: dict, limit: int = 12) -> list:
    with _LOCK:
        c = connect(cfg)
        try:
            c.row_factory = sqlite3.Row
            _expire(c)
            c.commit()
            return [dict(r) for r in c.execute(
                "SELECT * FROM commands ORDER BY created_at DESC LIMIT ?", (limit,))]
        finally:
            c.close()


# ------------------------------------------------------------------ 软件与任务（2026-11-21）
#
# 这一节的安全边界，改之前先读完：
#
#   主端只传「标识」——winget 的 PackageId、清单里的 app_id、版本号。
#   永远不传命令行，也不传卸载命令串。
#   agent 收到 app_id 后回自己的注册表里查卸载命令，查不到就拒绝执行。
#
#   这样即使主端被攻破（或者令牌泄露），攻击者最多能「卸载某台机器上
#   已经装着的某个软件」，而不是「在那台机器上执行任意命令」。
#   差别是：前者是可恢复的运维事故，后者是整台机器失守。
#
#   安装包这条例外，所以必须带 sha256：主端给 URL，agent 下载后先算哈希，
#   对得上才执行。不校验的话，主端就成了一个任意代码分发器。

# PackageId 实际只用字母数字和 . _ + -（如 7zip.7zip、Google.Chrome）
WINGET_ID = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._+\-]{1,127}$")
# 注册表键名可以做 app_id，允许中文、空格、花括号，但不许换行和 NUL
APP_ID = re.compile(r"^[^\r\n\x00]{1,160}$")

TASK_KINDS = {
    "refresh": "重新采集软件清单",
    "install": "安装软件",
    "uninstall": "卸载软件",
    "upgrade": "升级软件",
    "self_update": "升级 agent 自身",
}

TASK_TTL = 86400          # 一天没被取走就作废
TASK_RUNNING_TTL = 7200   # 执行超过 2 小时判超时（下载 + 装 MSI 够宽了）
MAX_SOFTWARE = 4000       # 单台机器清单上限，防止异常 agent 把库灌爆


def save_software(cfg: dict, device_id: str, items: list,
                  upgrades: list | None = None) -> dict:
    """整体替换一台机器的软件清单。upgrades 传 None 表示这次没查更新，保留旧值。"""
    now = _now()
    rows = []
    for it in (items or [])[:MAX_SOFTWARE]:
        if not isinstance(it, dict):
            continue
        aid = str(it.get("app_id") or "").strip()[:160]
        if not aid:
            continue
        try:
            size = int(it.get("size_kb") or 0)
        except (TypeError, ValueError):
            size = 0
        rows.append((
            device_id, aid,
            str(it.get("name") or aid)[:200],
            str(it.get("version") or "")[:60],
            str(it.get("publisher") or "")[:120],
            str(it.get("install_date") or "")[:24],
            max(0, size),
            str(it.get("source") or "registry")[:16],
            str(it.get("bits") or "")[:4],
            now,
        ))

    with _LOCK:
        c = connect(cfg)
        try:
            c.execute("DELETE FROM software WHERE device_id=?", (device_id,))
            c.executemany(
                "INSERT OR REPLACE INTO software"
                "(device_id,app_id,name,version,publisher,install_date,size_kb,source,bits,updated_at)"
                " VALUES (?,?,?,?,?,?,?,?,?,?)", rows)
            if upgrades is not None:
                c.execute("DELETE FROM upgrades WHERE device_id=?", (device_id,))
                up = []
                for u in (upgrades or [])[:MAX_SOFTWARE]:
                    if not isinstance(u, dict):
                        continue
                    aid = str(u.get("app_id") or "").strip()[:160]
                    if not aid:
                        continue
                    up.append((device_id, aid, str(u.get("name") or aid)[:200],
                               str(u.get("current") or "")[:60],
                               str(u.get("available") or "")[:60], now))
                c.executemany(
                    "INSERT OR REPLACE INTO upgrades"
                    "(device_id,app_id,name,current,available,updated_at)"
                    " VALUES (?,?,?,?,?,?)", up)
            c.commit()
        finally:
            c.close()
    return {"ok": True, "count": len(rows)}


def software_list(cfg: dict, device_id: str, q: str = "") -> dict:
    with _LOCK:
        c = connect(cfg)
        try:
            c.row_factory = sqlite3.Row
            sql = "SELECT * FROM software WHERE device_id=?"
            args: list = [device_id]
            if q:
                sql += " AND (name LIKE ? OR publisher LIKE ? OR app_id LIKE ?)"
                args += [f"%{q}%"] * 3
            sql += " ORDER BY name COLLATE NOCASE"
            items = [dict(r) for r in c.execute(sql, args)]
            ups = [dict(r) for r in c.execute(
                "SELECT * FROM upgrades WHERE device_id=? ORDER BY name COLLATE NOCASE",
                (device_id,))]
        finally:
            c.close()
    return {"ok": True, "device_id": device_id, "count": len(items),
            "items": items, "upgrades": ups}


def _task_clean(c: sqlite3.Connection) -> int:
    now = datetime.now()
    a = (now - timedelta(seconds=TASK_TTL)).isoformat(timespec="seconds")
    b = (now - timedelta(seconds=TASK_RUNNING_TTL)).isoformat(timespec="seconds")
    n = c.execute(
        "UPDATE tasks SET state='expired' WHERE state='pending' AND created_at < ?",
        (a,)).rowcount
    # 执行超时不放回重试：装了一半的软件再装一次可能搞出更怪的状态，
    # 停在 failed 让人看一眼，比自动重试安全。
    n += c.execute(
        "UPDATE tasks SET state='failed', log=log || ' | 执行超时（agent 可能已断开）', done_at=?"
        " WHERE state='running' AND taken_at < ?", (_now(), b)).rowcount
    return n


def enqueue_task(cfg: dict, device_id: str, kind: str, target: str = "",
                 args: dict | None = None) -> dict:
    """下发一条长任务。只接受标识，不接受命令。"""
    if kind not in TASK_KINDS:
        return {"ok": False, "error": f"不支持的任务类型：{kind}"}

    target = (target or "").strip()
    if kind in ("install", "upgrade") and target and not WINGET_ID.match(target):
        return {"ok": False, "error": "包 id 格式不对（只允许字母数字和 . _ + -）"}
    if kind in ("uninstall", "upgrade") and not target:
        return {"ok": False, "error": "缺少要操作的软件标识"}
    if target and not APP_ID.match(target):
        return {"ok": False, "error": "软件标识里有非法字符"}

    # 只取白名单字段。args 里塞 cmd / command 一律丢弃——
    # 这一处只要透传，上面所有的边界就白做了。
    safe: dict = {}
    if isinstance(args, dict):
        for k in ("url", "sha256", "silent"):
            v = args.get(k)
            if v not in (None, ""):
                safe[k] = str(v)[:2000]
    if kind == "install" and not target and not safe.get("url"):
        return {"ok": False, "error": "install 需要 winget 包 id，或上传安装包的下载地址"}

    with _LOCK:
        c = connect(cfg)
        try:
            c.row_factory = sqlite3.Row
            if not c.execute("SELECT id FROM devices WHERE id=?", (device_id,)).fetchone():
                return {"ok": False, "error": f"没有这台设备：{device_id}"}
            dup = c.execute(
                "SELECT id FROM tasks WHERE device_id=? AND kind=? AND target=?"
                " AND state IN ('pending','running')", (device_id, kind, target)).fetchone()
            if dup:
                return {"ok": True, "id": dup["id"], "duplicate": True}
            tid = "t" + secrets.token_hex(6)
            c.execute(
                "INSERT INTO tasks(id,device_id,kind,target,args_json,created_at)"
                " VALUES (?,?,?,?,?,?)",
                (tid, device_id, kind, target,
                 json.dumps(safe, ensure_ascii=False), _now()))
            c.commit()
        finally:
            c.close()
    return {"ok": True, "id": tid, "kind": kind, "target": target}


def take_task(cfg: dict, device_id: str) -> dict:
    with _LOCK:
        c = connect(cfg)
        try:
            c.row_factory = sqlite3.Row
            _task_clean(c)
            r = c.execute(
                "SELECT * FROM tasks WHERE device_id=? AND state='pending'"
                " ORDER BY created_at LIMIT 1", (device_id,)).fetchone()
            if not r:
                c.commit()
                return {"ok": True, "task": None}
            c.execute(
                "UPDATE tasks SET state='running', taken_at=?, progress=1 WHERE id=?",
                (_now(), r["id"]))
            c.commit()
            try:
                args = json.loads(r["args_json"] or "{}")
            except ValueError:
                args = {}
            return {"ok": True, "task": {
                "id": r["id"], "kind": r["kind"], "target": r["target"], "args": args}}
        finally:
            c.close()


def progress_task(cfg: dict, task_id: str, progress: int, log: str = "") -> dict:
    """执行中的进度回报。单独一个接口，因为装一个 MSI 可能要几分钟。"""
    with _LOCK:
        c = connect(cfg)
        try:
            n = c.execute(
                "UPDATE tasks SET progress=?, log=? WHERE id=? AND state='running'",
                (max(0, min(99, int(progress))), str(log)[:800], task_id)).rowcount
            c.commit()
        finally:
            c.close()
    return {"ok": bool(n)}


def finish_task(cfg: dict, task_id: str, ok: bool, log: str = "") -> dict:
    with _LOCK:
        c = connect(cfg)
        try:
            n = c.execute(
                "UPDATE tasks SET state=?, progress=?, log=?, done_at=? WHERE id=?",
                ("done" if ok else "failed", 100 if ok else 0,
                 str(log)[:2000], _now(), task_id)).rowcount
            c.commit()
        finally:
            c.close()
    return {"ok": bool(n)}


def recent_tasks(cfg: dict, limit: int = 20) -> list:
    with _LOCK:
        c = connect(cfg)
        try:
            c.row_factory = sqlite3.Row
            _task_clean(c)
            c.commit()
            return [dict(r) for r in c.execute(
                "SELECT * FROM tasks ORDER BY created_at DESC LIMIT ?", (limit,))]
        finally:
            c.close()


# ------------------------------------------------------------ 安装包分发
#
# winget 覆盖不到的软件（厂商私有安装包）走这条路：
#   主端把一个 .msi/.exe 登记进来 → 拿到 url 和 sha256 → 作为 install 任务的 args 下发。
# agent 下载后会自己重算 sha256，对不上就不落盘。
# 所以主端即使被攻破，也没法把任意内容塞进那台机器，除非它先污染这个目录——
# 而污染这个目录需要 view_token（人操作）。

PKG_NAME = re.compile(r"^[0-9a-f]{12}\.(msi|exe)$")


def pkg_dir(cfg: dict) -> Path:
    d = data_dir(cfg) / "pkgs"
    try:
        d.mkdir(parents=True, exist_ok=True)
    except OSError:
        pass
    return d


def register_pkg(cfg: dict, src: Path) -> dict:
    src = Path(src)
    if not src.is_file():
        return {"ok": False, "error": f"找不到文件：{src}"}
    if src.suffix.lower() not in (".msi", ".exe"):
        return {"ok": False, "error": "只接受 .msi / .exe"}
    try:
        data = src.read_bytes()
    except OSError as e:
        return {"ok": False, "error": f"读取失败：{e}"}
    sha = hashlib.sha256(data).hexdigest()
    name = f"{sha[:12]}{src.suffix.lower()}"
    try:
        shutil.copyfile(src, pkg_dir(cfg) / name)
    except OSError as e:
        return {"ok": False, "error": f"写入失败：{e}"}
    return {"ok": True, "name": name, "sha256": sha, "size": len(data),
            "url": f"/api/fleet/pkg/{name}"}


def pkg_list(cfg: dict) -> list:
    out = []
    try:
        files = list(pkg_dir(cfg).glob("*"))
    except OSError:
        return out
    for p in files:
        if not PKG_NAME.match(p.name):
            continue
        try:
            out.append({"name": p.name, "size": p.stat().st_size,
                        "mtime": datetime.fromtimestamp(p.stat().st_mtime)
                        .isoformat(timespec="seconds"),
                        "url": f"/api/fleet/pkg/{p.name}"})
        except OSError:
            continue
    out.sort(key=lambda x: x["mtime"], reverse=True)
    return out


# ------------------------------------------------------------ agent 版本发布
#
# 自升级 = 主端可以往被控机送一份可执行代码。这是整套系统里权限最大的一环，
# 所以拆成两侧：发布要 view_token（人在浏览器点），下载要 agent_token（机器来拿），
# 而且 agent 落地前必须自己重算一遍 sha256 —— 不信任传输过程。

def release_dir(cfg: dict) -> Path:
    d = data_dir(cfg) / "agent-rel"
    try:
        d.mkdir(parents=True, exist_ok=True)
    except OSError:
        pass
    return d


def publish_agent(cfg: dict, src: Path, version: str, notes: str = "") -> dict:
    src = Path(src)
    if not src.exists():
        return {"ok": False, "error": f"找不到文件：{src}"}
    if not re.match(r"^\d+\.\d+\.\d+$", (version or "").strip()):
        return {"ok": False, "error": "版本号要写成 1.2.3"}
    data = src.read_bytes()
    sha = hashlib.sha256(data).hexdigest()
    dst = release_dir(cfg) / f"pocket_agent-{version}.py"
    shutil.copyfile(src, dst)
    with _LOCK:
        c = connect(cfg)
        try:
            c.execute(
                "INSERT OR REPLACE INTO agent_release"
                "(version,file_path,sha256,size,notes,published_at,is_current)"
                " VALUES (?,?,?,?,?,?,1)",
                (version, str(dst), sha, len(data), notes[:500], _now()))
            c.execute("UPDATE agent_release SET is_current=0 WHERE version<>?", (version,))
            c.commit()
        finally:
            c.close()
    return {"ok": True, "version": version, "sha256": sha, "size": len(data)}


def agent_latest(cfg: dict) -> dict:
    with _LOCK:
        c = connect(cfg)
        try:
            c.row_factory = sqlite3.Row
            r = c.execute(
                "SELECT * FROM agent_release WHERE is_current=1 LIMIT 1").fetchone()
        finally:
            c.close()
    if not r:
        return {"ok": True, "release": None}
    return {"ok": True, "release": {
        "version": r["version"], "sha256": r["sha256"], "size": r["size"],
        "notes": r["notes"], "published_at": r["published_at"],
        "url": f"/api/fleet/agent/download/{r['version']}",
    }}


def release_list(cfg: dict) -> list:
    with _LOCK:
        c = connect(cfg)
        try:
            c.row_factory = sqlite3.Row
            return [dict(r) for r in c.execute(
                "SELECT version,sha256,size,notes,published_at,is_current"
                " FROM agent_release ORDER BY published_at DESC")]
        finally:
            c.close()


# ------------------------------------------------------------------ 本机自采（2026-11-20）
#
# 为什么本机不用 agent：这台 Mac mini 的 `launchctl bootstrap` 报 I/O error 5，
# 装不了新的 LaunchAgent；而 pocket 本身是早期装进去、开机就常驻的。
# 与其和 launchd 较劲，不如让已经在跑的进程顺手采一份——少一个常驻进程，
# 也少一个「谁去开机自启」的问题。
#
# 其他机器仍然用 agent/：那边没有这个限制，而且跨机器本来就该独立进程。

_SELF = {"started": False, "prev": {}, "cpu": {}}
SELF_INTERVAL = 30


def _self_cpu(store: dict) -> float:
    import psutil
    t = psutil.cpu_times()
    idle = float(t.idle or 0) + float(getattr(t, "iowait", 0) or 0)
    total = float(t.user or 0) + float(t.system or 0) + float(t.nice or 0) + float(t.idle or 0)
    for a in ("iowait", "irq", "softirq", "steal"):
        total += float(getattr(t, a, 0) or 0)
    pt, pi = store.get("t"), store.get("i")
    store["t"], store["i"] = total, idle
    if pt is None or pi is None or total <= pt:
        return 0.0
    return round(max(0.0, min(1.0, 1.0 - max(0.0, idle - pi) / (total - pt))) * 100.0, 1)


def _self_procs(prev: dict, n: int = 8) -> list[dict]:
    import psutil
    now = time.time()
    agg: dict[str, list] = {}
    for p in psutil.process_iter(["pid", "name", "cpu_times", "memory_info"]):
        try:
            i = p.info
            ct = i.get("cpu_times")
            if ct is None:
                continue
            name = (i.get("name") or "?").strip() or "?"
            mi = i.get("memory_info")
            a = agg.setdefault(name, [0.0, 0, 1 << 30])
            a[0] += (ct.user or 0.0) + (ct.system or 0.0)
            a[1] += int(mi.rss) if mi else 0
            a[2] = min(a[2], int(i.get("pid") or 0) or (1 << 30))
        except (psutil.NoSuchProcess, psutil.AccessDenied, psutil.ZombieProcess):
            continue
    out = []
    for name, (total, rss, pid) in agg.items():
        p0 = prev.get(name)
        pct = round(max(0.0, (total - p0[0]) / (now - p0[1]) * 100.0), 1) if p0 and now - p0[1] > 0.2 else 0.0
        out.append({"pid": pid, "name": name[:64], "cpu": pct, "rss": rss})
    prev.clear()
    prev.update({k: (v[0], now) for k, v in agg.items()})
    out.sort(key=lambda x: (x["cpu"], x["rss"]), reverse=True)
    return out[:n]


def _self_snapshot() -> dict:
    import os
    import platform
    import socket
    import psutil

    mem = psutil.virtual_memory()
    try:
        du = psutil.disk_usage(os.path.abspath(os.sep))
        used, total = int(du.used), int(du.total)
    except OSError:
        used, total = 0, 0
    return {
        "cpu": _self_cpu(_SELF["cpu"]),
        "mem_used": int(mem.used), "mem_total": int(mem.total),
        "disk_used": used, "disk_total": total,
        "uptime": int(time.time() - psutil.boot_time()),
        "procs": _self_procs(_SELF["prev"]),
        "os": platform.system().lower(),
        "host": socket.gethostname(),
        "agent_ver": "builtin",
    }


def start_self(cfg: dict) -> bool:
    """起一个 daemon 线程，每 30 秒把自己记一笔。失败就当没有，绝不影响主服务。"""
    if _SELF["started"]:
        return True
    try:
        import psutil  # noqa: F401
    except ImportError:
        return False

    dev = str(cfg.get("fleet_self_id") or "mac-mini")
    name = str(cfg.get("fleet_self_name") or "Mac mini")

    def loop() -> None:
        # 第一轮只为建立基准（CPU / 进程都要两次采样才有差值）
        try:
            _self_snapshot()
        except Exception:      # noqa: BLE001
            pass
        time.sleep(2)
        while True:
            try:
                b = _self_snapshot()
                b.update(device_id=dev, name=name, at=_now())
                report(cfg, b, ip="127.0.0.1")
            except Exception:  # noqa: BLE001
                pass
            time.sleep(SELF_INTERVAL)

    threading.Thread(target=loop, daemon=True, name="fleet-self").start()
    _SELF["started"] = True
    return True


# ------------------------------------------------------------------ 路由


def register(app, cfg: dict, cfg_file: Path) -> None:
    """挂到主服务上。只用两行调用，出错把这两行注释掉就回到原样。"""
    from fastapi import Depends, Header, HTTPException, Query
    from fastapi.responses import FileResponse, HTMLResponse

    init(cfg)
    ensure_token(cfg, cfg_file)
    ensure_view_token(cfg, cfg_file)

    # 本机自采：默认开（fleet_self=false 可关）。用独立 agent 的机器不受影响。
    if cfg.get("fleet_self", True):
        start_self(cfg)

    # 上报体积闸门：在 body 被读进内存**之前**拦。
    # 只卡 /api/fleet/report——agent 用 requests 发 JSON，一定带 Content-Length。
    # 响应带 Connection: close：body 没读完就回话，连接必须就此结束，
    # 否则 uvicorn 会试图把剩下的字节读干净再复用连接，反而把协议状态搞乱。
    @app.middleware("http")
    async def _cap_report(request: Request, call_next):
        if request.url.path == "/api/fleet/report":
            cl = (request.headers.get("content-length") or "").strip()
            if cl.isdigit() and int(cl) > MAX_REPORT_BYTES:
                return JSONResponse(
                    {"ok": False, "error": f"上报体积超过 {MAX_REPORT_BYTES} 字节"},
                    status_code=413, headers={"Connection": "close"})
        return await call_next(request)

    def _is_local(request: Request) -> bool:
        peer = (request.client.host if request.client else "") or ""
        if peer not in ("127.0.0.1", "::1", "localhost"):
            return False
        # 光看源 IP 不够。DNS rebinding 能让恶意网页的域名解析到 127.0.0.1，
        # 这时 peer 是 127.0.0.1，但 Host 是攻击者的域名——浏览器依然认为同源，
        # 「本机免令牌」就成了它的跳板。所以 Host 也必须是本机写法。
        host = (request.headers.get("host") or "").strip().lower()
        return (host in ("127.0.0.1", "localhost", "[::1]")
                or host.startswith("127.0.0.1:")
                or host.startswith("localhost:")
                or host.startswith("[::1]:"))

    def agent_auth(x_agent_token: str = Header(None), k: str = Query(None)) -> None:
        """agent 专用。**只能上报和取指令，读不到任何数据。**

        这条限制是这次改动的重点：NUCBOX_M8 要放到办公室，agent 配置就存在那台硬盘上，
        谁打开谁就能看到令牌。如果这个令牌还能读 /overview，
        那「不能查看任何东西」就是句空话。
        """
        if not token_ok(cfg, x_agent_token or k or ""):
            raise HTTPException(401, "agent 令牌无效")

    def view_auth(request: Request, k: str = Query(None),
                  x_view_token: str = Header(None)) -> str:
        """查看/发指令用。本机（127.0.0.1）直接放行，外部必须带 view_token。"""
        if _is_local(request):
            return "local"
        tok = k or x_view_token or ""
        if not view_ok(cfg, tok):
            raise HTTPException(401, "查看令牌无效")
        host = (request.client.host if request.client else "") or ""
        return f"remote:{host}"

    @app.post("/api/fleet/report", dependencies=[Depends(agent_auth)])
    def fleet_report(request: Request, body: dict) -> dict:
        # ip 只认 TCP 源地址。以前是读 X-Real-IP，等于让上报方自己填来源，
        # 想写什么写什么。走本机反代时 peer 是 127.0.0.1，那种情况才看 header。
        peer = (request.client.host if request.client else "") or ""
        ip = peer
        if peer in ("127.0.0.1", "::1"):
            ip = request.headers.get("x-real-ip") or peer
        return report(cfg, body, ip, request.headers.get("x-device-fp") or "")

    # ---- 指令：agent 侧 ----

    @app.post("/api/fleet/poll", dependencies=[Depends(agent_auth)])
    def fleet_poll(request: Request, body: dict) -> dict:
        """agent 定时来问「有没有活」。没人找它，它也不主动开端口——NAT 后面照样能用。"""
        dev = str(body.get("device_id") or "").strip()[:64]
        if not dev:
            return {"ok": False, "error": "device_id 必填"}
        # 指纹不对就别给活：否则拿着令牌冒用别人的 id，
        # 能把发给那台机器的「锁定 / 重启」吞掉，指令还以为自己在排队。
        ok, why = fp_ok(cfg, dev, request.headers.get("x-device-fp"))
        if not ok:
            return {"ok": False, "error": why}
        r = take(cfg, dev)
        # 长任务顺手一起带回去，省一次往返。
        # 但只给声明了能力的 agent：老版本不认识 task 字段，
        # 给它就会把任务领走然后一直挂着，直到两小时后判超时。
        caps = body.get("caps") or []
        if isinstance(caps, list) and "tasks" in caps:
            r["task"] = take_task(cfg, dev).get("task")
        return r

    @app.post("/api/fleet/result", dependencies=[Depends(agent_auth)])
    def fleet_result(request: Request, body: dict) -> dict:
        cid = str(body.get("id") or "").strip()[:32]
        if not cid:
            return {"ok": False, "error": "id 必填"}
        # 回执也要认人：否则冒名者可以替别人回一条「已锁定」。
        dev = device_of(cfg, "commands", cid)
        ok, why = fp_ok(cfg, dev, request.headers.get("x-device-fp"))
        if not ok:
            return {"ok": False, "error": why}
        return finish(cfg, cid, bool(body.get("ok")), str(body.get("output") or ""))

    # ---- 指令：主端侧 ----

    @app.post("/api/fleet/unbind")
    def fleet_unbind(request: Request, body: dict,
                     k: str = Query(None), x_view_token: str = Header(None)) -> dict:
        """清掉某台设备的指纹绑定，下次上报重新绑。

        这是安全机制的出口，不是后门：绑定之后主端一律不覆盖 fp，
        于是「真机还没装，被别人抢先注册了同名 id」这种情况下真机会一直被拒。
        没有这个口就只能去改数据库。只有查看令牌能调（本机免令牌）。
        """
        view_auth(request, k, x_view_token)
        dev = str(body.get("device_id") or "").strip()[:64]
        if not dev:
            return {"ok": False, "error": "device_id 必填"}
        with _LOCK:
            c = connect(cfg)
            try:
                n = c.execute("UPDATE devices SET fp='' WHERE id=?", (dev,)).rowcount
                c.commit()
            finally:
                c.close()
        if not n:
            return {"ok": False, "error": "没有这台设备"}
        return {"ok": True, "device_id": dev, "note": "已解绑，下次上报会重新绑定"}

    @app.post("/api/fleet/command")
    def fleet_command(request: Request, body: dict,
                      k: str = Query(None), x_view_token: str = Header(None)) -> dict:
        who = view_auth(request, k, x_view_token)
        return enqueue(cfg, str(body.get("device_id") or "").strip()[:64],
                       str(body.get("action") or "").strip()[:16], who)

    @app.get("/api/fleet/commands")
    def fleet_commands(request: Request, k: str = Query(None),
                       x_view_token: str = Header(None)) -> dict:
        view_auth(request, k, x_view_token)
        return {"ok": True, "items": recent_commands(cfg), "actions": ACTIONS}

    @app.get("/api/fleet/overview")
    def fleet_overview(request: Request, k: str = Query(None),
                       x_view_token: str = Header(None)) -> dict:
        view_auth(request, k, x_view_token)
        return overview(cfg)

    @app.post("/api/fleet/access")
    def fleet_access(request: Request, body: dict,
                     k: str = Query(None), x_view_token: str = Header(None)) -> dict:
        view_auth(request, k, x_view_token)
        return set_access(cfg, str(body.get("device_id") or ""), str(body.get("access") or ""))

    # ---- 软件清单与长任务：agent 侧 ----

    @app.post("/api/fleet/software", dependencies=[Depends(agent_auth)])
    def fleet_software_report(request: Request, body: dict) -> dict:
        dev = str(body.get("device_id") or "").strip()[:64]
        if not dev:
            return {"ok": False, "error": "device_id 必填"}
        ok, why = fp_ok(cfg, dev, request.headers.get("x-device-fp"))
        if not ok:
            return {"ok": False, "error": why}
        # "upgrades" 键在不在决定要不要覆盖旧的可升级列表：
        # agent 只在真的跑过 winget upgrade 时才带这个键。
        ups = body.get("upgrades") if "upgrades" in body else None
        return save_software(cfg, dev, body.get("items") or [], ups)

    @app.post("/api/fleet/task/progress", dependencies=[Depends(agent_auth)])
    def fleet_task_progress(request: Request, body: dict) -> dict:
        tid = str(body.get("id") or "").strip()[:32]
        if not tid:
            return {"ok": False, "error": "id 必填"}
        ok, why = fp_ok(cfg, device_of(cfg, "tasks", tid),
                        request.headers.get("x-device-fp"))
        if not ok:
            return {"ok": False, "error": why}
        try:
            prog = int(body.get("progress") or 0)
        except (TypeError, ValueError):
            prog = 0
        return progress_task(cfg, tid, prog, str(body.get("log") or ""))

    @app.post("/api/fleet/task/result", dependencies=[Depends(agent_auth)])
    def fleet_task_result(request: Request, body: dict) -> dict:
        tid = str(body.get("id") or "").strip()[:32]
        if not tid:
            return {"ok": False, "error": "id 必填"}
        ok, why = fp_ok(cfg, device_of(cfg, "tasks", tid),
                        request.headers.get("x-device-fp"))
        if not ok:
            return {"ok": False, "error": why}
        return finish_task(cfg, tid, bool(body.get("ok")), str(body.get("log") or ""))

    @app.get("/api/fleet/agent/latest", dependencies=[Depends(agent_auth)])
    def fleet_agent_latest() -> dict:
        """agent 问「最新版是多少」。返回哈希，agent 落地前自己再算一遍。"""
        return agent_latest(cfg)

    @app.get("/api/fleet/agent/download/{version}", dependencies=[Depends(agent_auth)])
    def fleet_agent_download(version: str) -> FileResponse:
        ver = str(version or "").strip()[:20]
        rel = (agent_latest(cfg).get("release") or {})
        if rel.get("version") != ver:
            raise HTTPException(404, "没有这个版本")
        p = release_dir(cfg) / f"pocket_agent-{ver}.py"
        if not p.exists():
            raise HTTPException(404, "发布文件已丢失")
        return FileResponse(str(p), media_type="text/x-python",
                            filename=f"pocket_agent-{ver}.py")

    # ---- 软件清单与长任务：主端侧 ----

    @app.get("/api/fleet/software")
    def fleet_software(request: Request, device: str = Query(""), q: str = Query(""),
                       k: str = Query(None), x_view_token: str = Header(None)) -> dict:
        view_auth(request, k, x_view_token)
        return software_list(cfg, device.strip()[:64], q.strip()[:80])

    @app.post("/api/fleet/task")
    def fleet_task(request: Request, body: dict,
                   k: str = Query(None), x_view_token: str = Header(None)) -> dict:
        view_auth(request, k, x_view_token)
        args = body.get("args") if isinstance(body.get("args"), dict) else None
        return enqueue_task(cfg, str(body.get("device_id") or "").strip()[:64],
                            str(body.get("kind") or "").strip()[:16],
                            str(body.get("target") or "").strip()[:160], args)

    @app.get("/api/fleet/tasks")
    def fleet_tasks(request: Request, k: str = Query(None),
                    x_view_token: str = Header(None)) -> dict:
        view_auth(request, k, x_view_token)
        return {"ok": True, "items": recent_tasks(cfg), "kinds": TASK_KINDS}

    @app.get("/api/fleet/agent/releases")
    def fleet_agent_releases(request: Request, k: str = Query(None),
                             x_view_token: str = Header(None)) -> dict:
        view_auth(request, k, x_view_token)
        return {"ok": True, "items": release_list(cfg)}

    @app.post("/api/fleet/pkg/register")
    def fleet_pkg_register(request: Request, body: dict,
                           k: str = Query(None), x_view_token: str = Header(None)) -> dict:
        """把一个主端本地安装包登记成可下发。file 是主端机器上的绝对路径。"""
        view_auth(request, k, x_view_token)
        src = str(body.get("file") or "").strip()
        if not src:
            return {"ok": False, "error": "缺少 file（主端本地路径）"}
        return register_pkg(cfg, Path(src).expanduser())

    @app.get("/api/fleet/pkg")
    def fleet_pkg_list(request: Request, k: str = Query(None),
                       x_view_token: str = Header(None)) -> dict:
        view_auth(request, k, x_view_token)
        return {"ok": True, "items": pkg_list(cfg)}

    @app.get("/api/fleet/pkg/{name}", dependencies=[Depends(agent_auth)])
    def fleet_pkg_get(name: str) -> FileResponse:
        # 文件名必须是登记时生成的 12 位哈希 + 扩展名，杜绝 ../ 之类的路径穿越
        if not PKG_NAME.match(name or ""):
            raise HTTPException(404, "没有这个包")
        p = pkg_dir(cfg) / name
        if not p.is_file():
            raise HTTPException(404, "没有这个包")
        return FileResponse(str(p), media_type="application/octet-stream",
                            filename=name)

    @app.post("/api/fleet/agent/publish")
    def fleet_agent_publish(request: Request, body: dict,
                            k: str = Query(None), x_view_token: str = Header(None)) -> dict:
        """把主端本地的一个 .py 发布成 agent 版本。file 是主端机器上的绝对路径。

        这是整套系统里权限最大的一个接口（等于往每台被控机送可执行代码），
        所以只走 view_auth：本机 127.0.0.1 或带 view_token。
        """
        view_auth(request, k, x_view_token)
        src = str(body.get("file") or "").strip()
        if not src:
            return {"ok": False, "error": "缺少 file（主端本地路径）"}
        return publish_agent(cfg, Path(src).expanduser(),
                             str(body.get("version") or "").strip(),
                             str(body.get("notes") or ""))

    @app.post("/api/fleet/prune", dependencies=[Depends(agent_auth)])
    def fleet_prune() -> dict:
        return prune(cfg)

    @app.get("/fleet", response_class=HTMLResponse)
    def fleet_page(request: Request) -> HTMLResponse:
        """页面放数据根，改完刷新即生效——跟 /dash 一个套路。

        本机（127.0.0.1）打开时把令牌注进去，省得每次手拼 ?k=。
        从局域网或 Tailscale 进来的一律不注，仍然要求 ?k= 或已存过的 localStorage 令牌。
        """
        p = page_path(cfg)
        if not p.exists():
            return HTMLResponse(
                "<meta charset='utf-8'><body style='font:15px -apple-system;padding:40px'>"
                f"<h3>多机页面还没生成</h3><p style='color:#666'>应当位于：{p}</p></body>",
                status_code=404)
        html = p.read_text(encoding="utf-8")
        host = (request.client.host if request.client else "") or ""
        if host in ("127.0.0.1", "::1", "localhost"):
            # 注入的是 view_token，不是 fleet_token。后者只给 agent，
            # 万一页面源码被谁翻到，也不至于把上报令牌一起带走。
            tok = json.dumps(str(cfg.get("view_token") or ""))
            html = html.replace("</head>", f"<script>window.__FK__={tok}</script></head>", 1)
        return HTMLResponse(html, headers={
            "Cache-Control": "no-store, max-age=0",
            "Referrer-Policy": "no-referrer",
            "X-Content-Type-Options": "nosniff",
        })
