#!/usr/bin/env python3
"""跨平台解释器解析 —— 「这个 .venv 里的 python 在哪」「这台机器到底有没有 python」。

0.2.6 之前有三处把 `.venv/bin/python` 写死（ai_trends_runner / dedupe_runner /
ocr_runner）。macOS 上没问题，Windows 上那串路径**永远不存在**，于是任务卡片上
写着「找不到 Python：C:\\...\\.venv/bin/python」—— 看起来像机器没装 Python，
实际上那台机器装着 Python 3.11，只是叫 `py.exe`、躺在
`%LOCALAPPDATA%\\Programs\\Python\\Python311` 里，PATH 上没有它。

约定（linkbox.py 一直在用，这里收成一处，别再各写各的）：
  POSIX    <venv>/bin/python
  Windows  <venv>\\Scripts\\python.exe

解析顺序：显式配置 > 当前解释器 > venv > PATH > Windows py 启动器 > 常见安装位置。
找不到时把**试过的每一条**都返回去，让界面能说清「我找过哪些地方」，
而不是甩一个绝对路径让人猜。
"""
from __future__ import annotations

import os
import shutil
import subprocess
import sys
from pathlib import Path


def venv_python(venv) -> Path:
    """venv 目录里的解释器。两个平台的目录名不一样，这是唯一的差别。"""
    v = Path(venv)
    return v / "Scripts" / "python.exe" if os.name == "nt" else v / "bin" / "python"


def _py_launcher() -> Path | None:
    """Windows 的 py 启动器：问它要 3.x 的真实解释器路径。

    这是 Windows 上最靠得住的一条 —— 官方安装包默认装 py.exe 并进 PATH，
    但 `python` 这个名字**未必**在 PATH 上（那是安装时的勾选项）。
    """
    if os.name != "nt":
        return None
    exe = shutil.which("py")
    if not exe:
        return None
    try:
        r = subprocess.run([exe, "-3", "-c", "import sys;print(sys.executable)"],
                           capture_output=True, text=True, timeout=10)
    except (OSError, subprocess.SubprocessError):
        return None
    out = (r.stdout or "").strip().splitlines()
    if not out:
        return None
    p = Path(out[-1].strip())
    return p if p.is_file() else None


def candidates(venv=None) -> list[Path]:
    """按可能性从高到低列出解释器候选。重复的去掉，顺序保留。"""
    out: list[Path] = []
    if venv:
        # venv 优先：要跑的脚本多半依赖它装好的包，退到系统 python 会因为缺依赖
        # 在 import 阶段就失败，错误信息还看不出是解释器选错了。
        out.append(venv_python(venv))
    if not getattr(sys, "frozen", False):
        # 打包版 sys.executable 是 Moshan.exe，不是解释器，不能拿来跑脚本
        out.append(Path(sys.executable))
    for name in ("python3", "python"):
        w = shutil.which(name)
        if w:
            out.append(Path(w))
    if os.name == "nt":
        la = _py_launcher()
        if la:
            out.append(la)
        roots: list[Path] = []
        env = (os.environ.get("LOCALAPPDATA") or "").strip()
        if env:
            roots.append(Path(env) / "Programs" / "Python")
        roots += [Path("C:/Python313"), Path("C:/Python312"),
                  Path("C:/Python311"), Path("C:/Python310")]
        for root in roots:
            if not root.is_dir():
                continue
            # 安装目录下要么直接是 python.exe，要么是 Python3xx\python.exe
            out.extend(sorted(root.glob("Python3*/python.exe"), reverse=True))
            out.extend(sorted(root.glob("python.exe"), reverse=True))
    else:
        for d in ("/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"):
            for name in ("python3", "python"):
                p = Path(d) / name
                if p.is_file():
                    out.append(p)
    seen: set[str] = set()
    uniq: list[Path] = []
    for p in out:
        k = str(p)
        if k not in seen:
            seen.add(k)
            uniq.append(p)
    return uniq


def resolve(explicit=None, venv=None) -> tuple[Path | None, list[str]]:
    """返回 (找到的解释器 或 None, 试过的路径列表)。"""
    tried: list[str] = []
    if explicit:
        p = Path(str(explicit))
        tried.append(str(p))
        if p.is_file():
            return p, tried
    for p in candidates(venv):
        tried.append(str(p))
        if p.is_file():
            return p, tried
    return None, tried


def not_found_hint(tried: list[str]) -> str:
    """给界面看的一句话。别只说「找不到」，要说清找过哪儿。"""
    if not tried:
        return "找不到 Python（没有任何候选路径）"
    head = "、".join(tried[:3])
    more = f" 等 {len(tried)} 处" if len(tried) > 3 else ""
    return f"找不到 Python（找过 {head}{more}）"
