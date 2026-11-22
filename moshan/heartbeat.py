#!/usr/bin/env python3
"""系统心跳：纯代码的服务自检 + 自愈，全程不调用任何模型。

挂在 pocket（launchd 常驻）的 daemon 线程里，每 interval_sec 跑一次。
为什么放在 pocket 里：这台机器的 `launchctl bootstrap` 报 I/O error，装不了新
LaunchAgent / cron；但 pocket 本身是早先装进去、开机就常驻的。由它来巡检并
拉起微信助手 gateway，等于用「已经在跑的服务」给「没托管的服务」做兜底。

自检的三类根因（都是确定性判断，不需要 4B）：
  1. gateway 掉线        -> HTTP 探活；掉线则清陈旧锁、拉起、轮询就绪
  2. Node 版本不够       -> 读 brew node 版本号比较；并校验启动脚本显式指向它
  3. 陈旧锁              -> 读锁文件里的 pid，pid 已死且端口不通则清理

另带三项只读巡检：LaunchAgent plist 是否在位/已加载、Ollama 11434 是否在线、
微信主会话体积（过大时先备份再截断到最近 30 条——微信助手主模型是本机 4B，
OpenClaw 侧窗口设为 32768，历史积累过大仍会出现「Context is too large」卡死）。
自愈有冷却 + 连续失败上限，避免反复拉起刷日志；只在确实掉线/超限时才动作。

可脱离 pocket 单独跑：
  python heartbeat.py --once             # 跑一次，人类可读
  python heartbeat.py --once --json      # 跑一次，JSON（给脚本/排错）
  python heartbeat.py --once --no-heal  # 只检查不修
  python heartbeat.py                   # 常驻循环
"""
from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from datetime import datetime
from pathlib import Path

BASE = Path(__file__).resolve().parent
HOME = Path.home()

OPENCLAW = HOME / ".openclaw"
LOCK_DIR = OPENCLAW / "tmp" / "openclaw-501"
SERVE_SH = OPENCLAW / "serve-gateway.sh"
PLIST = HOME / "Library" / "LaunchAgents" / "com.local-ai.openclaw-gateway.plist"
BREW_NODE = "/opt/homebrew/opt/node@22/bin/node"
MIN_NODE = (22, 22, 3)          # OpenClaw CLI 的版本门槛

GW_PORT = 18789
GW_URL_TMPL = "http://127.0.0.1:{port}/"
OLLAMA_URL = "http://127.0.0.1:11434/api/tags"

AGENT_DB = OPENCLAW / "agents" / "main" / "agent" / "openclaw-agent.sqlite"
OPENCLAW_MJS = Path("/opt/homebrew/lib/node_modules/openclaw/openclaw.mjs")
BACKUP_DIR = OPENCLAW / "backups"
SESSION_WARN_BYTES = 80 * 1024   # 主会话超过 80KB 就接近 4B 的 32k token 窗口
SESSION_KEEP_LINES = 30          # 截断时保留最近 N 条
SESSION_COMPACT_COOLDOWN = 600   # 两次截断至少隔 10 分钟

LOG_DIR = HOME / "Library" / "Logs"          # 系统惯例：进程 stdout / gateway 日志留这儿
# 结构化记录（state + 流水 + 磁盘采样）落数据根，跟个人数据一样能翻能备份（2026-11-18）。
# 数据根不可写时回退 LOG_DIR —— 自愈是兜底功能，不能因为一个目录没了就停摆。
SYS_DIR = HOME / "Documents" / "AI项目" / "10-系统记录"
HB_DIR = SYS_DIR / "心跳"
DISK_LOG = SYS_DIR / "磁盘" / "磁盘.jsonl"
_LEGACY_STATE = LOG_DIR / "cuntu-heartbeat-state.json"
_LEGACY_LOG = LOG_DIR / "cuntu-heartbeat.jsonl"
GW_LOG = LOG_DIR / "openclaw-gateway.log"
_MIGRATED = False


def _hb_dir() -> Path:
    """优先 10-系统记录/心跳，写不进去用 Library/Logs。"""
    try:
        HB_DIR.mkdir(parents=True, exist_ok=True)
        return HB_DIR
    except OSError:
        return LOG_DIR


def state_file() -> Path:
    d = _hb_dir()
    return d / ("最新.json" if d == HB_DIR else "cuntu-heartbeat-state.json")


def log_file() -> Path:
    d = _hb_dir()
    return d / ("流水.jsonl" if d == HB_DIR else "cuntu-heartbeat.jsonl")


