// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：证书层（对齐 `moshan/tls.py`）—— 找本机地址、备好 HTTPS 证书、给
//       `/api/status` 一份「用的哪张证书」。
// 不做：不读业务配置、不选端口；那些在 boot/main。
//
// 优先级：`certs/tailscale.crt`（公信 CA，手机零警告）> `certs/server.crt`
//        （自签，局域网点「继续」）。后者在 IP 变了或快过期时重签。
//
// 与 Python 的已知差别（有意为之，写在汇报里）：
//   - rcgen 0.13 + ring **不能生成 RSA 私钥**（ring 没有 RSA keygen，官方注释
//     就写着 key generation 只有 aws-lc-rs 支持）。Cargo.toml 不让动，所以这里
//     先按 Python 的口径试 RSA，拿不到就退回 ECDSA P-256。
//   - 校验既有证书时不用 openssl 命令（Windows 上根本没有），改用一个只认
//     「有效期 + SAN」的极简 DER 走查，三平台同一条路径。

use rcgen::{
    CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose,
    PKCS_ECDSA_P256_SHA256, PKCS_RSA_SHA256,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Tailscale 的安装位置。Windows 那两条要留着，否则「装了就优先用公信证书」
/// 这条在 Windows 上等于没实现。
const TAILSCALE_BINS: &[&str] = &[
    "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
    "/usr/local/bin/tailscale",
    "/opt/homebrew/bin/tailscale",
    r"C:\Program Files\Tailscale\tailscale.exe",
    r"C:\Program Files (x86)\Tailscale\tailscale.exe",
];

// ------------------------------------------------------------------ 本机地址

/// 跑一条命令，成功（退出码 0）返回 stdout；超时/失败/找不到命令返回 None。
///
/// 超时是自己轮询 `try_wait`：std 没有带超时的 `output()`，而 `tailscale status`
/// 在 daemon 没起来时能挂很久，不能让启动卡在这里。stdout 用独立线程抽干，
/// 免得子进程写满管道后卡死（`status --json` 的体量不小）。
fn run_cmd(argv: &[&str], timeout: Duration) -> Option<String> {
    let mut cmd = Command::new(argv[0]);
    cmd.args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().ok()?;
    let mut pipe = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        buf
    });

    let start = Instant::now();
    let mut ok = false;
    loop {
        match child.try_wait() {
            Ok(Some(st)) => {
                ok = st.success();
                break;
            }
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => break,
        }
    }
    let buf = reader.join().unwrap_or_default();
    if !ok {
        return None;
    }
    Some(String::from_utf8_lossy(&buf).to_string())
}

/// 主机名（不含域）。
fn hostname_short() -> String {
    #[cfg(unix)]
    {
        let mut buf = vec![0u8; 256];
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
        if rc == 0 {
            let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
            let name = String::from_utf8_lossy(&buf[..end]).to_string();
            let first = name.split('.').next().unwrap_or("").to_string();
            if !first.is_empty() {
                return first;
            }
        }
    }
    #[cfg(windows)]
    {
        if let Ok(v) = std::env::var("COMPUTERNAME") {
            let first = v.split('.').next().unwrap_or("").to_string();
            if !first.is_empty() {
                return first;
            }
        }
    }
    "localhost".to_string()
}

/// 证书里该有的本机名字。mac 用 Bonjour 的 `xxx.local`；其它平台用裸主机名。
pub fn local_hostname() -> String {
    #[cfg(target_os = "macos")]
    {
        if let Some(out) = run_cmd(&["scutil", "--get", "LocalHostName"], Duration::from_secs(3)) {
            let n = out.trim();
            if !n.is_empty() {
                return format!("{n}.local");
            }
        }
        return format!("{}.local", hostname_short());
    }
    #[cfg(not(target_os = "macos"))]
    {
        hostname_short()
    }
}

