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
import sys
import tempfile
import time
from pathlib import Path

TAILSCALE_BINS = ("/Applications/Tailscale.app/Contents/MacOS/Tailscale",
                  "/usr/local/bin/tailscale", "/opt/homebrew/bin/tailscale",
                  # Windows 装 Tailscale 后默认落在这里，加进来才能把
                  # Tailscale 的真证书（公信 CA，手机不再报警告）接上
                  r"C:\Program Files\Tailscale\tailscale.exe",
                  r"C:\Program Files (x86)\Tailscale\tailscale.exe")


def lan_ip() -> str | None:
    # Windows 上没有 ipconfig getifaddr 这种东西（`ipconfig` 是另一个命令，
    # 也没有 getifaddr 子命令），照原样跑只会白等三次超时。直接走下面的
    # 「连一个外网地址看自己用了哪张网卡」——这条在三条平台上都成立。
    if os.name != "nt":
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
    # psutil 在（两条平台都装了）就先用它：一次拿到所有网卡的地址，
    # Windows 上不用去猜 `ifconfig` 不存在这件事。
    try:
        import psutil
        for addrs in psutil.net_if_addrs().values():
            for a in addrs:
                if a.family == socket.AF_INET and (a.address or "").startswith(prefix):
                    return a.address
    except Exception:                               # noqa: BLE001
        pass
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
    if os.name == "nt":
        # Windows 没有 scutil，也没有 `xxx.local` 这个概念（除非装了 Bonjour），
        # 用裸主机名 —— 同一网段里 `\\EVO-X2` 这种写法是能解析的
        return (socket.gethostname() or "localhost").split(".")[0]
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
    """PATH 里（或安装包自带目录里）有没有 openssl 命令。

    顺序有讲究：打包版把 openssl.exe 放在**程序目录**，PATH 里根本没有它，
    所以先看自己身边，再问 PATH。以前只查 PATH，于是「把 openssl 打进包」
    这件事做了也等于没做 —— 打进去了但找不到。
    """
    override = os.environ.get("MOSHAN_OPENSSL", "").strip()
    if override and Path(override).exists():
        return override
    names = ("openssl.exe", "openssl") if os.name == "nt" else ("openssl",)
    roots = []
    for attr in ("_MEIPASS",):
        base = getattr(sys, attr, None)          # PyInstaller 解包目录
        if base:
            roots.append(Path(base))
    try:
        roots.append(Path(sys.executable).resolve().parent)   # 安装包根目录
    except (OSError, ValueError):
        pass
    roots.append(Path(__file__).resolve().parent)             # 源码运行时
    for root in roots:
        for n in names:
            for sub in (root, root / "openssl", root / "bin"):
                p = sub / n
                if p.is_file():
                    return str(p)
    return shutil.which("openssl")


# ------------------------------------------------------------------ cryptography 通道
# 为什么还要一条不依赖 openssl 的路：Windows 上根本没有 openssl 命令，
# 打包版也从来没把它打进去。`_generate()` 返回 None 之后，server.py 就把
# scheme 定成 http —— 令牌（URL 上的 ?k=）明文走局域网。这不是「少个功能」，
# 是「同网段谁抓一次包谁就拿到整台中台」。
#
# cryptography 是纯 Python 轮子（Windows 上 7 MB，装完约 15 MB），不需要任何
# 外部可执行文件，生成、解析、校验全在进程内。于是三条平台一条代码路径，
# 而 mac 上原来那条 openssl 路径原样保留当首选（它已经跑了很久，不出事）。
_UNSET = object()
_CRYPTO = _UNSET


def _crypto():
    """返回 (x509, hashes, serialization, rsa) 或 None（没装）。"""
    global _CRYPTO
    if _CRYPTO is not _UNSET:
        return _CRYPTO
    try:
        from cryptography import x509
        from cryptography.hazmat.primitives import hashes, serialization
        from cryptography.hazmat.primitives.asymmetric import rsa
        _CRYPTO = (x509, hashes, serialization, rsa)
    except ImportError:
        _CRYPTO = None
    return _CRYPTO