def _migrate() -> None:
    """Logs 里的旧记录搬一次：流水整体搬走，state 复制一份改名留底。"""
    global _MIGRATED
    if _MIGRATED:
        return
    _MIGRATED = True
    try:
        HB_DIR.mkdir(parents=True, exist_ok=True)
    except OSError:
        return
    old_l, new_l = _LEGACY_LOG, HB_DIR / "流水.jsonl"
    try:
        if old_l.is_file():
            if new_l.is_file():
                with new_l.open("a", encoding="utf-8") as f:
                    f.write(old_l.read_text(encoding="utf-8"))
            else:
                old_l.replace(new_l)
    except OSError:
        pass
    old_s, new_s = _LEGACY_STATE, HB_DIR / "最新.json"
    try:
        if old_s.is_file() and not new_s.is_file():
            new_s.write_bytes(old_s.read_bytes())
        if old_s.is_file():
            old_s.replace(old_s.with_name("cuntu-heartbeat-state.json.migrated-20261118"))
    except OSError:
        pass


_migrate()

_LOCK = threading.Lock()         # 同一时刻只允许一次自检
STATE: dict = {
    "running": False,
    "last_at": "",
    "last": None,
    "restart_count": 0,
    "last_restart_at": 0.0,
    "consecutive_offline": 0,
    "needs_attention": False,
    "last_compact_at": 0.0,
}


# --------------------------------------------------------------------------
# 配置与基础工具
# --------------------------------------------------------------------------
def load_config() -> dict:
    try:
        return json.loads((BASE / "config.json").read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}


def cfg_of(cfg: dict) -> dict:
    d = cfg.get("heartbeat") or {}
    return {
        "enabled": d.get("enabled", True) is not False,
        "interval": max(30, int(d.get("interval_sec", 120))),
        "cooldown": max(30, int(d.get("cooldown_sec", 180))),
        "max_restarts": max(1, int(d.get("max_restarts", 3))),
        "heal": d.get("heal", True) is not False,
        "port": int(d.get("gateway_port", GW_PORT)),
    }


def _now() -> str:
    return datetime.now().strftime("%Y-%m-%d %H:%M:%S")


def http_ok(url: str, timeout: float = 2.0) -> bool:
    """loopback 探活。禁代理：环境里可能设了 http_proxy，会把 127.0.0.1 也劫走。"""
    op = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    try:
        with op.open(urllib.request.Request(url, method="GET"), timeout=timeout) as r:
            return 200 <= r.status < 400
    except (urllib.error.URLError, OSError, ValueError):
        return False


def pid_alive(pid) -> bool:
    try:
        pid = int(pid)
    except (TypeError, ValueError):
        return False
    if pid <= 0:
        return False
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True                      # 存在但无权，保守视为活着
    except OSError:
        return False


def node_version(exe: str) -> tuple | None:
    try:
        r = subprocess.run([exe, "--version"], capture_output=True, text=True, timeout=6)
        m = re.search(r"(\d+)\.(\d+)\.(\d+)", (r.stdout or "") + (r.stderr or ""))
        return tuple(int(x) for x in m.groups()) if m else None
    except (OSError, subprocess.SubprocessError):
        return None


def _log(record: dict) -> None:
    try:
        p = log_file()
        p.parent.mkdir(parents=True, exist_ok=True)
        with p.open("a", encoding="utf-8") as f:
            f.write(json.dumps(record, ensure_ascii=False) + "\n")
    except OSError:
        pass


def disk_sample() -> dict:
    """每轮一行磁盘水位，落 10-系统记录/磁盘/磁盘.jsonl。

    单独一份的理由：想知道「哪天开始满的」得看曲线，而心跳流水里只记事件；
    两处采样同一时刻、同一个值，不存第二份判断。
    """
    try:
        u = shutil.disk_usage(str(SYS_DIR))
    except OSError:
        return {}
    total = u.total or 1
    return {
        "at": _now(),
        "used_pct": round(u.used * 100.0 / total, 1),
        "free_gb": round(u.free / 1e9, 1),
        "total_gb": round(u.total / 1e9, 1),
    }


def _log_disk(sample: dict) -> None:
    if not sample:
        return
    try:
        DISK_LOG.parent.mkdir(parents=True, exist_ok=True)
        with DISK_LOG.open("a", encoding="utf-8") as f:
            f.write(json.dumps(sample, ensure_ascii=False) + "\n")
    except OSError:
        pass


def _save_state(result: dict) -> None:
    try:
        p = state_file()
        p.parent.mkdir(parents=True, exist_ok=True)
        tmp = p.with_suffix(".tmp")
        tmp.write_text(json.dumps(result, ensure_ascii=False, indent=2), encoding="utf-8")
        tmp.replace(p)
    except OSError:
        pass


# --------------------------------------------------------------------------
# 检查项
# --------------------------------------------------------------------------
def check_gateway(port: int) -> bool:
    return http_ok(GW_URL_TMPL.format(port=port), timeout=2.0)


