#!/usr/bin/env python3
"""本机工作 —— 这台机器到底在忙什么。

2026-11-20：不再自己起服务（原 8775）。采集线程由口袋进程拉起
（`pocket/server.py::_start_hostwork()`），接口挂在同一台 8323 上，
页面是工作台的 `?v=host` + 完整页 `/hostwork`。单独跑仍然可以：
`python hostwork.py --serve`（只是工作台已经不需要它了）。

和工作台里那张「本机工作」卡片不是一回事：那卡片只列「谁在什么时候干了什么」，
这一页回答的是「谁把 CPU / 内存 / 时间 / token 花掉了」。

三路数据，各管一段：

  机器    10-系统记录/机器/快照.jsonl（metrics.py 每 2 分钟一笔，留 7 天）
          + 自己采的即时值（磁盘 / 内存 / 负载 / 已运行）
  进程    本进程每 10 秒采一次，按**进程名**合并（Electron 一个应用开十几个 helper）
          用 libproc 读（proc_listallpids + proc_pidinfo），**不 fork ps**：
          沙箱里 /bin/ps 会被拒，libproc 是纯 syscall，launchd / 双击 / 沙箱都能跑；
          顺带还能拿到累计 CPU 时间（"到底烧了多少秒"），这是 ps 的 %cpu 给不了的
  会话    ~/.workbuddy/projects/<项目>/<会话>.jsonl（WorkBuddy 自己写的流水）
          里面有 providerData.usage（输入 / 输出 / 缓存命中 token）和每次 function_call
          的工具名 —— MCP / Skill / Agent(专家) 全从工具名和参数里摘

落盘（都在 ~/Documents/AI项目/_主机/）：

  折算.json     价格与点数换算（可手改，页面只读它）
  会话缓存.json  每个会话文件解析到哪个字节，只增量读新追加的行（447MB 全量一次太慢）
  进程.jsonl    每 5 分钟一笔「前 15 个吃 CPU 的进程」，重启后历史还在

启动：python hostwork.py --serve --no-open
"""
from __future__ import annotations

import argparse
import ctypes
import hashlib
import json
import math
import os
import platform
import plistlib
import re
import shutil
import sqlite3
import struct
import subprocess
import sys
import threading
import time
import urllib.parse
import webbrowser
from collections import Counter, defaultdict, deque
from datetime import date, datetime, timedelta
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import trae_work                      # TRAE 取数：命令执行 / 进程证据 / OCR 补录
import cherry_usage                   # Cherry Studio 的用量账本（百炼 / 硅基流动，真金白银）
import billing                        # 计费账号：直接问供应商要余额与账单，给价表估算对账

BASE = Path(__file__).resolve().parent
HOME = Path.home()
UI_FILE = BASE / "hostwork_ui.html"

DEFAULT_PORT = 8775
# 数据根不再写死：CUNTU_ROOT 环境变量 → ~/.cuntu/config.json → ~/Documents/AI项目。
# 换台电脑装（或把数据放外置盘）只需改一处，见 paths.py。
import paths                                                  # noqa: E402
AI_ROOT = paths.data_root()
SYS_DIR = AI_ROOT / "10-系统记录"
SNAP_PATH = SYS_DIR / "机器" / "快照.jsonl"
DAILY_PATH = SYS_DIR / "使用报告" / "_数据" / "日聚合.jsonl"

OWN_DIR = paths.ensure(AI_ROOT / "_本机工作")
CACHE_PATH = OWN_DIR / "缓存" / "会话.json"
PROC_PATH = OWN_DIR / "进程.jsonl"
PRICE_PATH = OWN_DIR / "折算.json"

WB_ROOT = HOME / ".workbuddy" / "projects"
POCKET_LOG = (HOME / ".trae-cn" / "assistant" / "2026-11-11" / "local-ai-workspace"
              / "work" / "local-ai-workspace" / "pocket" / "pocket.jsonl")

# 自报账（别的 agent 自己写进来的 token）：Trae 的账本库加密、Cherry 只记次数，
# 抓不到就只能让它自己报。文件格式与读写规则全在 pocket/agentbus.py，这里只取
# 统计结果 —— 路径按 pocket 实际安装位置推，没装就把这块当空。
try:
    sys.path.insert(0, str(POCKET_LOG.parent))
    import agentbus                                        # noqa: E402
except Exception:                                          # noqa: BLE001
    agentbus = None

GB = 1073741824.0


# ---------------------------------------------------------------- 小工具

def _run(argv: list[str], timeout: float = 4) -> str:
    try:
        r = subprocess.run(argv, capture_output=True, text=True, timeout=timeout)
        return r.stdout or ""
    except (OSError, subprocess.SubprocessError):
        return ""


def _json_read(path: Path, default):
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return default


def _json_write(path: Path, obj) -> None:
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp = path.with_suffix(path.suffix + ".tmp")
        tmp.write_text(json.dumps(obj, ensure_ascii=False), encoding="utf-8")
        tmp.replace(path)
    except OSError:
        pass


def human_bytes(n: float) -> str:
    for unit in ("B", "KB", "MB", "GB", "TB"):
        if abs(n) < 1024 or unit == "TB":
            return f"{n:.0f} {unit}" if unit in ("B", "KB") else f"{n:.1f} {unit}"
        n /= 1024
    return f"{n:.1f} TB"


# ---------------------------------------------------------------- 机器指标

def _disk() -> dict:
    # statvfs 是 Unix 专有。Windows 上它不是「抛 OSError」而是**根本没有这个属性**，
    # 所以只 catch OSError 会漏 —— 一漏整份 /api/overview 直接降级，资源卡、
    # 「谁在干活」、节奏账全变成空表。这个坑在 EVO 上就是这么现形的。
    target = str(SYS_DIR if SYS_DIR.exists() else HOME)
    try:
        if hasattr(os, "statvfs"):
            st = os.statvfs(target)
            total = st.f_blocks * st.f_frsize
            free = st.f_bavail * st.f_frsize
        else:
            u = shutil.disk_usage(target)
            total, free = u.total, u.free
    except (OSError, AttributeError):
        return {"disk_pct": 0.0, "free_gb": 0.0, "total_gb": 0.0, "used_gb": 0.0}
    used = total - free
    return {"disk_pct": round(used * 100 / total, 1) if total else 0.0,
            "free_gb": round(free / GB, 1), "total_gb": round(total / GB, 1),
            "used_gb": round(used / GB, 1)}


_EXTRA_DISK_CACHE: dict[str, object] = {"at": 0.0, "list": []}


def _extra_disks(force: bool = False) -> list[dict]:
    """`/Volumes` 下真正可推出的盘（移动 SSD / U 盘），不是 APFS 系统卷。

    只看 statvfs 会把 Time Machine、xarts 这些虚拟卷也算进来，所以再问一次
    `diskutil info -plist`：Ejectable 为真才算外挂。
    缓存只压 8 秒（页面 25 秒刷一次，等于每次都真探一次），这样拔掉/弹出后
    下一轮就没了、插回去下一轮又回来；`force=1` 用于页面点「刷新」当场重探。
    """
    now = time.time()
    if not force and now - float(_EXTRA_DISK_CACHE["at"] or 0) < 8:
        return list(_EXTRA_DISK_CACHE["list"] or [])
    root = Path("/Volumes")
    out: list[dict] = []
    try:
        mounts = [p for p in root.iterdir() if p.is_dir() and not p.name.startswith(".")]
    except OSError:
        return []
    for p in mounts:
        try:
            info = plistlib.loads(subprocess.run(
                ["/usr/sbin/diskutil", "info", "-plist", str(p)],
                capture_output=True, timeout=3).stdout)
        except (OSError, ValueError, subprocess.SubprocessError):
            continue
        if not info or not info.get("Ejectable"):
            continue
        try:
            st = os.statvfs(str(p))
        except OSError:
            continue
        total = st.f_blocks * st.f_frsize
        free = st.f_bavail * st.f_frsize
        if total <= 0:
            continue
        used = total - free
        out.append({"name": str(info.get("VolumeName") or p.name),
                    "mount": str(info.get("MountPoint") or p),
                    "device": str(info.get("DeviceNode") or ""),
                    "disk_pct": round(used * 100 / total, 1),
                    "total_gb": round(total / GB, 1),
                    "free_gb": round(free / GB, 1),
                    "used_gb": round(used / GB, 1)})
    out.sort(key=lambda x: -x["disk_pct"])
    _EXTRA_DISK_CACHE.update(at=now, list=out)
    return out


# 网络速率要跨两次请求差分：把「累计字节 + 时刻」留在进程内，下次 overview 来算 B/s。
# 只看 <Link#N> 那一行（每个接口的汇总），跳过 lo0 回环，免得本机流量也算进去。
_NET_LAST: dict[str, float] = {"at": 0.0, "in": 0.0, "out": 0.0}


def _net() -> dict:
    """实时上下行速率（B/s）：netstat -ib 的累计字节做差分。

    光看累计值画不出「此刻多快」，所以留一份上次采样。第一次没有基线就只报
    累计、速率给 0；间隔小于 1 秒（两页同时刷）就不更新基线，避免除出尖峰。
    """
    now = time.time()
    bin_, bout = 0.0, 0.0
    try:
        out = subprocess.run(["/usr/sbin/netstat", "-ib"], capture_output=True,
                             timeout=3).stdout.decode("utf-8", "replace")
        for line in out.splitlines()[1:]:
            f = line.split()
            if len(f) < 10 or "<Link#" not in f[2] or f[0].startswith("lo"):
                continue
            try:
                bin_ += float(f[6]); bout += float(f[9])
            except (ValueError, IndexError):
                continue
    except (OSError, subprocess.SubprocessError):
        return {"in_bps": 0.0, "out_bps": 0.0, "in_total": 0.0, "out_total": 0.0}

    dt = now - _NET_LAST["at"]
    if dt >= 1 and _NET_LAST["at"]:
        r_in = max(0.0, (bin_ - _NET_LAST["in"]) / dt)
        r_out = max(0.0, (bout - _NET_LAST["out"]) / dt)
    else:
        r_in = r_out = 0.0
    if dt >= 1:
        _NET_LAST.update(at=now, **{"in": bin_, "out": bout})
    return {"in_bps": round(r_in, 1), "out_bps": round(r_out, 1),
            "in_total": round(bin_, 1), "out_total": round(bout, 1)}


def _power() -> dict:
    """功耗：走 ioreg 的 AppleSmartBattery.PowerTelemetryData（毫瓦）。

    Mac mini / Studio 这类没电池也照样在报 SystemPowerIn，正好当整机功耗用；
    有电池的笔记本再补上电量百分比和是否充电。取不到就全 0，不假装有数。
    """
    out = {"watts": 0.0, "battery_pct": None, "charging": None, "has_battery": False}
    try:
        raw = subprocess.run(["/usr/sbin/ioreg", "-a", "-r", "-c", "AppleSmartBattery",
                              "-d", "1"], capture_output=True, timeout=3).stdout
        arr = plistlib.loads(raw)
        o = arr[0] if isinstance(arr, list) and arr else (arr if isinstance(arr, dict) else {})
        tel = o.get("PowerTelemetryData") or {}
        mw = tel.get("SystemPowerIn") or tel.get("SystemLoad") or 0
        out["watts"] = round(float(mw) / 1000, 1)
        if bool(o.get("BatteryInstalled")):
            mx = float(o.get("MaxCapacity") or 0)
            cur = float(o.get("CurrentCapacity") or 0)
            out["has_battery"] = True
            out["battery_pct"] = round(cur * 100 / mx, 1) if mx else None
            out["charging"] = bool(o.get("IsCharging"))
    except (OSError, ValueError, plistlib.InvalidFileException,
            subprocess.SubprocessError, TypeError):
        pass
    return out


def _gpu() -> dict:
    """显存占用：ioreg IOGPU 的 PerformanceStatistics，Apple 芯片是统一内存。

    'In use system memory' 是 GPU 此刻真正在用的字节数，'Alloc system memory'
    是已分配上限；两者都按 GiB 报。顺带把 GPU 利用率带上，光看显存不知道忙不忙。
    """
    out = {"used_gb": 0.0, "alloc_gb": 0.0, "util_pct": 0.0}
    try:
        raw = subprocess.run(["/usr/sbin/ioreg", "-a", "-r", "-c", "IOGPU",
                              "-d", "1"], capture_output=True, timeout=3).stdout
        arr = plistlib.loads(raw)
        o = arr[0] if isinstance(arr, list) and arr else (arr if isinstance(arr, dict) else {})
        st = o.get("PerformanceStatistics") or {}
        out["used_gb"] = round(float(st.get("In use system memory") or 0) / GB, 2)
        out["alloc_gb"] = round(float(st.get("Alloc system memory") or 0) / GB, 2)
        out["util_pct"] = float(st.get("Device Utilization %") or 0)
    except (OSError, ValueError, plistlib.InvalidFileException,
            subprocess.SubprocessError, TypeError):
        pass
    return out


def _mem() -> dict:
    """口径与 metrics.py / syscheck.py 一致：active + wired + compressor。

    Windows 上没有 vm_stat/sysctl，交给 sysinfo（那边按平台各写了一条：
    有 psutil 走 psutil，没有就走 GlobalMemoryStatusEx）。不借这一步的话，
    Windows 上内存会一直报 0 GB —— 不崩，但比空还糟：看着像真值。
    """
    if os.name == "nt":
        try:
            import sysinfo
            m = sysinfo.sample().get("mem") or {}
            return {"mem_pct": m.get("pct") or 0.0, "mem_gb": m.get("used_gb") or 0.0,
                    "mem_total_gb": m.get("total_gb") or 0.0}
        except Exception:                              # noqa: BLE001
            return {"mem_pct": 0.0, "mem_gb": 0.0, "mem_total_gb": 0.0}
    out = _run(["/usr/bin/vm_stat"])
    m = re.search(r"page size of (\d+) bytes", out)
    page = int(m.group(1)) if m else 4096
    pages: dict[str, int] = {}
    for line in out.splitlines():
        mm = re.match(r"(.+?):\s+(\d+)\.", line)
        if mm:
            pages[mm.group(1).strip()] = int(mm.group(2))
    used = (pages.get("Pages active", 0) + pages.get("Pages wired down", 0)
            + pages.get("Pages occupied by compressor", 0)) * page
    mt = re.search(r"(\d+)", _run(["/usr/sbin/sysctl", "-n", "hw.memsize"]))
    total = int(mt.group(1)) if mt else 0
    return {"mem_pct": round(used * 100 / total, 1) if total else 0.0,
            "mem_gb": round(used / GB, 1),
            "mem_total_gb": round(total / GB, 1) if total else 0.0}


def _uptime_h() -> float:
    if os.name == "nt":                                # 没有 kern.boottime
        try:
            import sysinfo
            return float(sysinfo.sample().get("uptime_h") or 0.0)
        except Exception:                              # noqa: BLE001
            return 0.0
    bt = _run(["/usr/sbin/sysctl", "-n", "kern.boottime"])
    mb = re.search(r"sec = (\d+)", bt)
    return round(max(0, time.time() - int(mb.group(1))) / 3600, 1) if mb else 0.0


# ---------------------------------------------------------------- 机器的身份证
#
# 三段各管一件事，都带缓存 —— 这些数据「一天变不了一次」，但采样线程 10 秒一轮，
# 每轮去 fork system_profiler 是纯浪费：
#
#   spec()      这台是什么机器：型号 / 芯片 / 架构 / 内存 / 核心构成 / 显示器
#   software()  装了哪些软件：/Applications + ~/Applications + Homebrew
#   units()     装了哪些常驻服务：LaunchAgents / LaunchDaemons 里的每个 plist，带运行状态
#
# 型号名、GPU 核数、显示器这几样 sysctl 给不了，只能问 system_profiler（0.4 秒，
# 所以进 15 分钟缓存，绝不进 10 秒的采样循环）。

_SPEC = {"at": 0.0, "data": None}
_SPEC_TTL = 900
_SW = {"at": 0.0, "data": None}
_SW_TTL = 300
_UNITS = {"at": 0.0, "data": None}
_UNITS_TTL = 60

# 芯片规格表：**只放 sysctl 拿不到的东西**（内存带宽 / 制程 / NPU 核数），
# 核心数与缓存一律以 sysctl 实测为准 —— 表写错了也不能盖掉真值。
# 带宽取 Apple 官方参数页的基础档（M3 Max / M4 Max 高配更高，页面标注「官方标称」）。
_SOC = [
    ("m4 max", 546, 3, 16), ("m4 pro", 273, 3, 16), ("m4", 120, 3, 16),
    ("m3 max", 400, 3, 16), ("m3 pro", 150, 3, 16), ("m3", 100, 3, 16),
    ("m2 ultra", 800, 5, 32), ("m2 max", 400, 5, 16),
    ("m2 pro", 200, 5, 16), ("m2", 100, 5, 16),
    ("m1 ultra", 800, 5, 32), ("m1 max", 400, 5, 16),
    ("m1 pro", 200, 5, 16), ("m1", 68, 5, 16),
]


def _soc_of(chip: str) -> dict:
    """按芯片名查表。表的顺序是长名在前，'m4 pro' 先命中，不会掉进 'm4' 那档。"""
    k = (chip or "").lower()
    for name, bw, nm, npu in _SOC:
        if name in k:
            return {"bw_gbs": bw, "nm": nm, "npu_cores": npu}
    return {}


def _sysctl(key: str) -> str:
    return _run(["/usr/sbin/sysctl", "-n", key]).strip()


def _profiler() -> dict:
    """型号名 / 机型号 / GPU 核数 / Metal / 显示器 —— 一次 system_profiler 全给。"""
    raw = _run(["/usr/sbin/system_profiler", "SPHardwareDataType",
                "SPDisplaysDataType", "SPNVMeDataType"], timeout=12)
    out: dict = {}
    for key, pat in (("model_name", r"Model Name:\s*(.+)"),
                     ("model_number", r"Model Number:\s*(.+)"),
                     ("fw", r"System Firmware Version:\s*(.+)"),
                     ("metal", r"Metal Support:\s*(.+)"),
                     ("cores_txt", r"Total Number of Cores:\s*(\d+[^\n]*)")):
        m = re.search(pat, raw)
        if m:
            out[key] = m.group(1).strip()
    # CPU 段和 GPU 段都有「Total Number of Cores」，GPU 那个一定在 Type: GPU 之后
    g = re.search(r"Type:\s*GPU[\s\S]{0,400}?Total Number of Cores:\s*(\d+)", raw)
    if g:
        out["gpu_cores"] = int(g.group(1))
    # 内置盘型号：SPNVMeDataType 里「Model: APPLE SSD AP0256Z」。
    # 注意别用搜到的第一个 'Model' —— Hardware 段里有 Model Name / Model Identifier，
    # 这里必须锚定行首只有 Model: 的那一行。
    m = re.search(r"^\s+Model:\s*(.+)$", raw, re.M)
    if m:
        out["ssd"] = m.group(1).strip()
    # 显示器：进 Displays: 段后，缩进 8 的名字行 + 缩进 10 的三行属性
    displays, cur, inside = [], None, False
    for ln in raw.splitlines():
        s = ln.strip()
        if s == "Displays:":
            inside, cur = True, None
            continue
        if not inside or not s:
            continue
        ind = len(ln) - len(ln.lstrip())
        if ind <= 6:
            inside = False
            continue
        if ind == 8 and s.endswith(":") and not s.startswith(("Resolution", "UI Looks")):
            cur = {"name": s[:-1]}
            displays.append(cur)
        elif cur is not None:
            if s.startswith("Resolution:"):
                cur["res"] = s.split(":", 1)[1].strip().split(" (")[0]
            elif s.startswith("UI Looks like:"):
                cur["ui"] = s.split(":", 1)[1].strip()
            elif s.startswith("Main Display:"):
                cur["main"] = s.split(":", 1)[1].strip().lower() == "yes"
    out["displays"] = displays
    return out


# ---------------------------------------------------------------- Windows 探针
#
# macOS 那套 sysctl / system_profiler / launchd 在 Windows 上一条都不存在，
# 于是「机器规格」整张卡是空的：型号「未知机型」、芯片「—」、0 个应用、
# 0 个常驻服务（2026-11-25 在 EVO 上实测）。这不是能力差，是取值没写。
#
# 只用系统自带的东西，不引第三方：
#   · 机型 / 芯片 / 显卡 / 系统  ← PowerShell Get-CimInstance（Win32_* 那套 WMI）
#   · 已装软件                    ← 注册表 Uninstall 三处（无需子进程，毫秒级）
#   · 常驻服务                    ← psutil.win_service_iter（本来就为进程采样装了）
#
# 每样都进 15 分钟缓存：这些数据一天变不了一次，不该跟着 10 秒的采样循环跑。

_WIN_INFO = {"at": 0.0, "data": None}
_WIN_INFO_TTL = 900

# 一次取全：CIM 查询本身 0.2 秒，起进程 0.6 秒，分四次问就是四次启动开销。
_WIN_CIM_PS = (
    "$ErrorActionPreference='SilentlyContinue';"
    "$cs=Get-CimInstance Win32_ComputerSystem;"
    "$cp=Get-CimInstance Win32_Processor|Select-Object -First 1;"
    "$os=Get-CimInstance Win32_OperatingSystem;"
    "$gp=Get-CimInstance Win32_VideoController|Select-Object -First 1;"
    "$bios=Get-CimInstance Win32_BIOS;"
    "@{model=$cs.Model;vendor=$cs.Manufacturer;family=$cs.SystemFamily;"
    "cpu=$cp.Name;cores=$cp.NumberOfCores;threads=$cp.NumberOfLogicalProcessors;"
    "l2=$cp.L2CacheSize;l3=$cp.L3CacheSize;osname=$os.Caption;osver=$os.Version;"
    "osbuild=$os.BuildNumber;gpu=$gp.Name;bios=$bios.SMBIOSBIOSVersion}"
    "|ConvertTo-Json -Compress"
)


def _win_info() -> dict:
    """Windows 的机型/芯片/系统事实。取不到就回空 dict，调用方各自兜底。"""
    now = time.time()
    if _WIN_INFO["data"] is not None and now - _WIN_INFO["at"] < _WIN_INFO_TTL:
        return _WIN_INFO["data"]
    raw = _run(["powershell", "-NoProfile", "-NonInteractive", "-Command",
                _WIN_CIM_PS], timeout=25)
    d: dict = {}
    s = (raw or "").strip()
    if s.startswith("{"):
        try:
            got = json.loads(s)
            d = got if isinstance(got, dict) else {}
        except ValueError:
            d = {}
    _WIN_INFO.update(at=now, data=d)
    return d


def _specs_win(now: float) -> dict:
    """把 WMI 的事实翻成 macOS 那份 spec 的形状 —— 前端一行都不用改。"""
    info = _win_info()
    arch = platform.machine() or ""
    isa = {"AMD64": "x64", "X86_64": "x64", "ARM64": "ARM64"}.get(arch.upper(), arch or "—")
    mem_b = 0
    try:
        import psutil
        mem_b = int(psutil.virtual_memory().total)
    except Exception:                                  # noqa: BLE001
        # 没 psutil 时别报 0 GB —— 那看着像真值。sysinfo 在 Windows 走
        # GlobalMemoryStatusEx（系统自带），这条兜底不依赖任何第三方包。
        try:
            import sysinfo
            mem_b = int(float((sysinfo.sample().get("mem") or {}).get("total_gb") or 0) * GB)
        except Exception:                              # noqa: BLE001
            mem_b = 0
    cores = int(info.get("cores") or 0)
    threads = int(info.get("threads") or 0) or (os.cpu_count() or 0)
    l2 = int(info.get("l2") or 0)
    l3 = int(info.get("l3") or 0)
    cpu_name = str(info.get("cpu") or "").strip()
    if not cpu_name:
        cpu_name = os.environ.get("PROCESSOR_IDENTIFIER") or "—"
    os_name = str(info.get("osname") or "Windows").strip()
    # Caption 形如「Microsoft Windows 11 专业版」；前端会拼成「系统 Windows 11 …」，
    # 所以把「Microsoft 」和系统名重复的部分削掉
    os_name = re.sub(r"^Microsoft\s+", "", os_name)
    for pref in ("Windows 11", "Windows 10", "Windows Server"):
        if os_name.startswith(pref):
            os_name = "Windows"
            break
    disk = _disk()
    return {
        "at": round(now, 1),
        "os": {"name": os_name, "ver": str(info.get("osver") or ""),
               "build": str(info.get("osbuild") or ""), "kernel": "NT"},
        "model": {"id": str(info.get("model") or ""),
                  "name": str(info.get("family") or info.get("model") or ""),
                  "number": str(info.get("vendor") or ""),
                  "board": "", "soc": "", "fw": str(info.get("bios") or "")},
        "chip": {"name": cpu_name, "arch": arch or "—", "isa": isa},
        "cpu": {"cores": threads, "perf": cores or None, "eff": None,
                "l2_mb": round((l3 or l2) / 1024) or None},
        "gpu": {"cores": None, "metal": str(info.get("gpu") or "")},
        "mem": {"total_gb": round(mem_b / GB, 1) if mem_b else 0, "type": "内存"},
        "disk": {"total_gb": disk.get("total_gb"), "free_gb": disk.get("free_gb"),
                 "ssd": ""},
        "displays": [],
    }