def _san_objects(x509, hosts: list[str]) -> list:
    out = []
    for h in hosts:
        try:
            out.append(x509.IPAddress(ipaddress.ip_address(h)))
        except ValueError:
            out.append(x509.DNSName(h))
    return out


def _generate_crypto(d: Path, hosts: list[str]) -> tuple[Path, Path] | None:
    """用 cryptography 签一张自签证书，落盘成 PEM。"""
    lib = _crypto()
    if lib is None:
        return None
    x509, hashes, ser, rsa = lib
    from cryptography.x509.oid import ExtendedKeyUsageOID, NameOID
    import datetime as dt

    crt, key = d / "server.crt", d / "server.key"
    try:
        pk = rsa.generate_private_key(public_exponent=65537, key_size=2048)
        name = x509.Name([x509.NameAttribute(
            NameOID.COMMON_NAME, hosts[0] if hosts else "localhost")])
        now = dt.datetime.now(dt.timezone.utc)
        cert = (
            x509.CertificateBuilder()
            .subject_name(name)
            .issuer_name(name)                      # 自签：签发者就是自己
            .public_key(pk.public_key())
            .serial_number(x509.random_serial_number())
            # 往回留 5 分钟：别的机器时钟略慢时不至于判成「还没生效」
            .not_valid_before(now - dt.timedelta(minutes=5))
            # 825 天是 Apple 对服务端证书的上限；自签本来就不被信任，
            # 但贴着这个上限走，换到哪个浏览器都不会因为「有效期过长」被额外拦
            .not_valid_after(now + dt.timedelta(days=825))
            .add_extension(x509.BasicConstraints(ca=False, path_length=None), critical=True)
            .add_extension(x509.KeyUsage(
                digital_signature=True, key_encipherment=True,
                content_commitment=False, data_encipherment=False,
                key_agreement=False, key_cert_sign=False, crl_sign=False,
                encipher_only=False, decipher_only=False), critical=True)
            .add_extension(x509.ExtendedKeyUsage([ExtendedKeyUsageOID.SERVER_AUTH]),
                           critical=False)
            # SAN 才是浏览器的判定依据，CN 现在基本没人看了
            .add_extension(x509.SubjectAlternativeName(_san_objects(x509, hosts)),
                           critical=False)
            .add_extension(x509.SubjectKeyIdentifier.from_public_key(pk.public_key()),
                           critical=False)
            .sign(pk, hashes.SHA256())
        )
        key.write_bytes(pk.private_bytes(
            ser.Encoding.PEM, ser.PrivateFormat.PKCS8, ser.NoEncryption()))
        crt.write_bytes(cert.public_bytes(ser.Encoding.PEM))
    except Exception as exc:                        # noqa: BLE001
        print(f"[tls] cryptography 生成证书失败：{type(exc).__name__}: {exc}")
        return None
    try:
        os.chmod(key, 0o600)
        os.chmod(crt, 0o644)
    except OSError:
        pass                                    # Windows 上 chmod 基本是空操作
    (d / "cert-hosts.json").write_text(
        json.dumps(hosts, ensure_ascii=False), encoding="utf-8")
    return crt, key


def _valid_crypto(crt: Path, key: Path, lib) -> bool:
    x509, _hashes, ser, _rsa = lib
    try:
        cert = x509.load_pem_x509_certificate(crt.read_bytes())
        sk = ser.load_pem_private_key(key.read_bytes(), password=None)
    except Exception:                               # noqa: BLE001
        return False
    # 46.x 起有 not_valid_after_utc（带时区）；老版本只有 naive 的 not_valid_after
    exp = getattr(cert, "not_valid_after_utc", None) or cert.not_valid_after
    try:
        left = exp.timestamp() - time.time()
    except (AttributeError, OSError, ValueError):
        left = 0
    if left <= 86400:                               # 剩不到一天就重签，别等它过期
        return False
    try:
        return (cert.public_key().public_numbers()
                == sk.public_key().public_numbers())
    except (AttributeError, ValueError):
        return False


