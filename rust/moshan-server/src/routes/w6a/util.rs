// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：w6a 泳道自己的小工具 —— Python 取值/序列化口径、虫巢目录、fleet.db 访问、
//       端口探活、文件下载响应。
// 不做：不碰主账本、不读公共模块里的业务判定。
//
// 为什么 fleet.db 走 `sqlite3` 命令行而不是引一个 Rust sqlite 绑定：
// Cargo.toml 不归本泳道改，而**表结构必须与 Python 共用同一个文件**
// （`16-虫巢思维/fleet.db`，虫巢思维独立服务也在读它）。
// 换成 JSON 之类的旁路就等于把「多机账」搬进了另一个账本，是硬规则里明令禁止的。
// 单次请求是十几毫秒级的进程启动，这些接口本来就是低频读写（30 秒一报）。

#![allow(dead_code)]

use crate::jsonio;
use crate::paths::Ctx;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

// ------------------------------------------------------------------ Python 取值

/// Python 真值：None / False / 0 / "" / [] / {} 为假。
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// `v or default`（Python 的 or，不做类型转换）。
pub fn or<'a>(v: Option<&'a Value>, default: &'a Value) -> &'a Value {
    match v {
        Some(x) if truthy(x) => x,
        _ => default,
    }
}

/// `str(v)`。只用于 `str(body.get(k) or "")` 这类地方。
pub fn py_str(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        Value::Array(_) | Value::Object(_) => py_repr(v),
    }
}

/// `str(body.get(k) or "")` 的直译：假值一律空串。
pub fn str_or_empty(v: Option<&Value>) -> String {
    match v {
        Some(x) if truthy(x) => py_str(x),
        _ => String::new(),
    }
}

/// Python `repr`（str() 对容器用的就是它）。fleet 里只出现在错误消息里，够用即可。
fn py_repr(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'")),
        Value::Array(a) => {
            let items: Vec<String> = a.iter().map(py_repr).collect();
            format!("[{}]", items.join(", "))
        }
        Value::Object(o) => {
            let items: Vec<String> = o
                .iter()
                .map(|(k, x)| format!("'{}': {}", k, py_repr(x)))
                .collect();
            format!("{{{}}}", items.join(", "))
        }
    }
}

/// Python 切片 `s[:n]`（按**字符**算，不是字节）。
pub fn cut(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

pub fn py_strip(s: &str) -> String {
    // Python str.strip() 去的是 Unicode 空白，含 \u{3000}（全角空格）与 \u{feff}。
    s.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}' || c == '\u{3000}')
        .to_string()
}

/// Python `int(v)`：失败时返回 Err。
pub fn int_of(v: &Value) -> Result<i64, ()> {
    match v {
        Value::Bool(b) => Ok(*b as i64),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(i)
            } else if let Some(f) = n.as_f64() {
                Ok(f.trunc() as i64)
            } else {
                Err(())
            }
        }
        Value::String(s) => {
            let t = s.trim();
            t.parse::<i64>().map_err(|_| ())
        }
        _ => Err(()),
    }
}

/// Python `float(v)`：失败时返回 Err。
pub fn float_of(v: &Value) -> Result<f64, ()> {
    match v {
        Value::Bool(b) => Ok(*b as i64 as f64),
        Value::Number(n) => n.as_f64().ok_or(()),
        Value::String(s) => {
            let t = s.trim();
            t.parse::<f64>().map_err(|_| ())
        }
        _ => Err(()),
    }
}

/// `round(x, 1)`：按正确十进制舍入后再落回 f64 —— 与 Python 的 round 同口径
/// （Python 不是简单乘 10 四舍五入，`round(2.675, 2)` 是 2.67）。
pub fn round1(x: f64) -> f64 {
    format!("{:.1}", x).parse::<f64>().unwrap_or(x)
}

/// 数字落到 JSON：整数值的 float 仍要带 `.0`（Python 的 `1299.0`）。
pub fn num_f(x: f64) -> Value {
    serde_json::Number::from_f64(x).map(Value::Number).unwrap_or(Value::Null)
}

// ------------------------------------------------------------------ Python json.dumps

/// `json.dumps(v, ensure_ascii=False)`：**紧凑但带空格**的默认分隔符
/// （`, ` 与 `: `）。落库的 `procs` / `args_json` 用的就是这个口径，
/// 缩进/分隔符差一格，同一行数据在两边就读出不同的字节。
pub fn dumps_py(v: &Value) -> String {
    let mut out = String::new();
    write_py(v, &mut out);
    out
}

