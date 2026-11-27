"""灰色 · 扫码下载（手机端插件页 `/gray`）。

为什么单独成模块而不是直接改 ui.html：
  ui.html 是与上游开发目录**逐字同步**的文件（同 about.py 顶部的说明）。往里加
  DOM 或脚本，下一次同步就被覆盖，且不报错。所以走「服务端注入 + 独立页面」：
  inject() 往首页底部标签栏插一格「灰色」，页面本身是这里独立生成的 /gray。

这一页干两件事（用户 2026-11-25 提的）：
  1. 放二维码 → 填文件夹名 → 开始，调本机 `/Volumes/AI仓库/灰色` 的 CLI 下载网盘
  2. 另一个框：批量选图片（文件名就是最终名字）→ 存进同一个 `输出/<名字>/`

二维码与图片上传都走口袋自己的这条 HTTP 通道（同端口、同令牌），不另开服务；
口令校验与 pocket 一致：X-Token / ?k= / moshan_k cookie 三选一。
"""
from __future__ import annotations

import hmac
import json
import os
import re
import subprocess
import threading
import time
from pathlib import Path
from typing import Optional

from fastapi import FastAPI, HTTPException, Request, UploadFile, File
from fastapi.responses import HTMLResponse

GRAY_DIR = Path(os.environ.get("GRAY_DIR", "/Volumes/AI仓库/灰色")).expanduser()
CLI = GRAY_DIR / "qrget.sh"
OUT_ROOT = GRAY_DIR / "输出"

MAX_LINES = 2000
_TASKS: dict[str, dict] = {}
_QUEUE: list[str] = []            # 排队等跑的任务 id：CLI 单实例 + 共用 profile，只能串行
_ACTIVE: Optional[str] = None
_LOCK = threading.RLock()

_BAD_CHARS = re.compile(r'[\\/:*?"<>|\x00-\x1f]')
_SIZE_UNITS = {"B": 1, "KB": 1024, "MB": 1024 ** 2, "GB": 1024 ** 3}
_RE_TOTAL = re.compile(r"共 (\d+) 个目录 / (\d+) 个文件 / ([\d.]+)\s*(B|KB|MB|GB)")
_RE_FILE = re.compile(r"\[(\d+)/(\d+)\]\s+([\d.]+)%\s+([\d.]+)\s*(B|KB|MB|GB)")
_RE_TS = re.compile(r"^\[\d{2}:\d{2}:\d{2}\]\s*")


def _clean_name(raw: str) -> str:
    """文件夹名：去掉路径分隔符与控制字符，别让它跑出输出目录。"""
    s = _BAD_CHARS.sub("", (raw or "").strip()).strip(". ")
    s = s.replace("..", "")
    return s[:60]


def human(n: int) -> str:
    for unit in ("B", "KB", "MB", "GB"):
        if n < 1024 or unit == "GB":
            return f"{n:.0f} {unit}" if unit == "B" else f"{n:.1f} {unit}"
        n /= 1024.0
    return f"{n:.1f} GB"


def _unique_path(directory: Path, name: str) -> Path:
    dest = directory / name
    if not dest.exists():
        return dest
    stem, suffix = Path(name).stem, Path(name).suffix
    i = 2
    while (directory / f"{stem}-{i}{suffix}").exists():
        i += 1
    return directory / f"{stem}-{i}{suffix}"


def _to_bytes(num: str, unit: str) -> float:
    return float(num) * _SIZE_UNITS.get(unit.upper(), 1)


def _push(t: dict, line: str) -> None:
    t["lines"].append(line)
    if len(t["lines"]) > MAX_LINES:
        del t["lines"][: len(t["lines"]) - MAX_LINES]
    m = re.search(r"保存位置：(.+)$", line)
    if m:
        t["out"] = m.group(1).strip()

    # 进度只认 CLI 这两行：目录列完报总量、每下完一个文件报 [i/N]
    body = _RE_TS.sub("", line)
    p = t["prog"]
    m = _RE_TOTAL.search(body)
    if m:
        p["files_total"] = int(m.group(2))
        p["bytes_total"] = _to_bytes(m.group(3), m.group(4))
        p["phase"] = "download"
        p["t0"] = p["t0"] or time.time()
        return
    m = _RE_FILE.search(body)
    if m:
        p["files_done"] = int(m.group(1))
        p["files_batch"] = int(m.group(2))
        p["bytes_done"] += _to_bytes(m.group(4), m.group(5))
        p["t0"] = p["t0"] or time.time()
        p["phase"] = "download"
        return
    if "保存位置：" in body:
        p["phase"] = "done"


def _snapshot(t: dict) -> dict:
    """把内部进度算成前端要的：百分比、速度、剩余秒数。"""
    p = t["prog"]
    denom = p["files_batch"] or p["files_total"]
    done = p["files_done"]
    pct = (100.0 * done / denom) if denom else 0.0
    if p["phase"] == "done":
        pct = 100.0
    elapsed = (time.time() - p["t0"]) if p["t0"] else 0.0
    speed = int(p["bytes_done"] / elapsed) if elapsed > 1 and p["bytes_done"] else 0
    eta = None
    if p["phase"] == "download" and done > 0 and denom > done and elapsed > 0:
        eta = int(elapsed / done * (denom - done))
    return {
        "phase": p["phase"],
        "pct": round(min(pct, 100.0), 1),
        "files_total": p["files_total"] or denom,
        "files_batch": p["files_batch"] or denom,
        "files_done": done,
        "bytes_total": int(p["bytes_total"]),
        "bytes_done": int(p["bytes_done"]),
        "speed": speed,
        "eta": eta,
        "elapsed": int(elapsed),
    }