/// 所有网卡的 IPv4（unix 走 getifaddrs；Windows 返回空，由调用方兜底）。
fn iface_ips() -> Vec<String> {
    let mut out = Vec::new();
    #[cfg(unix)]
    unsafe {
        let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut head) != 0 {
            return out;
        }
        let mut cur = head;
        while !cur.is_null() {
            let a = (*cur).ifa_addr;
            if !a.is_null() && (*a).sa_family as i32 == libc::AF_INET {
                let sin = a as *const libc::sockaddr_in;
                let ip = u32::from_be((*sin).sin_addr.s_addr);
                out.push(format!(
                    "{}.{}.{}.{}",
                    (ip >> 24) & 0xff,
                    (ip >> 16) & 0xff,
                    (ip >> 8) & 0xff,
                    ip & 0xff
                ));
            }
            cur = (*cur).ifa_next;
        }
        libc::freeifaddrs(head);
    }
    out
}

/// 本机局域网 IPv4。先 `ipconfig getifaddr en0/en1/en2`（mac 上最准），
/// 再兜底「UDP connect 223.5.5.5 看自己用哪张网卡」——UDP 不真发包，
/// 只是让内核挑一条路由出来，三平台都成立。
///
/// Windows 上没有 `ipconfig getifaddr`（`ipconfig` 是另一个命令，也没有这个
/// 子命令），照原样跑只会白等三次超时，所以整段跳过。
pub fn lan_ip() -> Option<String> {
    if !cfg!(windows) {
        for iface in ["en0", "en1", "en2"] {
            if let Some(out) = run_cmd(&["ipconfig", "getifaddr", iface], Duration::from_secs(3)) {
                let ip = out.trim();
                if !ip.is_empty() {
                    return Some(ip.to_string());
                }
            }
        }
    }
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("223.5.5.5:80").ok()?;
    let ip = s.local_addr().ok()?.ip().to_string();
    if ip == "0.0.0.0" {
        None
    } else {
        Some(ip)
    }
}

/// Tailscale 登录后的 100.x 地址。
pub fn tailscale_ip() -> Option<String> {
    for ip in iface_ips() {
        if ip.starts_with("100.") {
            return Some(ip);
        }
    }
    for bin in TAILSCALE_BINS {
        if !Path::new(bin).exists() {
            continue;
        }
        if let Some(out) = run_cmd(&[bin, "ip", "-4"], Duration::from_secs(4)) {
            let line = out.trim().lines().next().unwrap_or("").trim().to_string();
            if !line.is_empty() {
                return Some(line);
            }
        }
    }
    None
}

/// MagicDNS 名字，例如 `pamphile-mac.tailnet-xxxx.ts.net`（去掉结尾的点）。
pub fn tailscale_name() -> Option<String> {
    for bin in TAILSCALE_BINS {
        if !Path::new(bin).exists() {
            continue;
        }
        let Some(out) = run_cmd(&[bin, "status", "--json"], Duration::from_secs(5)) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&out) else {
            continue;
        };
        let name = v
            .get("Self")
            .and_then(|s| s.get("DNSName"))
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim_end_matches('.')
            .to_string();
        if !name.is_empty() {
            return Some(name);
        }
    }
    None
}

/// 证书要覆盖的名字与地址（localhost / 127.0.0.1 / 主机名 / 局域网 IP /
/// Tailscale IP / MagicDNS 名 + 它的第一段）。去重保序。
pub fn alt_hosts() -> Vec<String> {
    let mut hosts = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        local_hostname(),
    ];
    for ip in [lan_ip(), tailscale_ip()].into_iter().flatten() {
        if !ip.is_empty() {
            hosts.push(ip);
        }
    }
    if let Some(name) = tailscale_name() {
        hosts.push(name.clone());
        let first = name.split('.').next().unwrap_or("").to_string();
        if !first.is_empty() {
            hosts.push(first);
        }
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for h in hosts {
        let h = h.trim().to_string();
        if !h.is_empty() && seen.insert(h.clone()) {
            out.push(h);
        }
    }
    out
}

// ------------------------------------------------------------------ 证书读写

