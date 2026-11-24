"""Moshan 的目录约定 —— 程序、状态、资料库三者物理分离。

为什么要分三层：
  * 程序目录（app_dir）随更新整体替换，用户不可写也不该写；
  * 状态目录（state_dir）放配置、证书、日志、版本缓存，属于「软件自己」；
  * 资料库目录（library_dir）放用户的文件、工作台页面、收件箱、回收站，属于「用户」。

更新只替换程序目录，另两者永不触碰 —— 这是「更新后设置和文件不丢」的实现基础。
"""
from __future__ import annotations

import json
import os
import socket
import sys
import time
from pathlib import Path

APP_NAME = "Moshan"
APP_DISPLAY_NAME = "墨山中台"

# 数据根可在启动前用环境变量覆盖，便于测试与多开
ENV_HOME = "MOSHAN_HOME"


def app_dir() -> Path:
    """程序目录（只读）。PyInstaller 打包后指向解包资源目录；源码运行时是 moshan/。"""
    if getattr(sys, "frozen", False):
        meipass = getattr(sys, "_MEIPASS", None)
        if meipass:
            return Path(meipass)
        return Path(sys.executable).resolve().parent
    return Path(__file__).resolve().parent


def resource(*parts: str) -> Path:
    """程序目录下的只读资源（ui.html、默认皮肤、安装器脚本等）。"""
    return app_dir().joinpath(*parts)


def state_dir() -> Path:
    """状态目录：配置、证书、日志、版本缓存。"""
    override = os.environ.get(ENV_HOME)
    if override:
        return Path(override).expanduser().resolve() / "state"
    if sys.platform == "darwin":
        return Path.home() / "Library" / "Application Support" / APP_NAME
    if os.name == "nt":
        base = os.environ.get("APPDATA") or (Path.home() / "AppData" / "Roaming")
        return Path(base) / APP_NAME
    return Path(os.environ.get("XDG_STATE_HOME", Path.home() / ".local" / "state")) / APP_NAME.lower()


def library_dir() -> Path:
    """资料库目录：用户可见的文件区（工作台、收件箱、回收站都在这里）。"""
    override = os.environ.get(ENV_HOME)
    if override:
        return Path(override).expanduser().resolve() / "library"
    return Path.home() / "Documents" / APP_NAME


def data_root() -> Path:
    """结构化数据的根：08-阅读、04-存图、10-系统记录、_工作台都挂在它下面。

    开发版这里是 `~/Documents/AI项目`（作者自己的目录结构），分发版就是资料库
    本身 —— 用户的东西全在一个目录里，备份、迁移、换机器只需拷这一处。
    凡是「数据根」的默认值都该走这里，别再各写一份 `Path.home()/Documents/...`。
    """
    return library_dir()


def config_file() -> Path:
    return state_dir() / "config.json"


# hostwork.py 的 _root_source() 用它判断「数据根是不是配置文件指定的」。
# 开发机那边对应物是 ~/.cuntu/config.json，交付版没有 cuntu，等价物就是
# Moshan 自己的 config.json。放在模块尾部求值，保证 state_dir() 已经定义了。
CONF_FILE = config_file()


def ensure(path) -> Path:
    """建好目录并原样返回 —— 开发版 cuntu 的 paths 一直有这个便利函数。

    0.2.4 把 hostwork / billing 收进包之后，它们在**模块级**就会调用
    `paths.ensure(data_root() / "_本机工作")`；本模块原来没这个函数，
    导入当场 AttributeError。补在这里，别让使用方各写一遍 mkdir。
    """
    p = Path(path)
    p.mkdir(parents=True, exist_ok=True)
    return p


def log_dir() -> Path:
    return state_dir() / "logs"


def versions_dir() -> Path:
    return state_dir() / "versions"


def certs_dir() -> Path:
    return state_dir() / "certs"


def workbench_dir() -> Path:
    """工作台：页面产物、外观设置、壁纸、第三方插件清单。位于资料库内，跟着用户数据走。"""
    return library_dir() / "_工作台"


def machine_file() -> Path:
    """数据根的「出生证」：这份资料库是哪台机器产生的。"""
    return workbench_dir() / "machine.json"


def _short_host(name: str) -> str:
    """Windows 主机名常带域名后缀（EVO-X2.corp.local），比前缀就算同一台。"""
    return str(name or "").split(".")[0].strip().lower()


def stamp_machine() -> dict:
    """给数据根盖来源章；已经有章就原样返回，绝不覆盖。

    为什么要盖：2026-11-25 发现 EVO 的 /dash 里显示着 mac 那台 629 条工作流水
    （含人名、行程）—— 部署时把 `09-个人工作台/流水` 整份拷过去，而程序本身
    分不出「这份数据是本机攒的，还是别人那台搬来的」。盖了章，拷贝会连章一起
    带走，换机器一读就知道来源不对。**有章不覆盖**是关键：覆盖等于把证据抹了。
    """
    p = machine_file()
    try:
        old = json.loads(p.read_text(encoding="utf-8"))
        if isinstance(old, dict) and old.get("host"):
            return old
    except (OSError, ValueError):
        pass
    rec = {"host": socket.gethostname(), "at": time.strftime("%Y-%m-%d %H:%M:%S")}
    try:
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(json.dumps(rec, ensure_ascii=False, indent=1), encoding="utf-8")
    except OSError:
        pass
    return rec


def foreign_origin() -> str:
    """数据根是不是别人那台搬来的：是就回原机器名，否则回空串。

    只报不拦 —— 拷过来的人可能就是想看历史，得让用户自己决定要不要清。
    真正做错的是一声不吭地把它当成自己的数据展示。
    """
    try:
        d = json.loads(machine_file().read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return ""
    host = str((d or {}).get("host") or "")
    cur = socket.gethostname()
    if not host or not cur:
        return ""
    return "" if _short_host(host) == _short_host(cur) else host


def ensure_layout() -> None:
    """首次启动时把需要的目录建出来（幂等）。"""
    for d in (
        state_dir(), log_dir(), versions_dir(), certs_dir(),
        library_dir(), workbench_dir(),
        workbench_dir() / "wallpapers",
        library_dir() / "收件箱",
        library_dir() / ".回收站",
    ):
        d.mkdir(parents=True, exist_ok=True)
    stamp_machine()


def describe() -> dict:
    """给「关于」页与日志用的一览。"""
    return {
        "app_dir": str(app_dir()),
        "state_dir": str(state_dir()),
        "library_dir": str(library_dir()),
        "config": str(config_file()),
        "frozen": bool(getattr(sys, "frozen", False)),
    }
