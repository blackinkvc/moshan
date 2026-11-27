// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：w5d 泳道自用的「Python 口径」小工具 —— 时间格式、路径展开、尺寸人类化、
//       md5、外部命令、端口探活、which、compact JSON。
// 不做：不碰 HTTP 契约（common.rs 管），不复制 storage 已有的文件工具。
//
// 为什么自己带一份：w4c/py.rs 只有开着 lane_w4c 才编译得到，而泳道之间不能
// 互相依赖（见 RUST-LANES.md）。这些函数都很短，重复一份不影响正确性。

#![allow(dead_code)]

use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------- 时间

/// `datetime.now().isoformat(timespec="seconds")`：本地时区、秒级、无时区后缀。
pub fn now_iso() -> String {
    chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string()
}

/// `datetime.now().strftime(fmt)`
pub fn local_fmt(fmt: &str) -> String {
    chrono::Local::now().format(fmt).to_string()
}

pub fn now_f() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub fn epoch_ms_i64() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------- 路径

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `Path(...).expanduser()`：只处理开头的 `~` / `~/`。
pub fn expanduser(raw: &str) -> PathBuf {
    if raw == "~" {
        return home();
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return home().join(rest);
    }
    PathBuf::from(raw)
}

/// `Path.resolve()`：存在就 canonicalize，不存在就保留原样（与 Python 3.6+ 的
/// strict=False 大体一致；父目录存在时会解析掉 /var → /private/var）。
pub fn py_resolve(p: &Path) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(p) {
        return c;
    }
    if let Some(parent) = p.parent() {
        if let Ok(c) = std::fs::canonicalize(parent) {
            if let Some(name) = p.file_name() {
                return c.join(name);
            }
        }
    }
    p.to_path_buf()
}

/// Python `str(Path)` 的口径。
pub fn path_str(p: &Path) -> String {
    p.to_string_lossy().to_string()
}