/// 读一张证书 + 私钥（PEM）。读不出来返回 None。
pub fn load_pair(
    crt: &Path,
    key: &Path,
) -> Option<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let crt_text = std::fs::read(crt).ok()?;
    let key_text = std::fs::read(key).ok()?;
    let mut crt_rd = std::io::BufReader::new(std::io::Cursor::new(crt_text));
    let mut key_rd = std::io::BufReader::new(std::io::Cursor::new(key_text));

    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut crt_rd)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    if certs.is_empty() {
        return None;
    }
    let key = rustls_pemfile::private_key(&mut key_rd).ok()??;
    Some((certs, key))
}

// ---- 极简 DER 走查：只抽「notAfter」与 SAN ----------------------------------
//
// 为什么要自己走一遍：Cargo.toml 不让动，没有 x509 解析库；而「证书还能不能
// 用」只需要两个字段。解析一失败就当作不可用（重签一张），绝不让它把启动带崩。
//
// Certificate ::= SEQUENCE { tbsCertificate, signatureAlgorithm, signatureValue }
// TBSCertificate ::= SEQUENCE { [0] version, serialNumber, signature, issuer,
//                               validity, subject, subjectPublicKeyInfo, [3] extensions }
// validity ::= SEQUENCE { notBefore Time, notAfter Time }

struct DerReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> DerReader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let s = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(s)
    }

    /// 读一个 TLV，返回 (tag, value)。到末尾或格式不对返回 None。
    fn tlv(&mut self) -> Option<(u8, &'a [u8])> {
        let tag = *self.buf.get(self.pos)?;
        self.pos += 1;
        let first = *self.buf.get(self.pos)?;
        self.pos += 1;
        let len = if first & 0x80 == 0 {
            first as usize
        } else {
            let n = (first & 0x7f) as usize;
            if n == 0 || n > 4 {
                return None;
            }
            let mut v = 0usize;
            for _ in 0..n {
                v = (v << 8) | (*self.buf.get(self.pos)? as usize);
                self.pos += 1;
            }
            v
        };
        Some((tag, self.take(len)?))
    }
}

struct CertFields {
    not_after: i64,
    names: Vec<String>,
}

fn parse_time(tag: u8, v: &[u8]) -> Option<i64> {
    let s = std::str::from_utf8(v).ok()?.trim();
    let s = s.strip_suffix('Z').unwrap_or(s);
    // DER 只允许 Z 结尾；带时区偏移的证书少见到可以直接判失败（重签即可）。
    let fmt = if tag == 0x17 { "%y%m%d%H%M%S" } else { "%Y%m%d%H%M%S" };
    let dt = chrono::NaiveDateTime::parse_from_str(s, fmt).ok()?;
    Some(dt.and_utc().timestamp())
}