def check_node() -> dict:
    """返回 brew node / PATH node / 启动脚本三部分的判定。"""
    brew = node_version(BREW_NODE)
    brew_ok = bool(brew and brew >= MIN_NODE)

    path_node = shutil.which("node") or ""
    path_ver = node_version(path_node) if path_node else None
    path_ok = bool(path_ver and path_ver >= MIN_NODE)

    launcher = ""
    try:
        launcher = SERVE_SH.read_text(encoding="utf-8", errors="replace")
    except OSError:
        pass
    # 关键：启动脚本必须显式写死 brew node 绝对路径，否则会退回 PATH 里的旧 node
    launcher_ok = BREW_NODE in launcher and 'exec "$NODE_BIN"' in launcher

    return {
        "brew": brew, "brew_ok": brew_ok,
        "path": path_ver, "path_ok": path_ok, "path_exe": path_node,
        "launcher_ok": launcher_ok, "launcher_exists": bool(launcher),
        "min": MIN_NODE,
    }


def scan_locks() -> list[dict]:
    """列出所有 gateway.*.lock 及其 pid 是否存活。"""
    items: list[dict] = []
    if not LOCK_DIR.is_dir():
        return items
    for f in sorted(LOCK_DIR.glob("gateway.*.lock")):
        try:
            data = json.loads(f.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            data = {}
        pid = data.get("pid")
        items.append({"file": f, "pid": pid, "alive": pid_alive(pid)})
    return items


def clean_stale_locks(items: list[dict]) -> list[str]:
    """只清 pid 已死的锁（含 .sqlite / -journal / .reclaim 伴随文件）。"""
    removed: list[str] = []
    for it in items:
        if it["alive"]:
            continue
        stem = it["file"].name
        for c in sorted(LOCK_DIR.glob(stem + "*")):
            try:
                if c.is_dir():
                    shutil.rmtree(c)
                else:
                    c.unlink()
                removed.append(c.name)
            except OSError:
                pass
    return removed


def check_session() -> dict:
    """只读检查主会话体积。微信助手主模型是本机 4B（OpenClaw 侧窗口 32768），
    transcript 一旦积累过大，LLM 式 auto-compaction 救不回来，表现为
    「Context is too large」报错卡片。这里用确定性阈值提前发现。"""
    import sqlite3
    out = {"db_exists": AGENT_DB.exists(), "sid": "", "events": 0, "bytes": 0}
    if not out["db_exists"]:
        return out
    try:
        con = sqlite3.connect(f"file:{AGENT_DB}?mode=ro", uri=True, timeout=4)
        try:
            row = con.execute(
                "SELECT session_id, count(*), sum(length(event_json)) "
                "FROM transcript_events GROUP BY session_id "
                "ORDER BY count(*) DESC LIMIT 1").fetchone()
        finally:
            con.close()
        if row:
            out.update({"sid": row[0] or "", "events": row[1], "bytes": row[2] or 0})
    except sqlite3.Error:
        pass
    return out


def backup_agent_db() -> str:
    """截断前先备份整个会话库，返回备份路径（失败返回空串）。"""
    try:
        BACKUP_DIR.mkdir(parents=True, exist_ok=True)
        ts = datetime.now().strftime("%Y%m%d-%H%M%S")
        dst = BACKUP_DIR / f"openclaw-agent-{ts}.sqlite"
        shutil.copy2(AGENT_DB, dst)
        # 只留最近 10 份，防备份目录无限膨胀
        olds = sorted(BACKUP_DIR.glob("openclaw-agent-*.sqlite"))
        for old in olds[:-10]:
            try:
                old.unlink()
            except OSError:
                pass
        return str(dst)
    except OSError:
        return ""


def compact_session(keep: int = SESSION_KEEP_LINES) -> dict:
    """走 gateway RPC 做截断式压缩（--max-lines 不经过 LLM，秒级）。"""
    if not OPENCLAW_MJS.exists():
        return {"ok": False, "error": f"找不到 {OPENCLAW_MJS}"}
    try:
        r = subprocess.run(
            [BREW_NODE, str(OPENCLAW_MJS), "sessions", "compact",
             "agent:main:main", "--max-lines", str(keep),
             "--timeout", "30000", "--json"],
            capture_output=True, text=True, timeout=60)
        body = {}
        try:
            body = json.loads((r.stdout or "")[-2000:] or "{}")
        except ValueError:
            pass
        ok = r.returncode == 0 and body.get("ok") is True
        return {"ok": ok, "kept": body.get("kept"),
                "error": None if ok else (r.stderr or r.stdout or "")[-200:]}
    except (OSError, subprocess.SubprocessError, ValueError) as e:
        return {"ok": False, "error": str(e)[:200]}


def check_launchd() -> dict:
    plist_exists = PLIST.exists()
    lint_ok = False
    if plist_exists:
        try:
            r = subprocess.run(["/usr/bin/plutil", "-lint", str(PLIST)],
                               capture_output=True, text=True, timeout=6)
            lint_ok = r.returncode == 0
        except (OSError, subprocess.SubprocessError):
            pass
    loaded = False
    try:
        r = subprocess.run(
            ["/bin/launchctl", "print", "gui/501/com.local-ai.openclaw-gateway"],
            capture_output=True, text=True, timeout=6)
        loaded = r.returncode == 0 and "state = running" in (r.stdout or "")
    except (OSError, subprocess.SubprocessError):
        pass
    return {"plist_exists": plist_exists, "lint_ok": lint_ok, "loaded": loaded}


# --------------------------------------------------------------------------
# 自愈
# --------------------------------------------------------------------------
def start_gateway() -> dict:
    """后台拉起 serve-gateway.sh；start_new_session 让它脱离 pocket 进程组。"""
    if not SERVE_SH.exists():
        return {"ok": False, "error": f"找不到启动脚本 {SERVE_SH}"}
    try:
        LOG_DIR.mkdir(parents=True, exist_ok=True)
        out = GW_LOG.open("a", encoding="utf-8")
        p = subprocess.Popen(
            ["/bin/bash", str(SERVE_SH)],
            stdin=subprocess.DEVNULL, stdout=out, stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        return {"ok": True, "pid": p.pid}
    except OSError as e:
        return {"ok": False, "error": str(e)[:200]}


def wait_ready(port: int, seconds: int = 50) -> tuple[bool, float]:
    """gateway 冷启动实测约 18 秒，轮询等到 HTTP 通。"""
    t0 = time.time()
    deadline = t0 + seconds
    while time.time() < deadline:
        if check_gateway(port):
            return True, round(time.time() - t0, 1)
        time.sleep(2)
    return False, round(time.time() - t0, 1)


# ---- 巡检服务（8774）：常驻，掉了就拉 ---------------------------------------
# 为什么由心跳来管：巡检自己发现不了「我不在」—— 它没跑的时候页面都打不开。
# 2026-11-20 之前确实没有任何人看它：凌晨关页后 lease 让它自己退了，launchd 里
# 没有 plist、心跳也不看 8774，于是连着 18 小时没人巡检。
INSPECT_PORT = 8774
INSPECT_CMD = HOME / "Documents" / "AI项目" / "_巡检" / "_系统巡检.command"


def start_inspect() -> dict:
    """拉起巡检。走 .command 而不是直连 syscheck.py —— 那里 export 了
    CUNTU_INSPECT_ALWAYS=1，少了它巡检会装 lease，页面一关又退，白拉。"""
    if not INSPECT_CMD.exists():
        return {"ok": False, "error": f"找不到 {INSPECT_CMD}"}
    try:
        LOG_DIR.mkdir(parents=True, exist_ok=True)
        out = (LOG_DIR / "cuntu-syscheck.log").open("a", encoding="utf-8")
        p = subprocess.Popen(
            ["/bin/bash", str(INSPECT_CMD)],
            stdin=subprocess.DEVNULL, stdout=out, stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        return {"ok": True, "pid": p.pid}
    except OSError as e:
        return {"ok": False, "error": str(e)[:200]}


def wait_inspect(seconds: int = 12) -> tuple[bool, float]:
    t0 = time.time()
    while time.time() < t0 + seconds:
        if http_ok(f"http://127.0.0.1:{INSPECT_PORT}/api/state", timeout=1.0):
            return True, round(time.time() - t0, 1)
        time.sleep(1)
    return False, round(time.time() - t0, 1)


# ---- App 监工：谁打开了、谁关了、谁闪退了、谁卡住了 -------------------------
# 跟巡检同一个道理：它自己发现不了「我不在」。但它不是端口服务，不能 HTTP 探活 ——
# 认它每 15 秒刷一次的 state.json。只判 pid 活不够：进程卡在某个调用里时
# pid 还在、账却已经不记了，那种「假活」才是最难发现的。
APPWATCH_STATE = LOG_DIR / "cuntu-appwatch.state.json"


def _appwatch_cmd(cfg: dict | None) -> Path:
    return _data_root(cfg) / "10-系统记录" / "_App监工.command"


def appwatch_state(max_age: int = 90) -> dict:
    try:
        d = json.loads(APPWATCH_STATE.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {"ok": False, "why": "state.json 读不出来（还没跑过？）"}
    pid = int(d.get("pid") or 0)
    if pid <= 0:
        return {"ok": False, "why": "state.json 里没有 pid"}
    try:
        os.kill(pid, 0)
    except OSError:
        return {"ok": False, "why": f"pid {pid} 已经没了"}
    try:
        age = time.time() - APPWATCH_STATE.stat().st_mtime
    except OSError:
        age = 0.0
    if age > max_age:
        return {"ok": False, "why": f"pid {pid} 在，但 {int(age)}s 没刷过心跳（可能卡住了）"}
    return {"ok": True, "pid": pid, "at": d.get("at"), "apps": d.get("apps"),
            "ax": bool(d.get("ax")), "age": int(age)}


def start_appwatch(cfg: dict | None = None) -> dict:
    cmd = _appwatch_cmd(cfg)
    if not cmd.exists():
        return {"ok": False, "error": f"找不到 {cmd}"}
    try:
        LOG_DIR.mkdir(parents=True, exist_ok=True)
        out = (LOG_DIR / "cuntu-appwatch.log").open("a", encoding="utf-8")
        p = subprocess.Popen(
            ["/bin/bash", str(cmd)],
            stdin=subprocess.DEVNULL, stdout=out, stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        return {"ok": True, "pid": p.pid}
    except OSError as e:
        return {"ok": False, "error": str(e)[:200]}


def wait_appwatch(seconds: int = 10) -> tuple[bool, float]:
    t0 = time.time()
    while time.time() < t0 + seconds:
        if appwatch_state().get("ok"):
            return True, round(time.time() - t0, 1)
        time.sleep(0.5)
    return False, round(time.time() - t0, 1)


def appwatch_tally(cfg: dict | None = None, hours: int = 24) -> dict:
    """近 N 小时账本里的坏消息：闪退 / 卡住 / 卡住后被强制退出。"""
    out = {"crash": 0, "hang": 0, "kill": 0, "names": []}
    p = _data_root(cfg) / "10-系统记录" / "App事件.jsonl"
    try:
        lines = p.read_text(encoding="utf-8", errors="replace").splitlines()[-4000:]
    except OSError:
        return out
    cut = datetime.now().timestamp() - hours * 3600
    for ln in lines:
        try:
            r = json.loads(ln)
        except ValueError:
            continue
        key = {"app_crash": "crash", "app_hang": "hang", "app_kill": "kill"}.get(
            str(r.get("event") or ""))
        if not key:
            continue
        try:
            t = datetime.strptime(str(r.get("ts"))[:19], "%Y-%m-%d %H:%M:%S").timestamp()
        except ValueError:
            continue
        if t < cut:
            continue
        out[key] += 1
        if len(out["names"]) < 3 and r.get("app"):
            out["names"].append(str(r["app"]))
    return out


def _data_root(cfg: dict | None) -> Path:
    """数据根：和 pocket 其它模块同一套（allowed_roots 第一个）。"""
    roots = (cfg or {}).get("allowed_roots") or []
    if roots:
        return Path(roots[0])
    return Path.home() / "Documents" / "AI项目"


def check_transfer_push(cfg: dict | None) -> dict:
    """传输助手 → 手机：那条**媒体推送**链路还通不通。

    为什么值得单独一项：传输助手不是一个端口，它跑在 pocket 里，
    探活永远是好的；而「网页发的图能不能到手机」走的是另一条链路
    （pocket → gateway → 微信 sendMessage）。2026-11-21 这条链断了一整天，
    图片/文件全回 `ret=-2 prepare failed`，纯文本却照常 —— 端口检查、
    页面、「发送成功」全都看不出问题，只有人去翻手机才发现。

    判据只有账本 `14-传输/推送失败.jsonl`，口径复用 transfer.push_summary_at，
    不在这里另写一套（另写必然走样）。
    """
    name = "传输助手 → 手机"
    p = _data_root(cfg) / "14-传输" / "推送失败.jsonl"
    try:
        sys.path.insert(0, str(Path(__file__).resolve().parent))
        import transfer
        s = transfer.push_summary_at(p, 6)
    except Exception as e:              # noqa: BLE001  读不了不该把整轮判成坏
        return {"id": "transfer_push", "level": "warn", "ok": True, "name": name,
                "detail": f"账本读不了（{str(e)[:60]}）"}
    total = int(s.get("total") or 0)
    bad = int(s.get("failed") or 0)
    if not total:
        return {"id": "transfer_push", "level": "warn", "ok": True, "name": name,
                "detail": "近 6 小时没往手机推过东西（没发就不用推）"}
    if not bad:
        return {"id": "transfer_push", "level": "warn", "ok": True, "name": name,
                "detail": f"近 6 小时推了 {total} 条，全到了"}
    names = "、".join([x for x in (s.get("names") or []) if x][:2])
    detail = (f"近 6 小时 {bad}/{total} 条没推到手机"
              + (f"（{names}）" if names else "")
              + (f"；最近一次 {str(s.get('at') or '')[11:16]}" if s.get("at") else "")
              + (f"；原因：{str(s.get('error') or '')[:90]}" if s.get("error") else ""))
    return {"id": "transfer_push", "level": "warn", "ok": False, "name": name,
            "detail": detail}


# --------------------------------------------------------------------------
# 跑一次
# --------------------------------------------------------------------------
def run_once(cfg: dict | None = None, heal: bool = True) -> dict:
    cfg = cfg or load_config()          # 下面的只读检查要 allowed_roots，留个完整配置
    c = cfg_of(cfg)
    port = c["port"]
    do_heal = c["heal"] and heal

    if not _LOCK.acquire(blocking=False):
        return {"ok": False, "error": "正在自检，等这次结束", "at": _now()}

    checks: list[dict] = []
    healed: list[str] = []
    try:
        STATE["running"] = True
        online = check_gateway(port)
        locks = scan_locks()
        stale = [x for x in locks if x["pid"] and not x["alive"]]
        node = check_node()
        launchd = check_launchd()
        ollama_ok = http_ok(OLLAMA_URL, timeout=2.0)

        # --- 0. 陈旧锁：pid 已死就是孤儿，无条件清（不只是掉线时）---
        # 留着它只会在下次启动时把 gateway 拒之门外，这正是 11-18 断线的直接原因。
        stale_pids = [x["pid"] for x in stale]
        if stale:
            if do_heal:
                removed = clean_stale_locks(stale)
                if removed:
                    healed.append(f"清理陈旧锁 {len(removed)} 个文件（{', '.join(removed[:3])}）")
                    _log({"at": _now(), "event": "stale_locks_removed",
                          "pids": stale_pids, "files": removed})
                    locks = scan_locks()          # 报告反映清理后的真实状态
                    stale = [x for x in locks if x["pid"] and not x["alive"]]
            else:
                healed.append(f"[仅检查] 发现 {len(stale)} 个陈旧锁，--no-heal 未清理")

        # --- 1. gateway 在线 + 掉线自愈 ---
        gw_action = ""
        if not online:
            STATE["consecutive_offline"] += 1
            now_ts = time.time()
            cooling = now_ts - STATE["last_restart_at"] < c["cooldown"]
            exhausted = STATE["consecutive_offline"] > c["max_restarts"]
            if do_heal and not cooling and not exhausted:
                r = start_gateway()
                if r.get("ok"):
                    STATE["last_restart_at"] = now_ts
                    STATE["restart_count"] += 1
                    ready, sec = wait_ready(port)
                    if ready:
                        online = True
                        STATE["consecutive_offline"] = 0
                        STATE["needs_attention"] = False
                        healed.append(f"gateway 已拉起并就绪（{sec}s，pid {r['pid']}）")
                        gw_action = f"已拉起，{sec}s 就绪"
                    else:
                        gw_action = f"已拉起但 {sec}s 内未就绪，下轮再看"
                        healed.append(gw_action)
                else:
                    gw_action = f"拉起失败：{r.get('error')}"
            else:
                if cooling:
                    left = int(c["cooldown"] - (now_ts - STATE["last_restart_at"]))
                    gw_action = f"冷却中，{left}s 后可重试"
                if exhausted:
                    STATE["needs_attention"] = True
                    gw_action = f"连续 {STATE['consecutive_offline']} 轮掉线，已停止自动重试，需人工查看"
        else:
            STATE["consecutive_offline"] = 0
            STATE["needs_attention"] = False

        checks.append({
            "id": "gateway_online", "level": "critical", "ok": online,
            "name": f"微信助手 gateway（:{port}）",
            "detail": ("HTTP 200，在线" if online else "端口不通") + (f"；{gw_action}" if gw_action else ""),
        })

        # --- 1.5 巡检服务（8774）：常驻，掉了就拉回来 ---
        # 巡检是「谁在盯着这台机器」的那个东西，它自己不能没人看。level 用 warn：
        # 能自愈的掉线不该把整体判成 critical，但页面上要看得见这一项没通过。
        insp_ok = http_ok(f"http://127.0.0.1:{INSPECT_PORT}/api/state", timeout=1.5)
        insp_action = ""
        if not insp_ok:
            if do_heal:
                r = start_inspect()
                if r.get("ok"):
                    ready, sec = wait_inspect(12)
                    insp_ok = ready
                    insp_action = (f"已拉起，{sec}s 就绪" if ready
                                   else "已拉起但 12s 内没就绪，下轮再看")
                    if ready:
                        healed.append(f"巡检服务已拉起（{sec}s，pid {r.get('pid')}）")
                        _log({"at": _now(), "event": "inspect_restarted",
                              "pid": r.get("pid"), "sec": sec})
                else:
                    insp_action = f"拉起失败：{r.get('error')}"
            else:
                insp_action = "--no-heal 未拉起"
        checks.append({
            "id": "inspect_alive", "level": "warn", "ok": insp_ok,
            "name": f"系统巡检（:{INSPECT_PORT}）",
            "detail": ("HTTP 200，常驻在跑" if insp_ok else "端口不通")
                      + (f"；{insp_action}" if insp_action else ""),
        })

        # --- 1.6 传输助手 → 手机：媒体推送那条链 ---
        # 端口活着不代表东西能到手：这条链断的时候（2026-11-21 图片全 prepare
        # failed），pocket 探活、页面、「发送成功」三处都是好的，只有手机是空的。
        checks.append(check_transfer_push(cfg))

        # --- 1.7 App 监工：开 / 关 / 闪退 / 卡住 的那本账 ---
        # 它记的是「用户视角发生过什么」，不是服务活不活。丢了不会有人喊，
        # 但下次再问「我昨天是不是重启过 Safari」就又只能靠猜。
        aw = appwatch_state()
        aw_action = ""
        if not aw.get("ok"):
            if do_heal:
                r = start_appwatch(cfg)
                if r.get("ok"):
                    ready, sec = wait_appwatch(10)
                    aw = appwatch_state()
                    aw_action = (f"已拉起，{sec}s 就绪" if ready
                                 else "已拉起但 10s 内还没刷心跳，下轮再看")
                    if ready:
                        healed.append(f"App 监工已拉起（{sec}s，pid {r.get('pid')}）")
                        _log({"at": _now(), "event": "appwatch_restarted",
                              "pid": r.get("pid"), "sec": sec})
                else:
                    aw_action = f"拉起失败：{r.get('error')}"
            else:
                aw_action = "--no-heal 未拉起"
        tally = appwatch_tally(cfg, 24)
        if aw.get("ok"):
            detail = (f"{aw.get('apps') or 0} 个 App 在监 · "
                      f"辅助功能{'可用（能判卡住）' if aw.get('ax') else '未授权（只记开关/闪退）'}")
        else:
            detail = str(aw.get("why") or "没在跑")
        bits = []
        if tally["crash"]:
            bits.append(f"闪退 {tally['crash']}")
        if tally["hang"]:
            bits.append(f"卡住 {tally['hang']}")
        if tally["kill"]:
            bits.append(f"强制退出 {tally['kill']}")
        if bits:
            detail += "；近 24 小时 " + " / ".join(bits)
            if tally["names"]:
                detail += "（" + "、".join(tally["names"][:3]) + "）"
        if aw_action:
            detail += "；" + aw_action
        checks.append({
            "id": "appwatch_alive", "level": "warn", "ok": bool(aw.get("ok")),
            "name": "App 监工（开/关/闪退/卡住）", "detail": detail,
        })

        # --- 2. Node 版本与启动脚本 ---
        ndetail = (f"brew node {'.'.join(map(str, node['brew'])) if node['brew'] else '缺失'}"
                   f" ≥ {'.'.join(map(str, MIN_NODE))}：{'通过' if node['brew_ok'] else '不通过'}")
        if not node["path_ok"]:
            if not node["path"]:
                ndetail += "；当前环境 PATH 里没有 node（launchd 下常见）—— 启动脚本已显式写死 brew node 绝对路径"
            else:
                ndetail += (f"；PATH 里的 node {'.'.join(map(str, node['path']))}"
                            f"（{node['path_exe']}）低于门槛 —— 启动脚本已显式绕开")
        if not node["launcher_ok"]:
            ndetail += "；⚠ 启动脚本未显式指向 brew node，下次启动可能复发版本问题"
        checks.append({
            "id": "node_version", "level": "critical",
            "ok": node["brew_ok"] and node["launcher_ok"],
            "name": "Node 版本与启动脚本", "detail": ndetail,
        })

        # --- 3. 陈旧锁 ---
        if stale:
            lock_detail = f"{len(locks)} 个锁文件，仍有 {len(stale)} 个 pid 已死"
        elif stale_pids:
            lock_detail = f"已清理 {len(stale_pids)} 个孤儿锁（pids {stale_pids}）；现 {len(locks)} 个锁文件均有效"
        else:
            lock_detail = f"{len(locks)} 个锁文件，无陈旧锁"
        checks.append({
            "id": "stale_locks", "level": "warn", "ok": not stale,
            "name": "陈旧锁", "detail": lock_detail,
        })

        # --- 4. LaunchAgent 托管（只读，不尝试 bootstrap）---
        ld = "未加载（当前沙箱无法 bootstrap；心跳兜底，下次登录自动加载）"
        if launchd["loaded"]:
            ld = "已由 launchd 托管（running）"
        elif not launchd["plist_exists"]:
            ld = "⚠ plist 缺失，登录自启不会生效"
        elif not launchd["lint_ok"]:
            ld = "⚠ plist 格式非法，登录自启不会生效"
        checks.append({
            "id": "launchd", "level": "info",
            "ok": launchd["plist_exists"] and launchd["lint_ok"],
            "name": "LaunchAgent 常驻配置", "detail": ld,
        })

        # --- 5. Ollama（4B 依赖，只读）---
        checks.append({
            "id": "ollama", "level": "warn", "ok": ollama_ok,
            "name": "Ollama（11434）",
            "detail": "在线" if ollama_ok else "不在线（摘要/简报会降级，不影响 gateway）",
        })

        # --- 6. 微信主会话体积（过大 -> 备份 + 截断式压缩）---
        sess = check_session()
        sess_kb = sess["bytes"] / 1024
        sess_action = ""
        if sess["db_exists"] and sess["bytes"] > SESSION_WARN_BYTES:
            cooling = time.time() - STATE["last_compact_at"] < SESSION_COMPACT_COOLDOWN
            if do_heal and online and not cooling:
                bk = backup_agent_db()
                r = compact_session(SESSION_KEEP_LINES)
                if r["ok"]:
                    STATE["last_compact_at"] = time.time()
                    after = check_session()
                    sess = after
                    sess_kb = sess["bytes"] / 1024
                    sess_action = (f"已备份 {Path(bk).name} 并截断到最近 {r['kept']} 条"
                                   f"（{sess['events']} 条 / {sess_kb:.0f}KB）")
                    healed.append(f"主会话过大，已截断压缩：{sess_action}")
                    _log({"at": _now(), "event": "session_compacted",
                          "backup": bk, "kept": r["kept"], "bytes_after": sess["bytes"]})
                else:
                    sess_action = f"截断失败：{r.get('error')}，需人工 /new 或 /compact"
            elif cooling:
                sess_action = "体积超阈值，但距上次截断不足 10 分钟，下轮再看"
            elif not online:
                sess_action = "体积超阈值，gateway 不在线无法压缩"
            else:
                sess_action = "体积超阈值（--no-heal 未处理）"
        sess_ok = not (sess["db_exists"] and sess["bytes"] > SESSION_WARN_BYTES)
        checks.append({
            "id": "session_size", "level": "warn", "ok": sess_ok,
            "name": "微信主会话体积",
            "detail": (f"无会话库" if not sess["db_exists"]
                       else f"{sess['events']} 条 / {sess_kb:.0f}KB"
                            f"（阈值 {SESSION_WARN_BYTES // 1024}KB，4B 窗口有限）")
                      + (f"；{sess_action}" if sess_action else ""),
        })

        ok = all(x["ok"] for x in checks if x["level"] == "critical")
        result = {
            "at": _now(), "ok": ok, "checks": checks, "healed": healed,
            "needs_attention": STATE["needs_attention"],
            "restart_count": STATE["restart_count"],
            "interval_sec": c["interval"],
        }
        STATE["last_at"] = result["at"]
        STATE["last"] = result
        _save_state(result)
        _log({"at": result["at"], "event": "heartbeat", "ok": ok,
              "online": online, "stale": len(stale), "healed": healed,
              "needs_attention": STATE["needs_attention"]})
        _log_disk(disk_sample())
        return result
    finally:
        STATE["running"] = False
        _LOCK.release()


def scheduler_loop(cfg: dict, on_done=None) -> None:
    """和 dedupe/trends 同构的 daemon 循环；启动立即跑一次，异常不死。"""
    c = cfg_of(cfg)
    while True:
        try:
            out = run_once(cfg)
            if on_done:
                on_done(out)
        except Exception as e:               # 定时线程不能因为一次异常就退出
            if on_done:
                on_done({"ok": False, "error": str(e)})
        time.sleep(c["interval"])


# --------------------------------------------------------------------------
def _main() -> None:
    ap = argparse.ArgumentParser(description="系统心跳：纯代码服务自检 + 自愈")
    ap.add_argument("--once", action="store_true", help="只跑一次（默认常驻循环）")
    ap.add_argument("--json", action="store_true", help="输出 JSON")
    ap.add_argument("--no-heal", action="store_true", help="只检查不自愈")
    args = ap.parse_args()
    cfg = load_config()

    if args.once:
        out = run_once(cfg, heal=not args.no_heal)
        if args.json:
            print(json.dumps(out, ensure_ascii=False, indent=2))
        else:
            print(f"心跳自检 {out['at']}  {'正常' if out['ok'] else '有问题'}")
            for x in out.get("checks", []):
                mark = "OK  " if x["ok"] else "FAIL"
                print(f"  [{mark}] {x['name']}: {x['detail']}")
            for h in out.get("healed", []):
                print(f"  → 自愈: {h}")
            if out.get("error"):
                print(f"  错误: {out['error']}")
        return
    scheduler_loop(cfg)


if __name__ == "__main__":
    _main()
