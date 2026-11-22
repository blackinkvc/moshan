#!/usr/bin/env python3
"""证书层：给口袋服务签发本地 HTTPS 证书。

为什么不是简单的“自签一张算了”：
  - 手机访问时如果证书换了，Safari 每次都要重新点“仍然继续”，所以证书要落盘复用；
  - 局域网 IP 会变（换 WiFi、DHCP 续租），证书的 SAN 就得跟着变，否则浏览器直接拒绝；
  - 如果用户在 certs/ 里放了 Tailscale 签发的真证书（tailscale cert 产出），
    必须优先用它 —— 那是公信 CA，手机不再有任何警告。

优先级：certs/tailscale.crt + .key  >  certs/server.crt + .key（必要时重签）
"""
from __future__ import annotations

import ipaddress
import json
import os
import shutil
import socket
import subprocess
import tempfile
from pathlib import Path

TAILSCALE_BINS = ("/Applications/Tailscale.app/Contents/MacOS/Tailscale",
                  "/usr/local/bin/tailscale", "/opt/homebrew/bin/tailscale")


def lan_ip() -> str | None:
    for iface in ("en0", "en1", "en2"):
        try:
            r = subprocess.run(["ipconfig", "getifaddr", iface],
                               capture_output=True, text=True, timeout=3)
            if r.returncode == 0 and r.stdout.strip():
                return r.stdout.strip()
        except (OSError, subprocess.SubprocessError):
            pass
    try:
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.connect(("223.5.5.5", 80))
        ip = s.getsockname()[0]
        s.close()
        return ip
    except OSError:
        return None


def tailscale_ip() -> str | None:
    """Tailscale 已经登录时返回 100.x 地址，否则 None。"""
    ip = _iface_ip_by_prefix("100.")
    if ip:
        return ip
    for bin_path in TAILSCALE_BINS:
        if not Path(bin_path).exists():
            continue
        try:
            r = subprocess.run([bin_path, "ip", "-4"], capture_output=True,
                               text=True, timeout=4)
            if r.returncode == 0 and r.stdout.strip():
                return r.stdout.strip().splitlines()[0].strip()
        except (OSError, subprocess.SubprocessError):
            pass
    return None


def tailscale_name() -> str | None:
    """MagicDNS 名字，例如 pamphile-mac.tailnet-xxxx.ts.net。"""
    for bin_path in TAILSCALE_BINS:
        if not Path(bin_path).exists():
            continue
        try:
            r = subprocess.run([bin_path, "status", "--json"],
                               capture_output=True, text=True, timeout=5)
            if r.returncode != 0 or not r.stdout.strip():
                continue
            d = json.loads(r.stdout)
            name = (d.get("Self") or {}).get("DNSName") or ""
            return name.rstrip(".") or None
        except (OSError, ValueError, subprocess.SubprocessError):
            pass
    return None


def _iface_ip_by_prefix(prefix: str) -> str | None:
    try:
        r = subprocess.run(["ifconfig"], capture_output=True, text=True, timeout=4)
    except (OSError, subprocess.SubprocessError):
        return None
    for line in r.stdout.splitlines():
        line = line.strip()
        if line.startswith("inet ") and prefix in line:
            return line.split()[1]
    return None


def local_hostname() -> str:
    try:
        n = subprocess.run(["scutil", "--get", "LocalHostName"],
                           capture_output=True, text=True, timeout=3).stdout.strip()
        if n:
            return f"{n}.local"
    except (OSError, subprocess.SubprocessError):
        pass
    return f"{socket.gethostname().split('.')[0]}.local"


def alt_hosts() -> list[str]:
    """证书需要覆盖的名字与地址，去重后返回。"""
    hosts = ["localhost", "127.0.0.1", local_hostname()]
    for ip in (lan_ip(), tailscale_ip()):
        if ip:
            hosts.append(ip)
    name = tailscale_name()
    if name:
        hosts.append(name)
        hosts.append(name.split(".")[0])
    seen, out = set(), []
    for h in hosts:
        h = h.strip()
        if h and h not in seen:
            seen.add(h)
            out.append(h)
    return out


def _san(hosts: list[str]) -> str:
    parts = []
    for h in hosts:
        try:
            ipaddress.ip_address(h)
            parts.append(f"IP:{h}")
        except ValueError:
            parts.append(f"DNS:{h}")
    return ",".join(parts)