fn cert_fields(der: &[u8]) -> Option<CertFields> {
    let mut top = DerReader::new(der);
    let (t, cert) = top.tlv()?;
    if t != 0x30 {
        return None;
    }
    let mut c = DerReader::new(cert);
    let (t, tbs) = c.tlv()?;
    if t != 0x30 {
        return None;
    }

    let mut b = DerReader::new(tbs);
    // version 是可选的 [0] EXPLICIT；存在时 tag 是 0xA0。
    if b.buf.get(b.pos) == Some(&0xa0) {
        b.tlv()?;
    }
    b.tlv()?; // serialNumber
    b.tlv()?; // signature
    b.tlv()?; // issuer
    let (vt, validity) = b.tlv()?;
    if vt != 0x30 {
        return None;
    }
    let mut v = DerReader::new(validity);
    let (nb_tag, nb_val) = v.tlv()?;
    parse_time(nb_tag, nb_val)?; // notBefore 只用来确认格式
    let (na_tag, na_val) = v.tlv()?;
    let not_after = parse_time(na_tag, na_val)?;

    b.tlv()?; // subject
    b.tlv()?; // subjectPublicKeyInfo

    let mut names = Vec::new();
    while let Some((t, val)) = b.tlv() {
        if t != 0xa3 {
            continue; // 只关心 [3] extensions
        }
        let mut e = DerReader::new(val);
        let Some((0x30, seq)) = e.tlv() else {
            continue;
        };
        let mut list = DerReader::new(seq);
        while let Some((xt, ext)) = list.tlv() {
            if xt != 0x30 {
                continue;
            }
            let mut ex = DerReader::new(ext);
            let Some((ot, oid)) = ex.tlv() else {
                continue;
            };
            // subjectAltName = 2.5.29.17 → 06 03 55 1D 11
            if ot != 0x06 || oid != [0x55, 0x1d, 0x11] {
                continue;
            }
            let Some((t3, v3)) = ex.tlv() else {
                continue;
            };
            let octets = if t3 == 0x01 {
                // critical BOOLEAN 可选，跳过
                match ex.tlv() {
                    Some((_, o)) => o,
                    None => continue,
                }
            } else {
                v3
            };
            let mut g = DerReader::new(octets);
            let Some((0x30, gseq)) = g.tlv() else {
                continue;
            };
            let mut items = DerReader::new(gseq);
            while let Some((nt, nv)) = items.tlv() {
                match nt {
                    0x82 => names.push(String::from_utf8_lossy(nv).to_string()), // dNSName
                    0x87 => match nv.len() {
                        4 => names.push(format!("{}.{}.{}.{}", nv[0], nv[1], nv[2], nv[3])),
                        16 => {
                            let mut a = [0u8; 16];
                            a.copy_from_slice(nv);
                            names.push(std::net::Ipv6Addr::from(a).to_string());
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
        }
    }

    Some(CertFields { not_after, names })
}

/// 读第一张证书的 DER（文件里是 PEM，得先剥壳）。
fn first_cert_der(crt: &Path) -> Option<CertificateDer<'static>> {
    let text = std::fs::read(crt).ok()?;
    let mut rd = std::io::BufReader::new(std::io::Cursor::new(text));
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut rd)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    certs.into_iter().next()
}

/// 证书没过期（剩超过一天）且私钥与它配套。
fn pair_valid(crt: &Path, key: &Path) -> bool {
    if !crt.is_file() || !key.is_file() {
        return false;
    }
    let Some(der) = first_cert_der(crt) else {
        return false;
    };
    let Some(fields) = cert_fields(der.as_ref()) else {
        return false;
    };
    let now = chrono::Utc::now().timestamp();
    if fields.not_after - now <= 86400 {
        return false;
    }
    // rustls 建配置时会校验「私钥的 SPKI 与证书公钥一致」，正好当配套检查用。
    let Some((certs, key_der)) = load_pair(crt, key) else {
        return false;
    };
    rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key_der)
        .is_ok()
}

/// 现有证书是否已经包含全部名字（IP 变了就要重签）。
/// 解析不出来时按 Python 的口径当作「够用」：宁可留一张旧证书，
/// 也不要每轮启动都重签。
fn cert_covers(crt: &Path, hosts: &[String]) -> bool {
    let Some(der) = first_cert_der(crt) else {
        return true;
    };
    let Some(fields) = cert_fields(der.as_ref()) else {
        return true;
    };
    hosts.iter().all(|h| fields.names.iter().any(|n| n == h))
}

fn chmod(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode); // Windows 上 chmod 基本是空操作
    }
}

/// 与 `tls.py` 的 `_TEMPLATE` 逐字相同。SAN 是浏览器的判定依据，CN 早没人看了。
const OPENSSL_CNF: &str = "[req]\ndistinguished_name = dn\nx509_extensions = v3_req\nprompt = no\n\n[dn]\nCN = {cn}\n\n[v3_req]\nbasicConstraints = critical,CA:FALSE\nkeyUsage = critical,digitalSignature,keyEncipherment\nextendedKeyUsage = serverAuth\nsubjectAltName = {san}\nsubjectKeyIdentifier = hash\nauthorityKeyIdentifier = keyid:always\n";