def specs(fresh: bool = False) -> dict:
    now = time.time()
    if not fresh and _SPEC["data"] and now - _SPEC["at"] < _SPEC_TTL:
        return _SPEC["data"]
    if os.name == "nt":
        data = _specs_win(now)
        _SPEC.update(at=now, data=data)
        return data
    prof = _profiler()
    chip = _sysctl("machdep.cpu.brand_string") or "—"
    arch = _sysctl("hw.machine") or "—"
    kv = _sysctl("kern.version")
    soc_code = re.search(r"RELEASE_ARM64_(\w+)", kv)
    num = lambda k: int(re.sub(r"\D", "", _sysctl(k)) or 0)   # noqa: E731
    mem_b = num("hw.memsize")
    disk = _disk()
    data = {
        "at": round(now, 1),
        "os": {"name": "macOS", "ver": _sysctl("kern.osproductversion"),
               "build": _sysctl("kern.osversion"), "kernel": _sysctl("kern.osrelease")},
        "model": {"id": _sysctl("hw.model"), "name": prof.get("model_name") or "",
                  "number": prof.get("model_number") or "",
                  "board": _sysctl("hw.targettype"),
                  "soc": soc_code.group(1) if soc_code else "",
                  "fw": prof.get("fw") or ""},
        "chip": {"name": chip, "arch": arch, "isa": "ARM64" if arch == "arm64" else arch.upper(),
                 **_soc_of(chip)},
        "cpu": {"cores": num("hw.ncpu"), "perf": num("hw.perflevel0.physicalcpu"),
                "eff": num("hw.perflevel1.physicalcpu"),
                "l1_kb": round(num("hw.l1icachesize") / 1024) or None,
                "l1d_kb": round(num("hw.l1dcachesize") / 1024) or None,
                "l2_mb": round(num("hw.l2cachesize") / 1048576) or None,
                "page_kb": round(num("hw.pagesize") / 1024) or None,
                "cacheline": num("hw.cachelinesize") or None},
        "gpu": {"cores": prof.get("gpu_cores"), "metal": prof.get("metal") or ""},
        "mem": {"total_gb": round(mem_b / GB, 1) if mem_b else 0,
                "type": "统一内存" if arch == "arm64" else "内存"},
        "disk": {"total_gb": disk.get("total_gb"), "free_gb": disk.get("free_gb"),
                 "ssd": prof.get("ssd") or ""},
        "displays": prof.get("displays") or [],
    }
    _SPEC.update(at=now, data=data)
    return data


def _run_env(argv: list[str], extra: dict, timeout: float = 8) -> str:
    try:
        r = subprocess.run(argv, capture_output=True, text=True, timeout=timeout,
                           env={**os.environ, **extra})
        return r.stdout or ""
    except (OSError, subprocess.SubprocessError):
        return ""


def _app_version(bundle: Path) -> str:
    try:
        with open(bundle / "Contents" / "Info.plist", "rb") as f:
            pl = plistlib.load(f)
        return str(pl.get("CFBundleShortVersionString") or pl.get("CFBundleVersion") or "")
    except (OSError, ValueError, plistlib.InvalidFileException):
        return ""


def _brew_bin() -> str:
    for p in ("/opt/homebrew/bin/brew", "/usr/local/bin/brew"):
        if Path(p).exists():
            return p
    return ""


