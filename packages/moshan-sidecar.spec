# -*- mode: python ; coding: utf-8 -*-
"""Tauri sidecar 打包配置：单文件 `moshan`，供 `src-tauri/bundle.externalBin` 使用。

与 `moshan.spec` 的唯一差别是产物形态：这里出 onefile，那边出 onedir。
为什么必须单文件：Tauri 的 externalBin 只会把一个可执行文件复制进 app，
不会跟着搬 onedir 的 `_internal/` 目录，缺了它 sidecar 一启动就 ImportError。
代价是首启要解包（约 2–5 秒），所以桌面壳先显示 loading.html 再导航。

注意：模块清单与 `moshan.spec` 必须保持一致。加新依赖时两处都要改，
漏改的表现是「安装版某个功能报 ModuleNotFoundError，源码态完全正常」。

用法：
    pyinstaller --noconfirm --clean --distpath dist/sidecar packages/moshan-sidecar.spec
"""
import re
import sys
from pathlib import Path

ROOT = Path(SPEC).resolve().parent.parent          # 仓库根
SRC = ROOT / "moshan"

_m = re.search(r'VERSION\s*=\s*"([^"]+)"', (SRC / "version.py").read_text(encoding="utf-8"))
VERSION = _m.group(1) if _m else "0.0.0"

# 与 moshan.spec 同规则：排除 exFAT 的 ._xxx 元数据，显式声明全部本地模块。
LOCAL_MODULES = sorted(
    p.stem for p in SRC.glob("*.py")
    if p.stem != "launcher" and not p.name.startswith(".")
)

EXTRA_HIDDEN = ["psutil", "cryptography", "cryptography.hazmat.backends.openssl"]

UVICORN_HIDDEN = [
    "uvicorn.logging",
    "uvicorn.loops", "uvicorn.loops.auto", "uvicorn.loops.asyncio",
    "uvicorn.protocols", "uvicorn.protocols.http", "uvicorn.protocols.http.auto",
    "uvicorn.protocols.http.h11_impl",
    "uvicorn.protocols.websockets", "uvicorn.protocols.websockets.auto",
    "uvicorn.protocols.websockets.websockets_impl",
    "uvicorn.lifespan", "uvicorn.lifespan.on", "uvicorn.lifespan.off",
]

EXCLUDES = [
    "lxml", "tkinter", "matplotlib", "numpy", "pandas", "scipy",
    "PIL", "pyarrow", "yt_dlp", "pytest", "IPython", "notebook",
    "PyQt5", "PySide2", "PySide6", "wx",
]

datas = [(str(SRC / "ui.html"), "."), (str(SRC / "dash.html"), ".")]
try:
    from PyInstaller.utils.hooks import collect_data_files

    datas += collect_data_files("certifi")
except Exception:                                       # noqa: BLE001
    pass

for extra in ("appearance.css", "appearance.json", "hostwork_ui.html"):
    p = SRC / extra
    if p.exists():
        datas.append((str(p), "."))

a = Analysis(
    [str(SRC / "launcher.py")],
    pathex=[str(SRC)],
    binaries=[],
    datas=datas,
    hiddenimports=LOCAL_MODULES + UVICORN_HIDDEN + EXTRA_HIDDEN,
    hookspath=[],
    hooksconfig={},
    runtime_hooks=[],
    excludes=EXCLUDES,
    noarchive=False,
    optimize=1,
)

pyz = PYZ(a.pure)

# onefile：binaries 与 datas 都交给 EXE，不再有 COLLECT。
exe = EXE(
    pyz,
    a.scripts,
    a.binaries,
    a.datas,
    [],
    name="moshan",
    debug=False,
    bootloader_ignore_signals=False,
    strip=False,
    upx=False,
    console=False,
    disable_windowed_traceback=False,
    argv_emulation=False,
    target_arch=None,
    codesign_identity=None,
    entitlements_file=None,
    icon=str(ROOT / ("packages/moshan.ico" if sys.platform.startswith("win") else "packages/moshan.icns")),
)