def _queue_position(tid: str) -> int:
    """队列里的第几位（把正在跑的那单也算进去）；不在队列里返回 0。"""
    with _LOCK:
        if tid not in _QUEUE:
            return 0
        return _QUEUE.index(tid) + (2 if _ACTIVE else 1)


def _launch_next() -> None:
    """空闲就取队头起进程。所有启动都走这里，保证同一时刻只有一个 CLI 在跑。"""
    global _ACTIVE
    with _LOCK:
        if _ACTIVE:
            return
        task = None
        while _QUEUE:
            tid = _QUEUE.pop(0)
            cand = _TASKS.get(tid)
            if not cand or cand.get("cancelled"):
                continue
            task = cand
            break
        if not task:
            return
        _ACTIVE = task["id"]
        task["state"] = "running"
        task["running"] = True
        task["started"] = time.time()
        task["prog"]["phase"] = "list"
        task["prog"]["t0"] = 0.0
    try:
        proc = subprocess.Popen(
            task["cmd"], cwd=str(GRAY_DIR), env=task["env"], text=True, bufsize=1,
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
        )
    except OSError as exc:
        task["running"] = False
        task["state"] = "error"
        task["code"] = -1
        _push(task, f"启动失败：{exc}")
        with _LOCK:
            if _ACTIVE == task["id"]:
                _ACTIVE = None
        _launch_next()
        return
    task["proc"] = proc
    threading.Thread(target=_consume, args=(task, proc), daemon=True).start()


def _consume(t: dict, proc: subprocess.Popen) -> None:
    """逐行读 CLI 输出；@@QR 那一行是给界面用的登录二维码，不混进日志。"""
    try:
        for raw in proc.stdout or []:
            line = raw.rstrip("\n")
            if not line.strip():
                continue
            if line.startswith("@@QR "):
                try:
                    t["qr"] = json.loads(line[5:])
                except ValueError:
                    pass
                continue
            _push(t, line)
    except Exception as exc:                                  # noqa: BLE001
        _push(t, f"[读取输出失败] {exc}")
    finally:
        code = proc.wait()
        t["code"] = code
        t["running"] = False
        if t.get("stopped"):
            t["state"] = "cancelled"
        else:
            t["state"] = "done" if code == 0 else "error"
        if t["prog"]["phase"] != "done":
            t["prog"]["phase"] = "done" if (code == 0 and not t.get("stopped")) else "error"
        if code != 0 and not any("失败" in x for x in t["lines"][-5:]):
            _push(t, f"任务结束（退出码 {code}）")
        with _LOCK:
            global _ACTIVE
            if _ACTIVE == t["id"]:
                _ACTIVE = None
        _launch_next()


