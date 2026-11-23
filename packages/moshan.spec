# -*- mode: python ; coding: utf-8 -*-
"""Moshan 打包配置（macOS / Windows 共用）。

产出形态：onedir（不是 onefile）。理由：
  * 启动快 —— onefile 每次运行要解包 60MB 到临时目录，首次启动能差 3–5 秒；
  * 好排查 —— 缺模块时能直接看到 _internal 里的文件；
  * 更新友好 —— 后续做 versions/ + current 符号链接时，onedir 才能整体替换。
macOS 再包一层 .app（BUNDLE），Windows 交给 Inno Setup 打安装程序。

用法：
    pyinstaller --noconfirm --clean packages/moshan.spec
"""
import re
import sys
from pathlib import Path

# SPEC 是 PyInstaller 注入的全局变量，指向本 spec 文件
ROOT = Path(SPEC).resolve().parent.parent          # 仓库根
SRC = ROOT / "moshan"                                # 平铺的 Python 源码目录

_m = re.search(r'VERSION\s*=\s*"([^"]+)"', (SRC / "version.py").read_text(encoding="utf-8"))
VERSION = _m.group(1) if _m else "0.0.0"

# 本地模块全部显式声明：server.py 用的是平铺 import（import storage 而非 from moshan import storage），
# PyInstaller 的静态分析能跟到大部分，但 dashboard 里有些是运行期 import，漏一个就白屏。
# 过滤掉以「.」开头的名字：仓库若放在 exFAT/网络盘上，macOS 会生成 ._xxx 元数据文件，
# glob("*.py") 会把它们当模块，构建日志里刷一堆「._bootstrap not found」。
LOCAL_MODULES = sorted(
    p.stem for p in SRC.glob("*.py")
    if p.stem != "launcher" and not p.name.startswith(".")
)

# psutil 在 hostwork._sample_procs_win 里是**函数内** import（macOS 不需要它，
# 不想让它在 mac 版里也把 hook 拖下水）。函数内 import 静态分析抓不到，必须显式列。
EXTRA_HIDDEN = ["psutil"]

UVICORN_HIDDEN = [
    "uvicorn.logging",
    "uvicorn.loops", "uvicorn.loops.auto", "uvicorn.loops.asyncio",
    "uvicorn.protocols", "uvicorn.protocols.http", "uvicorn.protocols.http.auto",
    "uvicorn.protocols.http.h11_impl",
    "uvicorn.protocols.websockets", "uvicorn.protocols.websockets.auto",
    "uvicorn.protocols.websockets.websockets_impl",
    "uvicorn.lifespan", "uvicorn.lifespan.on", "uvicorn.lifespan.off",
]

# 排除大件：lxml(19MB) 代码里完全没用到，但 beautifulsoup4 的 hook 会顺手带进来；
# 其余是纯前端/科学计算/测试依赖，本项目一律不用。
EXCLUDES = [
    "lxml", "tkinter", "matplotlib", "numpy", "pandas", "scipy",
    "PIL", "pyarrow", "yt_dlp", "pytest", "IPython", "notebook",
    "PyQt5", "PySide2", "PySide6", "wx",
]

datas = [(str(SRC / "ui.html"), ".")]
# CA 证书必须打进包：打包版用自带 OpenSSL，默认证书路径在别人机器上不存在，
# 缺了它安装版里所有 https（检查更新、抓网页、装插件）都验不过证书。
# PyInstaller 通常有 certifi 的 hook 会自动带上，这里再显式收一次 —— 这个坑
# 0.2.0 已经踩过一次，不值得赌 hook 的版本差异。
try:
    from PyInstaller.utils.hooks import collect_data_files

    datas += collect_data_files("certifi")
except Exception:                                       # noqa: BLE001
    pass
# 默认皮肤资源若存在则一并带上（缺失时运行期会退回纯渐变，不报错）
# hostwork_ui.html 是 /hostwork 详情页：0.2.4 起 hostwork 进包了，页面也得跟进来，
# 否则使用者点「本机工作」只会拿到 404（README 里那张表就是靠它）。
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

exe = EXE(
    pyz,
    a.scripts,
    [],
    exclude_binaries=True,
    name="Moshan",
    debug=False,
    bootloader_ignore_signals=False,
    strip=False,
    upx=False,                 # UPX 会触发 Gatekeeper/杀软误报，宁可大一点
    console=False,             # 桌面形态：不弹黑窗
    disable_windowed_traceback=False,
    argv_emulation=False,
    target_arch=None,          # 跟随构建机架构（macOS 上 arm64 原生）
    codesign_identity=None,    # 交给 CI 做 ad-hoc 签名
    entitlements_file=None,
    icon=None,
)

coll = COLLECT(
    exe,
    a.binaries,
    a.datas,
    strip=False,
    upx=False,
    name="Moshan",
)

if sys.platform == "darwin":
    app = BUNDLE(
        coll,
        name=f"Moshan.app",
        icon=None,
        bundle_identifier="cn.moshan.hub",
        info_plist={
            "CFBundleName": "Moshan",
            "CFBundleDisplayName": "墨山中台",
            "CFBundleShortVersionString": VERSION,
            "CFBundleVersion": VERSION,
            "LSMinimumSystemVersion": "12.0",
            "LSUIElement": False,          # 有 Dock 图标，Cmd+Q 可退出
            "NSHighResolutionCapable": True,
            "NSRequiresAquaSystemAppearance": False,
            # 明确要访问的目录，系统首次访问时会提示
            "NSDocumentsFolderUsageDescription": "用于存放你在中台里管理的文件与收件箱。",
        },
    )
