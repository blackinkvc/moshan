"""版本号与发布信息 —— 单一事实来源。

版本号只在这里改；构建脚本、更新器、关于页都读它。
"""
from __future__ import annotations

VERSION = "0.3.16"
BUILD = ""          # 构建时由 CI 注入短 SHA，源码运行时为空
CHANNEL = "stable"  # stable | beta

# 更新源：GitHub Release 的 latest 资产地址。
# 用户可在 config.json 的 update.repo 覆盖（镜像或自建）。
DEFAULT_REPO = "blackinkvc/moshan"


def version_tuple(v: str = VERSION) -> tuple:
    """把 '0.1.0' / 'v0.1.0' / '0.1.0-beta.2' 转成可比较的元组，预发布排在正式版之前。"""
    v = v.strip().lstrip("vV")          # Git tag 与 release.json 都带 v 前缀，先剥掉
    core = v.split("-", 1)[0]
    parts = []
    for seg in core.split("."):
        try:
            parts.append(int(seg))
        except ValueError:
            parts.append(0)
    while len(parts) < 3:
        parts.append(0)
    pre = v.split("-", 1)[1] if "-" in v else ""
    # 无预发布标记的正式版 > 任何预发布
    return (tuple(parts), 1 if not pre else 0, pre)


def is_newer(candidate: str, current: str = VERSION) -> bool:
    return version_tuple(candidate) > version_tuple(current)


def version_line() -> str:
    tail = f"+{BUILD}" if BUILD else ""
    return f"Moshan {VERSION}{tail}"