/// 找 `openssl` 命令：先看 `MOSHAN_OPENSSL`，再看 PATH。
/// 打包版里 openssl 可能就在程序旁边，所以先看自身目录（Python 侧踩过这个坑）。
fn openssl_bin() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("MOSHAN_OPENSSL") {
        let p = p.trim();
        if !p.is_empty() && Path::new(p).is_file() {
            return Some(PathBuf::from(p));
        }
    }
    let names: &[&str] = if cfg!(windows) {
        &["openssl.exe", "openssl"]
    } else {
        &["openssl"]
    };
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            roots.push(d.to_path_buf());
        }
    }
    roots.push(PathBuf::from("."));
    for root in roots {
        for n in names {
            for sub in [root.clone(), root.join("openssl"), root.join("bin")] {
                let p = sub.join(n);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    }
    for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        for n in names {
            let p = dir.join(n);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// openssl 通道：与 Python 的 `_generate()` 同一条命令、同一个配置模板，
/// 产出的是 **RSA 2048** —— 老安卓只认这个。
fn generate_openssl(dir: &Path, hosts: &[String]) -> Option<(PathBuf, PathBuf)> {
    let op = openssl_bin()?;
    let crt = dir.join("server.crt");
    let key = dir.join("server.key");
    let cn = hosts.first().map(|s| s.as_str()).unwrap_or("localhost");
    let san = hosts
        .iter()
        .map(|h| {
            if h.parse::<std::net::IpAddr>().is_ok() {
                format!("IP:{h}")
            } else {
                format!("DNS:{h}")
            }
        })
        .collect::<Vec<_>>()
        .join(",");
    let cnf = dir.join(format!("openssl-{}.cnf", std::process::id()));
    let text = OPENSSL_CNF.replace("{cn}", cn).replace("{san}", &san);
    std::fs::write(&cnf, text).ok()?;
    // 立刻把 fd 关掉：Windows 上「打开着就删」会 PermissionError，
    // Python 的 0.2.0 首次启动就是这么崩的（write 完即关，这里没有 fd 泄漏）。
    let out = std::process::Command::new(&op)
        .args([
            "req", "-x509", "-newkey", "rsa:2048", "-sha256", "-days", "825", "-nodes",
        ])
        .arg("-keyout")
        .arg(&key)
        .arg("-out")
        .arg(&crt)
        .arg("-config")
        .arg(&cnf)
        .output();
    let _ = std::fs::remove_file(&cnf);
    let out = out.ok()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        eprintln!("[tls] openssl 生成证书失败：{}", err.trim().chars().take(300).collect::<String>());
        return None;
    }
    chmod(&key, 0o600);
    chmod(&crt, 0o644);
    Some((crt, key))
}

fn generate(dir: &Path, hosts: &[String]) -> Option<(PathBuf, PathBuf)> {
    // 先走 openssl（RSA 2048，与 Python 同一条命令）。mac/多数 Linux 发行版都有；
    // Windows 没有，落到下面的 rcgen 分支（ECDSA P-256，老安卓可能不认 —— 这是
    // 唯一与 Python 有实质差别的地方，会在启动日志里明说）。
    if let Some((crt, key)) = generate_openssl(dir, hosts) {
        write_hosts_json(dir, hosts);
        return Some((crt, key));
    }
    let key = {
        eprintln!(
            "[tls] 没有可用的 openssl，改用 rcgen 签 ECDSA P-256 —— \
             老安卓可能不认这张证书（Python 在同样机器上签的是 RSA）"
        );
        KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).ok()?
    };

    let mut params = CertificateParams::new(hosts.to_vec()).ok()?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, hosts.first().map(|s| s.as_str()).unwrap_or("localhost"));

    // 有效期要精确到秒。rcgen 只给了 date_time_ymd（当天 0 点），
    // 但 OffsetDateTime 支持加 std::time::Duration，拼回去即可，不用把 time 引进来。
    let now = chrono::Utc::now();
    let stamp = |t: chrono::DateTime<chrono::Utc>| {
        use chrono::{Datelike, Timelike};
        rcgen::date_time_ymd(t.year(), t.month() as u8, t.day() as u8)
            + std::time::Duration::from_secs(t.time().num_seconds_from_midnight() as u64)
    };
    // 往回留 5 分钟：别的机器时钟略慢时不至于判成「还没生效」。
    params.not_before = stamp(now - chrono::Duration::minutes(5));
    // 825 天是 Apple 对服务端证书的上限。
    params.not_after = stamp(now + chrono::Duration::days(825));

    params.is_ca = IsCa::ExplicitNoCa; // 显式写 CA:FALSE（NoCa 会整个扩展都不写）
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];

    let cert = params.self_signed(&key).ok()?;

    let crt_path = dir.join("server.crt");
    let key_path = dir.join("server.key");
    std::fs::write(&crt_path, cert.pem()).ok()?;
    std::fs::write(&key_path, key.serialize_pem()).ok()?;
    chmod(&key_path, 0o600);
    chmod(&crt_path, 0o644);

    write_hosts_json(dir, hosts);
    Some((crt_path, key_path))
}

