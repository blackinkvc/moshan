#!/usr/bin/env python3
"""微信链接漏存守卫（**兜底**，正常情况下不会动作）。

2026-11-19 起，链接入站已经改成代码接管：openclaw 插件 pocket-intake 在模型
之前就把消息里的链接全部抽出来排进队列（intake.py），并回一句模板话。
4B 那边也收回了存链接的工具权限。所以这里的定位从「补漏主力」退到「保险丝」：

* 插件没加载、口袋重启、HTTP 超时 —— 这些情况下链接才真的没人接，
  这时守卫扫到就补一次，至少别丢数据；
* 判重靠 `pocket.jsonl` 里的 `link_start` / `article_start`（intake 也写这两个
  事件名，否则守卫会把已经排队的当成漏的，再抓一遍）。

为什么还留着：踩过一次「静默丢数据」—— 用户发三条，只存了一条，另外两条
既不进库也不报错。那是最难查的一类问题，宁可留一层笨办法。
"""
from __future__ import annotations

import json
import os
import re
import signal
import socket
import sqlite3
import subprocess
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path

BASE = Path(__file__).resolve().parent
import paths  # noqa: E402  （账本与状态在可写目录：写 .app 内部会破坏签名）
DATA = paths.base_dir()
REF_DB = Path.home() / ".openclaw" / "openclaw-weixin" / "ref-messages.sqlite"
OUTBOUND_DIR = Path.home() / ".openclaw" / "media" / "outbound"
LOG_PATH = DATA / "pocket.jsonl"
STATE_PATH = DATA / "_守卫状态.json"

INTERVAL = 60          # 多久扫一次
MIN_AGE = 90           # 消息至少这么老才判「漏了」
LOOKBACK = 6 * 3600    # 只回看这么久
KEEP_STATE = 7 * 24 * 3600
KEEP_OUTBOUND = 3600   # 网关手滑下来的出站临时文件留多久

_URL_RE = re.compile(r"https?://[^\s，。、；）)\]】\"'<>]+", re.I)


def _want_article(body: str) -> bool:
    """原话里明确要「文章」才直接走阅读。其余交给内容判定（linkbox 抓完再判）。"""
    t = body or ""
    if any(k in t for k in ("存图", "图集", "存图片")):
        return False
    return any(k in t for k in ("文章", "长文", "正文"))


def _url_key(url: str) -> str:
    """判重键：去掉句末标点，**不转小写**（小红书短链区分大小写）。"""
    return (url or "").rstrip(".,;:!?)]}>）】")


def inbound(since_ts: float) -> list[tuple[str, float]]:
    """最近的微信入站消息 [(正文, 时间戳)]，按时间正序。"""
    if not REF_DB.exists():
        return []
    try:
        con = sqlite3.connect(f"file:{REF_DB}?mode=ro", uri=True, timeout=3)
        try:
            rows = con.execute(
                "select body, created_at from quote_messages "
                "where direction='inbound' and created_at >= ? "
                "order by created_at asc",
                (int(since_ts * 1000),),
            ).fetchall()
        finally:
            con.close()
    except sqlite3.Error:
        return []
    return [(str(b or ""), float(c or 0) / 1000.0) for b, c in rows]


def started_keys() -> set[str]:
    """口袋已经为哪些链接开过任务。

    读 3000 行而不是几百行：心跳每 2 分钟一条，一天光心跳就 700 多行，
    窗口取小了会把「昨晚存过的」当成没存过、重复抓一遍。
    """
    if not LOG_PATH.exists():
        return set()
    try:
        lines = LOG_PATH.read_text(encoding="utf-8").splitlines()[-3000:]
    except OSError:
        return set()
    keys: set[str] = set()
    for ln in lines:
        ln = ln.strip()
        if not ln or '"link_start"' not in ln and '"article_start"' not in ln:
            continue
        try:
            rec = json.loads(ln)
        except ValueError:
            continue
        if rec.get("event") not in ("link_start", "article_start"):
            continue
        # 新记录直接带 url（多链接时 head 只有前半段，靠它认不准）
        u = str(rec.get("url") or "").strip()
        if u.startswith("http"):
            keys.add(_url_key(u))
            continue
        m = _URL_RE.search(str(rec.get("head") or ""))
        if m:
            keys.add(_url_key(m.group(0)))
    return keys