/// `Path.name`（末段）。
pub fn name_of(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// `Path.stem`：去掉最后一个后缀。
pub fn stem_of(p: &Path) -> String {
    let n = name_of(p);
    match n.rfind('.') {
        Some(i) if i > 0 => n[..i].to_string(),
        _ => n,
    }
}

/// `Path.suffix`：含点；没有后缀回空。
pub fn suffix_of(p: &Path) -> String {
    let n = name_of(p);
    match n.rfind('.') {
        Some(i) if i > 0 => n[i..].to_string(),
        _ => String::new(),
    }
}

/// `_root(cfg)`（extplugins / pluginpack 同一套）：allowed_roots[0]，
/// 否则 inbox_dir 的父目录。
pub fn data_root(cfg: &Value) -> PathBuf {
    if let Some(arr) = cfg.get("allowed_roots").and_then(|v| v.as_array()) {
        if let Some(first) = arr.first().and_then(|v| v.as_str()) {
            if !first.trim().is_empty() {
                return expanduser(first);
            }
        }
    }
    let inbox = cfg.get("inbox_dir").and_then(|v| v.as_str()).unwrap_or(".");
    expanduser(inbox)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

// ---------------------------------------------------------------- JSON

pub fn read_json(p: &Path) -> Option<Value> {
    std::fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
}

/// 原子写：先写邻居临时文件再 rename（与 Python 的 `tmp.replace(p)` 同义）。
pub fn atomic_write(p: &Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let tmp = p.with_file_name(format!("{name}.{}.tmp", std::process::id()));
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.flush()?;
    }
    std::fs::rename(&tmp, p)
}

/// `Path.with_suffix(".json.tmp")`：插件配置文件用的临时名。
pub fn with_json_tmp(p: &Path) -> PathBuf {
    let s = p.to_string_lossy().to_string();
    let base = s.strip_suffix(".json").unwrap_or(&s);
    PathBuf::from(format!("{base}.json.tmp"))
}

// ---------------------------------------------------------------- 值转换

pub fn s_of(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => (if *b { "True" } else { "False" }).to_string(),
        _ => String::new(),
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

pub fn f64_of(v: Option<&Value>) -> f64 {
    match v {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0.0),
        Some(Value::Bool(b)) => *b as i64 as f64,
        _ => 0.0,
    }
}

/// `d.get(k, default)` 在「键不存在」时取默认，键存在但不合类型时按 Python truthy 判。
pub fn get_default(v: &Value, k: &str, d: &str) -> Value {
    match v.get(k) {
        Some(Value::Null) => Value::String(d.to_string()),
        Some(x) => x.clone(),
        None => Value::String(d.to_string()),
    }
}

/// Python `str(x)[:n]`（按字符）。
pub fn trunc(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Python `round(x, nd)` 的十进制口径。
pub fn py_round(x: f64, nd: usize) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{:.*}", nd, x).parse().unwrap_or(x)
}

/// 整数拼进文本时 Python 的写法（浮点整值也带 `.0`）。
pub fn num_text(v: &Value) -> String {
    match v {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(u) = n.as_u64() {
                u.to_string()
            } else {
                let f = n.as_f64().unwrap_or(0.0);
                if f == f.trunc() && f.abs() < 1e16 {
                    format!("{f:.1}")
                } else {
                    format!("{f}")
                }
            }
        }
        Value::String(s) => s.clone(),
        Value::Bool(b) => (if *b { "True" } else { "False" }).to_string(),
        Value::Null => "None".to_string(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------- 字符串

/// `extplugins._slug`：非字母数字折叠成 `-`，小写，去首尾 `-`，截 28。
pub fn slug(text: &str) -> String {
    let re = regex::Regex::new(r"[^a-z0-9]+").unwrap();
    let s = re.replace_all(&text.to_lowercase(), "-").to_string();
    let s = s.trim_matches('-');
    trunc(s, 28)
}

/// `hashlib.md5(text.encode("utf-8")).hexdigest()`
pub fn md5_hex(data: &[u8]) -> String {
    md5(data)
}

/// `gray.human`：B 不带小数，其余一位小数。
pub fn human(n: i64) -> String {
    let mut v = n as f64;
    for unit in ["B", "KB", "MB", "GB"] {
        if v < 1024.0 || unit == "GB" {
            return if unit == "B" {
                format!("{v:.0} B")
            } else {
                format!("{v:.1} {unit}")
            };
        }
        v /= 1024.0;
    }
    format!("{v:.1} GB")
}

/// `storage.unique_path`：`名` → `名-2` … → `名-999`。
pub fn unique_path(directory: &Path, name: &str) -> PathBuf {
    let dest = directory.join(name);
    if !dest.exists() {
        return dest;
    }
    let p = Path::new(name);
    let stem = stem_of(p);
    let suffix = suffix_of(p);
    let mut i = 2;
    while directory.join(format!("{stem}-{i}{suffix}")).exists() {
        i += 1;
    }
    directory.join(format!("{stem}-{i}{suffix}"))
}

// ---------------------------------------------------------------- 外部命令

/// `subprocess.run(argv, capture_output=True, text=True, timeout=T)`。
/// 超时 / 起不来都返回 None。
pub fn run_cmd(argv: &[String], timeout_secs: u64, cwd: Option<&Path>) -> Option<(i32, String, String)> {
    if argv.is_empty() {
        return None;
    }
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
                std::thread::sleep(Duration::from_millis(30));
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

/// `shutil.which(name)`：PATH 上第一个可执行文件。
pub fn which(name: &str) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let p = dir.join(name);
        if is_exec_file(&p) {
            return Some(p);
        }
    }
    // launchd 起的进程 PATH 很短，extplugins 显式补两段。
    for dir in ["/opt/homebrew/bin", "/usr/local/bin"] {
        let p = Path::new(dir).join(name);
        if is_exec_file(&p) {
            return Some(p);
        }
    }
    None
}

pub fn is_exec_file(p: &Path) -> bool {
    crate::hostwork::platform::is_exec_file(p)
}

/// `socket.create_connection(("127.0.0.1", port), timeout)` 的简化。
pub fn port_open(port: i64, timeout_ms: u64) -> bool {
    if port <= 0 || port > 65535 {
        return false;
    }
    let addr = match format!("127.0.0.1:{port}").parse() {
        Ok(a) => a,
        Err(_) => return false,
    };
    std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(timeout_ms)).is_ok()
}

/// `os.urandom(n).hex()`
pub fn rand_hex(n: usize) -> String {
    use std::io::Read;
    let mut buf = vec![0u8; n];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok();
    if !ok {
        let mut seed = now_f().to_bits() ^ ((std::process::id() as u64) << 32);
        for b in buf.iter_mut() {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            *b = (seed & 0xff) as u8;
        }
    }
    buf.iter().map(|b| format!("{b:02x}")).collect::<String>()
}

/// 在临时目录下建一个 mkdtemp 风格目录（`prefix` + 随机后缀）。
pub fn mkdtemp(prefix: &str) -> PathBuf {
    let base = std::env::temp_dir();
    for _ in 0..50 {
        let p = base.join(format!("{prefix}{}", rand_hex(4)));
        if std::fs::create_dir(&p).is_ok() {
            return p;
        }
    }
    let p = base.join(format!("{prefix}{}", std::process::id()));
    let _ = std::fs::create_dir_all(&p);
    p
}

/// 递归删目录（失败忽略），对应 `shutil.rmtree(ignore_errors=True)`。
pub fn rmtree(p: &Path) {
    let _ = std::fs::remove_dir_all(p);
}

/// 复制目录树：`shutil.copytree(..., dirs_exist_ok=True, ignore=...)` 的等价物。
/// 跳过 `__MACOSX/.git/.svn/node_modules/__pycache__`、`._*`、`.DS_Store`。
pub fn copy_tree(src: &Path, dst: &Path) {
    let _ = std::fs::create_dir_all(dst);
    let Ok(rd) = std::fs::read_dir(src) else { return };
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().to_string();
        if skip_dir_or_name(&name) {
            continue;
        }
        let from = ent.path();
        let to = dst.join(&name);
        match ent.file_type() {
            Ok(ft) if ft.is_dir() => copy_tree(&from, &to),
            Ok(ft) if ft.is_file() => {
                let _ = std::fs::copy(&from, &to);
            }
            Ok(ft) if ft.is_symlink() => {
                // shutil.copytree 默认跟随链接，这里同样把目标内容拷过去。
                if from.is_dir() {
                    copy_tree(&from, &to);
                } else {
                    let _ = std::fs::copy(&from, &to);
                }
            }
            _ => {}
        }
    }
}