/// 与 `json.dumps(hosts, ensure_ascii=False)` 同形状（分隔符是 ", "，没有缩进）。
fn write_hosts_json(dir: &Path, hosts: &[String]) {
    let text = format!(
        "[{}]",
        hosts
            .iter()
            .map(|h| format!("\"{}\"", h.replace('\\', "\\\\").replace('"', "\\\"")))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let _ = std::fs::write(dir.join("cert-hosts.json"), text);
}

/// 备好可用证书，返回 (cert, key)。生不出来返回 None —— 调用方据此退化为明文。
///
/// 这里是一道硬保险：证书只是「手机能不能用 https」的事，生不出来就退化明文，
/// 绝不能把整个启动流程带崩（Python 侧被 PermissionError 从 finally 里冒出去
/// 干掉过一次，所以那边也是 try/except 全包）。
pub fn ensure(cert_dir: &Path) -> Option<(PathBuf, PathBuf)> {
    let dir = cert_dir.to_path_buf();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || ensure_inner(&dir)));
    match r {
        Ok(v) => v,
        Err(_) => {
            eprintln!("[tls] 证书准备 panic，退化为明文 HTTP");
            None
        }
    }
}

fn ensure_inner(dir: &Path) -> Option<(PathBuf, PathBuf)> {
    if std::fs::create_dir_all(dir).is_err() {
        eprintln!("[tls] 建不了证书目录 {}，退化为明文 HTTP", dir.display());
        return None;
    }

    // 1) Tailscale 签发的真证书优先：公信 CA，手机零警告。
    let ts_crt = dir.join("tailscale.crt");
    let ts_key = dir.join("tailscale.key");
    if pair_valid(&ts_crt, &ts_key) {
        return Some((ts_crt, ts_key));
    }

    let crt = dir.join("server.crt");
    let key = dir.join("server.key");
    let hosts = alt_hosts();
    if pair_valid(&crt, &key) && cert_covers(&crt, &hosts) {
        return Some((crt, key));
    }

    // 2) 有效期不足或缺名字：重签，旧的留一份便于排查。
    if crt.exists() {
        let _ = std::fs::rename(&crt, dir.join("server.crt.old"));
        let _ = std::fs::rename(&key, dir.join("server.key.old"));
    }
    generate(dir, &hosts)
}

/// `/api/status` 与启动日志用：证书目录、文件名、是不是公信 CA、覆盖了哪些名字。
/// 字段顺序照 Python（dir / file / trusted_ca / hosts）。
pub fn describe(cert_dir: &Path) -> Value {
    let using_ts = cert_dir.join("tailscale.crt").exists();
    json!({
        "dir": cert_dir.to_string_lossy().to_string(),
        "file": if using_ts { "tailscale.crt" } else { "server.crt" },
        "trusted_ca": using_ts,
        "hosts": alt_hosts(),
    })
}
