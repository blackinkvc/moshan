// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：w4c 泳道自己要用的小工具 —— 时间戳、命名规则、链接拆分、外部命令与
//       Python 解释器定位。
// 不做：不碰 HTTP 契约（那在 common.rs），不复制 storage 已有的路径/大小工具。
//
// 为什么要单独一份：article.py / linkbox.py / dedupe_runner.py 各自带了一套
// 「命名去重」「人类可读大小」「解释器在哪」的实现，细节还不一样
// （article 是 `名 (2)`，storage 是 `名-2`；linkbox 的 human 用 KiB 不是 KB）。
// 对拍看的是字节，只能照着各自的源码抄，不能统一。

#![allow(dead_code)]

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- 时间

/// `article.now()`：固定 +08:00 的 ISO 秒级时间。
/// 与 Python 一样**不看本机时区** —— 两边跑在不同机器上也得到同一口径。
pub fn now_cst() -> String {
    let t = chrono::Utc::now() + chrono::Duration::hours(8);
    t.format("%Y-%m-%dT%H:%M:%S+08:00").to_string()
}

/// `datetime.now().strftime(fmt)`：本机时区。
pub fn local_str(fmt: &str) -> String {
    chrono::Local::now().format(fmt).to_string()
}

pub fn epoch_secs() -> i64 {
    chrono::Local::now().timestamp()
}

// ---------------------------------------------------------------- 命名