def load_state() -> dict:
    try:
        data = json.loads(STATE_PATH.read_text(encoding="utf-8"))
        return data if isinstance(data, dict) else {}
    except (OSError, ValueError):
        return {}


def save_state(state: dict) -> None:
    now = time.time()
    keys = {k: v for k, v in (state.get("keys") or {}).items()
            if now - float(v or 0) < KEEP_STATE}
    try:
        STATE_PATH.write_text(
            json.dumps({"keys": keys}, ensure_ascii=False, indent=1),
            encoding="utf-8")
    except OSError:
        pass


def _post(base: str, token: str, path: str, text: str, hint: str = "") -> dict:
    req = urllib.request.Request(
        base.rstrip("/") + path,
        data=json.dumps({"text": text, "hint": hint}).encode("utf-8"),
        headers={"Content-Type": "application/json", "x-token": token},
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=20) as r:
        return json.loads(r.read().decode("utf-8", "replace") or "{}")


def sweep(cfg: dict, log=None) -> list[dict]:
    """扫一遍，把漏掉的补上。返回这次补了哪些（空列表＝没事）。"""
    now = time.time()
    handled = started_keys()
    state = load_state()
    seen = state.get("keys") or {}
    base = f"http://127.0.0.1:{int(cfg.get('local_port') or 8323)}"
    token = str(cfg.get("token") or "")
    done: list[dict] = []
    for body, ts in inbound(now - LOOKBACK):
        if now - ts < MIN_AGE:
            continue
        # 一条消息可能连发几个链接：逐个处理，以前只认第一个，后面几个静默丢失
        urls = []
        for m in _URL_RE.finditer(body):
            u = _url_key(m.group(0))
            if u and u not in urls:
                urls.append(u)
        for url in urls:
            if url in handled or url in seen:
                continue
            # 写了「文章」直接走阅读（内容不符它会自己退回存图）；否则先存图，
            # 由 linkbox 抓完按内容判，是文章再转存过去
            path = "/api/article/fetch" if _want_article(body) else "/api/link"
            try:
                _post(base, token, path, url, hint=body[:300])
            except (urllib.error.URLError, OSError, ValueError) as e:
                seen[url] = now          # 记下来，别每分钟重试
                if log:
                    log({"event": "wechat_guard_fail", "url": url,
                         "error": str(e)[:160]})
                continue
            seen[url] = now
            done.append({"url": url, "path": path})
            if log:
                log({"event": "wechat_guard", "url": url,
                     "mode": "文章" if path.endswith("fetch") else "存图"})
    if done or seen != (state.get("keys") or {}):
        save_state({"keys": seen})
    return done


# ---- 微信长轮询心跳（2026-11-20 加的，跟「漏存守卫」共用这个 60 秒循环）----
# 踩过两次同一个坑：网关进程活着、18789 端口通、页面各项都正常，但 getUpdates
# 已经不转了。表现就是「电脑发手机收得到，手机发回来电脑一点动静都没有」——
# 端口探活、巡检、页面全看着健康，实际上入站链路早就断了。
# 唯一能看出来的信号是 sync 游标的年龄：每一轮 getUpdates（35 秒一次，空轮询
# 也会回一个新的 buf）都会回写 accounts/*.sync.json，卡一天不动就是死了。
SYNC_DIR = Path.home() / ".openclaw" / "openclaw-weixin" / "accounts"
GATEWAY_PORT = 18789
SYNC_STALE = 300        # 5 分钟没动即判定卡死（正常 35 秒一轮，容得下 8 轮）
HEAL_COOLDOWN = 600     # 重启一次后 10 分钟内不再动手，免得反复杀
_last_heal = 0.0


def _port_up(port: int) -> bool:
    try:
        with socket.create_connection(("127.0.0.1", port), timeout=0.8):
            return True
    except OSError:
        return False


