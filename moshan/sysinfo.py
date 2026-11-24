"""这台机器现在怎么样：内存、负载、CPU、开机多久、进程占了多少。

为什么不复用现成的两份：
  · hostwork.py 是「本机工作」卡的账房 —— 要读进程库、算 token、扫目录，重；
    而且里面几处 os.statvfs 在 Windows 上直接 AttributeError，EVO 上实测
    /api/overview 就炸在这儿（2026-11-22）。
  · metrics.py 是给趋势图用的采样器，每次落一行进快照；Windows 上内存那条
    分支还是空的，取了也是 0。
隔网看一台机器的内存，只要几个数，自己量一遍最快，也不依赖第三方包：
有 psutil 就用（准、跨平台），没有就按系统各写一段。

返回形状固定，前端不必写 if：
    {"ok": True, "at": "2026-11-22 23:50:01",
     "mem": {"pct": 78.8, "used_gb": 12.6, "total_gb": 16.0, "avail_gb": 3.4},
     "cpu": {"pct": 68.3, "cores": 10, "load": [10.9, 9.7, 6.9]},
     "uptime_h": 167.3, "procs": 512, "rss_gb": 6.9, "os": "posix"}
取不到的项是 None，不是 0 —— 0% 看起来像「很闲」，None 才是「不知道」。
"""
from __future__ import annotations

import os
import re
import subprocess
import sys
import time

GB = float(1 << 30)

# Windows 上起控制台程序别弹黑框。这模块刻意不依赖 paths（要能单独跑），
# 所以那点常量就地写一份；POSIX 下是 0，等于不带任何开关。
_NO_WIN = getattr(subprocess, "CREATE_NO_WINDOW", 0) if os.name == "nt" else 0


def _ps():
    try:
        import psutil  # type: ignore
        return psutil
    except Exception:  # noqa: BLE001  —— 没装就当没有，别让整个接口挂掉
        return None


def _run(argv: list[str], timeout: float = 4.0) -> str:
    try:
        p = subprocess.run(argv, capture_output=True, text=True, timeout=timeout,
                           creationflags=_NO_WIN)
        return p.stdout or ""
    except Exception:  # noqa: BLE001
        return ""


def _round(v, n: int = 1):
    return None if v is None else round(float(v), n)


# ------------------------------------------------------------------ 内存
def _mem() -> dict:
    ps = _ps()
    if ps is not None:
        try:
            vm = ps.virtual_memory()
            total = float(vm.total)
            avail = float(getattr(vm, "available", None) or vm.free or 0)
            used = max(0.0, total - avail)
            return {"pct": _round(used * 100 / total if total else 0),
                    "used_gb": _round(used / GB),
                    "total_gb": _round(total / GB),
                    "avail_gb": _round(avail / GB)}
        except Exception:  # noqa: BLE001
            pass

    if os.name == "nt":
        # 没 psutil 的 Windows：GlobalMemoryStatusEx，系统自带，不用装东西
        try:
            import ctypes

            class _MEM(ctypes.Structure):
                _fields_ = [("dwLength", ctypes.c_ulong),
                            ("dwMemoryLoad", ctypes.c_ulong),
                            ("ullTotalPhys", ctypes.c_ulonglong),
                            ("ullAvailPhys", ctypes.c_ulonglong),
                            ("ullTotalPageFile", ctypes.c_ulonglong),
                            ("ullAvailPageFile", ctypes.c_ulonglong),
                            ("ullTotalVirtual", ctypes.c_ulonglong),
                            ("ullAvailVirtual", ctypes.c_ulonglong),
                            ("ullAvailExtendedVirtual", ctypes.c_ulonglong)]

            m = _MEM()
            m.dwLength = ctypes.sizeof(_MEM)
            if ctypes.windll.kernel32.GlobalMemoryStatusEx(ctypes.byref(m)):
                total = float(m.ullTotalPhys)
                avail = float(m.ullAvailPhys)
                used = max(0.0, total - avail)
                return {"pct": _round(used * 100 / total if total else 0),
                        "used_gb": _round(used / GB),
                        "total_gb": _round(total / GB),
                        "avail_gb": _round(avail / GB)}
        except Exception:  # noqa: BLE001
            pass
        return {}

    if sys.platform == "darwin":
        total = 0
        m = re.search(r"(\d+)", _run(["/usr/sbin/sysctl", "-n", "hw.memsize"]))
        if m:
            total = int(m.group(1))
        vmout = _run(["/usr/bin/vm_stat"])
        page = 4096
        pm = re.search(r"page size of (\d+) bytes", vmout)
        if pm:
            page = int(pm.group(1))
        free = inact = 0
        for key, slot in (("Pages free", "free"), ("Pages inactive", "inact")):
            km = re.search(key + r":\s+(\d+)", vmout)
            if km:
                if slot == "free":
                    free = int(km.group(1))
                else:
                    inact = int(km.group(1))
        if total:
            avail = (free + inact) * page
            used = max(0.0, total - avail)
            return {"pct": _round(used * 100 / total),
                    "used_gb": _round(used / GB),
                    "total_gb": _round(total / GB),
                    "avail_gb": _round(avail / GB)}
        return {}

    # Linux：/proc/meminfo 就够，不用起子进程
    try:
        kv = {}
        with open("/proc/meminfo", encoding="utf-8") as f:
            for line in f:
                k, _, v = line.partition(":")
                kv[k.strip()] = int((v.strip().split() or ["0"])[0]) * 1024
        total = float(kv.get("MemTotal") or 0)
        avail = float(kv.get("MemAvailable") or kv.get("MemFree") or 0)
        if total:
            used = max(0.0, total - avail)
            return {"pct": _round(used * 100 / total),
                    "used_gb": _round(used / GB),
                    "total_gb": _round(total / GB),
                    "avail_gb": _round(avail / GB)}
    except OSError:
        pass
    return {}