/// `article.safe_name`：非法字符换空格 → HTML 反转义 → 折叠空白 → 去首尾
/// 标点 → 截 60 字（按字符，不按字节）。
pub fn safe_name(s: &str, limit: usize) -> String {
    let re_bad = regex::Regex::new(r#"[\\/:*?"<>|\r\n\t]+"#).unwrap();
    let re_sp = regex::Regex::new(r"\s+").unwrap();
    let mut t = re_bad.replace_all(s, " ").into_owned();
    t = html_unescape(&t);
    t = re_sp.replace_all(&t, " ").into_owned();
    let trimmed = t.trim_matches(|c: char| " .。·-—_".contains(c)).to_string();
    let cut: String = trimmed.chars().take(limit).collect();
    let cut = cut.trim_end().to_string();
    if cut.is_empty() {
        "无标题".to_string()
    } else {
        cut
    }
}

/// `html.unescape` 的常用子集：命名场景里几乎只会出现这几种。
pub fn html_unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(semi) = rest.find(';') else {
            out.push_str(rest);
            return out;
        };
        let ent = &rest[1..semi];
        let rep = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some('\u{a0}'),
            _ if ent.starts_with("#x") || ent.starts_with("#X") => {
                u32::from_str_radix(&ent[2..], 16).ok().and_then(char::from_u32)
            }
            _ if ent.starts_with('#') => ent[1..].parse::<u32>().ok().and_then(char::from_u32),
            _ => None,
        };
        match rep {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                // 不认识的实体原样保留（与 html.unescape 对未知实体的处理不同，
                // 但两者都不该在标题里出现；保字形比臆测更安全）
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// `article.unique_dir`：`名` → `名 (2)` → … → `名 (99)` → `名 (秒)`。
/// 注意分隔符是**空格加圆括号**，跟 storage.unique_path 的 `-2` 不是一套。
pub fn unique_dir(parent: &Path, name: &str) -> PathBuf {
    let target = parent.join(name);
    if !target.exists() {
        return target;
    }
    for i in 2..100 {
        let cand = parent.join(format!("{name} ({i})"));
        if !cand.exists() {
            return cand;
        }
    }
    parent.join(format!("{name} ({})", epoch_secs()))
}

/// `linkbox.human`：`1.5 KiB` 这一套（与 storage.human 的 KB 分档不同）。
pub fn human_link(n: u64) -> String {
    let mut v = n as f64;
    for unit in ["B", "KiB", "MiB", "GiB"] {
        if v < 1024.0 || unit == "GiB" {
            return if unit == "B" {
                format!("{:.0} B", v)
            } else {
                format!("{:.1} {unit}", v)
            };
        }
        v /= 1024.0;
    }
    format!("{:.1} GiB", v)
}

// ---------------------------------------------------------------- 链接

/// `linkbox.split_links`：一条消息里的全部链接，按出现顺序去重，最多 10 条。
pub fn split_links(text: &str) -> Vec<String> {
    let re = regex::Regex::new(r#"https?://[^\s，。、；）)\]】"']+"#).unwrap();
    let mut out: Vec<String> = Vec::new();
    for m in re.find_iter(text) {
        let u = m.as_str().trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}', '>', '）', '】']);
        if u.is_empty() || out.iter().any(|x| x == u) {
            continue;
        }
        out.push(u.to_string());
        if out.len() >= 10 {
            break;
        }
    }
    out
}

/// `linkbox.url_key`：取第一条链接（**不转小写**，短链是 base62）。
pub fn url_key(text: &str) -> String {
    split_links(text).into_iter().next().unwrap_or_default()
}

pub fn first_line(s: &str) -> String {
    let t = s.trim();
    if t.is_empty() {
        return String::new();
    }
    t.split('\n').next().unwrap_or("").trim().to_string()
}

/// `str(x)[:n]`：按字符截断（Rust 的切片是字节，直接切会 panic）。
pub fn trunc(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

// ---------------------------------------------------------------- 数字

/// Python `round(x, nd)` 的浮点结果。用格式化再解析，走的是同一条
/// 「对二进制精确值做正确舍入」的路径 —— 自己 `(x*10).round()/10` 在
/// .05 这种边界上会差一格（round-half-away vs half-even）。
pub fn py_round(x: f64, nd: usize) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{:.*}", nd, x).parse().unwrap_or(x)
}

/// Python `int(round(x))`。
pub fn py_rint(x: f64) -> i64 {
    if !x.is_finite() {
        return 0;
    }
    format!("{:.0}", x).parse().unwrap_or(0)
}

/// Python 里 `{value}` 插一个 JSON 数字的样子：整数照原样，浮点带 `.0`。
/// `serde_json` 的 Display 会把 25.0 打成 `25`，那就是字节分叉。
pub fn num_str(v: &Value) -> String {
    match v {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(u) = n.as_u64() {
                u.to_string()
            } else {
                let f = n.as_f64().unwrap_or(0.0);
                float_repr(f)
            }
        }
        Value::String(s) => s.clone(),
        Value::Bool(b) => (if *b { "True" } else { "False" }).to_string(),
        Value::Null => "None".to_string(),
        _ => String::new(),
    }
}

/// Python `repr(float)`：整数值也要带 `.0`，其余用最短往返表示。
pub fn float_repr(f: f64) -> String {
    if f == f.trunc() && f.abs() < 1e16 {
        format!("{:.1}", f)
    } else {
        format!("{f}")
    }
}

pub fn f64_of(v: Option<&Value>) -> f64 {
    match v {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0.0),
        Some(Value::Bool(b)) => *b as i64 as f64,
        _ => 0.0,
    }
}

pub fn i64_of(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n.as_i64().unwrap_or_else(|| n.as_f64().unwrap_or(0.0) as i64),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        Some(Value::Bool(b)) => *b as i64,
        _ => 0,
    }
}

pub fn s_of(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => num_str(&Value::Number(n.clone())),
        Some(Value::Bool(b)) => (if *b { "True" } else { "False" }).to_string(),
        _ => String::new(),
    }
}

/// `d.get(k) or default`：空串/null/0/false 都退回默认值。
pub fn or_str(v: Option<&Value>, d: &str) -> String {
    let s = s_of(v);
    if s.is_empty() { d.to_string() } else { s }
}

// ---------------------------------------------------------------- 外部命令

