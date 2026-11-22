"""Moshan 启动器 —— 桌面双击后的唯一入口。

做四件事，顺序不能乱：
  1. 建目录骨架（state / library / 工作台 / 收件箱 / 回收站）
  2. 选一个本机明文端口（8323 起，被占就顺延），写回配置，保证浏览器一定打得开
  3. 后台等端口就绪，再唤起浏览器（首启进 /welcome，之后进带 token 的首页）
  4. 同进程启动服务本体

为什么不用 subprocess 起 server.py：PyInstaller 打包后没有独立 python 解释器，
再起一个进程是不可行的；而同进程调用要求 server 的启动逻辑是函数而不是
   if __name__ == "__main__" 块 —— 这正是前面把 main() 抽出来的原因。
"""
from __future__ import annotations

import json
import os
import socket
import sys
import threading
import time
import webbrowser
from pathlib import Path

import bootstrap
import paths
import version

PORT_SCAN_START = 8323
PORT_SCAN_END = 8399
MAIN_SCAN_START = 8321   # HTTPS 主端口：绑 0.0.0.0，被占时要顺延
MAIN_SCAN_END = 8399


def _port_free(host: str, port: int) -> bool:
    """探测端口是否真的可用。

    绝对不要设 SO_REUSEADDR：那会让 bind 在「端口已被别的进程监听」时也返回成功，
    于是我们误判为空闲，把服务绑到别人的端口上（实测踩过：主端口误选 8323，
    和另一个正在跑的服务撞车）。这里要的是「现在能不能独占」，不是「能不能重用」。
    """
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        try:
            s.bind((host, port))
            s.listen(1)
            return True
        except OSError:
            return False


def pick_port(cfg: dict, key: str = "local_port", start: int = PORT_SCAN_START,
              end: int = PORT_SCAN_END, host: str = "127.0.0.1",
              skip: set | None = None) -> int:
    """优先复用配置里的端口；被占则从 start 往后找一个空闲的。

    主端口（https）必须用 host=0.0.0.0 探测，因为它实际就绑在通配地址上 ——
    只探 127.0.0.1 会漏掉「别的进程已占 0.0.0.0:8321」这种最常见的情况。
    skip 里的端口不会入选，避免两个用途抢同一个号。
    """
    reserved = skip or set()
    prefer = int(cfg.get(key) or start)
    if prefer not in reserved and _port_free(host, prefer):
        return prefer
    for p in range(start, end):
        if p in reserved or p == prefer:
            continue
        if _port_free(host, p):
            return p
    return prefer  # 全占满属于极端情况，交给服务自己报错


def _wait_and_open(port: int, url: str, timeout: float = 40.0) -> None:
    """轮询到服务真的能应答再开浏览器，避免打开一个「连接被拒」的白页。

    MOSHAN_NO_BROWSER=1 时不唤起浏览器（用于测试与无头启动）。
    """
    no_browser = os.environ.get("MOSHAN_NO_BROWSER") == "1"
    deadline = time.time() + timeout
    while time.time() < deadline:
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
            s.settimeout(0.5)
            if s.connect_ex(("127.0.0.1", port)) == 0:
                time.sleep(0.35)  # 让路由注册完成
                if no_browser:
                    print(f"[Moshan] 服务就绪（未开浏览器）：{url}")
                    return
                try:
                    webbrowser.open(url)
                except Exception:  # noqa: BLE001 —— 打不开浏览器不算致命
                    pass
                print(f"[Moshan] 已打开 {url}")
                return
        time.sleep(0.4)
    print(f"[Moshan] 等待服务超时，请手动打开 http://127.0.0.1:{port}/")


def _redirect_stdio() -> None:
    """桌面模式没有终端，把 stdout/stderr 落到日志，出问题才有据可查。"""
    if not getattr(sys, "frozen", False):
        return
    log_dir = paths.log_dir()
    log_dir.mkdir(parents=True, exist_ok=True)
    fh = (log_dir / "moshan.log").open("a", buffering=1, encoding="utf-8")
    if sys.stdout is None or not sys.stdout.isatty():
        sys.stdout = fh          # type: ignore[assignment]
    if sys.stderr is None:
        sys.stderr = fh          # type: ignore[assignment]


def main() -> int:
    paths.ensure_layout()
    _redirect_stdio()

    import server  # 延迟导入：server 模块导入时会读配置、建 app，必须在目录就绪之后

    cfg = server.CFG
    keep_plain = int(cfg.get("plain_port") or 8322)
    port = pick_port(cfg, "local_port", PORT_SCAN_START, PORT_SCAN_END, "127.0.0.1",
                     skip={keep_plain})
    main_port = pick_port(cfg, "port", MAIN_SCAN_START, MAIN_SCAN_END, "0.0.0.0",
                          skip={port, keep_plain})

    changed = False
    if cfg.get("local_port") != port:
        cfg["local_port"] = port
        changed = True
    if cfg.get("port") != main_port:
        cfg["port"] = main_port
        changed = True
    if changed:
        try:
            server.CFG_PATH.write_text(json.dumps(cfg, ensure_ascii=False, indent=2), encoding="utf-8")
            os.chmod(server.CFG_PATH, 0o600)
        except OSError:
            pass

    first = bootstrap.is_first_run()
    target = f"http://127.0.0.1:{port}/welcome" if first else f"http://127.0.0.1:{port}/?k={server.TOKEN}"

    print(f"[Moshan] {version.version_line()}  首次启动={first}")
    for k, v in paths.describe().items():
        print(f"[Moshan]   {k}: {v}")

    threading.Thread(target=_wait_and_open, args=(port, target), daemon=True).start()
    return server.main()


if __name__ == "__main__":
    raise SystemExit(main())