def register(app: FastAPI, cfg: dict, token: str) -> None:
    qr_dir = Path(cfg.get("inbox_dir") or (Path.home() / "Documents")) / "_灰色二维码"
    max_mb = int(cfg.get("max_upload_mb", 500) or 500)
    limit = max_mb * 1024 * 1024

    def auth(request: Request) -> None:
        k = (request.query_params.get("k") or request.headers.get("x-token")
             or request.cookies.get("moshan_k") or "")
        if not k or not hmac.compare_digest(k, token):
            raise HTTPException(401, "令牌无效")

    def _save_upload(file: UploadFile, dest: Path) -> int:
        written = 0
        try:
            with dest.open("wb") as out:
                while True:
                    chunk = file.file.read(1 << 20)
                    if not chunk:
                        break
                    written += len(chunk)
                    if written > limit:
                        raise ValueError(f"超过 {max_mb}MB 上限")
                    out.write(chunk)
        except Exception:
            dest.unlink(missing_ok=True)
            raise
        return written

    def _image_name(file: UploadFile) -> str:
        raw = Path(file.filename or f"图片-{int(time.time())}").name
        return _BAD_CHARS.sub("", raw).strip() or f"图片-{int(time.time())}"

    # ------------------------------------------------------------------ 状态
    @app.get("/api/gray/status")
    def gray_status(request: Request) -> dict:
        auth(request)
        ready = CLI.exists() and os.access(CLI, os.X_OK)
        active = _TASKS.get(_ACTIVE or "")
        return {
            "ok": ready and OUT_ROOT.exists(),
            "root": str(GRAY_DIR),
            "out": str(OUT_ROOT),
            "running": bool(active and active.get("running")),
            "task": active["id"] if active and active.get("running") else "",
            "queue": len(_QUEUE),
            "mounted": GRAY_DIR.exists(),
        }

    # ------------------------------------------------------------ 二维码上传
    @app.post("/api/gray/qr")
    def gray_qr(request: Request, file: UploadFile = File(...)) -> dict:
        auth(request)
        if not GRAY_DIR.exists():
            raise HTTPException(400, "移动硬盘没挂上（找不到 灰色 目录）")
        qr_dir.mkdir(parents=True, exist_ok=True)
        safe = _image_name(file)
        dest = _unique_path(qr_dir, safe)
        try:
            n = _save_upload(file, dest)
        except ValueError as exc:
            raise HTTPException(400, str(exc)) from exc
        return {"path": str(dest), "name": dest.name, "size": human(n), "bytes": n}

    # -------------------------------------------------------------- 起下载
    @app.post("/api/gray/run")
    async def gray_run(request: Request) -> dict:
        """进队列就返回：忙的时候不挡人，接着填下一单即可，轮到了自动开跑。"""
        auth(request)
        body = await request.json()
        name = _clean_name(body.get("name", ""))
        if not name:
            raise HTTPException(400, "请先填文件夹名")
        target = str(body.get("path") or "").strip()
        if not target:
            raise HTTPException(400, "还没放二维码")
        qr_size = 0
        if target.startswith(("http://", "https://")):
            arg = target
        else:
            p = Path(target)
            if not p.is_file():
                raise HTTPException(400, "二维码没收到：文件不见了，请重新放一次")
            arg = str(p)
            qr_size = p.stat().st_size
        if not CLI.exists():
            raise HTTPException(400, "找不到灰色 CLI（移动硬盘没挂上？）")

        env = dict(os.environ)
        env["QRGET_GUI"] = "1"              # 让 CLI 把二维码送出来，不另开浏览器窗口
        env["FORCE_COLOR"] = "0"
        cmd = ["/bin/zsh", str(CLI), arg, "--name", name]
        lim = body.get("limit")             # 自检用：只下前 n 个，页面不放这个入口
        if isinstance(lim, int) and 0 < lim <= 20:
            cmd += ["--limit", str(lim)]

        with _LOCK:
            tid = f"{int(time.time() * 1000):x}{os.urandom(2).hex()}"
            task = {"id": tid, "name": name, "state": "queued", "running": False,
                    "code": None, "lines": [], "qr": None, "out": "",
                    "started": 0.0, "cmd": cmd, "env": env, "cancelled": False,
                    "prog": {"phase": "queued", "files_total": 0, "files_batch": 0,
                             "files_done": 0, "bytes_total": 0.0, "bytes_done": 0.0,
                             "t0": 0.0}}
            _TASKS[tid] = task
            _QUEUE.append(tid)
            for old in list(_TASKS)[:-6]:                     # 只留最近几个任务的日志
                if old not in _QUEUE and old != _ACTIVE:
                    _TASKS.pop(old, None)
        position = _queue_position(tid)
        _launch_next()
        running = task["state"] == "running"
        return {"id": tid, "name": name, "queued": not running, "position": position,
                "qr_received": True, "qr_size": human(qr_size) if qr_size else "链接"}

    @app.get("/api/gray/task/{tid}")
    def gray_task(tid: str, request: Request) -> dict:
        auth(request)
        t = _TASKS.get(tid)
        if not t:
            raise HTTPException(404, "没有这个任务")
        pos = _queue_position(tid)
        return {"id": tid, "name": t["name"], "lines": t["lines"], "qr": t["qr"],
                "out": t["out"], "running": t["running"], "code": t["code"],
                "state": t["state"], "queue_pos": pos,
                "progress": _snapshot(t)}

    @app.post("/api/gray/stop")
    def gray_stop(request: Request) -> dict:
        auth(request)
        stopped_queued = 0
        with _LOCK:
            for tid in list(_QUEUE):
                t = _TASKS.get(tid)
                if t:
                    t["cancelled"] = True
                    t["state"] = "cancelled"
                    _push(t, "已取消排队")
                _QUEUE.remove(tid)
                stopped_queued += 1
            t = _TASKS.get(_ACTIVE or "")
        if not t or not t.get("running"):
            return {"ok": True, "stopped": False, "cancelled": stopped_queued}
        proc = t.get("proc")
        if proc and proc.poll() is None:
            proc.terminate()
            try:
                proc.wait(timeout=6)
            except subprocess.TimeoutExpired:
                proc.kill()
        t["running"] = False
        t["stopped"] = True
        _push(t, "已手动停止（已下好的文件保留，重跑同一条会增量补齐）")
        return {"ok": True, "stopped": True, "cancelled": stopped_queued}

    # ----------------------------------------------------------- 批量存图
    @app.post("/api/gray/images")
    def gray_images(request: Request, files: list[UploadFile] = File(...)) -> dict:
        auth(request)
        name = _clean_name(request.query_params.get("name", ""))
        if not name:
            raise HTTPException(400, "请先填文件夹名")
        if not GRAY_DIR.exists():
            raise HTTPException(400, "移动硬盘没挂上（找不到 灰色 目录）")
        target = OUT_ROOT / name
        target.mkdir(parents=True, exist_ok=True)
        saved, failed = [], []
        batch = 0
        for f in files:
            ctype = (f.content_type or "").lower()
            if ctype and not ctype.startswith("image/"):
                failed.append({"name": f.filename or "", "error": "只收图片"})
                continue
            dest = _unique_path(target, _image_name(f))
            try:
                n = _save_upload(f, dest)
                if batch + n > 2 * 1024 * 1024 * 1024:      # 单次两 G，防止误选整段视频
                    dest.unlink(missing_ok=True)
                    raise ValueError("这一批太大，请分几次传")
                batch += n
                saved.append({"name": dest.name, "size": human(n), "path": str(dest)})
            except ValueError as exc:
                failed.append({"name": f.filename or "", "error": str(exc)})
            except OSError as exc:
                failed.append({"name": f.filename or "", "error": str(exc)[:120]})
        return {"saved": saved, "failed": failed, "dir": str(target)}

    # ---------------------------------------------------------------- 页面
    @app.get("/gray", response_class=HTMLResponse)
    def gray_page() -> HTMLResponse:
        return HTMLResponse(_page_html(), headers={"Cache-Control": "no-store"})