/// `subprocess.run(cmd, capture_output=True, text=True, timeout=T)`。
/// 超时返回 None（调用方自己决定文案）；找不到可执行文件也返回 None。
pub fn run_cmd(argv: &[String], timeout_secs: u64, cwd: Option<&Path>) -> Option<(i32, String, String)> {
    let mut cmd = std::process::Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    let mut child = cmd.spawn().ok()?;
    let start = Instant::now();
    let limit = Duration::from_secs(timeout_secs);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() >= limit {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(40));
            }
            Err(_) => {
                let _ = child.kill();
                return None;
            }
        }
    }
    let out = child.wait_with_output().ok()?;
    Some((
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

/// 12 位十六进制随机串（`uuid.uuid4().hex[:12]` 的替身）。
/// 不引 uuid crate：Cargo.toml 是别人的地盘，而且这里只要「够随机、是十六进制」。
pub fn rand_hex(n: usize) -> String {
    use std::io::Read;
    let mut buf = vec![0u8; n];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok();
    if !ok {
        let mut seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
            ^ (std::process::id() as u64) << 32;
        for b in buf.iter_mut() {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            *b = (seed & 0xff) as u8;
        }
    }
    buf.iter().map(|b| format!("{b:02x}")).collect::<String>()[..n].to_string()
}

// ---------------------------------------------------------------- 解释器

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `MOSHAN_CUNTU` 或 `~/Documents/cuntu`。
pub fn cuntu_dir() -> PathBuf {
    let raw = std::env::var("MOSHAN_CUNTU")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("{}/Documents/cuntu", home().to_string_lossy()));
    PathBuf::from(raw)
}

/// linkbox.CUNTU_PY：直接指向 venv 里的解释器，不做多级回退。
pub fn cuntu_py() -> PathBuf {
    let d = cuntu_dir();
    if cfg!(windows) {
        d.join(".venv").join("Scripts").join("python.exe")
    } else {
        d.join(".venv").join("bin").join("python")
    }
}

/// `pyexe.resolve(explicit, venv)` 的候选顺序（POSIX 分支）：
/// venv → 当前解释器 → PATH 上的 python3/python → /opt/homebrew|/usr/local|/usr/bin。
pub fn resolve_python(explicit: Option<&str>, venv: &Path) -> (Option<PathBuf>, Vec<String>) {
    let mut tried: Vec<String> = Vec::new();
    if let Some(e) = explicit.filter(|s| !s.trim().is_empty()) {
        let p = PathBuf::from(e);
        tried.push(p.to_string_lossy().to_string());
        if p.is_file() {
            return (Some(p), tried);
        }
    }
    let mut cands: Vec<PathBuf> = Vec::new();
    cands.push(if cfg!(windows) {
        venv.join("Scripts").join("python.exe")
    } else {
        venv.join("bin").join("python")
    });
    if let Ok(exe) = std::env::current_exe() {
        cands.push(exe);
    }
    for name in ["python3", "python"] {
        if let Some(p) = which(name) {
            cands.push(p);
        }
    }
    for dir in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"] {
        for name in ["python3", "python"] {
            let p = Path::new(dir).join(name);
            if p.is_file() {
                cands.push(p);
            }
        }
    }
    let mut seen: Vec<String> = Vec::new();
    for p in cands {
        let k = p.to_string_lossy().to_string();
        if seen.contains(&k) {
            continue;
        }
        seen.push(k.clone());
        tried.push(k);
        if p.is_file() {
            return (Some(p), tried);
        }
    }
    (None, tried)
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let p = dir.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// `pyexe.not_found_hint`
pub fn not_found_hint(tried: &[String]) -> String {
    if tried.is_empty() {
        return "找不到 Python（没有任何候选路径）".to_string();
    }
    let head = tried.iter().take(3).cloned().collect::<Vec<_>>().join("、");
    let more = if tried.len() > 3 {
        format!(" 等 {} 处", tried.len())
    } else {
        String::new()
    };
    format!("找不到 Python（找过 {head}{more}）")
}