fn write_py(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => escape(s, out),
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_py(x, out);
            }
            out.push(']');
        }
        Value::Object(m) => {
            out.push('{');
            for (i, (k, x)) in m.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                escape(k, out);
                out.push_str(": ");
                write_py(x, out);
            }
            out.push('}');
        }
    }
}

fn escape(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

// ------------------------------------------------------------------ 时间

/// `datetime.now().isoformat(timespec="seconds")`：本地时区、无偏移、秒级。
pub fn now_iso() -> String {
    chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string()
}

pub fn iso_of(dt: chrono::DateTime<chrono::Local>) -> String {
    dt.format("%Y-%m-%dT%H:%M:%S").to_string()
}

/// `datetime.fromisoformat(ts).timestamp()`。
/// Python 接受 `T` 或空格分隔、可带小数与偏移；这里覆盖这几种常见写法。
pub fn parse_ts(ts: &str) -> Option<f64> {
    use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
    let s = ts.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp() as f64 + dt.timestamp_subsec_micros() as f64 / 1e6);
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S"] {
        if let Ok(nd) = NaiveDateTime::parse_from_str(s, fmt) {
            let local = Local.from_local_datetime(&nd).single()?;
            return Some(local.timestamp() as f64
                + local.timestamp_subsec_micros() as f64 / 1e6);
        }
    }
    None
}

pub fn now_epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn local_ts_iso(offset_secs: i64) -> String {
    iso_of(chrono::Local::now() + chrono::Duration::seconds(offset_secs))
}

pub fn iso_days_ago(days: i64) -> String {
    local_ts_iso(-days * 86400)
}

pub fn iso_secs_ago(secs: i64) -> String {
    local_ts_iso(-secs)
}

// ------------------------------------------------------------------ 路径

/// `fleet._root(cfg)`：data_root → inbox_dir 的父目录 → ~/Documents/AI项目。
/// 注意与 `dash._root`（allowed_roots[0]）**不是**同一个口径，别混。
pub fn data_root(ctx: &Ctx) -> PathBuf {
    if let Some(dr) = ctx.cfg.get("data_root").and_then(|v| v.as_str()) {
        if !dr.trim().is_empty() {
            return crate::storage::expanduser(dr);
        }
    }
    if let Some(inbox) = ctx.cfg.get("inbox_dir").and_then(|v| v.as_str()) {
        if !inbox.trim().is_empty() {
            let p = crate::storage::expanduser(inbox);
            return p.parent().map(|x| x.to_path_buf()).unwrap_or(p);
        }
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join("Documents").join("AI项目")
}

pub fn data_dir(ctx: &Ctx) -> PathBuf {
    let d = crate::paths::rt_join(ctx, "16-虫巢思维");
    let _ = std::fs::create_dir_all(&d);
    d
}

pub fn db_path(ctx: &Ctx) -> PathBuf {
    data_dir(ctx).join("fleet.db")
}

pub fn pkg_dir(ctx: &Ctx) -> PathBuf {
    let d = data_dir(ctx).join("pkgs");
    let _ = std::fs::create_dir_all(&d);
    d
}

pub fn release_dir(ctx: &Ctx) -> PathBuf {
    let d = data_dir(ctx).join("agent-rel");
    let _ = std::fs::create_dir_all(&d);
    d
}

// ------------------------------------------------------------------ fleet.db

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS devices (
    id          TEXT PRIMARY KEY,
    name        TEXT,
    os          TEXT,
    host        TEXT,
    ip          TEXT,
    agent_ver   TEXT,
    access      TEXT DEFAULT 'control',
    fp          TEXT DEFAULT '',
    note        TEXT DEFAULT '',
    first_seen  TEXT,
    last_seen   TEXT
);
CREATE TABLE IF NOT EXISTS snapshots (
    device_id   TEXT NOT NULL,
    ts          TEXT NOT NULL,
    cpu         REAL,
    mem_used    INTEGER,
    mem_total   INTEGER,
    disk_used   INTEGER,
    disk_total  INTEGER,
    uptime      INTEGER,
    procs       TEXT,
    PRIMARY KEY (device_id, ts)
);
CREATE INDEX IF NOT EXISTS ix_snap_ts ON snapshots(ts);
CREATE TABLE IF NOT EXISTS commands (
    id          TEXT PRIMARY KEY,
    device_id   TEXT NOT NULL,
    action      TEXT NOT NULL,
    state       TEXT DEFAULT 'pending',
    result      TEXT DEFAULT '',
    created_at  TEXT,
    taken_at    TEXT,
    done_at     TEXT,
    issued_from TEXT DEFAULT ''
);
CREATE INDEX IF NOT EXISTS ix_cmd_dev ON commands(device_id, state);
CREATE TABLE IF NOT EXISTS software (
    device_id    TEXT NOT NULL,
    app_id       TEXT NOT NULL,
    name         TEXT,
    version      TEXT,
    publisher    TEXT,
    install_date TEXT,
    size_kb      INTEGER DEFAULT 0,
    source       TEXT DEFAULT 'registry',
    bits         TEXT DEFAULT '',
    updated_at   TEXT,
    PRIMARY KEY (device_id, app_id)
);
CREATE TABLE IF NOT EXISTS upgrades (
    device_id   TEXT NOT NULL,
    app_id      TEXT NOT NULL,
    name        TEXT,
    current     TEXT,
    available   TEXT,
    updated_at  TEXT,
    PRIMARY KEY (device_id, app_id)
);
CREATE TABLE IF NOT EXISTS tasks (
    id          TEXT PRIMARY KEY,
    device_id   TEXT NOT NULL,
    kind        TEXT NOT NULL,
    target      TEXT DEFAULT '',
    args_json   TEXT DEFAULT '{}',
    state       TEXT DEFAULT 'pending',
    progress    INTEGER DEFAULT 0,
    log         TEXT DEFAULT '',
    created_at  TEXT,
    taken_at    TEXT,
    done_at     TEXT
);
CREATE INDEX IF NOT EXISTS ix_task_dev ON tasks(device_id, state);
CREATE TABLE IF NOT EXISTS agent_release (
    version      TEXT PRIMARY KEY,
    file_path    TEXT,
    sha256       TEXT,
    size         INTEGER DEFAULT 0,
    notes        TEXT DEFAULT '',
    published_at TEXT,
    is_current   INTEGER DEFAULT 0
);
"#;

/// `fleet._LOCK`：Python 是进程内互斥 + sqlite 自身锁。这里照抄进程内互斥。
pub fn lock() -> std::sync::MutexGuard<'static, ()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn inited() -> &'static Mutex<Vec<PathBuf>> {
    static S: OnceLock<Mutex<Vec<PathBuf>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(Vec::new()))
}