def _covers_crypto(crt: Path, hosts: list[str], lib) -> bool:
    x509 = lib[0]
    try:
        cert = x509.load_pem_x509_certificate(crt.read_bytes())
        san = cert.extensions.get_extension_for_class(
            x509.SubjectAlternativeName).value
        have = {str(v) for v in san.get_values_for_type(x509.DNSName)}
        have |= {str(v) for v in san.get_values_for_type(x509.IPAddress)}
    except Exception:                               # noqa: BLE001
        # 解析不了就当「够用」：宁可留一张旧证书，也不要每轮启动都重签
        return True
    return all(h in have for h in hosts)


def _valid(crt: Path, key: Path) -> bool:
    """证书没过期且与私钥配套。"""
    if not (crt.exists() and key.exists()):
        return False
    lib = _crypto()
    if lib is not None:
        return _valid_crypto(crt, key, lib)
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
    lib = _crypto()
    if lib is not None:
        return _covers_crypto(crt, hosts, lib)
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
        # 没有 openssl：走 cryptography（Windows 上就是这个分支，原来在这里
        # 直接返回 None，整台中台退回明文 HTTP）
        return _generate_crypto(cert_dir, hosts)
    crt, key = cert_dir / "server.crt", cert_dir / "server.key"
    conf = _TEMPLATE.format(cn=hosts[0] if hosts else "localhost", san=_san(hosts))

    # mkstemp 返回的是「已经打开的 fd」+ 路径。这个 fd 必须立刻关掉：
    # POSIX 允许删除仍被打开的文件，所以 macOS 上一直没暴露；Windows 上
    # 最后 unlink 会直接 PermissionError: [WinError 32]，异常从 finally 冒出去，
    # 整个启动流程跟着挂掉（0.2.0/0.2.1 的 Windows 版首次启动就是这么崩的）。
    fd, tmp_name = tempfile.mkstemp(suffix=".cnf")
    os.close(fd)
    tmp = Path(tmp_name)
    try:
        tmp.write_text(conf, encoding="utf-8")
        try:
            r = subprocess.run([op, "req", "-x509", "-newkey", "rsa:2048", "-sha256",
                                "-days", "825", "-nodes", "-keyout", str(key),
                                "-out", str(crt), "-config", str(tmp)],
                               capture_output=True, text=True, timeout=60)
        except (OSError, subprocess.SubprocessError) as exc:   # 含超时、找不到 openssl
            print(f"[tls] 调用 openssl 失败：{exc}")
            return None
        if r.returncode != 0:
            print(f"[tls] 生成证书失败：{r.stderr.strip()[:300]}")
            return None
        os.chmod(key, 0o600)
        os.chmod(crt, 0o644)
        (cert_dir / "cert-hosts.json").write_text(
            json.dumps(hosts, ensure_ascii=False), encoding="utf-8")
        return crt, key
    finally:
        try:
            tmp.unlink()
        except OSError:
            # 清理失败不值得我们崩一次启动；临时目录里的残留由系统回收
            pass


def ensure(cert_dir: str | Path) -> tuple[Path, Path] | None:
    """确保有可用证书，返回 (cert, key)。Tailscale 真证书优先。

    这里是一道硬保险：证书只是「手机能不能用 https」的事，生不出来就退化成
    明文 HTTP，绝不能把整个启动流程带崩 —— 0.2.1 之前 Windows 首次启动就是
    被一个 PermissionError 从 finally 里冒出去干掉的。
    """
    try:
        return _ensure(cert_dir)
    except Exception as exc:  # noqa: BLE001
        print(f"[tls] 证书准备异常，退化为明文 HTTP：{exc}")
        return None


def _ensure(cert_dir: str | Path) -> tuple[Path, Path] | None:
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