pub fn skip_dir_or_name(name: &str) -> bool {
    matches!(name, "__MACOSX" | ".git" | ".svn" | "node_modules" | "__pycache__")
}

// ---------------------------------------------------------------- Map 便捷

/// 往 Map 里塞一个键；已存在则原位替换（Python dict 的赋值语义）。
pub fn put(m: &mut Map<String, Value>, k: &str, v: Value) {
    m.insert(k.to_string(), v);
}

/// Python 的 `v not in (None, "", [], {}, 0)` 中的「空」判定（0 不算空）。
pub fn is_blank(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        _ => false,
    }
}

/// `str(x)`：给错误文案拼接用。
pub fn py_str(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::Bool(b) => (if *b { "True" } else { "False" }).to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------- MD5

fn md5(data: &[u8]) -> String {
    let mut a0: u32 = 0x67452301;
    let mut b0: u32 = 0xefcdab89;
    let mut c0: u32 = 0x98badcfe;
    let mut d0: u32 = 0x10325476;

    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20,
        5, 9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23,
        6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613,
        0xfd469501, 0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193,
        0xa679438e, 0x49b40821, 0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d,
        0x02441453, 0xd8a1e681, 0xe7d3fbc8, 0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed,
        0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, 0xfffa3942, 0x8771f681, 0x6d9d6122,
        0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, 0x289b7ec6, 0xeaa127fa,
        0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, 0xf4292244,
        0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb,
        0xeb86d391,
    ];

    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_le_bytes());

    for chunk in msg.chunks(64) {
        let mut m = [0u32; 16];
        for i in 0..16 {
            m[i] = u32::from_le_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i {
                0..=15 => ((b & c) | (!b & d), i),
                16..=31 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let tmp = d;
            d = c;
            c = b;
            b = b.wrapping_add(
                a.wrapping_add(f)
                    .wrapping_add(K[i])
                    .wrapping_add(m[g])
                    .rotate_left(S[i]),
            );
            a = tmp;
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }

    let mut out = String::with_capacity(32);
    for v in [a0, b0, c0, d0] {
        for byte in v.to_le_bytes() {
            out.push_str(&format!("{byte:02x}"));
        }
    }
    out
}

// ---------------------------------------------------------------- 请求校验层

/// FastAPI 对缺失的 `File(...)` 字段回 422，body 是 pydantic 的 detail 数组。
/// 业务代码根本走不到这里，但少传一个 part 就会先撞上它，所以逐字复刻一份
/// （`url` 里带 pydantic 版本号，按本机 2.14 写死）。
pub fn validation_missing(field: &str) -> axum::response::Response {
    use axum::response::IntoResponse;
    let body = serde_json::json!({
        "detail": [{
            "type": "missing",
            "loc": ["body", field],
            "msg": "Field required",
            "input": null,
            "url": "https://errors.pydantic.dev/2.14/v/missing"
        }]
    });
    (axum::http::StatusCode::UNPROCESSABLE_ENTITY, axum::Json(body)).into_response()
}

// ---------------------------------------------------------------- JSONL

/// `json.dumps(v, ensure_ascii=False)` 的单行形态：分隔符是 `", "` / `": "`，
/// 中文原样。jsonio::dumps 是给 indent 用的，缩进 0 仍会换行，这里另写一份。
pub fn py_json_compact(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Array(a) => {
            let items: Vec<String> = a.iter().map(py_json_compact).collect();
            format!("[{}]", items.join(", "))
        }
        serde_json::Value::Object(o) => {
            let items: Vec<String> = o
                .iter()
                .map(|(k, x)| {
                    format!(
                        "{}: {}",
                        serde_json::to_string(k).unwrap_or_default(),
                        py_json_compact(x)
                    )
                })
                .collect();
            format!("{{{}}}", items.join(", "))
        }
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// 追加一行 JSON（`open(a)` 的语义：目录不在就建）。
pub fn append_jsonl(p: &std::path::Path, v: &serde_json::Value) {
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
        let _ = writeln!(f, "{}", py_json_compact(v));
    }
}