fn raw_sqlite(db: &Path, sql: &str) -> Result<String, String> {
    let out = std::process::Command::new("/usr/bin/sqlite3")
        .arg("-json")
        .arg("-batch")
        .arg("-cmd")
        .arg(".timeout 5000")
        .arg(db)
        .arg(sql)
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// 建表 + 老库补列（对应 `fleet.init()`），每进程每个库做一次。
pub fn ensure_db(db: &Path) -> Result<(), String> {
    {
        let g = inited().lock().unwrap_or_else(|e| e.into_inner());
        if g.iter().any(|p| p == db) {
            return Ok(());
        }
    }
    if let Some(parent) = db.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    raw_sqlite(db, "PRAGMA journal_mode=WAL;")?;
    raw_sqlite(db, "PRAGMA synchronous=NORMAL;")?;
    raw_sqlite(db, SCHEMA)?;
    // CREATE TABLE IF NOT EXISTS 不会给已存在的表加字段，老库要补 fp。
    let cols = raw_sqlite(db, "SELECT name FROM pragma_table_info('devices');")?;
    if !cols.contains("\"fp\"") && !cols.contains("\"name\":\"fp\"") {
        let _ = raw_sqlite(db, "ALTER TABLE devices ADD COLUMN fp TEXT DEFAULT '';");
    }
    inited()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(db.to_path_buf());
    Ok(())
}

/// SQL 字符串字面量。fleet 里所有外部输入都经这里，杜绝引号注入。
pub fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

pub fn lit_num(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 9e15 {
        format!("{}", n as i64)
    } else {
        format!("{}", n)
    }
}

/// 一条 SELECT，返回按列序排列的对象数组。
pub fn query(db: &Path, sql: &str) -> Result<Vec<Value>, String> {
    ensure_db(db)?;
    let out = raw_sqlite(db, sql)?;
    let t = out.trim();
    if t.is_empty() {
        return Ok(Vec::new());
    }
    match serde_json::from_str::<Value>(t) {
        Ok(Value::Array(a)) => Ok(a),
        Ok(v) => Ok(vec![v]),
        Err(e) => Err(format!("sqlite json: {e}")),
    }
}

/// 一条写语句，返回 rowcount（`cursor.rowcount`）。
pub fn exec(db: &Path, sql: &str) -> Result<i64, String> {
    ensure_db(db)?;
    let out = raw_sqlite(db, &format!("{sql}; SELECT changes() AS n;"))?;
    let t = out.trim();
    if t.is_empty() {
        return Ok(0);
    }
    let v: Value = serde_json::from_str(t).map_err(|e| e.to_string())?;
    let last = v
        .as_array()
        .and_then(|a| a.last())
        .and_then(|x| x.get("n"))
        .and_then(|x| x.as_i64())
        .unwrap_or(0);
    Ok(last)
}

/// 多条写语句，包在一个事务里（Python 那边是一次 commit）。
pub fn exec_tx(db: &Path, sql: &str) -> Result<(), String> {
    ensure_db(db)?;
    raw_sqlite(db, &format!("BEGIN; {sql}; COMMIT;"))?;
    Ok(())
}

/// `hashlib.sha256(data).hexdigest()`
pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().iter().map(|b| format!("{:02x}", b)).collect()
}