def _listeners(port: int) -> list[int]:
    try:
        out = subprocess.run(["/usr/sbin/lsof", "-ti", f":{port}", "-sTCP:LISTEN"],
                             capture_output=True, text=True, timeout=6)
    except (OSError, subprocess.SubprocessError):
        return []
    return [int(x) for x in out.stdout.split() if x.strip().isdigit()]


def _sync_age() -> float | None:
    """sync 游标文件的年龄（秒）。没有文件（还没登录）返回 None。"""
    try:
        files = list(SYNC_DIR.glob("*.sync.json"))
        if not files:
            return None
        return time.time() - max(f.stat().st_mtime for f in files)
    except OSError:
        return None


def check_weixin_liveness(log=None) -> None:
    global _last_heal
    age = _sync_age()
    if age is None or age < SYNC_STALE:
        return
    if not _port_up(GATEWAY_PORT):
        return                  # 端口都没了是另一码事，交给巡检把它拉起来
    if time.time() - _last_heal < HEAL_COOLDOWN:
        return
    _last_heal = time.time()
    if log:
        log({"event": "weixin_stall", "sync_age_sec": int(age), "action": "restart_gateway"})
    # 必须先杀再拉：serve-gateway.sh 开头会探端口，通的就 exit 0 跳过。
    killed = []
    for pid in _listeners(GATEWAY_PORT):
        try:
            os.kill(pid, signal.SIGTERM)
            killed.append(pid)
        except OSError:
            pass
    for _ in range(24):         # 等端口真正释放
        if not _port_up(GATEWAY_PORT):
            break
        time.sleep(0.25)
    try:
        import heartbeat
        r = heartbeat.start_gateway()
        ok = bool(r.get("ok"))
        err = r.get("error") or ""
    except Exception as e:      # noqa: BLE001
        ok, err = False, str(e)[:160]
    if log:
        log({"event": "weixin_restart", "pids": killed, "ok": ok, "error": err})


def clean_outbound(log=None) -> int:
    """打扫网关的出站临时文件。

    2026-11-20 起「电脑→手机」能发图了：走的是网关 `send` + base64，网关会把
    字节落到 ~/.openclaw/media/outbound/ 再上传 CDN，**发完不删**。一张图 3MB，
    放几天就是几百兆，所以顺手清掉一小时前的。只删这个目录里的普通文件。
    """
    n = 0
    try:
        if not OUTBOUND_DIR.is_dir():
            return 0
        cut = time.time() - KEEP_OUTBOUND
        for p in OUTBOUND_DIR.iterdir():
            try:
                st = p.stat()
            except OSError:
                continue
            if not p.is_file() or st.st_mtime > cut:
                continue
            try:
                p.unlink()
                n += 1
            except OSError:
                pass
    except OSError:
        return n
    if n and log:
        log({"event": "outbound_clean", "n": n})
    return n


def scheduler_loop(cfg: dict, log=None) -> None:
    """常驻线程。先睡一会儿再开扫：开机时助手自己正处理得欢，别插手。"""
    interval = int((cfg.get("wechat_guard") or {}).get("interval_sec") or INTERVAL)
    time.sleep(30)
    while True:
        try:
            sweep(cfg, log)
        except Exception as e:                       # 守卫自己不能死
            if log:
                log({"event": "wechat_guard_error", "error": str(e)[:200]})
        try:
            clean_outbound(log)
        except Exception as e:                       # noqa: BLE001
            if log:
                log({"event": "outbound_clean_error", "error": str(e)[:160]})
        # 入站链路的心跳：这一条跟补链接无关，但同一个 60 秒节奏正好够用
        try:
            check_weixin_liveness(log)
        except Exception as e:                       # noqa: BLE001
            if log:
                log({"event": "weixin_liveness_error", "error": str(e)[:160]})
        time.sleep(max(20, interval))


def _main() -> int:
    cfg = json.loads((DATA / "config.json").read_text(encoding="utf-8"))
    got = sweep(cfg, log=lambda r: print(json.dumps(r, ensure_ascii=False)))
    print(f"补了 {len(got)} 条" if got else "没有漏的")
    return 0


if __name__ == "__main__":
    raise SystemExit(_main())