# ---------------------------------------------------------------- 入口注入
# 底部标签栏在 ui.html 里是 `<nav>` 五颗按钮，ui.html 自己的脚本在 body 末尾就
# `querySelectorAll('nav button')` 绑过事件了；注入脚本（拼在 </body> 前）后插入，
# 不会被它接管，所以这颗按钮自己绑 click，只负责跳 /gray。
_TAB = """
<style>
  #mosh-gray-tab svg{width:21px;height:21px}
  #mosh-gray-tab .gb{width:21px;height:21px;border-radius:6px;display:flex;
    align-items:center;justify-content:center;color:#f1ede6;font-size:10px;
    font-weight:600;letter-spacing:.5px;background:linear-gradient(160deg,#5a5a55,#2c2c29);
    box-shadow:inset 0 1px 0 rgba(255,255,255,.16)}
</style>
<script>
(function(){
  function mount(){
    var nav = document.querySelector('nav');
    if (!nav || document.getElementById('mosh-gray-tab')) return;
    var K = new URLSearchParams(location.search).get('k')
          || localStorage.getItem('pocket_token') || '';
    var b = document.createElement('button');
    b.id = 'mosh-gray-tab';
    b.setAttribute('aria-label', '灰色');
    b.innerHTML = '<span class="gb">灰</span><span>灰色</span>';
    b.addEventListener('click', function(){
      location.href = '/gray' + (K ? '?k=' + encodeURIComponent(K) : '');
    });
    nav.appendChild(b);
  }
  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', mount);
  else mount();
  setTimeout(mount, 600); setTimeout(mount, 2000);
})();
</script>
"""


def inject(html: str) -> str:
    """2026-11-26 起不再往口袋首页底栏插格。

    底栏已经收成三格（文件 / 存 / 对话），再被插件插一格就是第四格、风格也不一路
    （深色方块 vs 线性图标）。入口改挂在顶栏「更多」抽屉里，跳到 /gray 时自己带令牌。
    保留这个函数是为了 server.py 里那处调用点不用改。
    """
    return html


def _page_html() -> str:
    return _PAGE