// ------------------------------------------------------------------ 探活

/// `socket.create_connection((host, port), timeout=0.35)`。
pub fn port_open(host: &str, port: u16, timeout_ms: u64) -> bool {
    use std::net::{SocketAddr, ToSocketAddrs};
    let host = if host.is_empty() { "127.0.0.1" } else { host };
    let addrs: Vec<SocketAddr> = match (host, port).to_socket_addrs() {
        Ok(a) => a.collect(),
        Err(_) => return false,
    };
    for a in addrs {
        if std::net::TcpStream::connect_timeout(&a, std::time::Duration::from_millis(timeout_ms))
            .is_ok()
        {
            return true;
        }
    }
    false
}

// ------------------------------------------------------------------ 下载

/// `FileResponse(path, media_type=..., filename=...)` 的最小复刻：
/// 类型、长度、`Content-Disposition` 三样，与 starlette 的写法逐字对齐。
pub fn file_download(path: &Path, media_type: &str, filename: &str) -> Response {
    match std::fs::read(path) {
        Ok(bytes) => {
            let mut res = bytes.into_response();
            let h = res.headers_mut();
            if let Ok(v) = HeaderValue::from_str(media_type) {
                h.insert(header::CONTENT_TYPE, v);
            }
            let disp = format!("attachment; filename=\"{}\"", filename.replace('"', "\\\""));
            if let Ok(v) = HeaderValue::from_str(&disp) {
                h.insert(header::CONTENT_DISPOSITION, v);
            }
            res
        }
        Err(_) => crate::routes::common::http_err(404, "Not Found"),
    }
}

// ------------------------------------------------------------------ 请求体

/// FastAPI `body: dict` 的参数校验形状。先鉴权再校验 —— 顺序与 FastAPI 一致
/// （依赖先跑，body 解析在后，所以没令牌时永远先回 401）。
pub enum Body {
    Obj(Value),
    Bad(Response),
}

fn pydantic(detail: Value) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        axum::Json(json!({ "detail": detail })),
    )
        .into_response()
}