def _software_win(now: float) -> dict:
    """Windows 装了哪些软件：注册表三处 Uninstall 键 + scoop / choco 的包目录。

    为什么读注册表而不是 `Get-Package`：后者要起 PowerShell、还要加载包管理模块，
    实测 3–8 秒；注册表是毫秒级，而且「设置 → 应用」列表就是从这里来的，口径一致。
    三处都要读：64 位、WOW6432Node（32 位程序）、当前用户（免安装的小工具）。
    """
    apps: list[dict] = []
    seen: set[str] = set()
    roots = [
        ("HKLM", r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall", False),
        ("HKLM", r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall", False),
        ("HKCU", r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall", True),
    ]
    try:
        import winreg
    except Exception:                                  # noqa: BLE001
        winreg = None                            # 非 Windows 或裁剪版 Python
    if winreg is not None:
        for hive, path, user in roots:
            base = winreg.HKEY_LOCAL_MACHINE if hive == "HKLM" else winreg.HKEY_CURRENT_USER
            try:
                k = winreg.OpenKey(base, path)
            except OSError:
                continue
            i = 0
            while True:
                try:
                    sub = winreg.EnumKey(k, i)
                except OSError:
                    break
                i += 1
                try:
                    sk = winreg.OpenKey(k, sub)

                    def gv(name, _sk=sk):
                        try:
                            return winreg.QueryValueEx(_sk, name)[0]
                        except OSError:
                            return ""
                    name = str(gv("DisplayName") or "").strip()
                    # 没有 DisplayName 的是补丁/驱动残留；SystemComponent=1 的是
                    # VC++ 运行库这类「不是用户装的东西」，混进来会淹掉真软件
                    if not name or int(gv("SystemComponent") or 0) == 1:
                        continue
                    if str(gv("ReleaseType") or "") in ("Update", "Hotfix", "Security Update"):
                        continue
                    if str(gv("ParentKeyName") or ""):
                        continue
                    key = name.lower()
                    if key in seen:
                        continue
                    seen.add(key)
                    apps.append({"name": name, "ver": str(gv("DisplayVersion") or ""),
                                 "user": bool(user)})
                except OSError:
                    continue
        try:
            winreg.CloseKey(k)
        except Exception:                              # noqa: BLE001
            pass
    apps.sort(key=lambda x: x["name"].lower())

    # 「命令」这一栏在 mac 上是 brew leaves。Windows 上等价物是 scoop / choco 装的
    # 命令行工具 —— 没有包管理器就空着，不硬凑。
    pkgs: list[str] = []
    for d in (HOME / "scoop" / "apps", Path("C:/ProgramData/chocolatey/lib")):
        try:
            pkgs += [p.name for p in sorted(d.iterdir()) if p.is_dir()]
        except OSError:
            continue
    mgr = ""
    if (HOME / "scoop" / "apps").exists():
        mgr = "scoop"
    elif Path("C:/ProgramData/chocolatey/lib").exists():
        mgr = "choco"
    return {"at": round(now, 1), "apps": apps, "formula": [],
            "leaves": sorted(set(pkgs)), "cask": [], "brew": mgr}


def software(fresh: bool = False) -> dict:
    """装了哪些软件：两个 Applications 目录（读 Info.plist 拿版本）+ Homebrew 两张表。

    brew 必须带 HOMEBREW_NO_AUTO_UPDATE，否则它每次先联网自更新（5 秒起，
    断网时更久），这里只要本地那份清单。
    """
    now = time.time()
    if not fresh and _SW["data"] and now - _SW["at"] < _SW_TTL:
        return _SW["data"]
    if os.name == "nt":
        data = _software_win(now)
        _SW.update(at=now, data=data)
        return data
    env = {"HOMEBREW_NO_AUTO_UPDATE": "1", "HOMEBREW_NO_ANALYTICS": "1",
           "HOMEBREW_NO_ENV_HINTS": "1"}
    apps = []
    for d in ("/Applications", str(HOME / "Applications")):
        try:
            names = sorted(os.listdir(d))
        except OSError:
            continue
        for n in names:
            if not n.endswith(".app"):
                continue
            b = Path(d) / n
            apps.append({"name": n[:-4], "ver": _app_version(b),
                         "user": d.startswith(str(HOME))})
    brew = _brew_bin()
    formula, leaves, cask = [], [], []
    if brew:
        formula = [x for x in _run_env([brew, "list", "--formula"], env).splitlines() if x.strip()]
        # leaves = 没有依赖者的包，也就是「你自己装的工具」；
        # list --formula 那 109 个里 82 个是 aom / brotli / c-ares 这类被拽进来的库，
        # 混进「装了哪些软件」只会让人以为装了一堆看不懂的东西。
        leaves = [x for x in _run_env([brew, "leaves"], env).splitlines() if x.strip()]
        cask = [x for x in _run_env([brew, "list", "--cask"], env).splitlines() if x.strip()]
    data = {"at": round(now, 1), "apps": apps, "formula": sorted(formula),
            "leaves": sorted(leaves), "cask": sorted(cask), "brew": brew}
    _SW.update(at=now, data=data)
    return data


def _launch_pids(domain: str) -> dict[str, int]:
    """一次 `launchctl print <domain>` 抓全部服务的 pid（3188 行、10 毫秒）。

    逐条 `launchctl print <domain>/<label>` 也能查，但 30 多个服务就是 30 多次 fork。
    输出里 service 段每行是「pid 状态码 label」，pid > 0 就是真在跑。
    """
    raw = _run(["/bin/launchctl", "print", domain], timeout=6)
    st: dict[str, int] = {}
    for m in re.finditer(r"^[ \t]+(\d+)[ \t]+(\S+)[ \t]+([A-Za-z0-9_.\-]+)[ \t]*$", raw, re.M):
        label = m.group(3)
        if "." in label:
            st.setdefault(label, int(m.group(1)))
    return st


def _plists(d: Path) -> list[dict]:
    out = []
    try:
        files = sorted(d.glob("*.plist"))
    except OSError:
        return out
    for p in files:
        try:
            with open(p, "rb") as f:
                pl = plistlib.load(f)
        except (OSError, ValueError, plistlib.InvalidFileException):
            continue
        if not isinstance(pl, dict):
            continue
        args = pl.get("ProgramArguments") or []
        prog = pl.get("Program") or (args[0] if args else "")
        out.append({"label": str(pl.get("Label") or p.stem), "file": p.name,
                    "prog": Path(str(prog)).name if prog else ""})
    return out


def units(fresh: bool = False) -> dict:
    """装了哪些常驻服务。分三层看：

    用户级 LaunchAgents  ~/Library/LaunchAgents    —— 能查运行状态（gui 域）
    全局 LaunchAgents    /Library/LaunchAgents     —— 同上
    系统级 LaunchDaemons /Library/LaunchDaemons    —— 要 root 才能 print，只列装了哪些

    顺带把 brew services 的两种常驻（mariadb / ollama）叠进去，它们在
    用户域里也有一份 plist，不重复列，只补上「brew 管的」这个标记。
    """
    now = time.time()
    if not fresh and _UNITS["data"] and now - _UNITS["at"] < _UNITS_TTL:
        return _UNITS["data"]
    if os.name == "nt":
        # Windows 没有 launchd，但「有哪些东西常驻着」这件事同样该答得上来。
        # 等价物是服务控制管理器：psutil.win_service_iter 一次给全（名字 / 显示名 /
        # 状态 / 启动类型 / pid），跟 mac 那份 plist 清单叠出来的行同形。
        # 只列「在跑」或「开机自启」的：Windows 光驱动就有两三百条 disabled，
        # 全摊上去这张卡就没法看了。
        rows = []
        try:
            import psutil
            for s in psutil.win_service_iter():
                try:
                    d = s.as_dict()
                except Exception:                      # noqa: BLE001
                    continue
                st = str(d.get("status") or "")
                start = str(d.get("start_type") or "")
                if st != "running" and start not in ("automatic", "auto"):
                    continue
                nm = str(d.get("name") or "")
                rows.append({"label": nm, "short": str(d.get("display_name") or nm),
                             "prog": start or "—", "pid": int(d.get("pid") or 0),
                             "running": st == "running", "brew": "", "scope": "win",
                             "file": ""})
        except Exception:                              # noqa: BLE001
            rows = []
        rows.sort(key=lambda x: (not x["running"], x["label"].lower()))
        data = {"at": round(now, 1), "user": rows, "global": [], "daemon": [],
                "brew_services": []}
        _UNITS.update(at=now, data=data)
        return data
    gui = _launch_pids("gui/%d" % os.getuid())
    brew_svc = {}
    brew = _brew_bin()
    if brew:
        raw = _run_env([brew, "services", "list"],
                       {"HOMEBREW_NO_AUTO_UPDATE": "1", "HOMEBREW_NO_ANALYTICS": "1"})
        for ln in raw.splitlines()[1:]:
            parts = ln.split()
            if len(parts) >= 2:
                brew_svc[parts[0]] = parts[1]        # started / stopped / none / error

    def pack(d: Path, scope: str, with_state: bool) -> list[dict]:
        rows = []
        for it in _plists(d):
            pid = gui.get(it["label"], 0) if with_state else 0
            lab = it["label"]
            # brew 管的服务在 launchd 里叫 homebrew.mxcl.mariadb / sh.brew.ollama，
            # 跟 brew services list 里的 mariadb / ollama 对不上，按尾巴匹配
            brew_st = next((v for k, v in brew_svc.items() if lab.endswith(k)), "")
            rows.append({**it, "scope": scope, "pid": pid, "running": pid > 0,
                         "brew": brew_st,
                         # 我自己的服务前缀都一样长，剥掉才看得见是谁；
                         # 别人的（Adobe / iStat）保留全名，剥了反而认不出
                         "short": next((lab[len(x):] for x in
                                        ("com.local-ai.", "homebrew.mxcl.", "sh.brew.")
                                        if lab.startswith(x)), lab)})
        return rows

    data = {
        "at": round(now, 1),
        "user": pack(HOME / "Library" / "LaunchAgents", "user", True),
        "global": pack(Path("/Library/LaunchAgents"), "global", True),
        "daemon": pack(Path("/Library/LaunchDaemons"), "daemon", False),
        "brew_services": [{"name": k, "status": v} for k, v in sorted(brew_svc.items())],
    }
    _UNITS.update(at=now, data=data)
    return data


# ---------------------------------------------------------------- 进程（libproc）

class _TaskInfo(ctypes.Structure):
    _fields_ = [("virtual_size", ctypes.c_uint64), ("resident_size", ctypes.c_uint64),
                ("total_user", ctypes.c_uint64), ("total_system", ctypes.c_uint64),
                ("threads_user", ctypes.c_uint64), ("threads_system", ctypes.c_uint64),
                ("policy", ctypes.c_int32), ("faults", ctypes.c_int32),
                ("pageins", ctypes.c_int32), ("cow_faults", ctypes.c_int32),
                ("messages_sent", ctypes.c_int32), ("messages_received", ctypes.c_int32),
                ("syscalls_mach", ctypes.c_int32), ("syscalls_unix", ctypes.c_int32),
                ("csw", ctypes.c_int32), ("threadnum", ctypes.c_int32),
                ("numrunning", ctypes.c_int32), ("priority", ctypes.c_int32)]


class _BsdInfo(ctypes.Structure):
    _fields_ = [("flags", ctypes.c_uint32), ("status", ctypes.c_uint32),
                ("xstatus", ctypes.c_uint32), ("pid", ctypes.c_uint32),
                ("ppid", ctypes.c_uint32), ("uid", ctypes.c_uint32),
                ("gid", ctypes.c_uint32), ("ruid", ctypes.c_uint32),
                ("rgid", ctypes.c_uint32), ("svuid", ctypes.c_uint32),
                ("svgid", ctypes.c_uint32), ("rfu_1", ctypes.c_uint32),
                ("comm", ctypes.c_char * 16), ("name", ctypes.c_char * 32),
                ("nfiles", ctypes.c_uint32), ("pgid", ctypes.c_uint32),
                ("pjobc", ctypes.c_uint32), ("e_tdev", ctypes.c_uint32),
                ("e_tpgid", ctypes.c_uint32), ("nice", ctypes.c_int32),
                ("start_sec", ctypes.c_uint64), ("start_usec", ctypes.c_uint64)]


PROC_PIDTASKINFO = 4
PROC_PIDTBSDINFO = 3
_libproc = None


def _lp():
    global _libproc
    if _libproc is None:
        _libproc = ctypes.CDLL("/usr/lib/libproc.dylib")
    return _libproc


def list_pids() -> list[int]:
    try:
        lp = _lp()
        n = lp.proc_listallpids(None, 0)
        if n <= 0:
            return []
        buf = (ctypes.c_int * (n + 64))()
        n2 = lp.proc_listallpids(ctypes.byref(buf), ctypes.sizeof(buf))
        return [buf[i] for i in range(max(0, n2))]
    except (OSError, AttributeError):
        return []


# 解释器（python / node）的可执行路径都一样，光看路径认不出是哪个项目在用：
# 口袋跑的是 .venv/bin/python，readlink 之后只剩 Python.framework 一条路径。
# 所以对这类进程多花几次 syscall 看它**打开着哪些文件** —— 日志、脚本都在项目目录下，
# 这是唯一稳定能把 pocket 和一堆别的 python 分开的线索。
_NEED_CWD = re.compile(r"(?i)(python|node|ruby|perl|deno|bun|bash|zsh|uvicorn|gunicorn)")
_VNODE = ctypes.create_string_buffer(8192)
PROC_PIDLISTFDS = 1
PROC_PIDFDVNODEPATHINFO = 2      # 注意：proc_pidfdinfo 的 flavor 是**另一套编号**，
                                 # vnodepathinfo 在 pidinfo 里是 9、在 fdinfo 里是 2（踩过）
PROX_FDTYPE_VNODE = 1
_PATH_RUN = re.compile(rb"/[ -~]{3,}")


class _FdInfo(ctypes.Structure):
    _fields_ = [("proc_fd", ctypes.c_int32), ("proc_fdtype", ctypes.c_uint32)]


def _fd_hint(pid: int, limit: int = 24) -> str:
    """这个进程打开的文件路径（截前 limit 个）。失败返回空，不影响采样。"""
    try:
        lp = _lp()
        need = lp.proc_pidinfo(pid, PROC_PIDLISTFDS, 0, None, 0)
        if need <= 0:
            return ""
        step = ctypes.sizeof(_FdInfo)
        buf = (ctypes.c_char * (need + step * 8))()
        got = lp.proc_pidinfo(pid, PROC_PIDLISTFDS, 0, buf, ctypes.sizeof(buf))
        if got <= 0:
            return ""
        out = []
        for i in range(min(got // step, limit * 3)):
            fi = _FdInfo.from_buffer(buf, i * step)
            if fi.proc_fdtype != PROX_FDTYPE_VNODE:
                continue
            r = lp.proc_pidfdinfo(pid, fi.proc_fd, PROC_PIDFDVNODEPATHINFO,
                                  _VNODE, ctypes.sizeof(_VNODE))
            if r > 0:
                # 返回的是整个 vnode 结构（前面全是二进制），路径在尾巴上，扫出来更稳
                for m in _PATH_RUN.findall(_VNODE.raw[:r])[:2]:
                    out.append(m.decode("utf-8", "ignore"))
            if len(out) >= limit:
                break
        return " ".join(out)
    except (OSError, AttributeError, ValueError):
        return ""


def read_proc(pid: int) -> dict | None:
    """一个进程的：名字 / 路径 / 常驻内存 / 累计 CPU 秒 / 启动时刻（解释器多带工作目录）。"""
    try:
        lp = _lp()
        ti = _TaskInfo()
        if lp.proc_pidinfo(pid, PROC_PIDTASKINFO, 0, ctypes.byref(ti),
                           ctypes.sizeof(ti)) <= 0:
            return None
        bi = _BsdInfo()
        start = 0.0
        if lp.proc_pidinfo(pid, PROC_PIDTBSDINFO, 0, ctypes.byref(bi),
                           ctypes.sizeof(bi)) > 0:
            start = float(bi.start_sec)
        name = bi.name.decode("utf-8", "replace") if start else ""
        if not name:
            nm = ctypes.create_string_buffer(256)
            lp.proc_name(pid, nm, 256)
            name = nm.value.decode("utf-8", "replace")
        path = ""
        pb = ctypes.create_string_buffer(4096)
        if lp.proc_pidpath(pid, pb, 4096) > 0:
            path = pb.value.decode("utf-8", "replace")
        if not name:
            name = Path(path).name or f"pid {pid}"
        files = _fd_hint(pid) if _NEED_CWD.search(name) else ""
        return {"name": name, "path": path, "files": files,
                "rss": ti.resident_size,
                "cpu_s": (ti.total_user + ti.total_system) / 1e9,
                "threads": int(ti.threadnum), "start": start}
    except (OSError, AttributeError, ValueError):
        return None


# ---- 逐进程的磁盘读写 / 能耗：proc_pid_rusage ----
# 活动监视器那几列（磁盘写入、能耗影响）用的就是这份数据，只是它没走 ps。
# rusage_info_v6 的结构：uuid 占 16 字节，之后是 53 个 uint64，其中
#   [16] diskio_bytesread  [17] diskio_byteswritten  [40] energy_nj
# 三个都是**累计值**（进程出生到现在），"此刻多快"得采样线程自己差分。
# 读不到（别人的进程 / 老系统）就整条返回 None，不填 0 假装「它没吃资源」。
_RUSAGE_V6 = 6
_RUSAGE_BUF = ctypes.create_string_buffer(8192)
_IO_READ, _IO_WRITE, _NJ = 16, 17, 40


def proc_rusage(pid: int) -> dict | None:
    try:
        lp = _lp()
        if not hasattr(lp, "proc_pid_rusage"):
            return None
        ctypes.memset(_RUSAGE_BUF, 0, 8192)
        if lp.proc_pid_rusage(pid, _RUSAGE_V6, _RUSAGE_BUF) != 0:
            return None
        v = struct.unpack_from("<53Q", _RUSAGE_BUF.raw, 16)
        return {"dread": v[_IO_READ], "dwrite": v[_IO_WRITE], "nj": v[_NJ]}
    except (OSError, AttributeError, ValueError):
        return None


# 逐进程网络：nettop 没有对应的 syscall，只能 fork 一次（约 5 秒，所以单独起
# 一条线程每 60 秒采一次，绝不能塞进 10 秒的进程采样里）。它给的是**累计字节**，
# 跨次调用可比（同一进程的计数不重置），差分即速率。
_NETP = {"at": 0.0, "cum": {}, "rate": {}, "ok": None, "err": ""}
_NETP_EVERY = 60
_NETP_STARTED = {"done": False}


def _netp_sample() -> None:
    try:
        raw = subprocess.run(["/usr/bin/nettop", "-P", "-x", "-L", "1",
                              "-J", "bytes_in,bytes_out"],
                             capture_output=True, timeout=60).stdout
    except (OSError, subprocess.SubprocessError) as e:
        _NETP.update(ok=False, err=f"{type(e).__name__}: {e}"[:120])
        return
    cum: dict[str, list[float]] = {}
    for line in raw.decode("utf-8", "replace").splitlines():
        f = line.strip().rstrip(",").split(",")
        if len(f) != 3 or not f[0] or f[0] == "":
            continue
        try:
            bi, bo = float(f[1]), float(f[2])
        except ValueError:
            continue
        # "WorkBuddy Helper.1234" → 进程名 + pid；pid 在采样表里能查到就归到同一组，
        # 查不到（刚起来的短命进程）就按名字本身算。
        head = f[0].rsplit(".", 1)
        name, pid = (head[0], head[1]) if len(head) == 2 and head[1].isdigit() else (f[0], "")
        key = _PID_GROUP.get(pid) or name
        cur = cum.setdefault(key, [0.0, 0.0])
        cur[0] += bi
        cur[1] += bo
    now = time.time()
    dt = now - float(_NETP["at"] or 0)
    prev = _NETP["cum"] or {}
    rate: dict[str, list[float]] = {}
    if prev and dt >= 5:
        for key, cur in cum.items():
            p = prev.get(key)
            if not p:
                continue
            rate[key] = [max(0.0, (cur[0] - p[0]) / dt), max(0.0, (cur[1] - p[1]) / dt)]
    _NETP.update(at=now, cum=cum, rate=rate, ok=True, err="")


def netp_loop() -> None:
    time.sleep(8)                      # 让进程采样先跑一轮，_PID_GROUP 才有东西
    while True:
        try:
            _netp_sample()
        except Exception:                                  # noqa: BLE001 采样线程不许死
            pass
        time.sleep(_NETP_EVERY)


# 采样器：内存里留最近 2 小时，每 5 分钟往 进程.jsonl 记一笔前 15 名。
_TICK_SECS = 10
_HIST_MAX = 720                      # 720 × 10s = 2 小时
_PROC_DUMP_EVERY = 12                # 12 次采样 = 2 分钟落一笔，重启后图表接得上
_PROC = {"at": 0.0, "apps": [], "cpu_pct": 0.0, "cpu_true": 0.0,
         "cpu_ready": False, "total_rss": 0.0}

# ---- 图标：把 .app 里的 icns 用 sips 转成 72px png 缓存起来 ----
# 不为每个进程现转：sips 是 fork，几百个进程每 10 秒转一次会把机器拖死。
# 只给排在前面的 ICON_MAX 个找，转一次落盘，之后直接读文件。
ICON_DIR = OWN_DIR / "图标"
_ICON_MAX = 64
_ICON_PREF = re.compile(r"(?i)^(appicon|applicationicon|app|icon)[^/]*\.icns$")
_ICON_SKIP = re.compile(r"(?i)(audio|doc|html|image|movie|video|text|pdf|font|"
                        r"folder|sound|generic|badge|template)")
_ICON_LOCK = threading.Lock()
_ICON_DONE: dict[str, dict] = {}          # key -> {"f": 文件名 or "", "at": 时间}
_ICON_Q: list[tuple[str, str]] = []       # 待转换 (key, path)


def _bundle_of(path: str) -> str:
    """从可执行文件路径里取出 .app 那一层。"""
    m = re.match(r"^(/.+?\.app)/", path or "")
    return m.group(1) if m else ""


def _pick_icns(bundle: str):
    try:
        files = [p for p in (Path(bundle) / "Contents" / "Resources").iterdir()
                 if p.suffix.lower() == ".icns"]
    except OSError:
        return None
    if not files:
        return None
    good = [p for p in files if not _ICON_SKIP.search(p.name)] or files
    pref = [p for p in good if _ICON_PREF.match(p.name)]
    try:
        return max(pref or good, key=lambda p: p.stat().st_size)
    except OSError:
        return None


def icon_name(key: str, path: str) -> str:
    """已缓存就返回 png 文件名；没有就排进队列，这次先返回空（前端走字母/齿轮兜底）。"""
    with _ICON_LOCK:
        hit = _ICON_DONE.get(key)
        if hit and (hit["f"] or time.time() - hit["at"] < 900):
            return hit["f"]
        if len(_ICON_Q) < 400 and all(k != key for k, _ in _ICON_Q):
            _ICON_Q.append((key, path))
    return ""


def _make_icon(key: str, path: str) -> str:
    bundle = _bundle_of(path)
    if not bundle:
        return ""
    icns = _pick_icns(bundle)
    if not icns:
        return ""
    try:
        mt = int(icns.stat().st_mtime)
    except OSError:
        return ""
    # 文件名带上 icns 路径和 mtime：应用升级换了图标，缓存自动失效
    name = hashlib.md5(f"{bundle}|{icns.name}|{mt}".encode()).hexdigest()[:12] + ".png"
    dst = ICON_DIR / name
    if dst.exists():
        return name
    try:
        ICON_DIR.mkdir(parents=True, exist_ok=True)
        subprocess.run(["sips", "-s", "format", "png", "--resampleHeightWidth", "72", "72",
                        str(icns), "--out", str(dst)],
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=15)
    except (OSError, subprocess.SubprocessError):
        return ""
    return name if dst.exists() else ""


def icon_worker() -> None:
    """一次转一个、转完睡一下：宁可图标晚半分钟出现，也不跟采样线程抢 CPU。"""
    while True:
        item = None
        with _ICON_LOCK:
            if _ICON_Q:
                item = _ICON_Q.pop(0)
        if not item:
            time.sleep(1.0)
            continue
        key, path = item
        try:
            got = _make_icon(key, path)
        except Exception:
            got = ""
        with _ICON_LOCK:
            _ICON_DONE[key] = {"f": got, "at": time.time()}


def proc_kind(name: str, path: str = "") -> str:
    """没有 .app 的进程（系统守护、脚本解释器）靠这个分类，前端给对应的小图标。"""
    low = (name or "").lower()
    if low.startswith("kernel"):
        return "kernel"
    if _bundle_of(path):
        return "app"
    hay = f"{path} {low}"
    if re.search(r"(?i)(python|node|ruby|perl|deno|bun|\.workbuddy/|local-ai-workspace)", hay):
        return "script"
    if (path or "").startswith(("/System/", "/usr/", "/sbin/", "/Library/Apple/")):
        return "sys"
    return "bin"
_HIST: deque = deque(maxlen=_HIST_MAX)
_prev_cpu: dict[str, float] = {}
_prev_ru: dict[str, tuple] = {}       # 上一轮的 (磁盘读, 磁盘写, 能耗纳焦)，用来差分
_PID_GROUP: dict[str, str] = {}       # pid → 归组名，给 nettop 反查（nettop 只知道 pid）
_tick_n = {"n": 0}
_proc_lock = threading.Lock()


class _HostLoad(ctypes.Structure):
    _fields_ = [("user", ctypes.c_uint32), ("system", ctypes.c_uint32),
                ("idle", ctypes.c_uint32), ("nice", ctypes.c_uint32)]


_HOST_CPU_LOAD_INFO = 3
_last_host_load: list[int] | None = None
_cpu_ready = False          # 有没有拿到第二个样本（没基线时率是假的 0，不能当真）


def cpu_true() -> float:
    """整机 CPU 使用率。进程表读不全（别人的进程 EPERM），所以 KPI 用这个内核计数：
    user + system + nice 的增量 ÷ 总增量。"""
    global _last_host_load
    try:
        libc = ctypes.CDLL("/usr/lib/libSystem.B.dylib")
        load = _HostLoad()
        cnt = ctypes.c_int(4)
        if libc.host_statistics(libc.mach_host_self(), _HOST_CPU_LOAD_INFO,
                                ctypes.byref(load), ctypes.byref(cnt)) != 0:
            return 0.0
        cur = [load.user, load.system, load.idle, load.nice]
    except (OSError, AttributeError):
        return 0.0
    global _cpu_ready
    prev = _last_host_load
    _last_host_load = cur
    if not prev:
        return 0.0
    d = [cur[i] - prev[i] for i in range(4)]
    total = sum(d)
    if total <= 0:
        return 0.0
    _cpu_ready = True
    return round((1 - d[2] / total) * 100, 1)

# 常见的「一堆 helper」合并：名字里带 Helper 的按主应用归并
_HELPER = re.compile(r"^(.*?)\s+Helper(\s*\(.*\))?$")
# 这些名字是运行时不是应用，得靠 .app 路径认出真正的主人
_GENERIC = {"electron", "chromium", "chrome", "node", "deno", "python", "python3",
            "python3.11", "python3.12", "python3.13", "java", "ruby", "php"}


def _app_from_path(path: str) -> str:
    m = re.search(r"/([^/]+)\.app/", path or "")
    return m.group(1) if m else ""


# 哪个软件的进程：按可执行文件路径认，比按进程名靠谱得多
# （pocket 跑的是 .venv/bin/python，名字跟别的 python 撞在一起）
# 分隔符一律写成 [\\/]：Windows 上是反斜杠，写死 `/` 在那边一条都匹配不上。
_OWNER_PAT = {
    "workbuddy": re.compile(r"(?i)workbuddy"),
    "trae": re.compile(r"(?i)(trae cn|trae\.app|[\\/]trae)"),
    "pocket": re.compile(r"(?i)(local-ai-workspace[\\/]pocket|pocket\.log"
                         r"|pocket[\\/]journal|[\\/]pocket[\\/]server\.py"
                         r"|[\\/]moshan[\\/])"),
}


def _group_name(name: str, path: str = "") -> str:
    m = _HELPER.match(name)
    base = (m.group(1).strip() or name) if m else name
    if base.lower() in _GENERIC:
        app = _app_from_path(path)
        if app:
            return app
    return base


# ---------------------------------------------------------------- Windows 采样
# macOS 走 libproc（零依赖、字段最全，路径/启动时刻/磁盘 IO 都有）。Windows 没有
# 对应的一套 syscall，用 psutil —— 它是唯一能跨平台拿到「每个进程的 CPU 时间 /
# 内存 / IO 计数」的东西。代价是多一个依赖（requirements.txt 里标成 Windows 必需），
# 换来的是「本机工作」的进程卡、CPU 曲线在 Windows 上不再是空白。
_WIN_IO_PREV: dict[str, tuple] = {}
_WIN_AT_PREV = [0.0]


def _sample_procs_win() -> dict:
    now = time.time()
    try:
        import psutil
    except ImportError:
        rec = {"at": round(now, 1), "apps": [], "cpu_pct": 0.0, "cpu_true": 0.0,
               "cpu_ready": False, "total_rss": 0.0,
               "why": "这台 Windows 上缺 psutil，采不到进程（pip install psutil 即可）"}
        with _proc_lock:
            _PROC.update(rec)
        return rec

    cpus = os.cpu_count() or 1
    try:
        whole = round(float(psutil.cpu_percent(interval=None)), 1)
    except Exception:                                       # noqa: BLE001
        whole = 0.0
    groups: dict[str, dict] = {}
    for p in psutil.process_iter(["pid", "name", "exe", "memory_info",
                                  "create_time", "cmdline"]):
        info = p.info or {}
        name = info.get("name") or ""
        if not name:
            continue
        # pid 0 = System Idle Process，它那点 CPU 时间是**空闲**时间，算进来
        # 会让「谁在干活」第一名永远是它（实测 2971%），整机占比也跟着失真。
        # pid 4 = System（内核），不是应用，页面上没有意义。
        if int(info.get("pid") or 0) < 8 or name.lower() in ("system idle process", "idle"):
            continue
        exe = info.get("exe") or ""
        key = _group_name(name, exe)
        g = groups.setdefault(key, {"cpu": 0.0, "mem": 0.0, "pids": 0, "path": exe,
                                    "start": 0.0, "io_r": 0.0, "io_w": 0.0,
                                    "owners": set()})
        try:
            # 这里的百分比已经是「上次调用到现在」，psutil 自己记着上一次的 cpu_times，
            # 所以第一次对每个进程都是 0 —— 与 mac 那条「等下一轮」的行为一致。
            g["cpu"] += float(p.cpu_percent(None))
        except Exception:                                   # noqa: BLE001  进程刚好退出
            pass
        mi = info.get("memory_info")
        if mi is not None:
            g["mem"] += float(getattr(mi, "rss", 0)) / 1048576.0
        g["pids"] += 1
        if not g["path"] and exe:
            g["path"] = exe
        ct = info.get("create_time") or 0
        if ct:
            g["start"] = min(g["start"] or ct, float(ct))
        try:
            io = p.io_counters()
            g["io_r"] += float(io.read_bytes)
            g["io_w"] += float(io.write_bytes)
        except Exception:                                   # noqa: BLE001  权限/不支持
            pass
        hay = f"{exe} {' '.join(info.get('cmdline') or [])}"
        for cid, pat in _OWNER_PAT.items():
            if pat.search(hay):
                g["owners"].add(cid)

    dt = now - (_WIN_AT_PREV[0] or now)
    _WIN_AT_PREV[0] = now
    apps = []
    for key, g in groups.items():
        prev = _WIN_IO_PREV.get(key)
        if prev and dt > 0:
            dr = max(0.0, (g["io_r"] - prev[0]) / dt)
            dw = max(0.0, (g["io_w"] - prev[1]) / dt)
        else:
            dr = dw = 0.0
        _WIN_IO_PREV[key] = (g["io_r"], g["io_w"])
        apps.append({
            "name": key,
            "cpu": round(g["cpu"], 1),
            "cpu_s": 0.0,                       # psutil 只给百分比，不给累计秒数
            "cores": round(g["cpu"] / 100, 2),
            "mem": round(g["mem"], 1),
            "dread": round(dr, 1),
            "dwrite": round(dw, 1),
            "net": 0.0,                         # Windows 没有按进程的网络计数
            "net_in": 0.0,
            "net_out": 0.0,
            "pwr": 0.0,                         # 也没有按进程的能耗
            "pids": g["pids"],
            "age": round(now - g["start"]) if g["start"] else None,
            "path": g["path"],
            "owners": sorted(g["owners"]),
        })
    apps.sort(key=lambda a: (-a["cpu"], -a["mem"]))
    for a in apps[:_ICON_MAX]:
        if a["path"]:
            icon_name(a["name"], a["path"])
    total_pct = round(sum(a["cpu"] for a in apps) / cpus, 1)
    rec = {"at": round(now, 1), "apps": apps,
           "cpu_pct": total_pct, "cpu_true": whole, "cpu_ready": bool(whole),
           "total_rss": round(sum(a["mem"] for a in apps), 1)}
    with _proc_lock:
        _PROC.update(rec)
        _HIST.append({"at": rec["at"], "cpu": rec["cpu_true"],
                      "apps": {a["name"]: [a["cpu"], a["mem"], a["dwrite"],
                                           a["dread"], a["net"], a["pwr"]]
                               for a in apps[:40]}})
    return rec


def sample_procs() -> dict:
    """采一次全量进程，按名字合并。返回 {at, apps:[...], cpu_pct, total_rss}。"""
    if os.name == "nt":
        return _sample_procs_win()
    pids = list_pids()
    cpu_now: dict[str, float] = {}
    rss: dict[str, float] = {}
    paths: dict[str, str] = {}
    owners: dict[str, set[str]] = {}
    starts: dict[str, float] = {}
    pids_by: dict[str, int] = defaultdict(int)
    io_r: dict[str, float] = {}                # 磁盘读（累计字节，按组相加）
    io_w: dict[str, float] = {}
    nj: dict[str, float] = {}                  # 能耗（累计纳焦，按组相加）
    for pid in pids:
        d = read_proc(pid)
        if not d:
            continue
        key = _group_name(d["name"], d["path"])
        cpu_now[key] = cpu_now.get(key, 0.0) + d["cpu_s"]
        rss[key] = rss.get(key, 0.0) + d["rss"]
        pids_by[key] += 1
        _PID_GROUP[str(pid)] = key             # nettop 那行「名字.pid」靠它归组
        ru = proc_rusage(pid)
        if ru:
            io_r[key] = io_r.get(key, 0.0) + ru["dread"]
            io_w[key] = io_w.get(key, 0.0) + ru["dwrite"]
            nj[key] = nj.get(key, 0.0) + ru["nj"]
        if d["path"] and key not in paths:
            paths[key] = d["path"]
        hit = owners.setdefault(key, set())
        hay = f"{d['path']} {d.get('files') or ''}"
        for cid, pat in _OWNER_PAT.items():
            if pat.search(hay):
                hit.add(cid)
        if d["start"]:
            starts[key] = min(starts.get(key, 0) or d["start"], d["start"])

    now = time.time()
    dt = now - (_PROC["at"] or now)
    cpus = os.cpu_count() or 1
    netrate = _NETP["rate"] or {}
    ru_now: dict[str, tuple] = {}
    apps = []
    for key, tot in cpu_now.items():
        prev = _prev_cpu.get(key)
        # 首次见到的进程没有基线：整段时间的 CPU 时间平摊到 dt 上，第一次会偏高，
        # 所以只给一个「累计秒数」参考，百分比等下一轮
        pct = 0.0 if prev is None or dt <= 0 else max(0.0, (tot - prev) / dt * 100)
        cur = (io_r.get(key, 0.0), io_w.get(key, 0.0), nj.get(key, 0.0))
        ru_now[key] = cur
        p_ru = _prev_ru.get(key)
        # 同一组的 helper 起了/退了会让「组累计值」跳变；负增量按 0 算，不往回倒扣
        if p_ru and dt > 0:
            dr = max(0.0, (cur[0] - p_ru[0]) / dt)
            dw = max(0.0, (cur[1] - p_ru[1]) / dt)
            pw = max(0.0, (cur[2] - p_ru[2]) / dt / 1e9)     # 纳焦/秒 = 瓦
        else:
            dr = dw = pw = 0.0
        nr = netrate.get(key)
        apps.append({
            "name": key,
            "cpu": round(pct, 1),
            "cpu_s": round(tot, 1),
            "cores": round(pct / 100, 2),
            "mem": round(rss.get(key, 0) / 1048576.0, 1),
            "dread": round(dr, 1),
            "dwrite": round(dw, 1),
            "net": round((nr[0] + nr[1]) if nr else 0.0, 1),
            "net_in": round(nr[0], 1) if nr else 0.0,
            "net_out": round(nr[1], 1) if nr else 0.0,
            "pwr": round(pw, 2),
            "pids": pids_by[key],
            "age": round(now - starts[key]) if key in starts else None,
            "path": paths.get(key, ""),
            "owners": sorted(owners.get(key) or ()),
        })
    _prev_cpu.clear()
    _prev_cpu.update(cpu_now)
    _prev_ru.clear()
    _prev_ru.update(ru_now)
    apps.sort(key=lambda a: (-a["cpu"], -a["mem"]))
    for a in apps[:_ICON_MAX]:                 # 顺手排图标转换（不阻塞采样）
        if a["path"]:
            icon_name(a["name"], a["path"])
    total_pct = round(sum(a["cpu"] for a in apps) / cpus, 1)     # 折算成整机百分比
    rec = {"at": round(now, 1), "apps": apps,
           "cpu_pct": total_pct, "cpu_true": cpu_true(), "cpu_ready": _cpu_ready,
           "total_rss": round(sum(a["mem"] for a in apps), 1)}
    with _proc_lock:
        _PROC.update(rec)
        # 每个进程一行六个数：[CPU%、内存MB、磁盘写Bps、磁盘读Bps、网络Bps、瓦]
        _HIST.append({"at": rec["at"], "cpu": rec["cpu_true"],
                      "apps": {a["name"]: [a["cpu"], a["mem"], a["dwrite"],
                                           a["dread"], a["net"], a["pwr"]]
                               for a in apps[:40]}})
    return rec


def _dump_procs(rec: dict) -> None:
    top = [[a["name"], a["cpu"], a["mem"]] for a in rec["apps"][:15] if a["cpu"] > 0.2]
    if not top:
        return
    line = json.dumps({"at": datetime.now().strftime("%Y-%m-%d %H:%M:%S"),
                       "cpu_pct": rec["cpu_pct"], "top": top}, ensure_ascii=False)
    try:
        PROC_PATH.parent.mkdir(parents=True, exist_ok=True)
        with PROC_PATH.open("a", encoding="utf-8") as f:
            f.write(line + "\n")
    except OSError:
        pass


def sampler_loop() -> None:
    if not _NETP_STARTED["done"]:
        _NETP_STARTED["done"] = True
        threading.Thread(target=netp_loop, name="hw-netproc", daemon=True).start()
    while True:
        try:
            rec = sample_procs()
            _tick_n["n"] += 1
            # TRAE 对话正文读不到，但窗口名 + 这份 CPU/内存采样能证明「什么时候在干活」
            trae_work.record_window(rec)
            if _tick_n["n"] % _PROC_DUMP_EVERY == 0:
                _dump_procs(rec)
        except Exception:                                   # 采样线程不许死
            pass
        time.sleep(_TICK_SECS)


def load_proc_history() -> None:
    """重启后别让「近两小时」空着：把 进程.jsonl 里 2 小时内的记录灌回内存。
    粒度是 5 分钟（落盘节奏），比内存里的 10 秒粗，但至少接得上。"""
    if not PROC_PATH.exists():
        return
    since = time.time() - 2 * 3600
    rows: list[dict] = []
    try:
        with PROC_PATH.open("r", encoding="utf-8", errors="replace") as f:
            for ln in f:
                if not ln.startswith("{"):
                    continue
                try:
                    d = json.loads(ln)
                except ValueError:
                    continue
                try:
                    ts = datetime.strptime(d.get("at") or "", "%Y-%m-%d %H:%M:%S").timestamp()
                except ValueError:
                    continue
                if ts >= since:
                    rows.append({"at": ts, "cpu": d.get("cpu_pct") or 0,
                                 # 老格式只有 [名字, CPU, 内存]，别的维度留 0：
                                 # 重启后这几段是断的，图上就那一段没有磁盘/网络
                                 "apps": {x[0]: [x[1], x[2] if len(x) > 2 else 0,
                                                 0, 0, 0, 0]
                                          for x in (d.get("top") or [])
                                          if isinstance(x, list) and len(x) >= 2}})
    except OSError:
        return
    with _proc_lock:
        for r in rows[-_HIST_MAX:]:
            _HIST.append(r)


# ---------------------------------------------------------------- 机器历史（快照）

def snapshots(days: int = 1) -> dict:
    """读 机器/快照.jsonl，按需降采样成三条折线。"""
    if not SNAP_PATH.exists():
        return {"disk": [], "mem": [], "load": [], "cpu": []}
    since = (datetime.now() - timedelta(days=days)).strftime("%Y-%m-%d %H:%M:%S")
    rows: list[dict] = []
    try:
        with SNAP_PATH.open("r", encoding="utf-8", errors="replace") as f:
            for ln in f:
                if '"at"' not in ln:
                    continue
                try:
                    d = json.loads(ln)
                except ValueError:
                    continue
                if str(d.get("at") or "") >= since:
                    rows.append(d)
    except OSError:
        return {"disk": [], "mem": [], "load": [], "cpu": []}
    # 图上那句「最多 160 个点」以前是句空话：// 取整会让 352 笔变成 176 个点。
    # 用 ceil 才真的封在 160 以内。
    step = max(1, -(-len(rows) // 160))
    picked = rows[::step]
    if rows and picked and picked[-1] is not rows[-1]:
        picked.append(rows[-1])
    return {
        "disk": [[r["at"][5:16], r.get("disk_pct")] for r in picked],
        "mem": [[r["at"][5:16], r.get("mem_pct")] for r in picked],
        "load": [[r["at"][5:16], r.get("load1")] for r in picked],
        "cpus": (rows[-1].get("cpus") if rows else None),
    }


def daily_history(limit: int = 30) -> list[dict]:
    if not DAILY_PATH.exists():
        return []
    out = []
    try:
        with DAILY_PATH.open("r", encoding="utf-8", errors="replace") as f:
            for ln in f:
                if not ln.startswith('{"date"'):
                    continue
                try:
                    out.append(json.loads(ln))
                except ValueError:
                    continue
    except OSError:
        return []
    return out[-limit:]


# ---------------------------------------------------------------- WorkBuddy 会话

SCAN = {"done": 0, "total": 0, "running": False, "started": "", "err": "",
        "bytes": 0, "cached_bytes": 0}
_SESS = {"files": {}}              # 缓存结构：{"v":3,"files":{path:{...}}}
_sess_lock = threading.Lock()
_loaded = {"v": False}
PARSE_VERSION = 17                 # 解析口径变了就 +1，缓存自动失效（17：加活跃时长 act/hw）
# 15（2026-11-21）：模型切换改成按「每轮首请求的 requestModelId」定格 —— 实际
# 调用的 model 会被 Auto 均衡每秒轮换几十次，拿它当切换会刷出上百条假记录。
# 14（2026-11-21）：errs 从 [ts,code,reset] 扩成五元组多了 kind/msg；新增
# switches（模型切换）与 turns（用户轮次边界）—— 老缓存没有这两项，只能重扫一遍。
                                   # v6：每会话多留最近若干轮「用户原话」（任务清单的子对话）
                                   # v7：只收 input_text，剔掉工具回灌的假 user 消息
                                   # v8：剔掉 task-notification / cb_summary 这类假发言
                                   # v9：贴图前缀压成「［图］」
                                   # v10：剔掉压缩摘要正文（compact-summary / 「接着摘要继续」）

# 用户原话清洗：WorkBuddy 把模式/专家上下文拼在正文前面的 <system-reminder> 里，
# 真正的话在 <user_query> 中；两者都没有的（老格式）就整段去标签后取。
_SR_RE = re.compile(r"<system-reminder[\s\S]*?</system-reminder>", re.I)
_UQ_RE = re.compile(r"<user_query>([\s\S]*?)</user_query>", re.I)
_TAG_RE = re.compile(r"</?[a-zA-Z][^>]{0,160}>")
_UTT_SKIP_RE = re.compile(
    r"^(This session is being continued|Summary of the conversation|\[Request interrupted"
    # 上下文压缩后 WorkBuddy 会自己发一句「接着上面的摘要继续」，以及摘要正文
    # （`data-role="compact-summary"` 那层被拆掉后剩下的就是「Summary: 1. …」）。
    # 这些都不是人说的话，混进任务清单里就成了假发言。
    r"|Please continue with the conversation|Summary: \s*\d+\.)",
    re.I)
# 这几类伪装成「用户发言」的系统消息不能进对话列表：
#   task-notification  后台任务跑完/失败的通知
#   cb_summary         上下文压缩后的旧对话摘要（整段重放一遍，全是重复）
_UTT_BLOCK_RE = re.compile(
    r"<task-notification|<cb_summary|<cb_compact|compact-summary"
    r"|<conversation_history_summary"
    r"|Summary of the conversation between", re.I)


def _utt_text(txt: str, limit: int = 130) -> str:
    """把一条用户消息压成一句可读的话；纯 meta 的返回空串。"""
    s = str(txt or "")
    if _UTT_BLOCK_RE.search(s):
        return ""
    m = _UQ_RE.search(s)
    s = m.group(1) if m else _SR_RE.sub(" ", s)
    s = _TAG_RE.sub(" ", s)
    # 贴图消息前面挂着一串 @image#1:Clipboard_Screenshot.png，读起来全是文件名
    had_img = bool(re.search(r"@image#\d+:", s))
    if had_img:
        s = re.sub(r"@image#\d+:\S*", " ", s)
    s = (s.replace("&quot;", '"').replace("&#39;", "'").replace("&lt;", "<")
          .replace("&gt;", ">").replace("&amp;", "&"))
    s = re.sub(r"\s+", " ", s).strip()
    if had_img:
        s = "［图］" + s
    if len(s) < 2 or _UTT_SKIP_RE.match(s):
        return ""
    return s[:limit] + ("…" if len(s) > limit else "")

# 模式 / 专家：WorkBuddy 把它们拼在每轮用户消息前的 system-reminder 里。
# 规则与 pocket/dash.py 里那两个正则、以及 _expert_name 的三级兜底保持一致 ——
# 两边各读同一份数据、同一套口径，页面上才不会一边叫「全栈自动化脚本工程师」、
# 一边叫「KP9AY2」。
_MODE_RE = re.compile(r"You are now in ([A-Za-z][A-Za-z ]{2,14}?) mode")
_EXPERT_RE = re.compile(r"<current-expert>[\s\S]{0,200}?(ex_[A-Za-z0-9]{6,})")

# 一轮任务被打断的原因。WorkBuddy 把它写成一条 role=assistant 的 message，
# status=incomplete，content 就是页面上那句提示；错误本体在 providerData.error。
#   14018 额度已用尽        积分/总量花完，只能买加量包，没有自动恢复
#   6004  频率限制/周期额度  提示里带重置时间。**免费模型的额度超限也是这个码**：
#                           后端文案还是「您的使用量已超出频率限制」，前端把它改写成
#                           「您使用 X 模型的免费额度已超出频率限制……您可以消耗积分
#                           继续使用该模型，或切换至其他模型」。所以日志里两种分不开，
#                           只能看 message 里有没有「免费」二字（有就单独归类）。
#   6021  队列满            排不进去，重试即可
#   11134 模型不可用        服务端临时故障，提示「请稍后重试或切换其他模型」
#   6020  排队中            会自动继续，不是中断，不收
ERR_KINDS = {14018: "credit", 6004: "quota", 6021: "queue", 11134: "unavailable"}
ERR_NET_CODES = ("ECONNRESET", "ETIMEDOUT", "ENOTFOUND", "EPIPE",
                 "ECONNREFUSED", "UND_ERR_SOCKET")
_FREE_RE = re.compile(r"免费")
# 重置时间有两种说法：后台文案「将在 X 重置」，免费额度那句前端改写成「可在 X 重置」
_RESET_RE = re.compile(r"(?:将在|可在)\s*(\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2})")

# 每种中断在页面上怎么叫。前端 dash.html 里有一份同名的 KIND 表，改这里要同步。
KIND_LABEL = {"credit": "积分花完", "quota": "额度超限", "free": "免费额度",
              "queue": "队列满", "unavailable": "模型不可用",
              "net": "网络中断", "other": "其他中断"}
_EXPERT_CACHE: dict[str, object] = {"mtime": -1.0, "map": {}}


def _expert_label(eid: str) -> str:
    """ex_hXSIP3lgdCnU → 全栈自动化脚本工程师。

    专家清单只存在当前登录用户目录下（storage/<user-id>/global/expert.json），
    用户 id 随机，所以 glob 取最新那份；查不到再退到 bundle-map 的 slug，
    最后才截 id 尾 6 位。
    """
    if not eid:
        return ""
    try:
        files = sorted((Path.home() / ".workbuddy").glob("storage/*/global/expert.json"),
                       key=lambda p: p.stat().st_mtime, reverse=True)
    except OSError:
        files = []
    if files:
        p = files[0]
        try:
            mt = p.stat().st_mtime
        except OSError:
            mt = -1.0
        if mt != _EXPERT_CACHE["mtime"]:
            amap: dict[str, str] = {}
            try:
                raw = json.loads(p.read_text(encoding="utf-8"))
            except (OSError, ValueError):
                raw = {}
            for row in (raw.get("recents") if isinstance(raw, dict) else None) or []:
                if isinstance(row, dict):
                    rid, nm = str(row.get("id") or ""), str(row.get("name") or "")
                    if rid and nm:
                        amap[rid] = nm
            try:
                bmap = json.loads((Path.home() / ".workbuddy" / "app" / "cache"
                                   / "experts" / "expert-bundle-map.json")
                                  .read_text(encoding="utf-8"))
            except (OSError, ValueError):
                bmap = {}
            if isinstance(bmap, dict):
                for rid, slug in bmap.items():
                    if str(rid).startswith("ex_") and slug:
                        amap.setdefault(str(rid), str(slug))
            _EXPERT_CACHE["map"] = amap
            _EXPERT_CACHE["mtime"] = mt
    return str((_EXPERT_CACHE["map"] or {}).get(eid) or eid[-6:])


def ensure_sessions_loaded() -> None:
    """先把磁盘上那份缓存装上：服务刚起、后台还没扫完时，页面也不该是空的。"""
    global _SESS
    if _loaded["v"]:
        return
    _loaded["v"] = True
    disk = _json_read(CACHE_PATH, {})
    if isinstance(disk, dict) and disk.get("v") == PARSE_VERSION:
        with _sess_lock:
            _SESS = disk


def _new_agg() -> dict:
    return {"id": "", "proj": "", "cwd": "", "title": "",
            "first": 0, "last": 0, "asks": 0, "calls": 0,
            "models": {}, "tools": {}, "skills": {}, "agents": {}, "mcps": {},
            "modes": {}, "experts": {},
            "utt": [],                 # 最近 30 轮用户原话 [ts, text]，任务清单展开用
            # 额度中断 [ts, code, reset_ms]：token 记「花了多少」，这里记「什么时候
            # 因为没额度停下来了」，两件事不能混在一本账里
            "errs": [],
            # 最后一次「真的跑成功了」的请求时间。用来判断额度限制还在不在：
            # 15:06 被额度卡住、17:31 又正常跑了几十次，那就不算「现在还卡着」——
            # 只看「最近一条报错的重置时间」会把早已绕过去的限制一直挂在页面顶上。
            "last_ok": 0,
            # 会话标题的时间线 [[ts, 名字], ...]：日志里的 ai-title 每改一次记一条。
            # 用户在侧栏改的名字不落日志，那份在 workbuddy.db；这条线只负责回答
            # 「配额断的那一刻，这个任务叫什麼」。
            "titles": [],
            # 活跃时长（2026-11-22）：相邻两次操作的间隔 ≤5 分钟才算「在干活」。
            #   act = {日期: 秒}、hw = {"YYYY-MM-DDTHH": 秒}、lastts = 上一笔的毫秒戳。
            # 为什么不用「首末时间差」：会话挂在后台一夜，跨度是 8 小时，人只坐了 40 分钟；
            # 按操作间隔切段更接近体感，也跟「时间投入」那种图的算法一致。
            "act": {}, "hw": {}, "lastts": 0,
            "days": {}}


def _day_of(ms: int) -> str:
    return datetime.fromtimestamp(ms / 1000.0).strftime("%Y-%m-%d")


def _reset_ms(msg: str) -> int:
    """从 6004 的提示里抠出「将在 2026-11-18 20:42:33 UTC+8 重置」。

    这是额度这一档最有用的一条：不是「当时停了」，而是「几点能接着干」。
    """
    m = _RESET_RE.search(msg or "")
    if not m:
        return 0
    try:
        return int(datetime.strptime(m.group(1), "%Y-%m-%d %H:%M:%S").timestamp() * 1000)
    except ValueError:
        return 0


def _proj_of(cwd: str) -> str:
    """项目名：剥掉 /Users/xxx/，只留人认得出的那一段（完整路径前端挂 title）。"""
    s = str(cwd or "").rstrip("/")
    for pre in ("/Users/", "/home/"):
        if s.startswith(pre):
            rest = s[len(pre):]
            i = rest.find("/")
            return rest[i + 1:] if i >= 0 else rest
    return s


def _bump(d: dict, k: str, n: int = 1) -> None:
    d[k] = d.get(k, 0) + n


# ------------------------------------------------------------ 会话的现名 / 生死
# 日志里只有 ai-title（那一刻的名字）。用户在侧栏改名不回写日志，删掉的会话
# 连 jsonl 都可能不在了 —— 这两件事只有 app 自己的库说得清：
#   custom_title  手动改的名（改过就以它为准）
#   deleted_at    删除时间戳；NULL = 本地没删，-1 = 云端没删的哨兵
#                       （别把 -1 当删了）
WB_DB = HOME / ".workbuddy" / "workbuddy.db"
_SIDX: dict = {"at": 0.0, "rows": {}}


def _session_index(ttl: float = 20.0) -> dict:
    """{session_id: {title, deleted, deleted_at}}。读不到库就返回空，各处退回日志名。"""
    now = time.time()
    if _SIDX.get("rows") and now - float(_SIDX.get("at") or 0) < ttl:
        return _SIDX["rows"]
    rows: dict = {}
    try:
        # 只读打开：app 正在写这个库，别去抢写锁；mode=ro 也不会把 WAL 合并坏
        con = sqlite3.connect(f"file:{WB_DB}?mode=ro", uri=True, timeout=1.5)
        try:
            # 空间名一起取回来：客户端的「项目」不在 cwd 上（cwd 永远是
            # ~/.workbuddy/projects/<slug>），真正的归属是 sessions.space_id。
            # 「个人中台 / 日程软件 / Call Recordings」这些名字全在 spaces 表里。
            spaces = {str(r[0]): str(r[1] or "")
                      for r in con.execute("SELECT id, name FROM spaces")}
            for sid, title, custom, del_at, sp_id in con.execute(
                    "SELECT id, title, custom_title, deleted_at, space_id"
                    "  FROM sessions"):
                d = int(del_at or 0)
                rows[str(sid)] = {
                    "title": str(custom or title or "").strip(),
                    "deleted": d > 0,
                    "deleted_at": d,
                    "space": spaces.get(str(sp_id or ""), ""),
                }
        finally:
            con.close()
    except Exception:                   # noqa: BLE001 —— 库被锁 / 没装 / 表变了
        rows = {}
    if rows:                            # 读失败不覆盖上一份，宁可用旧的
        _SIDX.update({"at": now, "rows": rows})
    return _SIDX.get("rows") or {}


def _title_at(agg: dict, ts: int) -> str:
    """这个时刻这个会话叫什么 —— 标题时间线里 ts 之前最后那一条。"""
    hist = agg.get("titles") or []
    hit = ""
    for t, s in hist:
        try:
            if int(t or 0) and int(t) <= ts:
                hit = s
            elif int(t or 0) > ts:
                break
        except (TypeError, ValueError):
            continue
    return str(hit or "")


def _note_model(agg: dict, rid, name, model, ts: int) -> None:
    """记下「这一轮用户选的是哪个模型」，变了才留一笔。

    WorkBuddy 那句「模型已从 Hy4 flash 更改为 DeepSeek-V4.1-Flash」是 UI 提示，
    jsonl 里没有这条记录。而且**不能拿实际调用的 model 比对** —— Auto/均衡模式
    下请求会在 hy4、glm-5.3-flash、deepseek 之间每秒级轮换（04:15 实测一分钟几十次），
    那是负载均衡，不是用户换模型。

    真正代表「用户选了什么」的是 providerData.requestModelId：
      deepseek-v4.1-flash → 显式选了它
      balanced-model      → 选了「均衡」，内部怎么调度不管
      auto / fast-model   → 选了 Auto / 快速
    外加 requestModelName（"Hy4 flash"、「均衡」）正好是 UI 文案里的名字。
    一轮只在第一次请求时定格，同轮后续的调度抖动全部忽略。
    """
    key = str(rid or model or "").strip()
    if not key:
        return
    label = str(name or model or key).strip()
    turns = agg.get("turns") or []
    cur = 0
    for tv in reversed(turns):          # 这条请求属于哪一轮
        if tv <= ts:
            cur = tv
            break
    prev = agg.get("turn_model")        # [轮次起点, key, label]
    if prev and prev[0] == cur:
        return                          # 这一轮的模型已经定过了
    if prev and prev[1] and prev[1] != key:
        lst = agg.setdefault("switches", [])
        lst.append([int(ts or 0), prev[2], label, prev[1], key])
        if len(lst) > 60:               # 老会话几百轮，留最近 60 次够翻
            del lst[:-60]
    agg["turn_model"] = [cur, key, label]


def _add_usage(agg: dict, pd: dict, day_row, ts: int = 0) -> None:
    """把一条 API 请求的用量记进「会话 / 当天 / 模型」三处。

    2026-11-21：以前只在 type=message 的分支里记，工具循环里每一次 function_call
    的用量全漏了 —— 一场 1200 次工具调用的会话，账上只算到 28 次。加「每小时
    token」时露了馅：同一小时 1.6 亿 token，卡片上 7 天合计才 4800 万。现在两边
    都记，口径统一成「每次请求」。
    """
    u = (pd or {}).get("usage") or {}
    if not isinstance(u, dict) or not u.get("totalTokens"):
        return
    model = str(pd.get("model") or pd.get("requestModelName") or "未知模型")
    # 这条请求是成功的（有 usage 才走到这里），记一笔「最后一次正常」的时间
    if ts and ts > int(agg.get("last_ok") or 0):
        agg["last_ok"] = ts
    _note_model(agg, pd.get("requestModelId"), pd.get("requestModelName"), model, ts)
    tin = int(u.get("inputTokens") or u.get("input_tokens") or 0)
    tout = int(u.get("outputTokens") or u.get("output_tokens") or 0)
    cached = 0
    det = u.get("inputTokensDetails") or []
    if isinstance(det, list) and det and isinstance(det[0], dict):
        cached = int(det[0].get("cached_tokens") or 0)
    agg["calls"] += 1
    m = agg["models"].setdefault(model, {"in": 0, "out": 0, "cached": 0, "n": 0})
    m["in"] += tin
    m["out"] += tout
    m["cached"] += cached
    m["n"] += 1
    dm = day_row()
    dm["calls"] += 1
    dm["in"] += tin
    dm["out"] += tout
    dm["cached"] += cached
    dmm = dm["models"].setdefault(model, {"in": 0, "out": 0, "cached": 0, "n": 0})
    dmm["in"] += tin
    dmm["out"] += tout
    dmm["cached"] += cached
    dmm["n"] += 1


def _apply_line(agg: dict, d: dict) -> None:
    t = d.get("type")
    ts = int(d.get("timestamp") or 0)
    if ts:
        agg["last"] = max(agg["last"], ts)
        if not agg["first"]:
            agg["first"] = ts
    if d.get("cwd") and not agg["cwd"]:
        agg["cwd"] = str(d["cwd"])
    if d.get("sessionId") and not agg["id"]:
        agg["id"] = str(d["sessionId"])

    if t == "ai-title":
        # 标题会变：一轮活干完，app 会按新内容重新起个名。只留「变过一次」的记录，
        # 重复写同一个名字不占地方 —— 额度那档要拿它回答「当时叫什么」。
        new = str(d.get("aiTitle") or "").strip()[:120]
        if new and new != agg.get("title"):
            agg.setdefault("titles", []).append([ts, new])
            agg["title"] = new
        return

    # 中断留痕：跟 token 记账分开记。token 回答「花了多少」，这一条回答
    # 「哪一刻因为什么被迫停下来」。同一次中断日志里会连着写好几条（重试），
    # 合并留给 quota_events()，这里只如实收。
    # 2026-11-21 扩：以前只认 429/6004/14018，于是「模型不可用 11134」「队列满
    # 6021」「ECONNRESET」这些同样把一轮活打断的错误全漏了；免费模型的额度超限
    # 又是 6004 换了层皮，单独归到 free。
    err = (d.get("providerData") or {}).get("error")
    if isinstance(err, dict) and ts:
        try:
            status = int(err.get("status") or 0)
        except (TypeError, ValueError):
            status = 0
        raw_code = err.get("code")
        try:
            code = int(raw_code or 0)
        except (TypeError, ValueError):
            code = 0
        msg = str(err.get("message") or "")
        kind = ERR_KINDS.get(code) or ""
        if kind == "quota" and _FREE_RE.search(msg):
            kind = "free"
        elif not kind:
            if str(raw_code or "") in ERR_NET_CODES or err.get("isNetworkError"):
                kind = "net"
            elif code and code != 6020:    # 6020 = 排队中，会自动继续
                kind = "other"
            elif status >= 500:            # 只有状态码没有业务码的 5xx
                kind = "other"
        if kind:
            # 五元组；老缓存里是三元组，quota_events 按长度兼容，不用重扫全库
            agg.setdefault("errs", []).append(
                [ts, code, _reset_ms(msg), kind, msg[:90]])

    if t not in ("message", "function_call"):
        return

    day = _day_of(ts) if ts else date.today().isoformat()

    # —— 活跃时长：两次操作挨得够近，才算这段时间人真在干活 ——
    # ≤5 分钟：把这一小段记进当天和「周几-几点」两个桶；超过就当下线了，不记。
    # 第一笔没有上文，垫 30 秒，免得一个只发了一句话的日子显示 0 分钟。
    if ts:
        prev = int(agg.get("lastts") or 0)
        if prev and ts > prev:
            gap = (ts - prev) / 1000.0
            sec = gap if gap <= 300 else 0.0
        elif prev:
            sec = 0.0                       # 同一毫秒的多行（一步里好几个块）
        else:
            sec = 30.0
        if sec > 0:
            act = agg.setdefault("act", {})
            act[day] = round(float(act.get(day) or 0) + sec, 1)
            hw = agg.setdefault("hw", {})
            hk = day + "T" + datetime.fromtimestamp(ts / 1000.0).strftime("%H")
            hw[hk] = round(float(hw.get(hk) or 0) + sec, 1)
        agg["lastts"] = max(prev, ts)

    def day_row() -> dict:
        return agg["days"].setdefault(day, {"calls": 0, "in": 0, "out": 0, "cached": 0,
                                            "asks": 0, "tools": {}, "models": {}})

    if t == "message":
        role = d.get("role")
        pd = d.get("providerData") or {}
        if role == "user":
            agg["asks"] += 1
            # 轮次边界：一条用户消息 = 一轮。模型切换要按轮归属 —— Auto 在同一轮里
            # 会为了负载均衡把 hy4 和 glm-5.3-flash 来回换（实测一轮切四五次），
            # 那不是「换模型」，只留下这一轮真正换过去的那个。
            tl = agg.setdefault("turns", [])
            if ts and (not tl or tl[-1] != ts):
                tl.append(ts)
                if len(tl) > 200:
                    del tl[:-200]
            day_row()["asks"] += 1
            # 模式和专家就挂在这条消息前面的 system-reminder 上：同样是「问了 40 轮」，
            # Agent 模式一步步试错和 Plan 模式先出方案，成本完全不是一回事。
            # 只拼「人说的话」：content 里还有 tool_result 之类的块，那些是工具
            # 输出回灌成的 user 消息，混进来就成了「X9TcQz call_… failed」这种噪音。
            txt = "".join(str(x.get("text") or "") for x in (d.get("content") or [])
                          if isinstance(x, dict)
                          and str(x.get("type") or "") in ("input_text", "text", ""))
            if txt:
                m = _MODE_RE.search(txt)
                if m:
                    _bump(agg.setdefault("modes", {}), m.group(1).strip().title())
                e = _EXPERT_RE.search(txt)
                if e:
                    _bump(agg.setdefault("experts", {}), _expert_label(e.group(1)))
                # 任务清单要把这段任务展开成「具体聊了哪几句」，所以原话也留一份。
                # 只留最近 30 轮：老会话几百轮，前端也不看那么远。
                one = _utt_text(txt)
                if one:
                    lst = agg.setdefault("utt", [])
                    # 连发两次「继续」这种不重复占位
                    if not any(x[1] == one for x in lst[-2:]):
                        lst.append([ts, one])
                        agg["utts"] = int(agg.get("utts") or 0) + 1
                    if len(lst) > 30:
                        del lst[:-30]
        _add_usage(agg, pd, day_row, ts)
        return

    # function_call：工具 / MCP / 技能 / 专家 全从这里认
    # 工具调用的那一趟请求也是真花了 token：它带着完整上下文，往往是账上的大头
    _add_usage(agg, d.get("providerData") or {}, day_row, ts)
    name = str(d.get("name") or "").strip()
    if not name:
        return
    raw = d.get("arguments")
    args = {}
    if isinstance(raw, str):
        try:
            args = json.loads(raw) if raw.strip().startswith("{") else {}
        except ValueError:
            args = {}
    elif isinstance(raw, dict):
        args = raw
    _bump(agg["tools"], name)
    _bump(day_row()["tools"], name)
    if name.startswith("mcp__"):
        parts = name.split("__")
        srv = parts[1] if len(parts) > 1 else name
        _bump(agg["mcps"], srv)
    elif name == "Skill":
        sk = str(args.get("skill") or args.get("name") or "").strip() or "未命名技能"
        _bump(agg["skills"], sk)
    elif name in ("Agent", "Task"):
        who = str(args.get("subagent_type") or args.get("description") or "").strip()
        who = re.sub(r"@.*$", "", who) or "子代理"
        _bump(agg["agents"], who)
    elif name == "DeferExecuteTool":
        # 延迟工具本身不是 MCP：它只是「把真正那个工具调起来」的壳。
        # 以前 inner 不是 mcp__ 开头也往 mcps 里塞，于是榜上出现
        # 「MCP TaskStop」「MCP skill_management」这种假条目。
        inner = str(args.get("toolName") or "").strip()
        if inner.startswith("mcp__"):
            _bump(agg["mcps"], inner.split("__")[1])
        elif inner:
            _bump(agg["tools"], inner)


def _scan_file(path: Path, prev: dict | None) -> dict:
    """增量解析：只读上次 offset 之后追加的完整行。"""
    try:
        st = path.stat()
    except OSError:
        return prev or {}
    same = (prev and prev.get("v") == PARSE_VERSION
            and prev.get("ino") == st.st_ino
            and prev.get("size", 0) <= st.st_size
            and prev.get("agg"))
    if same:
        off = int(prev.get("off") or 0)
        agg = prev["agg"]
        agg.setdefault("days", {})
    else:
        off = 0
        agg = _new_agg()
    if st.st_size == off:
        if prev:
            prev.update({"size": st.st_size, "mtime": st.st_mtime, "ino": st.st_ino})
            return prev
        return {}
    try:
        with path.open("rb") as f:
            f.seek(off)
            buf = f.read()
    except OSError:
        return prev or {}
    cut = buf.rfind(b"\n")
    if cut < 0:
        return prev or {}
    chunk = buf[:cut + 1]
    for raw in chunk.splitlines():
        if b'"session-meta"' in raw or b'"file-history-snapshot"' in raw:
            continue
        if not raw.startswith(b"{"):
            continue
        try:
            d = json.loads(raw)
        except ValueError:
            continue
        _apply_line(agg, d)
    agg["proj"] = path.parent.name
    SCAN["bytes"] += len(chunk)
    return {"v": PARSE_VERSION, "off": off + cut + 1, "size": st.st_size,
            "mtime": st.st_mtime, "ino": st.st_ino, "agg": agg}


_SCAN_LOCK = threading.Lock()
_SCAN_AT: dict[str, float] = {"t": 0.0}


def scan_sessions(force: bool = False) -> None:
    """扫全部会话文件。已缓存的只读新增字节；没变的直接跳过。"""
    # 后台线程和「页面按需补扫」可能同时进来，串起来别抢同一个 _SESS
    with _SCAN_LOCK:
        _scan_sessions(force=force)
        _SCAN_AT["t"] = time.time()


def scan_if_stale(max_age: float = 20.0) -> None:
    """页面来要数据时顺手补一次扫描。

    以前只有后台 scan_loop 每 5 分钟扫一次，任务清单里展开的「具体说了哪几句」
    最坏要晚 5 分钟才出现，而旁边的逐轮记录是现解析的 —— 两边一对，看着就像
    这张卡坏了。这里 max_age 内不重复扫，超过就增量补一次（只读新增字节）。
    """
    if time.time() - float(_SCAN_AT.get("t") or 0) < max_age:
        return
    scan_sessions()


def _scan_sessions(force: bool = False) -> None:
    global _SESS
    if not WB_ROOT.exists():
        return
    _loaded["v"] = True
    if force:
        _SESS = {"v": PARSE_VERSION, "files": {}}
    else:
        disk = _json_read(CACHE_PATH, {})
        if isinstance(disk, dict) and disk.get("v") == PARSE_VERSION:
            with _sess_lock:
                _SESS = disk
    # 按 mtime 新的先扫：页面最想看的永远是刚在说话的那个会话。
    # 单独包一层 —— glob 到 stat 之间文件被轮转掉会让整个扫描抛出去。
    def _mt(p: Path) -> float:
        try:
            return -p.stat().st_mtime
        except OSError:
            return 0.0

    files = sorted(WB_ROOT.glob("*/*.jsonl"), key=_mt)
    SCAN.update({"total": len(files), "done": 0, "running": True,
                 "started": datetime.now().strftime("%H:%M:%S"), "err": "", "bytes": 0})
    cur = dict(_SESS.get("files") or {})
    for i, path in enumerate(files, start=1):
        key = str(path)
        try:
            entry = _scan_file(path, cur.get(key))
        except Exception as e:                              # 单个文件坏了不拖累整体
            SCAN["err"] = f"{path.name}: {e}"
            entry = cur.get(key) or {}
        if entry:
            cur[key] = entry
        SCAN["done"] = i
        if i % 6 == 0:
            with _sess_lock:
                _SESS = {"v": PARSE_VERSION, "files": dict(cur)}
    with _sess_lock:
        _SESS = {"v": PARSE_VERSION, "files": cur}
    _json_write(CACHE_PATH, _SESS)
    SCAN["running"] = False


def sessions_snapshot() -> list[dict]:
    ensure_sessions_loaded()
    with _sess_lock:
        items = list((_SESS.get("files") or {}).values())
    out = []
    for e in items:
        agg = e.get("agg") or {}
        if not agg.get("last"):
            continue
        out.append(agg)
    out.sort(key=lambda a: -a["last"])
    return out


# ---------------------------------------------------------------- 额度中断
# 用户要的两件事：「积分花完导致任务停了」和「试用额度没了」都得留痕。
# 前者是 14018（只能买加量包），后者是 6004（等窗口重置）。这两件事跟 token
# 明细不是一本账：token 记「花了多少」，这里记「哪一刻被迫停下来」。

QUOTA_MERGE_MS = 10 * 60 * 1000


SWITCH_AFTER_ERR_MS = 5 * 60 * 1000


def quota_events(days: int = 90) -> dict:
    """最近「被打断」和「换过模型」。

    一次中断在日志里是一串 429（11-21 03:56 连着五条，每次重试都写一条）。
    按请求数统计会把「停了一次」说成「停了五次」，所以按「同会话 + 同类型 +
    间隔 10 分钟内」合并成一条，n 记重试次数；中断次数按合并后的条数算。

    2026-11-21 扩了两类：
      · 中断不只有额度：模型不可用 11134、队列满 6021、ECONNRESET 都会把一轮
        活打断，以前全漏了；免费模型的额度超限是 6004 换了层皮，归到 free。
      · 「模型已从 A 更改为 B」是 UI 提示，日志里没有这条 —— 只能靠相邻两次
        请求的 providerData.model 变化反推。切之前 5 分钟内有报错标 after_err
        （Auto 兜底换的），其余的算用户自己在模型选择器里换的。
    """
    ensure_sessions_loaded()
    with _sess_lock:
        items = list((_SESS.get("files") or {}).items())
    idx = _session_index()
    lo = int((time.time() - days * 86400) * 1000) if days else 0
    raw = []
    sws = []
    ok_ts = 0                       # 最后一次成功的模型调用（判断额度限制还在不在）
    for path, e in items:
        agg = e.get("agg") or {}
        ok_ts = max(ok_ts, int(agg.get("last_ok") or 0))
        rows = agg.get("errs") or []
        sw_rows = agg.get("switches") or []
        if not rows and not sw_rows:
            continue
        proj = _proj_of(str(agg.get("cwd") or "")) or str(agg.get("proj") or "")
        # 现名以 app 的库为准（侧栏改过的名字不回写日志）；库里没有就退回日志名。
        sid = str(agg.get("id") or "").strip() or Path(str(path)).stem
        meta = idx.get(sid) or {}
        cur = str(meta.get("title") or "").strip()[:60] or str(agg.get("title") or "")[:60]
        # 删没删：库里 deleted_at 有时间戳算删了；日志文件本身都没了也算
        # （硬删的会话会从 sessions 表整行消失，只剩缓存里这一份账）。
        gone = bool(meta.get("deleted")) or not Path(str(path)).exists()
        turned: set = set()            # 这一份会话里已经记过的轮次
        err_ts: list[int] = []
        for row in rows:
            try:
                ts, code, reset = int(row[0]), int(row[1]), int(row[2])
            except (TypeError, ValueError, IndexError):
                continue
            # 老缓存是三元组 [ts, code, reset]；新的是五元组，多的两个是 kind
            # 和 message 摘要。kind 缺了就按 code 反推 —— 不用为此重扫全库。
            kind, msg = "", ""
            if len(row) >= 5:
                kind, msg = str(row[3] or ""), str(row[4] or "")
            kind = kind or ERR_KINDS.get(code) or ""
            if not ts:
                continue
            err_ts.append(ts)
            if ts < lo or kind not in KIND_LABEL:
                continue
            # 那一刻它叫什么：跟现在不一样才留，前端写成「现名（原 旧名）」
            was = _title_at(agg, ts)[:60]
            raw.append((ts, code, reset, kind, msg, proj, cur,
                        was if was and was != cur else "", gone, sid))
        for row in sw_rows:
            try:
                sts, frm, to = int(row[0]), str(row[1]), str(row[2])
            except (TypeError, ValueError, IndexError):
                continue
            if not sts or sts < lo:
                continue
            # 同一轮里只留第一次切换：Auto 一轮内会把模型来回调度好几次，
            # 全记下来的话 90 天能堆出上百条 hy4 ↔ glm-5.3-flash 的抖动。
            turns = agg.get("turns") or []
            if turns:
                ti = 0
                for i, tv in enumerate(turns):
                    if tv <= sts:
                        ti = i
                    else:
                        break
                key = (sid, turns[ti])
                if key in turned:
                    continue
                turned.add(key)
            after = any(0 <= sts - t <= SWITCH_AFTER_ERR_MS for t in err_ts)
            was = _title_at(agg, sts)[:60]
            sws.append({"at": sts, "frm": frm, "to": to, "auto": after,
                        "proj": proj, "title": cur,
                        "was": was if was and was != cur else "",
                        "gone": gone, "sid": sid})
    raw.sort(key=lambda x: x[0])
    out: list[dict] = []
    for ts, code, reset, kind, msg, proj, title, was, gone, sid in raw:
        prev = out[-1] if out else None
        # 合并要同一个会话：两个会话在同一个目录里先后被卡住是两件事，
        # 只看「同项目 + 同类型 + 10 分钟」会把 A 的标题配到 B 的事件上。
        if (prev and prev["kind"] == kind and prev["sid"] == sid
                and ts - prev["last"] <= QUOTA_MERGE_MS):
            prev["last"] = ts
            prev["n"] += 1
            prev["reset"] = max(prev["reset"], reset)
            if not prev["title"] and title:
                prev["title"] = title
            if not prev["was"] and was:
                prev["was"] = was
            if not prev["msg"] and msg:
                prev["msg"] = msg
        else:
            out.append({"code": code, "kind": kind, "at": ts, "last": ts, "n": 1,
                        "reset": reset, "proj": proj, "title": title,
                        "was": was, "gone": gone, "sid": sid, "msg": msg})
    out.reverse()                      # 最近的在最上面

    now = time.time() * 1000

    def _fmt(ms: int) -> str:
        return datetime.fromtimestamp(ms / 1000).strftime("%m-%d %H:%M") if ms else ""

    def _hm(ms: int) -> str:
        return datetime.fromtimestamp(ms / 1000).strftime("%H:%M") if ms else ""

    for e in out:
        e["at_s"] = _fmt(e["at"])
        e["hm"] = _hm(e["at"])
        # 早就过去的重置时间没意义，别挂在一段历史记录上冒充「马上能用」。
        # 另一个不算数的：这条限制之后已经跑成功过（换模型绕过去了），
        # 再写「23:58 重置」会让人以为还得等到那一刻。
        e["reset_s"] = (_fmt(e["reset"])
                        if e["reset"] > now and ok_ts <= e["last"] else "")
        e["kind_s"] = KIND_LABEL.get(e["kind"], e["kind"])
    for s in sws:
        s["at_s"] = _fmt(s["at"])
        s["hm"] = _hm(s["at"])
    sws.sort(key=lambda x: -x["at"])
    credit = [e for e in out if e["kind"] == "credit"]
    # 免费额度也算「额度超限」这一类：都要等重置，只是免费模型还能「消耗积分继续」
    quota = [e for e in out if e["kind"] in ("quota", "free")]
    other = [e for e in out if e["kind"] not in ("credit", "quota", "free")]
    # 现在还卡着吗：最近一段是「额度超限」、重置时间还没到、并且之后再没成功跑过。
    # 第三条最要紧 —— 换到别的模型接着干活后，那条 429 早就绕过去了，
    # 只按「重置时间没到」判断会在页面顶上挂一条假的「现在还被卡着」。
    blocked = 0
    if out and out[0]["kind"] in ("quota", "free") and out[0]["reset"] > now \
            and ok_ts <= out[0]["last"]:
        blocked = out[0]["reset"]
    return {
        "days": days,
        "events": out[:40],
        "credit_n": len(credit),        # 积分花完的次数（要花钱才能解）
        "quota_n": len(quota),          # 额度超限的次数（等重置）
        "free_n": sum(1 for e in out if e["kind"] == "free"),
        "other_n": len(other),          # 模型不可用 / 队列满 / 网络
        "retries": sum(e["n"] for e in out),
        "last": out[0]["last"] if out else 0,
        "last_s": _fmt(out[0]["last"]) if out else "",
        "last_kind": out[0]["kind"] if out else "",
        # 最后一次正常调用：页面拿它说明「没新记录不是没采到，是之后真的没再断」
        "last_ok": ok_ts,
        "last_ok_s": _fmt(ok_ts) if ok_ts else "",
        "blocked_until": blocked,
        "blocked_until_s": _fmt(blocked) if blocked else "",
        # 模型切换：跟中断放一份返回里 —— 用户关心的是「被打断之后换了什么」
        "switches": sws[:20],
        "switch_n": len(sws),
        "kinds": KIND_LABEL,
    }


_HOUR_WARM = {"done": False}


def scan_loop() -> None:
    while True:
        try:
            scan_sessions()
        except Exception as e:
            SCAN["err"] = str(e)
            SCAN["running"] = False
        # 小时账第一轮要把老文件全量回扫一遍（缓存从「最近 30 小时」扩到「30 天」），
        # 放在这里预热，别让用户翻到某天时在页面上等十秒。
        if not _HOUR_WARM["done"]:
            _HOUR_WARM["done"] = True
            try:
                hourly_tokens(fresh=True)
            except Exception as e:                      # noqa: BLE001
                print("hourly warm failed:", e, file=sys.stderr)
        # 120 秒兜一轮（以前 300）：页面来要数据时还会 scan_if_stale 补一次，
        # 这里只负责「没人看的时候也别落下太多」。
        time.sleep(120)


# ---------------------------------------------------------------- 口袋事件

_POCKET_SCRIPT = {
    "link_start": "存图.py", "link_done": "存图.py",
    "article_start": "文章.py", "article_done": "文章.py",
    "ocr_sweep": "ocr_runner.py", "ocr": "ocr_runner.py",
    "refetch": "ocr_runner.py",
    "heartbeat_done": "heartbeat.py", "heartbeat_scheduler": "heartbeat.py",
    "report_done": "report_runner.py",
    "dedupe_scheduler": "dedupe_runner.py", "dedupe_manual": "dedupe.py",
    "trends_scheduler": "trends.py",
    "wechat_guard_scheduler": "wechat_guard.py", "wechat_guard": "wechat_guard.py",
    "upload": "server.py", "share": "server.py", "rename": "server.py",
    "zip": "server.py", "trash": "server.py",
    "chat": "chat（本地模型）", "chat_quick": "chat（本地模型）",
    "inbox_group": "server.py", "inbox_name": "server.py",
}
_pocket_cache = {"mtime": 0.0, "rows": []}


def pocket_events(days: int = 0) -> dict:
    """口袋自己的事件流水：按脚本汇总次数 / 耗时 / 失败。"""
    if not POCKET_LOG.exists():
        return {"scripts": [], "recent": [], "tokens": 0}
    try:
        mt = POCKET_LOG.stat().st_mtime
    except OSError:
        return {"scripts": [], "recent": [], "tokens": 0}
    if _pocket_cache["mtime"] != mt:
        rows = []
        try:
            with POCKET_LOG.open("r", encoding="utf-8", errors="replace") as f:
                for ln in f:
                    if ln.startswith("{"):
                        try:
                            rows.append(json.loads(ln))
                        except ValueError:
                            continue
        except OSError:
            pass
        _pocket_cache.update({"mtime": mt, "rows": rows})
    rows = _pocket_cache["rows"]
    since = (datetime.now().timestamp() - days * 86400) if days else 0
    agg: dict[str, dict] = {}
    recent: list[dict] = []
    tokens = 0
    for r in rows:
        ts = str(r.get("ts") or "")
        when = 0.0
        try:
            when = datetime.strptime(ts[:19].replace("T", " "), "%Y-%m-%d %H:%M:%S").timestamp()
        except ValueError:
            when = 0.0
        if since and when and when < since:
            continue
        ev = str(r.get("event") or "")
        script = _POCKET_SCRIPT.get(ev)
        if not script:
            continue
        a = agg.setdefault(script, {"script": script, "n": 0, "ms": 0, "fail": 0, "last": ""})
        a["n"] += 1
        a["ms"] += int(r.get("ms") or 0)
        if r.get("ok") is False or r.get("error"):
            a["fail"] += 1
        if ts > a["last"]:
            a["last"] = ts
        if r.get("tokens"):
            tokens += int(r["tokens"])
        if len(recent) < 40 and ev in ("link_done", "article_done", "chat", "ocr_sweep"):
            recent.append({"at": ts[5:16].replace("T", " "), "event": ev,
                           "script": script,
                           "detail": str(r.get("title") or r.get("text") or r.get("names") or "")[:60],
                           "ms": int(r.get("ms") or 0),
                           "ok": not (r.get("ok") is False or r.get("error"))})
    scripts = sorted(agg.values(), key=lambda x: -x["n"])
    recent.reverse()
    return {"scripts": scripts, "recent": recent[:20], "tokens": tokens}


# ---------------------------------------------------------------- 折算（token → 点数 → 钱）

PRICE_DEFAULT = {
    "note": "价格单位：元 / 百万 token。points_per_mtok = 每百万 token 折多少点数；"
            "填 null 表示这套计费没有点数概念（页面会把点数那栏画成「—」）。"
            "数字是估算值，不是账单 —— 想对账就改这里。",
    "currency": "¥",
    "models": {
        "deepseek-v4.1-flash": {"in": 2.0, "cached": 0.2, "out": 3.0, "points_per_mtok": None},
        "deepseek-v4.1": {"in": 2.0, "cached": 0.2, "out": 3.0, "points_per_mtok": None},
        "qwen3.5-4b": {"in": 0, "cached": 0, "out": 0, "points_per_mtok": None, "local": True},
        "claude-3-5-sonnet": {"in": 21.3, "cached": 2.1, "out": 106.5, "points_per_mtok": None},
    },
    "default": {"in": None, "cached": None, "out": None, "points_per_mtok": None},
}


def price_config() -> dict:
    cfg = _json_read(PRICE_PATH, None)
    if not isinstance(cfg, dict) or not cfg.get("models"):
        _json_write(PRICE_PATH, PRICE_DEFAULT)
        return json.loads(json.dumps(PRICE_DEFAULT))
    merged = json.loads(json.dumps(PRICE_DEFAULT))
    merged.update({k: v for k, v in cfg.items() if k != "models"})
    merged["models"] = {**PRICE_DEFAULT["models"], **(cfg.get("models") or {})}
    return merged


def _price_for(model: str, cfg: dict) -> dict:
    models = cfg.get("models") or {}
    if model in models:
        return models[model]
    low = model.lower()
    for k, v in models.items():
        if low.startswith(k.lower()) or k.lower() in low:
            return v
    return cfg.get("default") or {}


def _explicit_price(model: str, cfg: dict) -> dict | None:
    """只认价表里点名的模型，不吃 default 兜底（自报账用，见定价那一轮）。"""
    models = cfg.get("models") or {}
    if model in models:
        return models[model]
    low = model.lower()
    for k, v in models.items():
        if low.startswith(k.lower()) or k.lower() in low:
            return v
    return None


def _cost_of(stat: dict, price: dict) -> tuple[float | None, float | None]:
    """返回 (点数, 金额)；缺价格就 None（页面上就是一条横杠）。"""
    total = stat.get("in", 0) + stat.get("out", 0)
    cached = min(stat.get("cached", 0), stat.get("in", 0))
    fresh = max(0, stat.get("in", 0) - cached)
    pm = price.get("points_per_mtok")
    pts = None if pm is None else total / 1e6 * float(pm)
    ins, outs, cs = price.get("in"), price.get("out"), price.get("cached")
    if ins is None and outs is None:
        # 只有积分单价、没拆 in/cached/out 时，用「元/百万 token」混合单价兜底。
        # 该值 = points_per_mtok × 每点多少钱，是 99% 缓存命中后的均值，不是标价。
        cpm = price.get("cny_per_mtok")
        cost = None if cpm is None else total / 1e6 * float(cpm)
    else:
        cost = (fresh * float(ins or 0) + cached * float(cs if cs is not None else (ins or 0))
                + stat.get("out", 0) * float(outs or 0)) / 1e6
    return pts, cost


# ---------------------------------------------------------------- 汇总

RANGES = {"today": ("今天", 1), "7d": ("7 天", 7), "30d": ("30 天", 30),
          "all": ("全部", 0)}


def _range_days(key: str) -> int:
    return RANGES.get(key, RANGES["7d"])[1]


def _day_keys(days: int) -> list[str]:
    if not days:
        return []
    today = date.today()
    return [(today - timedelta(days=i)).isoformat() for i in range(days - 1, -1, -1)]


def _apps_with_icons(apps: list[dict]) -> list[dict]:
    """补 icon（png 文件名，空＝还没转出来或不是 .app）和 kind（app/sys/kernel/script/bin）。"""
    out = []
    for i, a in enumerate(apps):
        if i < _ICON_MAX * 2:
            a = dict(a)
            path = a.get("path") or ""
            a["icon"] = icon_name(a["name"], path)
            a["kind"] = proc_kind(a["name"], path)
            # 系统自带的 bundle 里没 icns 的（XProtect 这类），与其给个首字母，
            # 不如归到「系统」用齿轮 —— 首字母在系统进程上没有信息量
            if not a["icon"] and a["kind"] == "app" and path.startswith(("/System/", "/Library/Apple/")):
                a["kind"] = "sys"
        out.append(a)
    return out


# 客户端 ↔ 进程：左边那棵树上的色块要和进程榜里同一个软件同色，靠 owners 对起来
def _client_proc(cid: str) -> dict:
    if cid not in _OWNER_PAT:
        return {}
    pat = _OWNER_PAT[cid]
    best = None
    for a in (_PROC.get("apps") or []):
        mine = cid in (a.get("owners") or []) or pat.search(f"{a.get('path') or ''} {a['name']}")
        if not mine:
            continue
        if best is None or a.get("mem", 0) > best.get("mem", 0):
            best = a
    if not best:
        return {}
    return {"name": best["name"], "icon": icon_name(best["name"], best.get("path") or ""),
            "kind": proc_kind(best["name"], best.get("path") or "")}


def clients_view(days: int, cfg: dict) -> list[dict]:
    """谁在干活：WorkBuddy（模型 / 工具 / MCP / 技能 / 专家）+ 口袋（按脚本）。"""
    sess = sessions_snapshot()
    keep = set(_day_keys(days)) if days else None
    wb = {"id": "workbuddy", "name": "WorkBuddy", "glyph": "✦", "kind": "client",
          "calls": 0, "asks": 0, "ms": 0,
          "tokens": {"in": 0, "out": 0, "cached": 0},
          "kids": [], "sessions": 0, "last": ""}
    models: dict[str, dict] = {}
    tools: Counter = Counter()
    mcps: Counter = Counter()
    skills: Counter = Counter()
    agents: Counter = Counter()
    modes: Counter = Counter()
    experts: Counter = Counter()
    for s in sess:
        used = False
        for day, dm in (s.get("days") or {}).items():
            if keep and day not in keep:
                continue
            used = True
            wb["calls"] += dm.get("calls", 0)
            wb["asks"] += dm.get("asks", 0)
            for k in ("in", "out", "cached"):
                wb["tokens"][k] += dm.get(k, 0)
            tools.update(dm.get("tools") or {})
            for m, v in (dm.get("models") or {}).items():
                cur = models.setdefault(m, {"in": 0, "out": 0, "cached": 0, "n": 0})
                for k in ("in", "out", "cached", "n"):
                    cur[k] += v.get(k, 0)
        if not used:
            continue
        wb["sessions"] += 1
        for k, cnt in (s.get("mcps") or {}).items():
            mcps[k] += cnt
        for k, cnt in (s.get("skills") or {}).items():
            skills[k] += cnt
        for k, cnt in (s.get("agents") or {}).items():
            agents[k] += cnt
        for k, cnt in (s.get("modes") or {}).items():
            modes[k] += cnt
        for k, cnt in (s.get("experts") or {}).items():
            experts[k] += cnt
        ts = s.get("last") or 0
        if ts:
            at = datetime.fromtimestamp(ts / 1000).strftime("%m-%d %H:%M")
            if at > wb["last"]:
                wb["last"] = at

    kids = []
    for m, v in sorted(models.items(), key=lambda kv: -(kv[1]["in"] + kv[1]["out"])):
        pts, cost = _cost_of(v, _price_for(m, cfg))
        kids.append({"kind": "model", "name": m, "n": v["n"],
                     "tokens": {"in": v["in"], "out": v["out"], "cached": v["cached"]},
                     "points": pts, "cost": cost})
    # 模式和专家紧跟模型：它们回答的是「这些轮是怎么跑起来的」，比「用过哪些
    # 工具」更靠近钱和人。agents 是 Agent/Task 子代理，跟「专家」不是一回事 ——
    # 那个标签在页面上叫「子代理」，别混。
    for md, n in modes.most_common():
        kids.append({"kind": "mode", "name": md, "n": n})
    for ex, n in experts.most_common():
        kids.append({"kind": "expert", "name": ex, "n": n})
    for srv, n in mcps.most_common():
        kids.append({"kind": "mcp", "name": srv, "n": n})
    for sk, n in skills.most_common():
        kids.append({"kind": "skill", "name": sk, "n": n})
    for ag, n in agents.most_common():
        kids.append({"kind": "agent", "name": ag, "n": n})
    for t, n in tools.most_common(24):
        kids.append({"kind": "tool", "name": t, "n": n})
    wb["kids"] = kids

    out = [wb]
    tr = trae_work.client_view(days)
    if tr:                                  # TRAE 哪怕对话读不到，也能靠命令/进程入账
        tr["proc"] = _client_proc("trae")
        out.append(tr)

    pk = pocket_events(days)
    if pk["scripts"]:
        n = sum(s["n"] for s in pk["scripts"])
        ms = sum(s["ms"] for s in pk["scripts"])
        out.append({
            "id": "pocket", "name": "口袋", "glyph": "◈", "kind": "client",
            "calls": n, "asks": 0, "ms": ms, "sessions": 0, "last": "",
            "tokens": {"in": 0, "out": 0, "cached": 0},
            "kids": [{"kind": "script", "name": s["script"], "n": s["n"], "ms": s["ms"],
                      "fail": s["fail"]} for s in pk["scripts"]],
        })
    for c in out:
        c["proc"] = _client_proc(c["id"])
    return out


# ---------------------------------------------------------------- 每小时 token
# 「花费与用量」第三个视图要的是「今天每个小时烧了多少」—— 会话 agg 里只有按天
# 的 days，没有小时。重算全量（481 MB）不划算，所以单开一份只抠 usage 行的轻量
# 账：先用子串把 99% 的非 usage 行筛掉再 json.loads，单文件 170 MB 也就一两秒；
# 之后按 offset 增量读，页面每次来要数据只读新增的那几行。
HOURLY_PATH = OWN_DIR / "缓存" / "小时.json"
_HOUR = {"v": 2, "files": {}, "at": 0.0}
_HOUR_LOCK = threading.Lock()
_HOUR_TTL = 45.0
_HOUR_DAYS = 30                      # 缓存留 30 天：日期能往回翻，不能只有最近一天

# 谁的日志 → 哪个客户端。目前只有 WorkBuddy 把用量写在本机；以后 Claude Code /
# Codex 也落了这种日志，在这一张表里加一行就自动多一根柱子，别处不用动。
_HOUR_ROOTS = [(WB_ROOT, "workbuddy")]


def _hour_key(ms: int) -> str:
    return datetime.fromtimestamp(ms / 1000.0).strftime("%Y-%m-%dT%H")


def _norm_day(s: str) -> str:
    """只认 'YYYY-MM-DD'，别的（空串 / 乱写 / 未来的日子）一律给今天。"""
    s = str(s or "").strip()
    if re.fullmatch(r"\d{4}-\d{2}-\d{2}", s):
        try:
            datetime.strptime(s, "%Y-%m-%d")
            return s
        except ValueError:
            pass
    return datetime.now().strftime("%Y-%m-%d")


def _hour_scan_file(path: Path, prev: dict | None, app: str = "workbuddy") -> dict:
    try:
        st = path.stat()
    except OSError:
        return prev or {}
    same = (prev and prev.get("ino") == st.st_ino
            and prev.get("size", 0) <= st.st_size and prev.get("hours") is not None)
    if same:
        off = int(prev.get("off") or 0)
        hours = prev["hours"]
    else:
        off = 0
        hours = {}
    if st.st_size == off:
        if prev:
            prev.update({"size": st.st_size, "mtime": st.st_mtime, "ino": st.st_ino})
            return prev
        return {}
    try:
        with path.open("rb") as f:
            f.seek(off)
            buf = f.read()
    except OSError:
        return prev or {}
    cut = buf.rfind(b"\n")
    if cut < 0:
        return prev or {}
    chunk = buf[:cut + 1]
    for raw in chunk.splitlines():
        if b'"usage"' not in raw:
            continue
        try:
            d = json.loads(raw)
        except ValueError:
            continue
        pd = d.get("providerData") or {}
        u = pd.get("usage") or {}
        if not isinstance(u, dict) or not u.get("totalTokens"):
            continue
        ts = int(d.get("timestamp") or 0)
        if not ts:
            continue
        k = _hour_key(ts)
        b = hours.setdefault(k, {"in": 0, "out": 0, "cached": 0, "calls": 0,
                                 "models": {}, "apps": {}})
        b.setdefault("apps", {})
        tin = int(u.get("inputTokens") or u.get("input_tokens") or 0)
        tout = int(u.get("outputTokens") or u.get("output_tokens") or 0)
        cache = 0
        det = u.get("inputTokensDetails") or []
        if isinstance(det, list) and det and isinstance(det[0], dict):
            cache = int(det[0].get("cached_tokens") or 0)
        model = str(pd.get("model") or pd.get("requestModelName") or "未知模型")
        b["in"] += tin
        b["out"] += tout
        b["cached"] += cache
        b["calls"] += 1
        m = b["models"].setdefault(model, {"in": 0, "out": 0, "cached": 0, "calls": 0})
        m["in"] += tin
        m["out"] += tout
        m["cached"] += cache
        m["calls"] += 1
        # 客户端维度：同一个桶里可能混着两个客户端，各记各的
        a = b["apps"].setdefault(app, {"in": 0, "out": 0, "cached": 0, "calls": 0})
        a["in"] += tin
        a["out"] += tout
        a["cached"] += cache
        a["calls"] += 1
    # 老桶直接扔掉：文件一直在长，不清理的话缓存会跟着会话数一辈子长下去
    keep_days = {(datetime.now() - timedelta(days=i)).strftime("%Y-%m-%d")
                 for i in range(_HOUR_DAYS)}
    hours = {k: v for k, v in hours.items() if k[:10] in keep_days}
    return {"v": 2, "off": off + cut + 1, "size": st.st_size,
            "mtime": st.st_mtime, "ino": st.st_ino, "app": app, "hours": hours}


def _hour_render(state: dict, day: str) -> list[dict]:
    """补空桶：某个小时没调用也要占一根 0 柱，否则时间轴会被压缩成「有活的小时」。"""
    keys = [f"{day}T{h:02d}" for h in range(24)]
    want = set(keys)
    agg: dict[str, dict] = {}
    for e in (state.get("files") or {}).values():
        for k, v in (e.get("hours") or {}).items():
            if k not in want:
                continue
            cur = agg.setdefault(k, {"in": 0, "out": 0, "cached": 0, "calls": 0,
                                     "models": {}, "apps": {}})
            for f in ("in", "out", "cached", "calls"):
                cur[f] += v.get(f, 0)
            for m, mv in (v.get("models") or {}).items():
                mc = cur["models"].setdefault(m, {"in": 0, "out": 0, "cached": 0, "calls": 0})
                for f in ("in", "out", "cached", "calls"):
                    mc[f] += mv.get(f, 0)
            for a, av in (v.get("apps") or {}).items():
                ac = cur["apps"].setdefault(a, {"in": 0, "out": 0, "cached": 0, "calls": 0})
                for f in ("in", "out", "cached", "calls"):
                    ac[f] += av.get(f, 0)
    out = []
    for k in keys:
        v = agg.get(k) or {"in": 0, "out": 0, "cached": 0, "calls": 0,
                           "models": {}, "apps": {}}
        out.append({"key": k, "h": k[-2:], "in": v["in"], "out": v["out"],
                    "cached": v["cached"], "calls": v["calls"],
                    "models": v["models"], "apps": v["apps"]})
    return out


def hourly_tokens(day: str = "", fresh: bool = False) -> list[dict]:
    """某一天的 24 根柱子。day 为空就是今天。"""
    day = _norm_day(day)
    with _HOUR_LOCK:
        if not fresh and (time.time() - float(_HOUR.get("at") or 0)) < _HOUR_TTL:
            return _hour_render(_HOUR, day)
        if not _HOUR.get("files"):
            disk = _json_read(HOURLY_PATH, {})
            if isinstance(disk, dict) and disk.get("v") == 2:
                _HOUR["files"] = disk.get("files") or {}
        files = {}
        prev = _HOUR.get("files") or {}
        now = time.time()
        cand = []
        for root, app in _HOUR_ROOTS:
            if not root.exists():
                continue
            for p in root.glob("*/*.jsonl"):
                try:
                    mt = p.stat().st_mtime
                except OSError:
                    continue
                # 30 天内动过的必须扫；没动过但缓存里没它，也补扫一次 ——
                # 不然「翻到上个月某天」永远是空的。
                if now - mt < _HOUR_DAYS * 86400 or str(p) not in prev:
                    cand.append((mt, p, app))
        cand.sort(key=lambda x: -x[0])       # 刚说过话的会话先扫：页面先看到它
        for _, p, app in cand:
            e = _hour_scan_file(p, prev.get(str(p)), app)
            if e:
                files[str(p)] = e
        # 自报账：不进 _HOUR_ROOTS（那不是日志目录，格式也不同），单独折成同样的
        # 小时桶塞进来。key 用 '@' 开头，一眼能看出不是真文件；_hour_render 只认
        # hours，所以下面「按 agent」那根柱子会自动多出 trae 这一层，前端不用改。
        if agentbus is not None:
            try:
                ah = agentbus.hour_buckets(AI_ROOT, _HOUR_DAYS)
                if ah:
                    files["@agents/usage.jsonl"] = {
                        "v": 2, "off": 0, "size": 0, "mtime": 0, "ino": 0,
                        "app": "agents", "hours": ah}
            except Exception as e:                     # noqa: BLE001
                print("agent usage failed:", e, file=sys.stderr)
        _HOUR.update({"v": 2, "files": files, "at": time.time()})
        # 落盘时剔掉 '@' 那条：它是每次现算的，存进缓存只会在重启后变成
        # 一份谁也不更新的旧账（prev 里同一个 key 不会被 cand 覆盖）。
        _json_write(HOURLY_PATH, {"v": 2, "files": {
            k: v for k, v in files.items() if not k.startswith("@")}})
        return _hour_render(_HOUR, day)


def hourly_days() -> list[str]:
    """有哪些天有账（倒序）—— 日期箭头按这个翻，翻不到空白天。"""
    with _HOUR_LOCK:
        st = _HOUR
    ds = set()
    for e in (st.get("files") or {}).values():
        for k in (e.get("hours") or {}):
            ds.add(k[:10])
    return sorted(ds, reverse=True)


def hourly_bundle(day: str = "", fresh: bool = False) -> dict:
    """页面切日期只要这一份：那天的 24 桶 + 能选的日子。"""
    d = _norm_day(day)
    hours = hourly_tokens(d, fresh=fresh)
    return {"day": d, "today": datetime.now().strftime("%Y-%m-%d"),
            "hours": hours, "days": hourly_days()}


def _billing_est(cherry: dict) -> dict:
    """把 Cherry 账本里「按月、按供应商」的估算折成 billing 要的形状：
    {"dashscope": {"by_ym": {"2026-08": {"cny": x, "calls": n}, ...}}}"""
    est: dict[str, dict] = {}
    for m in (cherry.get("months") or []):
        ym = m.get("ym") or ""
        for k, v in (m.get("by_vendor") or {}).items():
            est.setdefault(k, {"by_ym": {}})["by_ym"][ym] = {
                "cny": (v or {}).get("cny") or 0,
                "calls": (v or {}).get("calls") or 0}
    return est


def billing_payload(est: dict | None = None) -> dict:
    """计费账号那一整块。est 不传就自己去 Cherry 账本里摘本月的按供应商估算。"""
    if est is None:
        try:
            est = _billing_est(cherry_usage.view(0))
        except Exception:                              # noqa: BLE001
            est = {}
    return billing.view(est)


def overview(range_key: str = "7d", fresh: bool = False) -> dict:
    days = _range_days(range_key)
    cfg = price_config()
    with _proc_lock:
        proc = json.loads(json.dumps(_PROC))
        hist = list(_HIST)
    mem = _mem()
    disk = _disk()
    try:
        load = [round(x, 2) for x in os.getloadavg()]
    except (OSError, AttributeError):               # Windows 没有 getloadavg
        load = [0.0, 0.0, 0.0]

    clients = clients_view(days, cfg)

    # 每小时 token（第三视图）。单独一份轻量账，坏了也不能拖垮整页数据。
    # 默认给「今天」的 24 个自然小时；翻别的日子走 /api/hourly，不重拉这一大坨。
    try:
        hourly = hourly_tokens()
        hour_days = hourly_days()
    except Exception as e:
        hourly, hour_days = [], []
        print("hourly failed:", e, file=sys.stderr)

    # 额度中断（积分花完 / 周期额度用完）。固定 90 天：这种事一两个月才几次，
    # 跟着「今天 / 7 天」的 chip 走只会让这张表大部分时间是空的。
    try:
        quota = quota_events(90)
    except Exception as e:
        quota = {"days": 90, "events": [], "credit_n": 0, "quota_n": 0, "retries": 0,
                 "last": 0, "last_s": "", "last_kind": "", "blocked_until": 0,
                 "blocked_until_s": "", "last_ok": 0, "last_ok_s": "",
                 "switches": [], "switch_n": 0, "free_n": 0, "other_n": 0,
                 "kinds": KIND_LABEL, "err": str(e)}
        print("quota failed:", e, file=sys.stderr)

    # Cherry Studio 的实付账（阿里百炼 / 硅基流动）。这一块是**真金白银**：
    # WorkBuddy 那边是「积分换 token」，Cherry 是「按 token 直接扣账户余额」，
    # 两套货币不能混在一张表里加总，所以单独一块、单独标价口径。
    try:
        cherry = cherry_usage.view(days)
    except Exception as e:                             # noqa: BLE001
        cherry = {"found": False, "err": str(e), "cny": 0.0, "cny_cherry": 0.0,
                  "calls": 0, "models": [], "by_day": [], "unpriced": []}
        print("cherry failed:", e, file=sys.stderr)

    # 计费账号：百炼 / 硅基流动这些都有折扣、代金券、赠送额度，价表乘 token 算出来的
    # 数永远对不上账单。这里把「官方的余额和本月账单」也取进来，跟本地估算并排放，
    # 差多少一眼看得出 —— 差得多就说明有折扣或赠送，别拿估算当准数。
    try:
        bill = billing_payload(_billing_est(cherry))
    except Exception as e:                             # noqa: BLE001
        bill = {"ok": False, "err": str(e), "accounts": [], "n": 0,
                "total_balance": 0, "total_month": 0, "store": ""}
        print("billing failed:", e, file=sys.stderr)

    # token 明细（按天 / 按模型）
    keep = set(_day_keys(days)) if days else None
    by_day: dict[str, dict] = {}
    by_model: dict[str, dict] = {}
    for s in sessions_snapshot():
        for day, dm in (s.get("days") or {}).items():
            if keep and day not in keep:
                continue
            b = by_day.setdefault(day, {"d": day, "in": 0, "out": 0, "cached": 0, "calls": 0})
            b["in"] += dm.get("in", 0)
            b["out"] += dm.get("out", 0)
            b["cached"] += dm.get("cached", 0)
            b["calls"] += dm.get("calls", 0)
            for m, v in (dm.get("models") or {}).items():
                cur = by_model.setdefault(m, {"model": m, "in": 0, "out": 0,
                                              "cached": 0, "n": 0})
                for k in ("in", "out", "cached", "n"):
                    cur[k] += v.get(k, 0)
    # 自报账并进同一份按天 / 按模型：它记的就是 token，跟 WorkBuddy 的账同口径，
    # 单开一块等于让人自己心算。差别只有一处 —— 订阅制的客户端没有单价，
    # 标 src="agent"，下面定价那一轮就不会把它算进「未定价模型」里（那不是漏填，
    # 是本来就没有单价）；真填了 cost（按量付费）才照实计钱。
    if agentbus is not None:
        try:
            for day, dm in agentbus.by_day(AI_ROOT, days).items():
                if keep and day not in keep:
                    continue
                b = by_day.setdefault(day, {"d": day, "in": 0, "out": 0,
                                            "cached": 0, "calls": 0})
                b["in"] += dm.get("in", 0)
                b["out"] += dm.get("out", 0)
                b["cached"] += dm.get("cached", 0)
                b["calls"] += dm.get("calls", 0)
                for m, v in (dm.get("models") or {}).items():
                    cur = by_model.setdefault(m, {"model": m, "in": 0, "out": 0,
                                                  "cached": 0, "n": 0})
                    for k in ("in", "out", "cached", "n"):
                        cur[k] += v.get(k, 0)
                    cur["src"] = "agent"
                    if v.get("cost") is not None:
                        cur["cost_paid"] = round((cur.get("cost_paid") or 0)
                                                 + float(v["cost"]), 4)
        except Exception as e:                         # noqa: BLE001
            print("agent usage overview failed:", e, file=sys.stderr)
    rows = []
    tot_pts = 0.0
    tot_cost = 0.0
    any_pts = False
    any_cost = False
    priced_tokens = 0
    all_tokens = 0
    unpriced: list[str] = []
    for m, v in sorted(by_model.items(), key=lambda kv: -(kv[1]["in"] + kv[1]["out"])):
        # 自报账只按「价表里写明了这个模型」才算钱：订阅制的客户端（Trae 等）
        # 本来就没有按 token 的单价，套 default 会凭空造出一笔积分消耗。
        # 它自己填了 cost 才照实计入（按量付费那种）。
        is_agent = v.get("src") == "agent"
        price = (_explicit_price(m, cfg) if is_agent else _price_for(m, cfg)) or {}
        pts, cost = _cost_of(v, price)
        if is_agent and v.get("cost_paid") is not None:
            cost = round((cost or 0) + float(v["cost_paid"]), 4)
        tok = v["in"] + v["out"]
        all_tokens += tok
        # 「有价」= 能算出钱，或者能算出积分。只认钱的话，纯积分口径会被报成
        # 「已定价 0%」，看着像全都没填价格。
        if cost is not None or pts is not None:
            priced_tokens += tok
        elif not is_agent:
            unpriced.append(m)      # 订阅制没单价不算「漏填」，别塞进未定价清单
        if pts is not None:
            tot_pts += pts
            any_pts = True
        if cost is not None:
            tot_cost += cost
            any_cost = True
        row = {"model": m, "in": v["in"], "out": v["out"], "cached": v["cached"],
               "total": tok, "n": v["n"],
               "points": pts, "cost": cost}
        if is_agent:
            row["src"] = "agent"
        rows.append(row)
    totals = {"in": sum(r["in"] for r in rows), "out": sum(r["out"] for r in rows),
              "cached": sum(r["cached"] for r in rows), "calls": sum(r["n"] for r in rows)}
    totals["total"] = totals["in"] + totals["out"]

    # 工具总榜（跨客户端，WorkBuddy 那边占绝大多数）
    return {
        "now": datetime.now().strftime("%Y-%m-%d %H:%M:%S"),
        "range": {"key": range_key, "label": RANGES.get(range_key, RANGES["7d"])[0],
                  "days": days},
        "machine": {
            "disk": disk, "extra_disks": _extra_disks(force=fresh), "mem": mem, "load": load,
            "net": _net(), "power": _power(), "gpu": _gpu(),
            "cpus": os.cpu_count() or 1, "uptime_h": _uptime_h(),
            "cpu_pct": proc.get("cpu_true") or proc.get("cpu_pct") or 0.0,
            "cpu_ready": bool(proc.get("cpu_ready")),
            "cpu_proc_pct": proc.get("cpu_pct") or 0.0,
            "rss_total_gb": round((proc.get("total_rss") or 0) / 1024, 2),
        },
        "series": snapshots(max(days, 1) if days else 7),
        # 这台是什么机器 / 装了哪些软件 / 装了哪些常驻服务（三段各有自己的缓存，
        # 不会每 25 秒去 fork system_profiler 和 brew）
        "spec": specs(),
        "installed": software(),
        "units": units(),
        "apps": _apps_with_icons(proc.get("apps") or []),
        # 按**时间**截一小时，不是按笔数：重启后 _HIST 前段是从 进程.jsonl 灌回来的
        # 2 分钟一笔的老记录，跟后面 10 秒一笔的挤在一起，按 360 笔截出来会变成
        # 两小时宽，横轴跟着就撒谎了。
        "app_series": _app_series([h for h in hist if (h.get("at") or 0) >= time.time() - 3600]),
        "netproc": {"ok": _NETP["ok"], "every": _NETP_EVERY,
                    "err": _NETP["err"]},
        "clients": clients,
        "tokens": {
            "by_day": [by_day[k] for k in sorted(by_day)],
            "by_model": rows,
            "totals": totals,
            "hourly": hourly,
            "hour_days": hour_days,
            "hour_day": datetime.now().strftime("%Y-%m-%d"),
            "cache_hit": round(totals["cached"] / totals["in"], 3) if totals["in"] else None,
        },
        # 「什么时候因为没额度停了」：跟 tokens / money 并列，不是它们的子集
        "quota": quota,
        # Cherry Studio 的实付账：跟 money 并列，但口径不同（人民币原价，不是积分）
        "cherry": cherry,
        # 计费账号（余额 / 官方账单）：比「估算」更硬的第二个口径
        "billing": bill,
        "money": {
            "currency": cfg.get("currency") or "¥",
            "rows": rows,
            "points": round(tot_pts, 1) if any_pts else None,
            "cost": round(tot_cost, 2) if any_cost else None,
            "any_points": any_pts, "any_cost": any_cost,
            "priced_ratio": round(priced_tokens / all_tokens, 3) if all_tokens else None,
            "unpriced": unpriced[:6],
            "formula": ("积分 = 总 token ÷ 100 万 × 该模型实测积分单价（不是官方 API 价）"
                        if (any_pts and not any_cost) else
                        "输入(未命中)×单价 + 输入(缓存命中)×缓存价 + 输出×输出价；单位 元/百万 token"),
            "points_formula": "点数 = 总 token ÷ 100 万 × points_per_mtok",
            "config": str(PRICE_PATH),
        },
        "pocket": pocket_events(days),
        "daily": daily_history(min(days or 30, 30)),
        "scan": dict(SCAN),
        "procs_path": str(PROC_PATH),
    }


_SER_KEYS = ("cpu", "mem", "dw", "dr", "net", "pwr")


def _hist_val(v, i: int) -> float:
    """_HIST 里每个进程是六个数；老格式（重启后从 进程.jsonl 灌回来的）只有一个 CPU。"""
    try:
        if isinstance(v, (list, tuple)):
            return float(v[i]) if i < len(v) and v[i] is not None else 0.0
        return float(v) if i == 0 and v is not None else 0.0
    except (TypeError, ValueError):
        return 0.0


def _app_series(hist: list[dict], step: int = 3, per: int = 3, cap: int = 12) -> dict:
    """近一小时的逐进程走势：六个维度各一串点，同一根时间轴。

    给「进程消耗」那张卡画折线用 —— 它要能切 CPU / 内存 / 磁盘写 / 网络 / 耗电，
    所以不能只回 CPU 一条。降采样到 3 个 tick 一个点（30 秒），640px 宽的图
    一个点正好 5px，再密就看不清了，也太占响应体。

    hack：六个维度各取峰值前 `per` 名再合并去重 —— 只在某一维冒尖的进程
    （比如只写盘、不占 CPU 的备份）也要留在候选里，否则切到那一维就是空图。
    """
    if not hist:
        return {"at": [], "step": _TICK_SECS * step, "keys": list(_SER_KEYS),
                "cpu": [], "apps": []}
    picked = hist[::step]
    if picked[-1] is not hist[-1]:
        picked.append(hist[-1])
    names: list[str] = []
    for i in range(len(_SER_KEYS)):
        peak: dict[str, float] = {}
        for h in picked:
            for n, v in (h.get("apps") or {}).items():
                x = _hist_val(v, i)
                if x > peak.get(n, -1.0):
                    peak[n] = x
        for n, _ in sorted(peak.items(), key=lambda kv: -kv[1])[:per]:
            if n not in names:
                names.append(n)
    out = []
    for n in names[:cap]:
        cols = [[round(_hist_val((h.get("apps") or {}).get(n), i), 1) for h in picked]
                for i in range(len(_SER_KEYS))]
        out.append({"name": n, "v": cols})
    return {"at": [datetime.fromtimestamp(h["at"]).strftime("%H:%M") for h in picked],
            "step": _TICK_SECS * step, "keys": list(_SER_KEYS),
            "cpu": [round(h.get("cpu") or 0, 1) for h in picked],
            "apps": out}


# ---------------------------------------------------------------- 逐轮工作记录
# 数据流方向：**这一页是数据源，工作台那张卡片是调用方**（跟资源卡片读
# /api/overview 一个方向）。所以逐轮记录必须在这里从会话 jsonl 解析出来，
# 不能反过来去读工作台的 /api/dash —— 那一来工作台没开这里就空，二来
# 「数据源」和「展示端」的关系就颠倒了。
#
# 解析口径只有一份：按路径 importlib 装 pocket/dash.py，直接调它的
# _session_reqs / _req_item（纯函数，模块级无副作用，实测 0.04s）。
# 这样工作台卡片调用本页接口拿到的行，跟它自己解析出来的一模一样，
# 不会出现两边字段名/截断规则漂移。
POCKET_DASH = POCKET_LOG.parent / "dash.py"
# 2026-11-24：按档位分桶 + 先用旧账、后台刷新（见 rounds_items 的说明）。
# 原来是一个槽位、TTL 8 秒；重算一锅 1.5~2.5 秒，前端 15 秒轮询一次，
# 等于每次打开都撞在重算上。现在过期也先回旧账，另起线程补算。
_ROUNDS_CACHE: dict[str, object] = {"by": {}, "busy": set()}
_ROUNDS_LOCK = threading.Lock()
_ROUNDS_BUILD_LOCK = threading.Lock()
_ROUNDS_TTL = 8.0
_ROUNDS_MOD: dict[str, object] = {"m": None}

_WB_CLIENTS = (("WorkBuddy", "_wb_items"),
               ("Cherry Studio", "_cherry_items"),
               ("Claude Code", "_claude_items"))


# ---------------------------------------------------------------- 任务命名
# 逐轮记录里 task 是用户输入的原话（一整句命令），又长又难扫。这里抽「动作+对象」
# 生成一个短标题，原文仍留在 task/detail 里，展开就能看。纯规则、不调模型。
_TITLE_ACTIONS = [
    # 位置/摆放类优先，别被后面的「恢复」误判成修复
    ("移到|挪到|放在|放到|插回|拔掉|弹出|换位置|移动位置|拖动|调大小|缩放", "调整"),
    ("谁在做|谁做的|是不是.*做的|到底是谁", "核查"),
    ("确认|核查|查下|查一下|检查下|检查一下|排查|看看为什么|看下为什么|查清", "核查"),
    ("修复|修好|修正|改正|恢复|解决", "修复"),
    ("新增|增加|添加|加上|加一个|加个|新建|做一个|做成", "新增"),
    ("移除|去掉|删掉|删除|拿掉|不用再|取消", "移除"),
    ("改成|改为|换成|调整|修改|优化|重做|重新设计|重构|第二行", "调整"),
    ("补充|填进去|录入|补录|记录下|也记录|也要显示", "补充"),
    ("设计|先别做|先分析|想想|想个办法|出个方案|给个思路", "设计"),
    ("显示|展示", "显示"),
    ("记录|追踪|监控|统计", "记录"),
    ("下载|抓取|爬取|拉取|采集|刮削", "采集"),
    ("导入|迁移|放到|转移", "迁移"),
]
# 具体业务对象（优先级高）
_TITLE_TOPICS_SPEC = [
    "外置磁盘", "外置盘", "外挂的磁盘", "移动硬盘", "项目token", "token", "点数", "折算",
    "显存", "耗电", "功耗", "网速", "网络", "磁盘", "内存", "大模型颜色", "模型颜色",
    "任务清单", "工作流水", "逐轮记录", "具体工作记录", "工作记录", "磁盘使用",
    "指标列", "资源视图", "媒体墙", "电影墙", "海报墙", "电视墙", "海报", "我的观看",
    "打分", "评分", "收藏", "打tag", "存图", "存文章", "文章", "图片", "ocr",
    "微信", "链接", "下载工作流", "文件传输", "记账", "行程", "待办", "日记", "备忘",
    "关注追踪", "巡检", "日报周报", "报告", "封面", "标签", "分类", "回收站", "command z",
    "claude code", "cherry studio", "deepseek", "4b模型", "4b", "nas", "zip", "github",
    "模型", "专家", "模式", "技能", "mcp", "连接器", "子代理", "代理",
]
# 泛化对象：只有当句子里没有具体词时才用它，避免「本机工作/workbuddy」盖掉「token」
_TITLE_TOPICS_GENERIC = [
    "本机工作", "口袋工作", "工作台", "页面", "卡片", "布局", "视图", "表格", "大小位置",
    "任务名", "项目名称", "日期", "顶区", "agent", "workbuddy", "pocket", "trae",
]
_TITLE_TOPICS = _TITLE_TOPICS_SPEC + _TITLE_TOPICS_GENERIC
_TITLE_STOP = re.compile(
    r"https?://\S+|@[^\s，。；、]{2,}|#[^\s，。；、]{2,}"
    r"|[（(][^）)]{0,40}[）)]|^\s*\d+[)）]\s*"
)


def _task_title(text: str) -> str:
    """从一句用户输入抽短标题，如「外置磁盘放在第二行…」→「调整·外置盘」。"""
    s = str(text or "").strip()
    if not s:
        return "未命名任务"
    # 去掉 @路径 / #长文本 / 括号补充 / 编号前缀
    s = _TITLE_STOP.sub(" ", s)
    s = re.sub(r"[\"“”'‘’《》<>.。，、；;：:！!？?~…\-—_/\\]+", " ", s)
    s = re.sub(r"\s+", " ", s).strip()
    if not s:
        return "未命名任务"

    low = s.lower()
    # 动作：按词典顺序取第一个命中的
    action = ""
    for pat, label in _TITLE_ACTIONS:
        if re.search(pat, low):
            action = label
            break

    # 对象：先在具体词里找，命中就取文中最靠前的；没有再退到泛化词。
    def _pick(words):
        hits = []
        for w in words:
            i = low.find(w.lower())
            if i >= 0:
                hits.append((i, -len(w), w))
        hits.sort()
        return hits[0][2] if hits else ""

    topic = _pick(_TITLE_TOPICS_SPEC) or _pick(_TITLE_TOPICS_GENERIC)

    if topic and action:
        # 「谁在做/是不是做的」这种没有具体设备对象，归到「任务归属」更达意
        if action == "核查" and re.search(r"谁在做|谁做的|是不是.*做的|到底是谁|找到的吗", low):
            topic = "任务归属"
        title = action + "·" + topic
    elif topic:
        title = topic
    elif action:
        # 没有明确对象：取动作后面的一小截当对象
        title = action + "·" + s[:10]
    else:
        # 兜底：去掉常见口水词后截前 14 字
        s = re.sub(r"^(然后|接着|另外|就是|这个|那个|你看|你看下|帮我|我想|我现在|现在|如果|有个)", "", s).strip()
        title = s[:14]
    title = re.sub(r"\s+", "", title)
    return (title[:18] + "…") if len(title) > 18 else title


def _rounds_parser():
    """装载工作台那份解析层；失败返回 None，由调用方降级成空列表。

    两条路，顺序不能反：
      1) 开发机 —— cuntu/dash.py（POCKET_DASH），最新解析逻辑在那儿先跑；
      2) 交付版 —— 包内 `dash` 模块。安装版没有 ~/Documents/cuntu，
         以前这里直接 return None，于是「逐轮」在别人机器上永远是空，
         而作者本机永远复现不了。0.2.4 把 hostwork 打进包了，解析层同理。
    """
    if _ROUNDS_MOD["m"] is not None:
        return _ROUNDS_MOD["m"]
    if not POCKET_DASH.exists():
        try:
            import dash as _bundled_dash
            _ROUNDS_MOD["m"] = _bundled_dash
            return _bundled_dash
        except Exception:                          # noqa: BLE001
            return None
    try:
        import importlib.util
        sys.path.insert(0, str(POCKET_DASH.parent))
        spec = importlib.util.spec_from_file_location("_hw_dash_parse", POCKET_DASH)
        m = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(m)
        _ROUNDS_MOD["m"] = m
    except Exception:                              # noqa: BLE001
        return None
    return _ROUNDS_MOD["m"]


def _rounds_compute(range_key: str) -> dict | None:
    """真算一遍逐轮记录，算完写进缓存。解析层读不到时返回 None。"""
    now = time.time()
    days = _range_days(range_key)
    m = _rounds_parser()
    if m is None:
        return None
    since_ms = (time.time() - days * 86400) * 1000 if days else 0
    items: list[dict] = []
    for _name, fn_name in _WB_CLIENTS:
        fn = getattr(m, fn_name, None)
        if not fn:
            continue
        try:
            items.extend(fn(since_ms))
        except Exception:                      # 一家客户端读挂了不拖垮整张表
            continue
    # TRAE 的对话正文在加密库里，走自己的证据链（命令 / 进程 / OCR 补录）
    try:
        items.extend(trae_work.items(since_ms))
    except Exception:
        pass
    items.sort(key=lambda x: (x.get("end") or x.get("start") or ""), reverse=True)
    # 只回前端用得上的字段：每轮带 18 步明细，80 条就是 160KB+
    # title 是由原话抽出的短任务名，task 仍是原文（展开可见）
    keep = [{k: it.get(k) for k in (
        "id", "sid", "task", "detail", "rest", "status", "start", "end",
        "secs", "t0",
        "sources", "facets", "ctx_labels", "client", "children", "more_steps",
        "project", "tokens")} for it in items[:80]]
    # 会话库补两样这轮账本里没有的东西：
    #   space      —— 这轮属于哪个空间（个人中台 / 日程软件 / …）。cwd 里
    #                 永远只有客户端名，真正的归属在 sessions.space_id。
    #   sess_title —— 这个会话叫什么。它就是「任务」：一轮一轮的 task 是
    #                 用户当次敲的指令，会话标题才是这件事的名字。
    sidx = _session_index()
    for row in keep:
        row["title"] = _task_title(row.get("task"))
        meta = sidx.get(str(row.get("sid") or "")) or {}
        row["space"] = str(meta.get("space") or "")
        row["sess_title"] = str(meta.get("title") or "")
        row["sess_gone"] = bool(meta.get("deleted"))
    data = {"ok": True, "items": keep, "at": datetime.now().strftime("%H:%M:%S")}
    with _ROUNDS_LOCK:
        _ROUNDS_CACHE["by"][range_key] = (now, data)
    return data


def _rounds_refresh(range_key: str) -> None:
    """过期了：先让调用方拿旧账，后台补算一锅。同一档位同一时间只补一锅。"""
    with _ROUNDS_LOCK:
        if range_key in _ROUNDS_CACHE["busy"]:
            return
        _ROUNDS_CACHE["busy"].add(range_key)

    def run() -> None:
        try:
            with _ROUNDS_BUILD_LOCK:           # 7d 与 30d 别同时压 _HOST_CACHE
                _rounds_compute(range_key)
        except Exception:                      # noqa: BLE001  后台算挂了不影响已返回的账
            pass
        finally:
            with _ROUNDS_LOCK:
                _ROUNDS_CACHE["busy"].discard(range_key)

    threading.Thread(target=run, name="rounds-refresh", daemon=True).start()


def _rounds_view(data: dict, range_key: str) -> dict:
    """按档位出账。返回浅拷贝：调用方改 items 不会把缓存改坏。"""
    if range_key == "today":
        day = date.today().isoformat()
        return dict(data, items=[x for x in data["items"]
                                 if str(x.get("start") or "").startswith(day)])
    return dict(data, items=list(data["items"]))


def rounds_items(range_key: str = "7d") -> dict:
    """逐轮工作记录：本页从会话流水自己解析。

    2026-11-24 改成「先用旧账、后台刷新」：重算一锅要 1.5~2.5 秒（WorkBuddy
    那近 600 条每轮都要重新组装），而前端 15 秒轮询一次 —— 8 秒 TTL 意味着
    **每次打开都撞在重算上**，这就是「点开本机工作要等很久」的真正原因。
    现在过期了也立刻回上一锅，另一个线程补算，页面不再等解析；
    只有进程刚起来、这一档还没算过的那一刻才同步等（那一次本来就躲不掉）。
    """
    key = range_key
    with _ROUNDS_LOCK:
        slot = _ROUNDS_CACHE["by"].get(key)
        if slot is None and key == "today":
            # 「今天」是「7 天」的子集：7 天那锅已经在手边就直接切，
            # 别为同一批数据再解析一遍（首次算 today 要 0.8 秒，纯浪费）。
            slot = _ROUNDS_CACHE["by"].get("7d")
            if slot is not None:
                key = "7d"
    if slot is not None:
        at, data = slot
        if time.time() - at >= _ROUNDS_TTL:
            _rounds_refresh(key)
        return _rounds_view(data, range_key)
    with _ROUNDS_BUILD_LOCK:
        with _ROUNDS_LOCK:                     # 排队期间别人可能已经算好了
            slot = _ROUNDS_CACHE["by"].get(range_key)
        if slot is not None:
            return _rounds_view(slot[1], range_key)
        data = _rounds_compute(range_key)
    if data is None:
        return {"ok": False, "error": "会话解析层读不到", "items": []}
    return _rounds_view(data, range_key)


# ---------------------------------------------------------------- 节奏（五张卡共用的账）
# 2026-11-22：月相 / 时间投入 / 活跃时段 / 知识地层 / 知识指数，同一份聚合喂五张卡。
# 口径（都从会话缓存现算，不再扫一遍 jsonl）：
#   交互分钟 = agg["act"]（相邻操作间隔 ≤5 分钟的那部分），不是会话首末跨度；
#   活跃时段 = agg["hw"] 按「周几-几点」汇总成 7×24；
#   知识地层 = 每天的 token 按「空间」（sessions.space_id → spaces.name）分到那一周；
#              筛到单个空间时改按项目分层（要不然整幅图只剩一个颜色）。
# 为什么另起一个接口而不是塞 /api/dash：这张账要 180 天，dashboard 那份只关心今天。
# 缓存键是 (天数, 范畴) 二元组 —— 只按天数分键的话，切了范畴会拿到上一摊的旧数据。
_PULSE_CACHE: dict[str, object] = {"at": 0.0, "key": (0, ""), "data": None}
_PULSE_SPACE_FALLBACK = "未归属空间"


def pulse(days: int = 75, scope: str = "") -> dict:
    """最近 N 天的节奏账：每天 / 每小时 / 每周。20 秒缓存（按 N + 范畴分键）。

    scope 决定「只看哪一摊工作」：
      ""            全部，跨所有空间加总
      "space:<名>"  只算这个空间的会话；地层改按项目分层
      "proj:<slug>" 只算这个项目目录的会话
    切范畴只是加过滤条件，数据源还是 WorkBuddy 自己那份会话流水，
    不去读别的 agent 的账 —— 想通吃各家，那是「概览」的事。
    """
    try:
        days = max(7, min(int(days or 75), 200))
    except (TypeError, ValueError):
        days = 200
    scope = str(scope or "").strip()
    skind, sval = "", ""
    if scope.startswith("space:"):
        skind, sval = "space", scope[6:].strip()
    elif scope.startswith("proj:"):
        skind, sval = "proj", scope[5:].strip()
    if not sval:                        # 解析不出范畴就当全部，别悄悄返回空账
        skind, sval, scope = "", "", ""
    now = time.time()
    cached = _PULSE_CACHE.get("data")
    if (cached is not None and _PULSE_CACHE.get("key") == (days, scope)
            and now - float(_PULSE_CACHE.get("at") or 0) < 20):
        return cached                                  # type: ignore[return-value]
    ensure_sessions_loaded()
    today = date.today()
    since = (today - timedelta(days=days - 1)).isoformat()
    with _sess_lock:
        files = list((_SESS.get("files") or {}).values())
    sidx = _session_index()

    def drow(d: str) -> dict:
        # layers：这一天的 token 按「层名」分开记。全部视图层名 = 空间，
        # 筛到单个空间时层名 = 项目。不能只留一个 space 字段 —— 那样一天里
        # 两个空间都有活时，后一个会被前一个吞掉，筛完再算就没法拆了。
        return daymap.setdefault(d, {"d": d, "act": 0.0, "tok": 0, "in": 0, "out": 0,
                                     "ask": 0, "call": 0, "tools": {}, "space": "",
                                     "layers": {}})

    daymap: dict[str, dict] = {}
    # 活跃时段要能切「近 30 / 90 / 180 天」：三份一起算完，前端切窗口就不必再打接口
    HW_SPAN = (30, 90, 180)
    hw_since = {s: (today - timedelta(days=s - 1)).isoformat() for s in HW_SPAN}
    hours = [0.0] * 168
    hours_by = {s: [0.0] * 168 for s in HW_SPAN}
    # 范畴菜单要用全量：不管当前筛的是谁，可选项都得把所有空间列出来
    opt_sp: dict[str, int] = {}
    opt_pj: dict[str, int] = {}
    # 当前范畴「最后一次真的有记录」是哪天（不受 days 窗口限制）：
    # 筛到一个小空间时窗口内常常全空，得能告诉用户数据到底在多远之前。
    last_ts = 0
    for e in files:
        agg = e.get("agg") or {}
        srow = sidx.get(str(agg.get("id") or "")) or {}
        sp_name = str(srow.get("space") or "") or _PULSE_SPACE_FALLBACK
        proj = str(agg.get("proj") or "")
        win_tok = 0
        for d, dm in (agg.get("days") or {}).items():
            if d >= since:
                win_tok += int(dm.get("in") or 0) + int(dm.get("out") or 0)
        if win_tok:
            opt_sp[sp_name] = int(opt_sp.get(sp_name) or 0) + win_tok
            if proj:
                opt_pj[proj] = int(opt_pj.get(proj) or 0) + win_tok
        if skind == "space" and sp_name != sval:
            continue
        if skind == "proj" and proj != sval:
            continue
        last_ts = max(last_ts, int(agg.get("last") or 0))
        # 地层怎么分层：全部 → 按空间；筛到某一摊 → 按会话标题。
        # ① 筛完还按空间分层的话整幅图只剩一个颜色，等于没画；
        # ② 按项目目录分层也不行 —— 这边项目目录基本是 WorkBuddy/spaces/<时间戳>，
        #    27 个一模一样的名字；标题才是人认得出的「在忙什么」。
        if skind:
            layer = str(srow.get("title") or "").strip()[:26] or "未命名会话"
        else:
            layer = sp_name
        for d, dm in (agg.get("days") or {}).items():
            if d < since:
                continue
            row = drow(d)
            tin = int(dm.get("in") or 0)
            tout = int(dm.get("out") or 0)
            row["in"] += tin
            row["out"] += tout
            row["tok"] += tin + tout
            if tin + tout:
                lay = row["layers"]
                lay[layer] = int(lay.get(layer) or 0) + tin + tout
            row["ask"] += int(dm.get("asks") or 0)
            row["call"] += int(dm.get("calls") or 0)
            for k, v in (dm.get("tools") or {}).items():
                row["tools"][k] = int(row["tools"].get(k) or 0) + int(v or 0)
            if sp_name and not row["space"]:
                row["space"] = sp_name        # 一天里两个空间都有活时，取先出现的那个
        for d, sec in (agg.get("act") or {}).items():
            if d >= since:
                drow(d)["act"] += float(sec or 0)
        for k, sec in (agg.get("hw") or {}).items():
            if k[:10] < since:
                continue
            try:
                dt = datetime.strptime(k[:13], "%Y-%m-%dT%H")
            except ValueError:
                continue
            idx = dt.weekday() * 24 + dt.hour
            v = float(sec or 0)
            hours[idx] += v
            for s in HW_SPAN:
                if s <= days and k[:10] >= hw_since[s]:
                    hours_by[s][idx] += v

    out: list[dict] = []
    for i in range(days - 1, -1, -1):
        d = (today - timedelta(days=i)).isoformat()
        r = daymap.get(d) or {"d": d, "act": 0.0, "tok": 0, "in": 0, "out": 0,
                              "ask": 0, "call": 0, "tools": {}, "space": ""}
        r["act"] = round(float(r["act"]) / 60.0, 1)     # 秒 → 分钟
        out.append(r)

    weeks: dict[str, dict] = {}
    for r in out:
        d = date.fromisoformat(r["d"])
        mon = d - timedelta(days=d.weekday())
        w = weeks.setdefault(mon.isoformat(), {
            "w": mon.isoformat(), "to": (mon + timedelta(days=6)).isoformat(),
            "tok": 0, "act": 0.0, "ask": 0, "spaces": {}})
        w["tok"] += r["tok"]
        w["act"] += r["act"]
        w["ask"] += r["ask"]
        for k, v in (r.get("layers") or {}).items():
            w["spaces"][k] = int(w["spaces"].get(k) or 0) + int(v)

    tot: dict[str, int] = {}
    for r in out:
        for k, v in (r.get("layers") or {}).items():
            tot[k] = tot.get(k, 0) + int(v)
    spaces = [k for k, _ in sorted(tot.items(), key=lambda kv: -kv[1])]

    # 筛到某一摊时层数可能爆炸（一个空间里几十个会话，每个标题一层）。
    # 只留量最大的 7 层 + 一档「其他」：图例放不下，颜色也不够分。
    layer_of = "space"
    if skind:
        layer_of = "sess"
        if len(spaces) > 8:
            keep = spaces[:7]
            keep_set = set(keep)
            rest_name = "其他 %d 个会话" % (len(spaces) - 7)
            has_rest = False
            for w in weeks.values():
                sp = w["spaces"]
                rest = 0
                for k in [k for k in sp if k not in keep_set]:
                    rest += int(sp.pop(k) or 0)
                if rest:
                    sp[rest_name] = int(sp.get(rest_name) or 0) + rest
                    has_rest = True
            spaces = keep + ([rest_name] if has_rest else [])

    data = {
        "ok": True,
        "at": datetime.now().strftime("%H:%M:%S"),
        "days": out,
        "hours": [round(x / 60.0, 1) for x in hours],
        "hours_by": {str(s): [round(x / 60.0, 1) for x in hours_by[s]] for s in HW_SPAN},
        "weeks": [weeks[k] for k in sorted(weeks)],
        "spaces": spaces,
        # 图里的颜色这一维叫什么：默认按空间，筛到某一摊就变成「会话」
        "layer_of": layer_of,
        "scope": scope,
        # 这个范畴最后一条记录是哪天（窗口外也算）。空态要拿它说「数据在多久以前」
        "last_day": (datetime.fromtimestamp(last_ts / 1000.0).strftime("%Y-%m-%d")
                     if last_ts else ""),
        "opts": {
            "spaces": [{"k": "space:" + k, "n": k, "tok": v}
                       for k, v in sorted(opt_sp.items(), key=lambda kv: -kv[1])],
            "projs": [{"k": "proj:" + k, "n": k, "tok": v}
                      for k, v in sorted(opt_pj.items(), key=lambda kv: -kv[1])],
        },
        "span": {"from": out[0]["d"] if out else "", "to": out[-1]["d"] if out else ""},
    }
    _PULSE_CACHE.update({"at": now, "key": (days, scope), "data": data})
    return data


# ---------------------------------------------------------------- 任务清单
# 2026-11-19：逐轮记录是「这一轮做了什么」，截图 OCR 识别出来的 TRAE 侧栏
# 名字其实是「有哪些任务/项目」。两种东西分开：任务清单卡只看名字 + 日期，
# 不冒充执行记录。数据从各 agent 现有采集层拿，不另起炉灶。
_TASKS_CACHE: dict[str, object] = {"at": 0.0, "key": "", "data": None}
_TASK_GLYPH = {"TRAE": "◈", "WorkBuddy": "◉", "Cherry Studio": "❀",
               "Claude Code": "✦", "Cursor": "▸", "pocket": "◆"}


WB_DB = HOME / ".workbuddy" / "workbuddy.db"


def wb_sessions(limit: int = 80) -> list[dict]:
    """WorkBuddy 侧栏那份「任务」名单：sessions 表就是真源。

    标题（custom_title 优先）是用户在侧栏看到的任务名，别再自己从第一句话
    猜短标题 —— 猜出来的「调整·外置盘」跟侧栏两套名字对不上。
    deleted_at 为 null 或 -1 的算在册（这家库用 -1 表示「没删过」）。
    """
    if not WB_DB.exists():
        return []
    try:
        c = sqlite3.connect(f"file:{WB_DB}?mode=ro", uri=True, timeout=2.0)
        c.row_factory = sqlite3.Row
        rows = c.execute(
            "select id, coalesce(nullif(custom_title, ''), title) nm, status,"
            "       coalesce(last_activity_at, updated_at, created_at) ts, space_id,"
            "       coalesce(nullif(group_title, ''), '') grp"
            "  from sessions"
            " where coalesce(nullif(custom_title, ''), title) is not null"
            "   and (deleted_at is null or deleted_at < 0)"
            f" order by ts desc limit {int(limit)}").fetchall()
        spaces = {str(r["id"]): str(r["name"] or "")
                  for r in c.execute("select id, name from spaces")}
        c.close()
    except Exception:                                  # noqa: BLE001
        return []
    out = []
    for r in rows:
        out.append({"id": str(r["id"]), "name": str(r["nm"] or "").strip(),
                    "status": str(r["status"] or ""), "ts": int(r["ts"] or 0),
                    "space": spaces.get(str(r["space_id"] or ""), ""),
                    "group": str(r["grp"] or "")})
    return [x for x in out if x["name"]]


def _span(days: dict) -> dict:
    """{"2026-11-17": 3, ...} → 前端画甘特/写「跨 N 天」用的那一段。

    from/to 是**活跃日**的边界（不是会话创建/最后保存时间）：一个任务
    10-13 建、10-20 才开始干活，甘特就该从 20 号画起。
    """
    ds = sorted(d for d in (days or {}) if d)
    return {"from": ds[0] if ds else "", "to": ds[-1] if ds else "",
            "n": len(ds),
            "days": [{"d": d, "a": int((days or {}).get(d) or 1)} for d in ds]}


def wb_task_group() -> dict | None:
    """WorkBuddy 一组：任务 = 一个会话，子项 = 这段会话里每轮「你说了什么」。

    子项来自 hostwork 自己的增量解析缓存（agg.utt），不再二次扫 400MB 流水。
    """
    sess = wb_sessions()
    if not sess:
        return None
    ensure_sessions_loaded()
    by_id = {}
    for a in sessions_snapshot():
        sid = str(a.get("id") or "")
        if sid:
            by_id[sid] = a
    tasks = []
    for s in sess:
        a = by_id.get(s["id"]) or {}
        utt = sorted((a.get("utt") or []), key=lambda x: int(x[0] or 0))
        kids = []
        for item in utt[-14:]:
            try:
                ts, txt = int(item[0] or 0), str(item[1] or "")
            except (TypeError, ValueError, IndexError):
                continue
            if not txt:
                continue
            kids.append({"at": datetime.fromtimestamp(ts / 1000).strftime("%m-%d %H:%M")
                         if ts else "", "text": txt})
        asks = int(a.get("utts") or 0)         # 只数真说过话的轮次（通知/摘要不算）
        # 每天说了几轮：甘特的条要按天分段（哪几天真干了、哪几天断了），
        # 只有首末两头的话，中间跳过的日子会被画成一整条，看着像天天在干。
        dc: dict[str, int] = {}
        # token 跟轮数一起从同一份 days 里取：任务清单按「活跃日」分组，同一条
        # 任务会在每一天各出现一次，所以必须给出**按天**的量（tok_day），
        # 否则宽卡上那行数字到底是当天还是累计，谁也看不出来。
        # 口径与 24 小时视图一致：总量 = 输入 + 输出（缓存命中已含在输入里）。
        t_in = t_out = t_cached = 0
        t_day: dict[str, int] = {}
        for d, row in (a.get("days") or {}).items():
            if not d or not isinstance(row, dict):
                continue
            n = int(row.get("asks") or 0)
            dc[d] = max(1, n) if n else max(1, int(row.get("calls") or 0))
            i0 = int(row.get("in") or 0)
            o0 = int(row.get("out") or 0)
            t_in += i0
            t_out += o0
            t_cached += int(row.get("cached") or 0)
            if i0 or o0:
                t_day[d] = i0 + o0
        span = _span(dc)
        if not span["n"] and s["ts"]:
            # 只有标题、没有带时间戳的正文（老会话）：至少把它排到那一天，
            # 否则甘特和「跨几天」这里都是空的。
            one = _day_of(s["ts"])
            span = {"from": one, "to": one, "n": 1, "days": [{"d": one, "a": 1}]}
        tasks.append({
            "name": s["name"], "date": span["to"] or (_day_of(s["ts"]) if s["ts"] else ""),
            "source": "会话", "raw": (kids[-1]["text"] if kids else ""),
            "count": len(kids), "asks": max(asks, len(kids)),
            "children": kids, "state": s["status"], "space": s["space"],
            "span": span,
            # token：任务清单行尾那个数字（只有 WorkBuddy 有 token 账；
            # TRAE 的对话库是加密的，那边只能给任务名 + 命令证据）
            "tok": t_in + t_out, "tok_in": t_in, "tok_out": t_out,
            "tok_cached": t_cached, "tok_day": t_day,
        })
    return {"agent": "WorkBuddy", "glyph": "◉", "count": len(tasks), "tasks": tasks}


def tasks_view(range_key: str = "7d") -> dict:
    """任务清单：每个 agent 名下有哪些任务，任务下面挂这段任务的具体对话。

    WorkBuddy 直接读它自己的 sessions 表（标题就是侧栏那个名字），子项是
    逐轮缓存里的用户原话；TRAE 的对话正文在加密库里，只有任务名 + 命令证据。
    """
    now = time.time()
    cached = _TASKS_CACHE.get("data")
    # 2026-11-23：缓存从 20 秒压到 6 秒。首页那张 /api/dash 是实时的，这张卡
    # 以前要叠 20 秒缓存 + 前端 25 秒 TTL，看上去永远慢一拍。增量扫描只要
    # 十几毫秒，没必要省这点。
    # key 必须带上 range：以前不分档，切了 7d/30d 还回上一档的内容。
    if (cached is not None and _TASKS_CACHE.get("key") == range_key
            and now - float(_TASKS_CACHE.get("at") or 0) < 6):
        return cached

    # 任务下面的「说了哪几句」来自会话扫描缓存。页面来要数据时顺手补扫一次，
    # 不然最新那几轮得等后台 2 分钟那一轮才出现 —— 旁边的逐轮记录是现解析的，
    # 两边一对就像这张卡滞后了。
    try:
        scan_if_stale(6)
        now = time.time()
    except Exception:                                  # noqa: BLE001
        pass

    days = _range_days(range_key)
    since_ms = (time.time() - days * 86400) * 1000 if days else 0

    groups: list[dict] = []
    try:
        wb = wb_task_group()
    except Exception:                                  # noqa: BLE001
        wb = None
    if wb:
        groups.append(wb)

    # 其余 agent：从逐轮记录归并，子项就是那几轮的原话（同一任务下最多 14 条）
    buckets: dict[str, dict[str, dict]] = {}

    def put(agent: str, name: str, when: str, source: str, raw: str = "",
            utt: str = "", span: tuple[str, str] | None = None) -> None:
        """记一条。span=(起, 止) 用于「只知道首末、不知道中间哪天干了」的源
        （TRAE 项目会话），前端会把这段画成一条淡一点的连续条。"""
        name = str(name or "").strip()
        if not name or len(name) > 120:
            return
        d = str(when or "")[:10]
        g = buckets.setdefault(agent, {})
        old = g.get(name)
        if old is None:
            old = {"name": name, "raw": "", "date": "", "source": source,
                   "asks": 0, "children": [], "_days": {}, "_coarse": False}
            g[name] = old
        days = [d] if d else []
        if span:
            days = [x for x in (str(span[0] or "")[:10], str(span[1] or "")[:10]) if x]
            if len(set(days)) >= 2:
                old["_coarse"] = True
        for one_d in days:
            old["_days"][one_d] = old["_days"].get(one_d, 0) + 1
            if one_d > (old["date"] or ""):
                old["date"] = one_d
        if raw and not old.get("raw"):
            old["raw"] = str(raw)[:300]
        one = _utt_text(utt, 110) if utt else ""
        if one:
            old["children"].append({"at": str(when or "")[5:16], "text": one})
            old["children"] = old["children"][-14:]
            old["asks"] = len(old["children"])
            old["count"] = len(old["children"])

    # 1) TRAE 项目会话：state.vscdb 里「哪个会话开在哪个目录」是明文，会话 id
    #    前半段又是时间戳 → 任务名 + 真实起止日都有了，不再全进「历史任务」。
    trae_named: set[str] = set()
    try:
        for row in trae_work.session_projects():
            trae_named.add(row["task"])
            put("TRAE", row["task"], row.get("to") or row.get("from") or "",
                row.get("source") or "项目会话", span=(row.get("from"), row.get("to")))
    except Exception:                                  # noqa: BLE001
        pass

    # 2) TRAE 截图 OCR 的侧栏任务名：上面认出来的不重复加。
    #    date 传空 —— 这些名字是「哪天截的图」而不是「哪天做的」，混进某一天会
    #    让人以为那天真干了这些活。前端把空日期归到「历史任务」一组。
    try:
        for row in trae_work.task_rows(0):          # 任务清单不按 since 过滤：它就是全量名单
            if str(row["task"]).strip() in trae_named:
                continue
            put("TRAE", row["task"], "", row["source"])
    except Exception:                                  # noqa: BLE001
        pass

    # 3) TRAE 命令记录 / 进程证据：这些是真执行过的，命令本身当子项
    try:
        for it in trae_work.items(since_ms):
            put("TRAE", it.get("task"), it.get("start") or "",
                "命令" if any(s.get("name") == "命令记录" for s in (it.get("sources") or []))
                else "进程", utt=str(it.get("task") or ""))
    except Exception:                                  # noqa: BLE001
        pass

    # 4) Claude Code / Cherry Studio… 有逐轮记录的都按短标题归并
    try:
        rd = rounds_items(range_key)
        for it in (rd.get("items") or []):
            agent = str(it.get("client") or "").strip()
            if not agent or agent in ("TRAE", "WorkBuddy"):
                continue
            raw = str(it.get("task") or "")
            put(agent, it.get("title") or _task_title(raw), it.get("start") or "",
                "会话", raw, utt=raw)
    except Exception:                                  # noqa: BLE001
        pass

    for agent, tasks in buckets.items():
        rows = sorted(tasks.values(),
                      key=lambda x: (x.get("date") or "", len(x.get("children") or [])),
                      reverse=True)
        for r in rows:
            sp = _span(r.pop("_days", {}) or {})
            sp["coarse"] = bool(r.pop("_coarse", False))
            r["span"] = sp
            r.setdefault("count", len(r.get("children") or []))
            r.setdefault("asks", r["count"])
            if not r.get("date"):
                r["date"] = sp.get("to") or ""
        groups.append({"agent": agent, "glyph": _TASK_GLYPH.get(agent, "●"),
                       "count": len(rows), "tasks": rows})
    groups.sort(key=lambda g: g["count"], reverse=True)

    data = {"ok": True, "groups": groups,
            "at": datetime.now().strftime("%H:%M:%S"),
            # 会话流水上次扫到几点（页面脚注上那个「数据 03:31」）。
            # 这个时间很久不动，就是采集侧卡住了，不是页面没刷。
            "scan_at": (datetime.fromtimestamp(float(_SCAN_AT.get("t") or 0))
                        .strftime("%Y-%m-%d %H:%M:%S") if _SCAN_AT.get("t") else "")}
    _TASKS_CACHE.update(at=now, key=range_key, data=data)
    return data


# ---------------------------------------------------------------- 数据源自检
# 「别人电脑上装完，打开就有数据」靠的就是这一段：所有 agent 的数据目录都是
# **按约定位置找**，找到就读，找不到就当这台机器没装 —— 不猜、不写死用户名。
# 页面把这份清单摊开，一眼能看出「没数据」是因为没装，还是因为路径不对。
_SOURCE_SPECS = [
    ("WorkBuddy 会话流水", lambda: WB_ROOT, "*/*.jsonl"),
    ("WorkBuddy 任务库", lambda: WB_DB, ""),
    ("TRAE 会话索引", lambda: getattr(trae_work, "STATE_DB", Path("/nonexistent")), ""),
    ("TRAE 项目快照", lambda: getattr(trae_work, "SNAP_ROOT", Path("/nonexistent")), "*"),
    ("TRAE 命令记录", lambda: trae_work.CMD_ROOT, "*"),
    ("TRAE 侧栏 OCR", lambda: trae_work.SEED_PATH, ""),
    # 「逐轮」这份账在 Windows 上只有一个源活着：TRAE 自己写的会话记忆。
    # 不列出来的话，自检页面会显示 TRAE 全线没数据，而实际上逐轮是好的
    # —— 缺的只是深挖（命令/快照）。少列一行，误导一整轮排查。
    ("TRAE 会话记忆", lambda: getattr(trae_work, "MEM_ROOT", Path("/nonexistent")),
     "*/*/*.jsonl"),
    ("TRAE 窗口采样", lambda: getattr(trae_work, "WIN_PATH", Path("/nonexistent")), ""),
    ("TRAE 自述账", lambda: getattr(trae_work, "NOTE_PATH", Path("/nonexistent")), ""),
    # 任何 agent 都能报的那本账（Codex / Claude / 自建 harness），跟上面老的并存
    ("Agent 自述账", lambda: getattr(trae_work, "AGENT_NOTE_PATH",
                                    Path("/nonexistent")), ""),
    ("Codex", lambda: HOME / ".codex" / "sessions", "*/*/*/rollout-*.jsonl"),
    ("Claude Code", lambda: HOME / ".claude" / "projects", "*/*.jsonl"),
    # 同一个库，两件事：这个库以前只被当成「有没有装 Cherry」，现在里面的
    # ai_usage_record 还供着花费账（cherry_usage），所以名字写全一点。
    ("Cherry Studio 用量账本", lambda: (HOME / "Library" / "Application Support"
                                        / "CherryStudio" / "Data" / "cherrystudio.sqlite"), ""),
    ("Cursor", lambda: HOME / ".cursor" / "chats", "*"),
]
_SOURCES_CACHE: dict[str, object] = {"at": 0.0, "data": None}


def sources_report(force: bool = False) -> dict:
    """哪些数据源在这台机器上、各有多少条。30 秒缓存，只 stat 不解析。"""
    now = time.time()
    if not force and _SOURCES_CACHE.get("data") and now - float(_SOURCES_CACHE.get("at") or 0) < 30:
        return dict(_SOURCES_CACHE["data"])           # type: ignore[arg-type]
    rows = []
    for name, fn, pat in _SOURCE_SPECS:
        try:
            path = Path(fn())
        except Exception:                              # noqa: BLE001
            continue
        r = {"name": name, "path": str(path), "found": path.exists(), "count": 0, "hint": ""}
        if r["found"]:
            try:
                if pat and path.is_dir():
                    r["count"] = sum(1 for _ in path.glob(pat))
                elif path.is_dir():
                    r["count"] = sum(1 for _ in path.iterdir())
                else:
                    r["count"] = 1
            except OSError:
                r["hint"] = "读不了（权限）"
        else:
            r["hint"] = "这台机器上没找到"
        rows.append(r)
    data = {"ok": True, "root": str(AI_ROOT), "root_from": _root_source(),
            "items": rows, "found": sum(1 for r in rows if r["found"]),
            "at": datetime.now().strftime("%H:%M:%S")}
    _SOURCES_CACHE.update(at=now, data=data)
    return dict(data)


def _root_source() -> str:
    """数据根是从哪来的：环境变量 / 配置文件 / 默认位置。"""
    if os.environ.get("CUNTU_ROOT", "").strip():
        return "环境变量 CUNTU_ROOT"
    try:
        if paths.CONF_FILE.exists():
            return "配置文件 ~/.cuntu/config.json"
    except Exception:                                  # noqa: BLE001
        pass
    return "默认位置 ~/Documents/AI项目"


# ---------------------------------------------------------------- HTTP

class Handler(BaseHTTPRequestHandler):
    server_version = "CuntuHostWork/1.0"

    def log_message(self, fmt, *args):
        pass

    def _send(self, body: bytes, ctype: str) -> None:
        self.send_response(200)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.send_header("X-Content-Type-Options", "nosniff")
        self.end_headers()
        try:
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def _cors(self) -> None:
        """工作台（跑在 8323）要跨端口读这台机器 —— 只放行本机来源。
        服务本来只听 127.0.0.1，且是只读数据，回显 Origin 就够，不开放 *。"""
        o = self.headers.get("Origin") or ""
        if o.startswith(("http://127.0.0.1", "http://localhost", "http://[::1]")):
            self.send_header("Access-Control-Allow-Origin", o)
            self.send_header("Vary", "Origin")

    def _json(self, obj, code: int = 200):
        body = json.dumps(obj, ensure_ascii=False).encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self._cors()
        self.end_headers()
        try:
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def do_OPTIONS(self):
        self.send_response(204)
        self._cors()
        self.send_header("Access-Control-Allow-Methods", "GET, OPTIONS")
        self.send_header("Access-Control-Allow-Headers", "Content-Type")
        self.send_header("Access-Control-Max-Age", "600")
        self.end_headers()

    def do_GET(self):
        p = urllib.parse.urlparse(self.path)
        qs = urllib.parse.parse_qs(p.query)
        if p.path in ("/", "/index.html"):
            html = UI_FILE.read_text(encoding="utf-8") if UI_FILE.exists() else \
                "<meta charset='utf-8'><h3>缺 hostwork_ui.html</h3>"
            return self._send(html.encode("utf-8"), "text/html; charset=utf-8")
        if p.path == "/api/overview":
            key = (qs.get("range") or ["7d"])[0]
            if key not in RANGES:
                key = "7d"
            fresh = (qs.get("fresh") or [""])[0] in ("1", "true", "yes")
            try:
                return self._json(overview(key, fresh=fresh))
            except Exception as e:                      # 接口不许把服务打挂
                return self._json({"ok": False, "error": f"{type(e).__name__}: {e}"}, 500)
        if p.path == "/api/rounds":
            key = (qs.get("range") or ["7d"])[0]
            if key not in RANGES:
                key = "7d"
            try:
                return self._json(rounds_items(key))
            except Exception as e:
                return self._json({"ok": False, "error": f"{type(e).__name__}: {e}",
                                   "items": []}, 500)
        if p.path == "/api/tasks":
            key = (qs.get("range") or ["7d"])[0]
            if key not in RANGES:
                key = "7d"
            try:
                return self._json(tasks_view(key))
            except Exception as e:
                return self._json({"ok": False, "error": f"{type(e).__name__}: {e}",
                                   "groups": []}, 500)
        if p.path == "/api/sources":
            try:
                return self._json(sources_report(force=bool(qs.get("force"))))
            except Exception as e:
                return self._json({"ok": False, "error": f"{type(e).__name__}: {e}",
                                   "items": []}, 500)
        if p.path == "/api/billing":
            try:
                return self._json(billing_payload())
            except Exception as e:
                return self._json({"ok": False, "error": f"{type(e).__name__}: {e}",
                                   "accounts": []}, 500)
        if p.path == "/api/billing/providers":
            return self._json(billing.providers_payload())
        if p.path == "/api/icon":
            f = (qs.get("f") or [""])[0]
            if not re.fullmatch(r"[0-9a-f]{6,32}\.png", f):     # 只认自己生成的名字，防目录穿越
                return self._json({"ok": False, "error": "文件名不对"}, 400)
            fp = ICON_DIR / f
            if not fp.exists():
                return self._json({"ok": False, "error": "没有这个图标"}, 404)
            try:
                body = fp.read_bytes()
            except OSError:
                return self._json({"ok": False, "error": "读不出来"}, 500)
            self.send_response(200)
            self.send_header("Content-Type", "image/png")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "public, max-age=604800")
            self.send_header("X-Content-Type-Options", "nosniff")
            self.end_headers()
            try:
                self.wfile.write(body)
            except (BrokenPipeError, ConnectionResetError):
                pass
            return
        if p.path == "/api/ping":
            return self._json({"ok": True, "app": "hostwork", "port": PORT_INFO["port"],
                               "scan": dict(SCAN)})
        return self._json({"ok": False, "error": "没这个接口"}, 404)

    def do_POST(self):
        p = urllib.parse.urlparse(self.path)
        n = int(self.headers.get("Content-Length") or 0)
        body = {}
        if n:
            try:
                body = json.loads(self.rfile.read(n).decode("utf-8") or "{}")
            except ValueError:
                body = {}
        if p.path == "/api/rescan":
            threading.Thread(target=scan_sessions, kwargs={"force": bool(body.get("force"))},
                             daemon=True).start()
            return self._json({"ok": True})
        if p.path == "/api/billing/save":
            return self._json(billing.save_account(body))
        if p.path == "/api/billing/del":
            return self._json(billing.del_account(str(body.get("id") or "")))
        if p.path == "/api/billing/pull":
            # 拉账要出网（百炼那套签名 + 各家 API），最慢的一家十几秒，给足余量
            return self._json(billing.pull(str(body.get("id") or ""),
                                           body.get("ids") or None))
        return self._json({"ok": False, "error": "没这个接口"}, 404)


PORT_INFO = {"port": DEFAULT_PORT}


def probe_self(port: int) -> bool:
    """别只看端口开没开：隔壁服务也会占 877x。真发一个 ping 认身份。"""
    import urllib.request
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{port}/api/ping", timeout=0.7) as r:
            d = json.loads(r.read().decode("utf-8"))
        return d.get("app") == "hostwork"
    except Exception:
        return False


def open_url(url: str) -> None:
    try:
        webbrowser.open(url)
    except Exception:
        pass


def serve(port: int = DEFAULT_PORT, open_browser: bool = True, detached: bool = True) -> int:
    global PORT_INFO
    load_proc_history()
    threading.Thread(target=sampler_loop, daemon=True).start()
    threading.Thread(target=icon_worker, daemon=True).start()
    threading.Thread(target=scan_loop, daemon=True).start()
    for p in range(port, port + 12):
        if probe_self(p):
            PORT_INFO["port"] = p
            print(f"本机工作已经在跑：http://127.0.0.1:{p}/")
            if open_browser:
                open_url(f"http://127.0.0.1:{p}/")
            return 0
    srv = None
    for p in range(port, port + 12):
        try:
            srv = ThreadingHTTPServer(("127.0.0.1", p), Handler)
            port = p
            break
        except OSError:
            continue
    if srv is None:
        print("端口都被占了")
        return 2
    PORT_INFO["port"] = port
    url = f"http://127.0.0.1:{port}/"
    print(f"本机工作已起：{url}")
    if open_browser:
        open_url(url)
    if detached:
        try:
            srv.serve_forever()
        except KeyboardInterrupt:
            pass
        finally:
            srv.server_close()
        return 0
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        print("\n已停。")
    finally:
        srv.server_close()
    return 0


def main(argv=None) -> int:
    if hasattr(sys.stdout, "reconfigure"):
        try:
            sys.stdout.reconfigure(encoding="utf-8", errors="replace")
        except Exception:
            pass
    ap = argparse.ArgumentParser(description="本机工作：机器 / 进程 / 会话 token")
    ap.add_argument("--serve", action="store_true")
    ap.add_argument("--port", type=int, default=DEFAULT_PORT)
    ap.add_argument("--no-open", action="store_true")
    ap.add_argument("--json", action="store_true", help="终端打一份 overview")
    ap.add_argument("--range", default="7d", choices=list(RANGES))
    ap.add_argument("--rescan", action="store_true", help="强制重扫全部会话")
    ap.add_argument("--doctor", action="store_true",
                    help="只看数据源：这台机器上找到了哪些 agent 目录")
    a = ap.parse_args(argv)
    if a.doctor:
        # 换台电脑第一件事：跑这个看「有没有数据、为什么没有」，不用开页面
        r = sources_report(force=True)
        print(f"数据根 {r['root']}（{r['root_from']}）")
        def _pad(t: str, n: int) -> str:
            # 中文占两格：按显示宽度补空格，终端里才对得齐
            w = sum(2 if ord(ch) > 0x2E80 else 1 for ch in t)
            return t + " " * max(0, n - w)

        for x in r["items"]:
            mark = "✓" if x["found"] else "×"
            extra = (f"{x['count']} 条" if x["count"] else x["hint"]) if x["found"] else x["hint"]
            print(f"  {mark} {_pad(x['name'], 20)}{_pad(extra, 16)}{x['path']}")
        print(f"共 {r['found']}/{len(r['items'])} 个源可用")
        return 0
    if a.rescan:
        t0 = time.time()
        scan_sessions(force=True)
        print(f"重扫完成 {SCAN['done']}/{SCAN['total']} 个会话，{time.time() - t0:.1f}s")
        return 0
    if a.serve:
        return serve(port=a.port, open_browser=not a.no_open)
    o = overview(a.range)
    if a.json:
        print(json.dumps(o, ensure_ascii=False, indent=1))
        return 0
    m = o["machine"]
    print(f"本机工作 {o['now']}")
    print(f"  磁盘 {m['disk']['disk_pct']}% · 内存 {m['mem']['mem_pct']}% "
          f"({m['mem']['mem_gb']}/{m['mem']['mem_total_gb']} GB) · 负载 {m['load']} "
          f"· CPU {m['cpu_pct']}% · 已运行 {m['uptime_h']}h")
    for a_ in o["apps"][:8]:
        print(f"    {a_['name'][:32]:32s} {a_['cpu']:5.1f}%  {a_['mem']:7.1f} MB  "
              f"累计 {a_['cpu_s']:7.1f}s  {a_['pids']} 进程")
    t = o["tokens"]["totals"]
    mo = o["money"]
    print(f"  token 输入 {t['in']:,} · 输出 {t['out']:,} · 缓存命中 {t['cached']:,}"
          f" · 调用 {t['calls']} 次")
    print(f"  折算 点数 {mo['points'] if mo['points'] is not None else '—'}"
          f" · 金额 {mo['cost'] if mo['cost'] is not None else '—'} {mo['currency']}")
    ch = o.get("cherry") or {}
    if ch.get("found"):
        ct = ch.get("tokens") or {}
        print(f"  Cherry 实付 {ch.get('cny')} 元"
              f"（{ch.get('calls')} 次 · {ct.get('in', 0):,} 输入 / {ct.get('out', 0):,} 输出）"
              f" · Cherry 自算 {ch.get('cny_cherry')} 元")
    for c in o["clients"]:
        print(f"  [{c['name']}] 调用 {c['calls']} · 子项 {len(c['kids'])}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