# ------------------------------------------------------------------ CPU
def _cpu() -> dict:
    ps = _ps()
    out: dict = {"pct": None, "cores": os.cpu_count() or None, "load": None}
    try:
        out["load"] = [round(x, 2) for x in os.getloadavg()]
    except (AttributeError, OSError):
        # Windows 没有 loadavg，这是系统的事实，不是没取到
        pass

    if ps is not None:
        # 自己采两次算差值，不用 cpu_percent(interval=…)：
        # 它在本进程里第一次调用无论如何都返回 0.0（没有上一次的基线），
        # 而中台是长驻进程，重启后第一眼看到 CPU 0% 会以为接口坏了。
        try:
            t1 = ps.cpu_times()
            time.sleep(0.12)
            t2 = ps.cpu_times()

            def _vals(t):
                # user/system/idle 各平台都有，iowait 等有就算、没有就跳过
                keys = [k for k in ("user", "system", "idle", "iowait", "nice",
                                    "irq", "softirq", "dpc", "interrupt")
                        if getattr(t, k, None) is not None]
                return {k: float(getattr(t, k)) for k in keys}, keys

            v1, keys = _vals(t1)
            v2, _ = _vals(t2)
            delta = {k: max(0.0, v2[k] - v1[k]) for k in keys}
            tot = sum(delta.values())
            idle = delta.get("idle", 0.0)
            if tot > 0:
                out["pct"] = _round(max(0.0, min(100.0, (tot - idle) * 100 / tot)))
            return out
        except Exception:  # noqa: BLE001
            pass

    if os.name == "nt":
        # GetSystemTimes 差值：0.15 秒两次，够看个大概，不装任何东西
        try:
            import ctypes
            from ctypes import wintypes

            k32 = ctypes.windll.kernel32

            def _snap():
                idle, kern, user = (wintypes.FILETIME(), wintypes.FILETIME(),
                                    wintypes.FILETIME())
                k32.GetSystemTimes(ctypes.byref(idle), ctypes.byref(kern),
                                   ctypes.byref(user))
                def q(ft):
                    return (ft.dwHighDateTime << 32) | ft.dwLowDateTime
                return q(idle), q(kern), q(user)

            i0, k0, u0 = _snap()
            time.sleep(0.15)
            i1, k1, u1 = _snap()
            di, dk, du = i1 - i0, k1 - k0, u1 - u0
            busy = dk + du - di          # kern 里含 idle，减掉才是忙的时间
            tot = dk + du
            if tot > 0:
                out["pct"] = _round(max(0.0, min(100.0, busy * 100 / tot)))
        except Exception:  # noqa: BLE001
            pass
    return out


# --------------------------------------------------------------- 开机多久
def _uptime_h():
    ps = _ps()
    if ps is not None:
        try:
            return _round((time.time() - ps.boot_time()) / 3600)
        except Exception:  # noqa: BLE001
            pass
    if os.name == "nt":
        try:
            import ctypes
            return _round(ctypes.windll.kernel32.GetTickCount64() / 3600000)
        except Exception:  # noqa: BLE001
            pass
    try:
        with open("/proc/uptime", encoding="utf-8") as f:
            return _round(float(f.read().split()[0]) / 3600)
    except (OSError, ValueError, IndexError):
        pass
    m = re.search(r"kern.boottime:\s*\{ sec = (\d+)", _run(["/usr/sbin/sysctl", "-n", "kern.boottime"]))
    if m:
        return _round((time.time() - int(m.group(1))) / 3600)
    return None


# ------------------------------------------------- 进程数与进程占的内存合计
_RSS_CACHE = {"at": 0.0, "val": None}
RSS_TTL = 8.0


def _procs():
    """进程数 + 合计 RSS。psutil 逐个读要 100ms 上下，缓存 8 秒 ——
    页面 30 秒才刷一次，这个数没必要每次都重新数一遍。"""
    ps = _ps()
    if ps is None:
        return None, None
    now = time.time()
    if _RSS_CACHE["val"] is not None and now - _RSS_CACHE["at"] < RSS_TTL:
        return _RSS_CACHE["val"]
    n = rss = None
    try:
        n = len(ps.pids())
        tot = 0
        for p in ps.process_iter(["memory_info"]):
            try:
                mi = p.info.get("memory_info")
                if mi is not None:
                    tot += int(mi.rss)
            except Exception:  # noqa: BLE001  —— 进程随时可能消失，跳过就是
                continue
        rss = _round(tot / GB)
    except Exception:  # noqa: BLE001
        pass
    val = (n, rss)
    _RSS_CACHE.update({"at": now, "val": val})
    return val


def sample() -> dict:
    """量一遍。整套跑下来 Mac 上 ~10ms（psutil 都在内存里），Windows 无 psutil
    时多一次 0.15 秒的 CPU 采样。"""
    mem = _mem()
    cpu = _cpu()
    n, rss = _procs()
    return {
        "ok": True,
        "at": time.strftime("%Y-%m-%d %H:%M:%S"),
        "mem": mem,
        "cpu": cpu,
        "uptime_h": _uptime_h(),
        "procs": n,
        "rss_gb": rss,
        "os": "win" if os.name == "nt" else ("mac" if sys.platform == "darwin" else "posix"),
    }


if __name__ == "__main__":       # 直接跑一遍看看，别等到页面上才发现少个数
    import json
    print(json.dumps(sample(), ensure_ascii=False, indent=2))