/// CPython `json` 解析失败时的 (pos, msg)。
///
/// pydantic 的 `json_invalid` 里两段都来自这里：`loc=["body", pos]`、
/// `ctx.error = msg`。serde 的报错文案与 CPython 完全不同，照抄不了，
/// 所以这里按 CPython 的规则重扫一遍 —— 只为了错误体那两个字段。
pub fn py_json_error(raw: &[u8]) -> (usize, String) {
    let s: Vec<char> = String::from_utf8_lossy(raw).chars().collect();
    let mut i = 0usize;
    fn ws(s: &[char], i: &mut usize) {
        while *i < s.len() && matches!(s[*i], ' ' | '\t' | '\n' | '\r') {
            *i += 1;
        }
    }
    fn string(s: &[char], i: &mut usize) -> Result<(), (usize, String)> {
        let start = *i;
        *i += 1; // 开引号
        while *i < s.len() {
            match s[*i] {
                '"' => {
                    *i += 1;
                    return Ok(());
                }
                '\\' => {
                    *i += 2;
                }
                _ => *i += 1,
            }
        }
        Err((start, "Unterminated string starting at".to_string()))
    }
    fn value(s: &[char], i: &mut usize) -> Result<(), (usize, String)> {
        ws(s, i);
        if *i >= s.len() {
            return Err((*i, "Expecting value".to_string()));
        }
        match s[*i] {
            '"' => string(s, i),
            '{' => {
                *i += 1;
                ws(s, i);
                if *i < s.len() && s[*i] == '}' {
                    *i += 1;
                    return Ok(());
                }
                loop {
                    ws(s, i);
                    if *i >= s.len() {
                        return Err((*i, "Expecting property name enclosed in double quotes".to_string()));
                    }
                    if s[*i] != '"' {
                        return Err((*i, "Expecting property name enclosed in double quotes".to_string()));
                    }
                    string(s, i)?;
                    ws(s, i);
                    if *i >= s.len() || s[*i] != ':' {
                        return Err((*i, "Expecting ':' delimiter".to_string()));
                    }
                    *i += 1;
                    value(s, i)?;
                    ws(s, i);
                    if *i >= s.len() {
                        return Err((*i, "Expecting ',' delimiter".to_string()));
                    }
                    if s[*i] == ',' {
                        let comma = *i;
                        *i += 1;
                        ws(s, i);
                        if *i < s.len() && s[*i] == '}' {
                            return Err((comma, "Illegal trailing comma before end of object".to_string()));
                        }
                        continue;
                    }
                    if s[*i] == '}' {
                        *i += 1;
                        return Ok(());
                    }
                    return Err((*i, "Expecting ',' delimiter".to_string()));
                }
            }
            '[' => {
                *i += 1;
                ws(s, i);
                if *i < s.len() && s[*i] == ']' {
                    *i += 1;
                    return Ok(());
                }
                loop {
                    value(s, i)?;
                    ws(s, i);
                    if *i >= s.len() {
                        return Err((*i, "Expecting ',' delimiter".to_string()));
                    }
                    if s[*i] == ',' {
                        let comma = *i;
                        *i += 1;
                        ws(s, i);
                        if *i < s.len() && s[*i] == ']' {
                            return Err((comma, "Illegal trailing comma before end of array".to_string()));
                        }
                        continue;
                    }
                    if s[*i] == ']' {
                        *i += 1;
                        return Ok(());
                    }
                    return Err((*i, "Expecting ',' delimiter".to_string()));
                }
            }
            't' => lit(s, i, "true"),
            'f' => lit(s, i, "false"),
            'n' => lit(s, i, "null"),
            c if c == '-' || c.is_ascii_digit() => {
                *i += 1;
                while *i < s.len()
                    && (s[*i].is_ascii_digit() || matches!(s[*i], '.' | 'e' | 'E' | '+' | '-'))
                {
                    *i += 1;
                }
                Ok(())
            }
            _ => Err((*i, "Expecting value".to_string())),
        }
    }
    fn lit(s: &[char], i: &mut usize, word: &str) -> Result<(), (usize, String)> {
        for c in word.chars() {
            if *i >= s.len() || s[*i] != c {
                return Err((*i, "Expecting value".to_string()));
            }
            *i += 1;
        }
        Ok(())
    }
    ws(&s, &mut i);
    if let Err(e) = value(&s, &mut i) {
        return e;
    }
    ws(&s, &mut i);
    if i < s.len() {
        return (i, "Extra data".to_string());
    }
    (0, "Expecting value".to_string())
}

pub fn parse_body(raw: &[u8], content_type: &str, headers_has_ct: bool) -> Body {
    let ct = content_type.to_ascii_lowercase();
    let is_json = ct.starts_with("application/json") || ct.contains("+json");
    if raw.is_empty() {
        return Body::Bad(pydantic(json!([{
            "type": "missing",
            "loc": ["body"],
            "msg": "Field required",
            "input": null,
            "url": "https://errors.pydantic.dev/2.14/v/missing",
        }])));
    }
    if !is_json {
        // 非 JSON 内容类型：FastAPI 把原始字节当字符串交给 pydantic。
        let text = String::from_utf8_lossy(raw).to_string();
        return Body::Bad(pydantic(json!([{
            "type": "dict_type",
            "loc": ["body"],
            "msg": "Input should be a valid dictionary",
            "input": text,
            "url": "https://errors.pydantic.dev/2.14/v/dict_type",
        }])));
    }
    let _ = headers_has_ct;
    match serde_json::from_slice::<Value>(raw) {
        Ok(Value::Object(m)) => Body::Obj(Value::Object(m)),
        Ok(v) => Body::Bad(pydantic(json!([{
            "type": "dict_type",
            "loc": ["body"],
            "msg": "Input should be a valid dictionary",
            "input": v,
            "url": "https://errors.pydantic.dev/2.14/v/dict_type",
        }]))),
        Err(_) => {
            let (pos, msg) = py_json_error(raw);
            Body::Bad(pydantic(json!([{
                "type": "json_invalid",
                "loc": ["body", pos],
                "msg": "JSON decode error",
                "input": {},
                "ctx": { "error": msg },
            }])))
        }
    }
}

// ------------------------------------------------------------------ 杂项

pub fn to_map(v: &Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}

pub fn write_json_file(path: &Path, v: &Value) {
    let _ = jsonio::atomic_write(path, &jsonio::dumps(v, 1));
}

/// `datetime.now().strftime("%Y-%m-%d %H:%M:%S")`
pub fn now_space() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}