_TEMPLATE = """[req]
distinguished_name = dn
x509_extensions = v3_req
prompt = no

[dn]
CN = {cn}

[v3_req]
basicConstraints = critical,CA:FALSE
keyUsage = critical,digitalSignature,keyEncipherment
extendedKeyUsage = serverAuth
subjectAltName = {san}
subjectKeyIdentifier = hash
authorityKeyIdentifier = keyid:always
"""


def _openssl() -> str | None:
    return shutil.which("openssl")


def _valid(crt: Path, key: Path) -> bool:
    """证书没过期且与私钥配套。"""
    if not (crt.exists() and key.exists()):
        return False
    op = _openssl()
    if not op:
        return crt.exists() and key.exists()
    try:
        r = subprocess.run([op, "x509", "-in", str(crt), "-noout", "-checkend", "86400"],
                           capture_output=True, text=True, timeout=8)
        if r.returncode != 0:
            return False
        a = subprocess.run([op, "x509", "-in", str(crt), "-noout", "-pubkey"],
                           capture_output=True, text=True, timeout=8).stdout
        b = subprocess.run([op, "pkey", "-in", str(key), "-pubout"],
                           capture_output=True, text=True, timeout=8).stdout
        return bool(a.strip()) and a.strip() == b.strip()
    except (OSError, subprocess.SubprocessError):
        return True


def _cert_covers(crt: Path, hosts: list[str]) -> bool:
    """现有证书是否已经包含全部名字（IP 变了就要重签）。"""
    op = _openssl()
    if not op:
        return True
    try:
        r = subprocess.run([op, "x509", "-in", str(crt), "-noout", "-ext",
                            "subjectAltName"], capture_output=True, text=True, timeout=8)
    except (OSError, subprocess.SubprocessError):
        return True
    text = r.stdout
    for h in hosts:
        if f"DNS:{h}" in text or f"IP Address:{h}" in text:
            continue
        return False
    return True


def _generate(cert_dir: Path, hosts: list[str]) -> tuple[Path, Path] | None:
    op = _openssl()
    if not op:
        return None
    crt, key = cert_dir / "server.crt", cert_dir / "server.key"
    conf = _TEMPLATE.format(cn=hosts[0] if hosts else "localhost", san=_san(hosts))
    tmp = Path(tempfile.mkstemp(suffix=".cnf")[1])
    try:
        tmp.write_text(conf, encoding="utf-8")
        r = subprocess.run([op, "req", "-x509", "-newkey", "rsa:2048", "-sha256",
                            "-days", "825", "-nodes", "-keyout", str(key),
                            "-out", str(crt), "-config", str(tmp)],
                           capture_output=True, text=True, timeout=60)
        if r.returncode != 0:
            print(f"[tls] 生成证书失败：{r.stderr.strip()[:300]}")
            return None
        os.chmod(key, 0o600)
        os.chmod(crt, 0o644)
        (cert_dir / "cert-hosts.json").write_text(
            json.dumps(hosts, ensure_ascii=False), encoding="utf-8")
        return crt, key
    finally:
        tmp.unlink(missing_ok=True)


def ensure(cert_dir: str | Path) -> tuple[Path, Path] | None:
    """确保有可用证书，返回 (cert, key)。Tailscale 真证书优先。"""
    d = Path(cert_dir)
    d.mkdir(parents=True, exist_ok=True)

    ts_crt, ts_key = d / "tailscale.crt", d / "tailscale.key"
    if _valid(ts_crt, ts_key):
        return ts_crt, ts_key

    crt, key = d / "server.crt", d / "server.key"
    hosts = alt_hosts()
    if _valid(crt, key) and _cert_covers(crt, hosts):
        return crt, key
    # 有效期不足或缺名字：重签，旧的留一份便于排查
    if crt.exists():
        try:
            crt.replace(d / "server.crt.old")
            key.replace(d / "server.key.old")
        except OSError:
            pass
    return _generate(d, hosts)


def describe(cert_dir: str | Path) -> dict:
    """给 /api/status 和启动日志用的证书信息。"""
    d = Path(cert_dir)
    using_ts = (d / "tailscale.crt").exists()
    return {
        "dir": str(d),
        "file": "tailscale.crt" if using_ts else "server.crt",
        "trusted_ca": using_ts,
        "hosts": alt_hosts(),
    }