_PAGE = r"""<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1,viewport-fit=cover">
<meta name="apple-mobile-web-app-capable" content="yes">
<meta name="apple-mobile-web-app-title" content="灰色">
<meta name="theme-color" content="#f6f6f4">
<meta name="referrer" content="no-referrer">
<title>灰色</title>
<style>
:root{
  --bg:#f6f6f4; --card:#fff; --line:#e4e4e0; --text:#1a1a18; --muted:#75756e;
  --accent:#1d6b52; --accent-soft:#e6f2ec; --warn:#b4570f; --warn-soft:#fbeee2;
}
@media (prefers-color-scheme:dark){
  :root{--bg:#131313;--card:#1d1d1b;--line:#2e2e2b;--text:#ececea;--muted:#9a9a92;
        --accent:#6fd0a8;--accent-soft:#1b2c25;--warn:#e08b4a;--warn-soft:#33241a;}
}
*{box-sizing:border-box;-webkit-tap-highlight-color:transparent}
/* .picked 之类设了 display:flex，会盖掉 hidden 的 display:none，这里统一压住 */
[hidden]{display:none!important}
body{margin:0;background:var(--bg);color:var(--text);min-height:100dvh;
  font:15px/1.5 -apple-system,BlinkMacSystemFont,"PingFang SC","Helvetica Neue",sans-serif}
header{position:sticky;top:0;z-index:20;display:flex;align-items:center;gap:10px;
  padding:calc(env(safe-area-inset-top) + 10px) 14px 10px;background:var(--bg);
  border-bottom:1px solid var(--line)}
.back{color:var(--muted);text-decoration:none;font-size:24px;line-height:1;padding:0 4px}
.brand{display:flex;align-items:center;gap:8px;font-weight:600;font-size:16px;letter-spacing:.4px}
.logo{width:26px;height:26px;border-radius:8px;display:flex;align-items:center;justify-content:center;
  font-size:13px;color:#f1ede6;background:linear-gradient(160deg,#5a5a55,#262624);
  box-shadow:inset 0 1px 0 rgba(255,255,255,.18)}
.st{margin-left:auto;font-size:12px;color:var(--muted)}
main{padding:12px 12px calc(env(safe-area-inset-bottom) + 28px)}
.card{background:var(--card);border:1px solid var(--line);border-radius:14px;
  padding:13px;margin-bottom:11px}
.ch{display:flex;align-items:center;gap:7px;font-size:13.5px;font-weight:600;margin-bottom:10px}
.ch .n{width:19px;height:19px;border-radius:50%;background:var(--accent-soft);color:var(--accent);
  font-size:11px;font-weight:600;display:flex;align-items:center;justify-content:center;font-style:normal}
.ch .sp{flex:1}
.field{width:100%;border:1px solid var(--line);background:var(--bg);color:var(--text);
  border-radius:11px;padding:11px 12px;font-size:15px;outline:none;font-family:inherit}
.field:focus{border-color:var(--accent)}
.hint{font-size:11.5px;color:var(--muted);line-height:1.5;margin-top:7px}
button{font-family:inherit;font-size:14px;border:none;border-radius:11px;cursor:pointer;
  background:none;color:inherit;padding:0}
button:disabled{opacity:.4}
.primary{background:var(--accent);color:#fff;padding:11px 16px;font-weight:500}
.ghost{background:var(--accent-soft);color:var(--accent);padding:11px 16px;font-weight:500}
.mini{font-size:12px;color:var(--accent);padding:4px 8px;background:var(--accent-soft);border-radius:8px}
.wide{display:block;width:100%;margin-top:11px}
.drop{border:1.5px dashed var(--line);border-radius:12px;padding:22px 12px;text-align:center;
  background:var(--bg)}
.drop:active{border-color:var(--accent);background:var(--accent-soft)}
.dmain{font-size:14px}
.dsub{font-size:11.5px;color:var(--muted);margin-top:5px}
.picked{display:flex;align-items:center;gap:10px;padding:9px;border:1px solid var(--line);
  border-radius:12px;background:var(--bg)}
.picked img{width:52px;height:52px;object-fit:contain;border-radius:8px;background:#fff}
.pmeta{flex:1;min-width:0}
.pmeta div{font-size:13px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.psub{font-size:11px;color:var(--muted);margin-top:2px}
.qitem{display:flex;align-items:center;gap:9px;padding:7px 2px;border-bottom:1px solid var(--line);
  font-size:13px}
.qitem:last-child{border-bottom:none}
.qitem .nm{flex:1;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.qitem .sz{font-size:11px;color:var(--muted)}
.qitem .rm{color:var(--muted);font-size:16px;padding:0 4px}
.log{max-height:44vh;overflow:auto;font:11.5px/1.65 ui-monospace,Menlo,monospace;
  color:var(--muted);white-space:pre-wrap;word-break:break-all;background:var(--bg);
  border-radius:9px;padding:9px;margin-top:2px}
.mask{position:fixed;inset:0;z-index:60;display:none;align-items:center;justify-content:center;
  background:rgba(0,0,0,.55);padding:22px}
.mask.show{display:flex}
.qcard{background:var(--card);border-radius:18px;padding:18px;max-width:320px;width:100%;
  text-align:center;box-shadow:0 12px 40px rgba(0,0,0,.3)}
.qcard img{width:100%;max-width:230px;border-radius:10px;background:#fff;display:block;margin:0 auto}
.qcard .qt{font-size:13px;margin-bottom:12px;color:var(--muted)}
.qcard .qf{font-size:12px;color:var(--muted);margin-top:12px;line-height:1.5}
.toast{position:fixed;left:50%;transform:translateX(-50%);bottom:80px;background:var(--text);
  color:var(--bg);padding:9px 16px;border-radius:20px;font-size:13px;opacity:0;
  transition:opacity .2s;pointer-events:none;z-index:70;max-width:82%;text-align:center}
.toast.show{opacity:.94}
.done{color:var(--accent);font-weight:600}
.bad{color:var(--warn)}
/* 二维码状态：收没收到一眼看清 */
.qstate{font-size:12px;margin-top:9px;display:flex;align-items:center;gap:5px;color:var(--muted)}
.qstate.ok{color:var(--accent)}
.qstate.no{color:var(--warn)}
/* 进度：文件数 + 条 + 剩余时间 */
.pwrap{margin-bottom:9px}
.pline{display:flex;justify-content:space-between;gap:8px;font-size:12px;color:var(--muted)}
.pline b{color:var(--text);font-weight:600}
.pbar{height:6px;border-radius:99px;background:var(--line);overflow:hidden;margin-top:7px}
.pbar i{display:block;height:100%;width:0;background:var(--accent);border-radius:99px;
  transition:width .35s ease}
.pbar i.indet{width:32%;animation:indet 1.15s ease-in-out infinite}
@keyframes indet{0%{margin-left:-32%}100%{margin-left:100%}}
.qbadge{font-size:11px;color:var(--warn);background:var(--warn-soft);border-radius:99px;
  padding:2px 8px;font-weight:400}
</style>
</head>
<body>
<header>
  <a class="back" id="back" href="/">‹</a>
  <div class="brand"><span class="logo">灰</span>灰色</div>
  <span class="st" id="st">检查中</span>
</header>
<main>
  <div class="card">
    <div class="ch">文件夹名</div>
    <input id="name" class="field" placeholder="自己填，程序不代填"
           autocomplete="off" autocapitalize="off" autocorrect="off" spellcheck="false">
    <div class="hint" id="outHint">下载的文件和下面那批图片，都会放进 灰色/输出/&lt;名字&gt;/</div>
  </div>

  <div class="card">
    <div class="ch"><i class="n">1</i>二维码 → 下载网盘</div>
    <div class="drop" id="qrDrop">
      <div class="dmain">点这里选二维码图片</div>
      <div class="dsub">截图 / 相册里的都行，微云、阿里云盘通用</div>
    </div>
    <div class="picked" id="qrPicked" hidden>
      <img id="qrThumb" alt="">
      <div class="pmeta"><div id="qrName">--</div><div class="psub" id="qrState"></div></div>
      <button class="mini" id="qrAgain">重选</button>
    </div>
    <input type="file" id="qrInput" accept="image/*" hidden>
    <div class="qstate" id="qrStatus">还没放二维码</div>
    <button class="primary wide" id="runBtn" disabled>开始下载</button>
  </div>

  <div class="card">
    <div class="ch"><i class="n">2</i>一批图片 → 直接存</div>
    <div class="hint" style="margin-top:0">图片的文件名就是最终名字，不改名、不压缩，全存进上面那个文件夹</div>
    <button class="ghost wide" id="imgPick">选图片（可多选）</button>
    <input type="file" id="imgInput" accept="image/*" multiple hidden>
    <div id="imgList"></div>
    <button class="primary wide" id="imgSave" disabled>开始存图</button>
  </div>

  <div class="card" id="logCard" hidden>
    <div class="ch">进度<span class="sp"></span><span class="qbadge" id="qBadge" hidden></span><button class="mini" id="stopBtn" hidden>停止</button></div>
    <div class="pwrap">
      <div class="pline"><span id="pTxt">准备中…</span><span class="pet" id="pEta"></span></div>
      <div class="pbar"><i id="pFill"></i></div>
    </div>
    <div class="log" id="log"></div>
  </div>
</main>

<div class="mask" id="qrMask">
  <div class="qcard">
    <div class="qt" id="qrTip">用微信扫一扫登录</div>
    <img id="qrImg" alt="登录二维码">
    <div class="qf">扫完不用管，会自动接着下载。码过期会自动换新的。</div>
    <button class="mini" id="qrHide" style="margin-top:12px">先收起来</button>
  </div>
</div>
<div class="toast" id="toast"></div>

<script>
(function(){
var $=function(s){return document.querySelector(s);};
var K = new URLSearchParams(location.search).get('k') || localStorage.getItem('pocket_token') || '';
document.getElementById('back').href = '/' + (K ? '?k=' + encodeURIComponent(K) : '');

var nameEl=$('#name'), runBtn=$('#runBtn'), imgSave=$('#imgSave'), stEl=$('#st');
var qrPath='', qrBusy=false, imgs=[], taskId='', timer=null, rendered=0, qrShown=false;

function toast(t){var el=$('#toast');el.textContent=t;el.classList.add('show');
  clearTimeout(el._t);el._t=setTimeout(function(){el.classList.remove('show');},2200);}
function esc(s){return String(s==null?'':s).replace(/[&<>"]/g,function(c){
  return {'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c];});}
function human(n){if(n<1024)return n+' B';if(n<1048576)return (n/1024).toFixed(1)+' KB';
  if(n<1073741824)return (n/1048576).toFixed(1)+' MB';return (n/1073741824).toFixed(1)+' GB';}
function fmtSpeed(b){if(!b)return '';if(b<1024)return b+' B/s';
  if(b<1048576)return Math.round(b/1024)+' KB/s';return (b/1048576).toFixed(1)+' MB/s';}
function fmtLeft(s){if(s==null)return '';if(s<60)return '约剩 '+s+' 秒';
  var m=Math.floor(s/60), r=s%60;return '约剩 '+m+' 分'+(r?(' '+r+' 秒'):'');}

/* 二维码状态：收到没收到，一眼看清 */
function setQrState(txt, cls){
  var el=$('#qrStatus');
  el.textContent = txt;
  el.className = 'qstate' + (cls ? ' ' + cls : '');
}

function refresh(){
  var nm=nameEl.value.trim();
  runBtn.disabled = !(nm && qrPath && !qrBusy);
  imgSave.disabled = !(nm && imgs.length);
  $('#outHint').innerHTML = nm
    ? '会放进 <b>灰色/输出/' + esc(nm) + '/</b>'
    : '下载的文件和下面那批图片，都会放进 灰色/输出/&lt;名字&gt;/';
}
function jurl(p){ return p + (p.indexOf('?')>=0?'&':'?') + 'k=' + encodeURIComponent(K); }
function jget(p){
  return fetch(jurl(p), {headers:{'X-Token':K}}).then(function(r){
    if(r.status===401){toast('令牌无效，回口袋首页重进');throw new Error('令牌无效');}
    if(!r.ok){return r.text().then(function(t){throw new Error(t.slice(0,120));});}
    return r.json();
  });
}
function jpost(p, body){
  return fetch(jurl(p), {method:'POST',headers:{'X-Token':K,'Content-Type':'application/json'},
    body:JSON.stringify(body||{})}).then(function(r){
    if(r.status===401){toast('令牌无效，回口袋首页重进');throw new Error('令牌无效');}
    if(!r.ok){return r.text().then(function(t){
      var m=t; try{m=JSON.parse(t).detail||t;}catch(e){}
      throw new Error(String(m).slice(0,140));});}
    return r.json();
  });
}
function postForm(url, fd, onPct){
  return new Promise(function(res, rej){
    var x=new XMLHttpRequest();
    x.open('POST', url);
    x.setRequestHeader('X-Token', K);
    x.timeout=600000;
    if(onPct && x.upload){x.upload.onprogress=function(e){
      if(e.lengthComputable) onPct(Math.round(e.loaded/e.total*100));};}
    x.onload=function(){
      if(x.status>=200&&x.status<300){try{res(JSON.parse(x.responseText));}catch(e){rej(new Error('响应异常'));}}
      else{var m=x.responseText;try{m=JSON.parse(m).detail||m;}catch(e){}
        rej(new Error(String(m).slice(0,140)||('HTTP '+x.status)));}
    };
    x.onerror=function(){rej(new Error('网络中断'));};
    x.ontimeout=function(){rej(new Error('超时'));};
    x.send(fd);
  });
}

/* ---------------- 名字 ---------------- */
nameEl.addEventListener('input', refresh);

/* ---------------- 二维码 ---------------- */
function pickQr(){
  var f=$('#qrInput').files[0];
  if(!f) return;
  qrBusy=true; refresh();
  $('#qrPicked').hidden=false;
  $('#qrDrop').hidden=true;
  $('#qrName').textContent=f.name;
  $('#qrState').textContent='上传中 0%';
  var rd=new FileReader();
  rd.onload=function(){ $('#qrThumb').src=rd.result; };
  rd.readAsDataURL(f);
  var fd=new FormData(); fd.append('file', f);
  setQrState('正在送到本机 0%…','');
  postForm('/api/gray/qr', fd, function(p){ setQrState('正在送到本机 '+p+'%…',''); })
    .then(function(d){
      qrPath=d.path; qrBusy=false;
      $('#qrState').textContent='已放入 · '+d.size;
      setQrState('✓ 二维码已收到，本机已保存','ok');
      refresh();
    })
    .catch(function(e){
      qrBusy=false; qrPath='';
      $('#qrState').innerHTML='<span class="bad">没传上</span>';
      setQrState('✗ 没收到：'+e.message,'no');
      refresh();
    });
}
$('#qrDrop').addEventListener('click', function(){ $('#qrInput').click(); });
$('#qrAgain').addEventListener('click', function(){ $('#qrInput').click(); });
$('#qrInput').addEventListener('change', pickQr);
['dragover','drop'].forEach(function(t){
  $('#qrDrop').addEventListener(t, function(e){e.preventDefault();});
});
$('#qrDrop').addEventListener('drop', function(e){
  var f=e.dataTransfer.files && e.dataTransfer.files[0];
  if(!f) return;
  var dt=new DataTransfer(); dt.items.add(f);
  $('#qrInput').files=dt.files;
  pickQr();
});

/* ---------------- 图片 ---------------- */
function paintImgs(){
  var box=$('#imgList'); box.innerHTML='';
  imgs.forEach(function(f,i){
    var d=document.createElement('div'); d.className='qitem';
    d.innerHTML='<span class="nm">'+esc(f.name)+'</span><span class="sz">'+human(f.size)+'</span>'
      +'<button class="rm" data-i="'+i+'">×</button>';
    box.appendChild(d);
  });
  box.querySelectorAll('.rm').forEach(function(b){
    b.addEventListener('click', function(){
      imgs.splice(Number(b.dataset.i),1); paintImgs(); refresh();
    });
  });
  refresh();
}
$('#imgPick').addEventListener('click', function(){ $('#imgInput').click(); });
$('#imgInput').addEventListener('change', function(){
  Array.prototype.forEach.call(this.files, function(f){
    var dup=imgs.some(function(x){return x.name===f.name && x.size===f.size;});
    if(!dup) imgs.push(f);
  });
  this.value='';
  paintImgs();
  if(imgs.length){
    toast(nameEl.value.trim()
      ? '已选 '+imgs.length+' 张，点「开始存图」'
      : '已选 '+imgs.length+' 张，先填文件夹名');
  }
});

/* ---------------- 存图 ---------------- */
imgSave.addEventListener('click', function(){
  var nm=nameEl.value.trim();
  if(!nm){toast('先填文件夹名');return;}
  if(!imgs.length){toast('还没选图片');return;}
  imgSave.disabled=true;
  var fd=new FormData();
  imgs.forEach(function(f){ fd.append('files', f); });
  $('#logCard').hidden=false;
  var st=document.createElement('div');
  st.className='log'; st.id='upLog'; st.textContent='上传中…';
  $('#logCard').appendChild(st);
  postForm('/api/gray/images?name='+encodeURIComponent(nm), fd, function(p){
    st.textContent='上传中 '+p+'%';
  }).then(function(d){
    st.textContent = '存到 ' + d.dir + '\n'
      + (d.saved||[]).map(function(s){return '· '+s.name+'  '+s.size;}).join('\n')
      + (d.failed&&d.failed.length ? '\n失败：'+d.failed.map(function(x){return x.name+'（'+x.error+'）';}).join('、') : '');
    st.classList.add('done');
    toast('已存 '+(d.saved||[]).length+' 张');
    imgs=[]; paintImgs();
  }).catch(function(e){
    st.textContent='失败：'+e.message; st.classList.add('bad');
  }).then(function(){ setTimeout(function(){ var x=$('#upLog'); if(x) x.remove(); }, 60000); refresh(); });
});

/* ---------------- 下载任务 ---------------- */
function showQr(q){
  var m=$('#qrMask');
  if(q && q.state==='wait' && q.img){
    if($('#qrImg').getAttribute('src')!==q.img) $('#qrImg').src=q.img;
    if(q.tip) $('#qrTip').textContent=q.tip;
    if(!qrShown){ qrShown=true; m.classList.add('show'); }
    return;
  }
  if(!qrShown) return;
  qrShown=false; m.classList.remove('show');
  if(q && q.state==='ok'){ setSt('登录成功，继续下载','done'); }
}
function setSt(t, cls){ stEl.textContent=t; stEl.className='st '+(cls||''); }
$('#qrHide').addEventListener('click', function(){
  qrShown=false; $('#qrMask').classList.remove('show');
});

function paintProgress(t){
  var pr=t.progress||{}, fill=$('#pFill'), txt=$('#pTxt'), eta=$('#pEta'), badge=$('#qBadge');
  var qpos=t.queue_pos||0;
  if(qpos>0){ badge.hidden=false; badge.textContent='排队第 '+qpos+' 位'; } else { badge.hidden=true; }
  if(t.state==='queued'){
    txt.textContent = qpos>1 ? '排队中，前面还有 '+(qpos-1)+' 单' : '排队中，马上开始';
    eta.textContent=''; fill.classList.remove('indet'); fill.style.width='0%';
    return;
  }
  if(t.state==='cancelled'){ txt.textContent='已取消'; eta.textContent=''; return; }
  if(t.state==='running' && !pr.files_total){
    txt.textContent='正在读取目录…'; eta.textContent='';
    fill.classList.add('indet');
    return;
  }
  fill.classList.remove('indet');
  if(t.state==='error'){
    txt.textContent='失败';
    eta.textContent='已完成 '+(pr.files_done||0)+' 个';
    return;
  }
  if(pr.phase==='done' || pr.pct>=100){
    txt.innerHTML='全部完成 · 共 <b>'+(pr.files_total||pr.files_done||0)+'</b> 个文件';
    eta.textContent=pr.bytes_total?human(pr.bytes_total):'';
    fill.style.width='100%';
    return;
  }
  var pct=pr.pct||0;
  var batch=pr.files_batch||pr.files_total||0;
  var all=pr.files_total||batch;
  var label='已完成 <b>'+(pr.files_done||0)+'</b> / <b>'+batch+'</b> 个文件';
  if(all>batch) label+='（全量 '+all+'）';
  txt.innerHTML=label+' · '+pct.toFixed(1)+'%';
  eta.textContent=[pr.speed?fmtSpeed(pr.speed):'', fmtLeft(pr.eta)].filter(Boolean).join(' · ');
  fill.style.width=pct+'%';
}

function paintTask(t){
  var box=$('#log');
  if(t.lines.length < rendered){ box.innerHTML=''; rendered=0; }
  for(var i=rendered;i<t.lines.length;i++){
    var div=document.createElement('div');
    div.textContent=t.lines[i];
    box.appendChild(div);
  }
  if(t.lines.length>rendered){ rendered=t.lines.length; box.scrollTop=box.scrollHeight; }
  showQr(t.qr);
  paintProgress(t);
  if(t.state==='queued'){
    setSt('排队中',''); return;
  }
  if(t.running){ setSt('下载中…',''); $('#stopBtn').hidden=false; return; }
  clearInterval(timer); timer=null;
  $('#stopBtn').hidden=true;
  if(t.state==='done' && t.code===0){
    setSt('完成','done');
    if(t.out){ var d=document.createElement('div'); d.className='done';
      d.textContent='保存位置：'+t.out; box.appendChild(d); }
  }else if(t.state==='cancelled'){
    setSt('已停止','');
  }else{
    setSt('失败','bad');
  }
}
function poll(){
  if(!taskId) return;
  jget('/api/gray/task/'+taskId).then(paintTask).catch(function(){});
}
runBtn.addEventListener('click', function(){
  var nm=nameEl.value.trim();
  if(!nm){toast('先填文件夹名');return;}
  if(!qrPath){toast('先放二维码');return;}
  runBtn.disabled=true;
  var sentPath=qrPath;
  jpost('/api/gray/run', {path:sentPath, name:nm}).then(function(d){
    taskId=d.id; rendered=0;
    $('#logCard').hidden=false;
    $('#log').innerHTML='';
    $('#stopBtn').hidden=false;
    $('#pFill').style.width='0%';
    $('#pTxt').textContent='准备中…'; $('#pEta').textContent='';
    // 二维码确认：后端已经拿到文件才回来这一句
    setQrState(d.qr_received ? '✓ 二维码已收到，已交给下载器' : '✗ 二维码没收到',
               d.qr_received ? 'ok' : 'no');
    if(d.queued){
      setSt('排队中','');
      toast('已排队，第 '+d.position+' 位；可以继续填下一单');
    }else{
      setSt('下载中…','');
    }
    // 交出去就把输入清空：随时可以接着填第二个任务
    qrPath=''; qrBusy=false;
    $('#qrPicked').hidden=true; $('#qrDrop').hidden=false;
    $('#qrThumb').removeAttribute('src'); $('#qrInput').value='';
    $('#qrState').textContent='';
    refresh();
    clearInterval(timer);
    timer=setInterval(poll, 1200);
    poll();
  }).catch(function(e){
    setQrState('✗ 没收到：'+e.message,'no');
    toast(e.message); refresh();
  });
});
$('#stopBtn').addEventListener('click', function(){
  if(!taskId) return;
  jpost('/api/gray/stop', {}).then(function(){ toast('已请求停止'); refresh(); })
    .catch(function(e){ toast(e.message); });
});

/* ---------------- 启动 ---------------- */
jget('/api/gray/status').then(function(s){
  if(!s.mounted){ setSt('移动硬盘未挂载','bad'); return; }
  if(!s.ok){ setSt('灰色不可用','bad'); return; }
  setSt('就绪','done');
  if(s.running && s.task){                    // 手机上重新打开时接上还在跑的任务
    taskId=s.task; rendered=0;
    $('#logCard').hidden=false; $('#stopBtn').hidden=false;
    setSt('下载中…'); timer=setInterval(poll,1200); poll();
    if(s.queue) toast('队列里还有 '+s.queue+' 单');
  }
}).catch(function(e){ setSt('连接失败','bad'); });
refresh();
})();
</script>
</body>
</html>
"""
