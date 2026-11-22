"""把 HTTPS 的信任链锚到随包分发的 CA 证书上。

为什么需要这个模块
------------------
应用里所有对外请求都走 `urllib.request.urlopen()`（12 处：检查更新、抓网页、
装插件、探测端点…）。urllib 走的是 `ssl.create_default_context()`，它调的
`load_default_certs()` 依赖 OpenSSL **编译期写死**的默认证书路径。

PyInstaller 打出来的应用带的是自己的 OpenSSL，那个默认路径在别人的机器上
通常不存在（macOS 上是 `/usr/local/etc/openssl@3/cert.pem` 之类的 Homebrew 路径）。
结果就是：安装版里任意一个 https 请求都抛

    CERTIFICATE_VERIFY_FAILED: unable to get local issuer certificate

而源码运行时完全看不出来 —— 系统 python 能找到 `/etc/ssl/cert.pem`，
site-packages 里也有 certifi。这个 bug 只在安装版出现，所以本地自检、
CI、源码态手工验证全都拦不住它。0.2.0 就是这样发出去的：装机用户点
「检查更新」必然失败，抓网页、装插件一样。

修法
----
启动最早期把 `SSL_CERT_FILE` 指向 certifi 自带的 cacert.pem。OpenSSL 读
默认证书时会先看这个环境变量，所以之后每一次 urlopen 都拿到正确的根证书。
不 monkeypatch ssl、不动业务代码，避免和上游逐字同步的文件产生冲突。

调用点
------
- `launcher.py`：打包入口，`main()` 一进来就调（覆盖安装版的所有路径）。
- `server.py`：直接 `python server.py` 时也能生效（源码态本来就能连，
  这里只是让两种启动方式行为一致）。
"""

from __future__ import annotations

import os
import ssl
import sys
from pathlib import Path
from typing import Optional

ENV_CERT_FILE = "SSL_CERT_FILE"

_installed = ""


def _candidates() -> list:
    """按优先级列出可能的 cacert.pem。"""
    out = []

    # 1) 正常安装/打包：certifi 就在 sys.path 上
    try:
        import certifi

        out.append(Path(certifi.where()))
    except Exception:                                   # noqa: BLE001
        pass

    # 2) PyInstaller onedir：数据文件被摊到 _internal（或顶层）里
    base = getattr(sys, "_MEIPASS", "")
    if base:
        for rel in ("certifi/cacert.pem", "_internal/certifi/cacert.pem", "cacert.pem"):
            out.append(Path(base) / rel)

    # 3) 系统 CA，作为最后兜底（Linux 发行版一般都有）
    out += [Path("/etc/ssl/cert.pem"), Path("/etc/ssl/certs/ca-certificates.crt"),
            Path("/etc/pki/tls/certs/ca-bundle.crt")]

    return out


def _certifi_path() -> Optional[Path]:
    for p in _candidates():
        try:
            if p.is_file() and p.stat().st_size > 1024:
                return p
        except OSError:
            continue
    return None


def install() -> str:
    """设置 SSL_CERT_FILE，返回实际使用的证书路径（找不到则返回空串）。

    幂等；用户自己设了 SSL_CERT_FILE 时不覆盖 —— 企业自签网关那种场景
    需要尊重用户在启动脚本里指定的证书。
    """
    global _installed
    if _installed:
        return _installed

    existing = (os.environ.get(ENV_CERT_FILE) or "").strip()
    if existing and Path(existing).is_file():
        _installed = existing
        return _installed

    found = _certifi_path()
    if not found:
        return ""

    os.environ[ENV_CERT_FILE] = str(found)
    _installed = str(found)
    return _installed


def stats() -> dict:
    """自检用：当前默认上下文里到底装进去多少张根证书。

    `x509` 为 0 就意味着任何 https 都会失败 —— 这是判断「安装版能不能联网」
    最直接的一个数，不需要真的发出请求。
    """
    path = install()
    try:
        ctx = ssl.create_default_context()
        count = int(ctx.cert_store_stats().get("x509", 0))
    except Exception as exc:                            # noqa: BLE001
        return {"cafile": path, "x509": -1, "error": f"{type(exc).__name__}: {exc}"}
    return {"cafile": path, "x509": count, "frozen": bool(getattr(sys, "frozen", False))}
