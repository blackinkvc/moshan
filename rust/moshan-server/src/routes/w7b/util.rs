// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：w7b 泳道自用的小工具 —— 两种鉴权口径、进程内更新状态、Python 取值语义、
//       JSON 落盘格式、时间、文件响应（照抄 Starlette FileResponse 的头）。
// 不做：不碰共享地基（auth/paths/storage/jsonio/common），也不做业务判断。
//
// 为什么鉴权要写两份：`about.py` 的那组路由自己比令牌，失败回
// `{"ok": false, "error": "需要访问令牌"}`（401）；而 `server.py` 的
// `/api/update/push|status|rollback` 用 `auth_agent`，它在回环来源上直接放行。
// 本进程只绑 127.0.0.1（main.rs），所以后者在这里就是「不校验」——与
// `w6b/agent.rs`、`w6a/perms.rs` 同一口径。两种形状混用一次前端就分不清。

#![allow(dead_code)]

use crate::paths::Ctx;
use axum::body::Bytes;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------- 鉴权

/// `about.py:_token_ok`：只认 `X-Token` 与 `?k=`，**不认 Cookie**，且两端都 strip。
pub fn about_token_ok(ctx: &Ctx, headers: &HeaderMap, uri: &Uri) -> bool {
    let got = headers
        .get("x-token")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| {
            crate::routes::common::query_map(uri)
                .into_iter()
                .find(|(k, _)| k == "k")
                .map(|(_, v)| v)
        })
        .unwrap_or_default();
    let got = got.trim();
    // `hmac.compare_digest` 在两边都是 str 时按字节比；这里同样要求令牌非空。
    !ctx.token.is_empty() && got.as_bytes() == ctx.token.as_bytes()
}

/// `about.py:_deny`：401 + `{"ok": false, "error": "需要访问令牌"}`。
pub fn about_deny() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"ok": false, "error": "需要访问令牌"})),
    )
        .into_response()
}

/// `about.py` 那组路由的鉴权闸门。
pub fn about_gate(ctx: &Ctx, headers: &HeaderMap, uri: &Uri) -> Option<Response> {
    if about_token_ok(ctx, headers, uri) {
        None
    } else {
        Some(about_deny())
    }
}

/// `server.py:auth_agent`：回环来源直接放行。本进程只监听 127.0.0.1，
/// 所有连接都是回环，因此不需要（也无法）区分来源。保留这个空函数是为了
/// 让「这里本该有一道鉴权」在代码里看得见。
pub fn agent_gate() {}

// ---------------------------------------------------------------- FastAPI 形状

pub fn json_status(code: u16, v: Value) -> Response {
    (
        StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_REQUEST),
        Json(v),
    )
        .into_response()
}

/// 未捕获异常时 Starlette 的默认响应。
pub fn internal_error() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "Internal Server Error",
    )
        .into_response()
}

/// FastAPI 缺必填 body 时的 422。
pub fn missing_body() -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({"detail": [{
            "type": "missing",
            "loc": ["body"],
            "msg": "Field required",
            "input": null,
            "url": "https://errors.pydantic.dev/2.14/v/missing",
        }]})),
    )
        .into_response()
}

/// FastAPI 缺必填查询参数时的 422。
pub fn missing_query(field: &str) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({"detail": [{
            "type": "missing",
            "loc": ["query", field],
            "msg": "Field required",
            "input": null,
            "url": "https://errors.pydantic.dev/2.14/v/missing",
        }]})),
    )
        .into_response()
}

/// `body: dict` 必填的接口：缺体 / 非对象 / 坏 JSON 各自 422。
pub fn parse_body(raw: &Bytes) -> Result<Value, Response> {
    if raw.is_empty() {
        return Err(missing_body());
    }
    match serde_json::from_slice::<Value>(raw) {
        Ok(Value::Object(map)) => Ok(Value::Object(map)),
        Ok(Value::Null) => Err(missing_body()),
        Ok(v) => Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"detail": [{
                "type": "dict_type",
                "loc": ["body"],
                "msg": "Input should be a valid dictionary",
                "input": v,
                "url": "https://errors.pydantic.dev/2.14/v/dict_type",
            }]})),
        )
            .into_response()),
        Err(e) => Err(json_invalid(&e)),
    }
}

/// 坏 JSON 的 422（pydantic 的 `json_invalid`）。
pub fn json_invalid(e: &serde_json::Error) -> Response {
    let text = e.to_string();
    let msg = if text.contains("key must be a string")
        || text.contains("EOF while parsing an object")
    {
        "Expecting property name enclosed in double quotes"
    } else if text.contains("EOF while parsing a string") {
        "Unterminated string starting at"
    } else if text.contains("trailing characters") {
        "Extra data"
    } else if text.contains("control character") {
        "Invalid control character at"
    } else {
        "Expecting value"
    };
    let pos = e.column().saturating_sub(1);
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({"detail": [{
            "type": "json_invalid",
            "loc": ["body", pos],
            "msg": "JSON decode error",
            "input": {},
            "ctx": {"error": msg},
        }]})),
    )
        .into_response()
}

/// FastAPI `int` 查询参数非法时的 422。
pub fn bad_int_query(field: &str, input: &str) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({"detail": [{
            "type": "int_parsing",
            "loc": ["query", field],
            "msg": "Input should be a valid integer, unable to parse string as an integer",
            "input": input,
            "url": "https://errors.pydantic.dev/2.14/v/int_parsing",
        }]})),
    )
        .into_response()
}

// ---------------------------------------------------------------- Python 取值

/// Python 真值：None/False/0/""/[]/{} 为假。
pub fn py_truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// `str(v)`（JSON 侧能出现的形态）。浮点按 repr 口径，整数不带 `.0`。
pub fn py_str(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::Bool(b) => (if *b { "True" } else { "False" }).to_string(),
        Value::String(s) => s.clone(),
        Value::Number(n) => number_str(n),
        Value::Array(_) | Value::Object(_) => serde_json::to_string(v).unwrap_or_default(),
    }
}

pub fn number_str(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    let f = n.as_f64().unwrap_or(0.0);
    if f == f.trunc() && f.abs() < 1e16 {
        format!("{:.1}", f)
    } else {
        format!("{f}")
    }
}

/// `v or default`：空串 / None / 0 / false / 空容器都退回默认值。
pub fn py_or_str(v: Option<&Value>, default: &str) -> String {
    if py_truthy(v) {
        py_str(v.unwrap())
    } else {
        default.to_string()
    }
}

/// `int(v or default)`：Python 的 int()（浮点向零截断、字符串去空白再解析）。
pub fn py_int_or(v: Option<&Value>, default: i64) -> Result<i64, String> {
    let Some(v) = v else { return Ok(default) };
    match v {
        Value::Null => Ok(default),
        Value::Bool(b) => Ok(*b as i64),
        Value::Number(n) => Ok(n.as_i64().unwrap_or_else(|| n.as_f64().unwrap_or(0.0) as i64)),
        Value::String(s) => {
            if s.trim().is_empty() {
                return Ok(default);
            }
            s.trim()
                .parse::<i64>()
                .map_err(|_| format!("invalid literal for int() with base 10: {s:?}"))
        }
        Value::Array(a) => Err(format!("int() argument must be a string, a bytes-like object or a number, not 'list' (len {})", a.len())),
        Value::Object(_) => Err("int() argument must be a string, a bytes-like object or a number, not 'dict'".into()),
    }
}

/// `int(x)` 用于展示/求和（失败按 0）。
pub fn int_of(v: Option<&Value>) -> i64 {
    py_int_or(v, 0).unwrap_or(0)
}

/// `str(x)[:n]`：按字符截断。
pub fn trunc(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

// ---------------------------------------------------------------- 时间

pub fn now_epoch() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// `datetime.now().isoformat(timespec="seconds")`。
pub fn now_iso() -> String {
    Local::now().format("%Y-%m-%dT%H:%M:%S").to_string()
}

/// `datetime.now().isoformat(timespec="seconds").astimezone()` 的带偏移形态。
pub fn iso_with_offset(dt: &DateTime<Local>) -> String {
    dt.format("%Y-%m-%dT%H:%M:%S%:z").to_string()
}

/// `datetime.fromisoformat(s.replace("Z", "+00:00")).astimezone()`；解析失败返回 None。
pub fn parse_when(raw: &str) -> Option<DateTime<Local>> {
    let s = raw.replace('Z', "+00:00");
    if let Ok(d) = DateTime::parse_from_rfc3339(&s) {
        return Some(d.with_timezone(&Local));
    }
    for fmt in [
        "%Y-%m-%dT%H:%M:%S%.f%z",
        "%Y-%m-%d %H:%M:%S%.f%z",
        "%Y-%m-%dT%H:%M%z",
        "%Y-%m-%d %H:%M%z",
    ] {
        if let Ok(d) = DateTime::parse_from_str(&s, fmt) {
            return Some(d.with_timezone(&Local));
        }
    }
    for fmt in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%d",
    ] {
        if let Ok(n) = NaiveDateTime::parse_from_str(&s, fmt) {
            if let Some(d) = Local.from_local_datetime(&n).single() {
                return Some(d);
            }
        }
    }
    None
}

/// `datetime.fromisoformat(at).timestamp()`；失败 0。
pub fn iso_epoch(raw: &str) -> Option<f64> {
    parse_when(raw).map(|d| d.timestamp_millis() as f64 / 1000.0)
}

// ---------------------------------------------------------------- JSON 落盘

/// Python `json.dumps(..., ensure_ascii=False)`：默认分隔符是 `", "` / `": "`。
pub fn dump_default(v: &Value) -> String {
    let mut out = String::new();
    write_value(v, None, 0, ", ", ": ", &mut out);
    out
}

/// `json.dumps(..., ensure_ascii=False, separators=(",", ":"))`。
pub fn dump_compact(v: &Value) -> String {
    let mut out = String::new();
    write_value(v, None, 0, ",", ":", &mut out);
    out
}

fn pad(out: &mut String, indent: usize, depth: usize) {
    for _ in 0..indent * depth {
        out.push(' ');
    }
}

fn write_value(
    v: &Value,
    indent: Option<usize>,
    depth: usize,
    item_sep: &str,
    kv_sep: &str,
    out: &mut String,
) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&number_str(n)),
        Value::String(s) => escape_into(s, out),
        Value::Array(a) => {
            if a.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(item_sep);
                }
                if let Some(ind) = indent {
                    out.push('\n');
                    pad(out, ind, depth + 1);
                }
                write_value(x, indent, depth + 1, item_sep, kv_sep, out);
            }
            if let Some(ind) = indent {
                out.push('\n');
                pad(out, ind, depth);
            }
            out.push(']');
        }
        Value::Object(m) => {
            if m.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (k, x)) in m.iter().enumerate() {
                if i > 0 {
                    out.push_str(item_sep);
                }
                if let Some(ind) = indent {
                    out.push('\n');
                    pad(out, ind, depth + 1);
                }
                escape_into(k, out);
                out.push_str(kv_sep);
                write_value(x, indent, depth + 1, item_sep, kv_sep, out);
            }
            if let Some(ind) = indent {
                out.push('\n');
                pad(out, ind, depth);
            }
            out.push('}');
        }
    }
}

fn escape_into(s: &str, out: &mut String) {
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

/// `Path.with_suffix(p.suffix + extra)` 的等价物：在文件名末尾追加。
/// 对 "a.zip" → "a.zip.part"；对 "a" → "a.part"。
pub fn append_suffix(p: &Path, extra: &str) -> PathBuf {
    let name = p
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    p.with_file_name(format!("{name}{extra}"))
}

/// `paths.app_dir()` 已经 `resolve()` 过；Rust 侧从环境变量拿到的是原始写法
///（macOS `/var` vs `/private/var`）。所有要出现在响应里的程序目录都走这里，
/// 否则 `/api/update/status` 的 `running_from` 会因为一个软链接前缀而差字节。
pub fn app_dir(ctx: &Ctx) -> PathBuf {
    std::fs::canonicalize(&ctx.app_dir).unwrap_or_else(|_| ctx.app_dir.clone())
}

// ---------------------------------------------------------------- 配置

/// `paths.config_file()`（已在 paths.rs 里实现，这里只转发，避免两处口径）。
pub fn config_file() -> PathBuf {
    crate::paths::config_file()
}

pub fn read_config() -> Value {
    std::fs::read_to_string(config_file())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or(Value::Null)
}

/// `about._push_mode()`：每次重读配置文件，`update.mode == "push"`。
pub fn push_mode() -> bool {
    let d = read_config();
    let upd = match d.get("update") {
        Some(Value::Object(m)) => m.clone(),
        _ => return false,
    };
    let raw = match upd.get("mode") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(v) => py_str(v),
    };
    raw.trim().to_lowercase() == "push"
}

/// `_CFG` 里那份 `update` 子对象：进程启动时抓一份，`/api/update/mode` 改完同步更新。
/// Python 同样是内存态（`_CFG`），改配置文件不重启不会影响 `update.repo`。
static CFG_UPDATE: OnceLock<Mutex<Value>> = OnceLock::new();

fn cfg_update_cell() -> &'static Mutex<Value> {
    CFG_UPDATE.get_or_init(|| Mutex::new(Value::Null))
}

pub fn init_cfg(ctx: &Ctx) {
    let mut cell = cfg_update_cell().lock().unwrap();
    if cell.is_null() {
        *cell = match ctx.cfg.get("update") {
            Some(v @ Value::Object(_)) => v.clone(),
            _ => Value::Object(Map::new()),
        };
    }
}

pub fn update_cfg() -> Value {
    cfg_update_cell().lock().unwrap().clone()
}

pub fn set_update_cfg(v: Value) {
    *cfg_update_cell().lock().unwrap() = v;
}

/// `_CFG.get("update", {}).get("repo", "")`；非字符串按 `str()`。
pub fn repo_from_cfg() -> String {
    let upd = update_cfg();
    match upd.get("repo") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(v) => py_str(v),
    }
}

// ---------------------------------------------------------------- 更新状态

/// `about.STATE` 的字段与顺序照抄（顺序就是 JSON 的字节顺序）。
pub struct Upd {
    pub checked_at: f64,
    pub info: Option<Value>,
    pub error: String,
    pub phase: String,
    pub done: i64,
    pub total: i64,
    pub path: String,
    pub msg: String,
}

pub fn upd() -> &'static Mutex<Upd> {
    static S: OnceLock<Mutex<Upd>> = OnceLock::new();
    S.get_or_init(|| {
        Mutex::new(Upd {
            checked_at: 0.0,
            info: None,
            error: String::new(),
            phase: "idle".to_string(),
            done: 0,
            total: 0,
            path: String::new(),
            msg: String::new(),
        })
    })
}

pub fn busy() -> &'static Mutex<bool> {
    static B: OnceLock<Mutex<bool>> = OnceLock::new();
    B.get_or_init(|| Mutex::new(false))
}

/// `dict(STATE)`：键顺序 checked_at / info / error / phase / done / total / path / msg。
pub fn snapshot() -> Value {
    let st = upd().lock().unwrap();
    let mut m = Map::new();
    m.insert("checked_at".into(), json!(st.checked_at));
    m.insert("info".into(), st.info.clone().unwrap_or(Value::Null));
    m.insert("error".into(), json!(st.error));
    m.insert("phase".into(), json!(st.phase));
    m.insert("done".into(), json!(st.done));
    m.insert("total".into(), json!(st.total));
    m.insert("path".into(), json!(st.path));
    m.insert("msg".into(), json!(st.msg));
    Value::Object(m)
}

// ---------------------------------------------------------------- 文件响应

/// Starlette `FileResponse` 的 Content-Disposition 口径。
fn disposition(filename: &str) -> String {
    let quoted = quote_url(filename);
    if quoted != filename {
        format!("attachment; filename*=utf-8''{quoted}")
    } else {
        format!("attachment; filename=\"{filename}\"")
    }
}

fn quote_url(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(*b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

/// 直接吐文件 —— 照抄 Starlette 0.36.3 的 `FileResponse`（fastapi 0.110 锁的就是它）。
///
/// 三处实测出来的细节，少一条就和 Python 分叉：
///
/// 1. **没有 Range 分支**。`FileResponse.__call__` 从头到尾只读全量、永远 200；
///    实测 Python 收到 `Range: bytes=0-1` 也回 200 + `content-length: 15`，
///    也没有 `accept-ranges`。我们多给 206 反而是错。
/// 2. `init_headers` 对 `media_type` 以 `text/` 开头且不含 `charset=` 的，
///    追加 `"; charset=utf-8"` —— 所以下载 txt 是 `text/plain; charset=utf-8`。
/// 3. `set_stat_headers`：`content-length = str(st_size)`、
///    `last-modified = formatdate(st_mtime, usegmt=True)`、
///    `etag = '"' + md5(f"{st_mtime}-{st_size}") + '"'`。
///    注意 etag 里那个 `st_mtime` 是 **Python float 的 str**（带 `.0`、带小数位），
///    不是整数秒，所以下面要按 Python 的 `str(float)` 口径渲染。
pub fn file_response(p: &Path, mime: &str, cache: Option<&str>, filename: Option<&str>) -> Response {
    let meta = match std::fs::metadata(p) {
        Ok(m) => m,
        Err(_) => return crate::routes::common::http_err(404, "文件不存在"),
    };
    let size = meta.len();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as f64 + d.subsec_nanos() as f64 / 1e9)
        .unwrap_or(0.0);
    let bytes = match std::fs::read(p) {
        Ok(b) => b,
        Err(_) => return crate::routes::common::http_err(404, "文件不存在"),
    };
    let mut res = bytes.into_response();
    let h = res.headers_mut();
    // headers={"Cache-Control": ...} 先落（Python 侧 raw_headers 里它排第一）
    if let Some(c) = cache {
        if let Ok(v) = HeaderValue::from_str(c) {
            h.insert(header::CACHE_CONTROL, v);
        }
    }
    let ctype = if mime.starts_with("text/") && !mime.to_lowercase().contains("charset=") {
        format!("{mime}; charset=utf-8")
    } else {
        mime.to_string()
    };
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&ctype).unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    if let Some(f) = filename {
        if let Ok(v) = HeaderValue::from_str(&disposition(f)) {
            h.insert(header::CONTENT_DISPOSITION, v);
        }
    }
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(size));
    h.insert(
        header::LAST_MODIFIED,
        HeaderValue::from_str(&http_date(mtime)).unwrap_or(HeaderValue::from_static("")),
    );
    let etag = format!("\"{}\"", md5_hex(format!("{}-{}", py_float_str(mtime), size).as_bytes()));
    if let Ok(v) = HeaderValue::from_str(&etag) {
        h.insert(header::ETAG, v);
    }
    res
}

/// `str(float)` 的口径：整数值要带 `.0`，其余取最短往返表示（Rust `{}` 与
/// Python `repr` 都是最短往返，差别只在整数没有小数点这一处）。
fn py_float_str(f: f64) -> String {
    if f == f.trunc() && f.abs() < 1e16 {
        format!("{f:.1}")
    } else {
        format!("{f}")
    }
}

/// `email.utils.formatdate(ts, usegmt=True)`：`Wed, 21 Oct 2015 07:28:00 GMT`。
fn http_date(ts: f64) -> String {
    let secs = ts as i64;
    match chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0) {
        Some(dt) => dt.format("%a, %d %b %Y %H:%M:%S GMT").to_string(),
        None => "Thu, 01 Jan 1970 00:00:00 GMT".to_string(),
    }
}

// ---------------------------------------------------------------- MD5

/// `hashlib.md5(data).hexdigest()`。Cargo.toml 不能加依赖，自己带一份。
fn md5_hex(data: &[u8]) -> String {
    let mut a0: u32 = 0x67452301;
    let mut b0: u32 = 0xefcdab89;
    let mut c0: u32 = 0x98badcfe;
    let mut d0: u32 = 0x10325476;

    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
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
        for (i, slot) in m.iter_mut().enumerate() {
            *slot = u32::from_le_bytes([
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

// ---------------------------------------------------------------- MIME

/// `storage.mime_of()`：先查 MIME_EXTRA，再退回 `mimetypes.guess_type`。
pub fn mime_of(p: &Path) -> String {
    let ext = p
        .extension()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let extra = match ext.as_str() {
        "md" | "markdown" => Some("text/markdown"),
        "txt" => Some("text/plain"),
        "json" => Some("application/json"),
        "csv" => Some("text/csv"),
        "log" => Some("text/plain"),
        "yaml" | "yml" => Some("text/yaml"),
        "py" => Some("text/x-python"),
        "sh" => Some("text/x-shellscript"),
        "html" => Some("text/html"),
        "css" => Some("text/css"),
        "js" | "ts" => Some("text/javascript"),
        "svg" => Some("image/svg+xml"),
        "heic" => Some("image/heic"),
        "heif" => Some("image/heif"),
        "m4a" => Some("audio/mp4"),
        "m4v" => Some("video/x-m4v"),
        "mkv" => Some("video/x-matroska"),
        "webm" => Some("video/webm"),
        "epub" => Some("application/epub+zip"),
        "pages" | "numbers" | "key" => Some("application/octet-stream"),
        _ => None,
    };
    if let Some(m) = extra {
        return m.to_string();
    }
    let guess = match ext.as_str() {
        "pdf" => "application/pdf",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "ico" => "image/vnd.microsoft.icon",
        "tif" | "tiff" => "image/tiff",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "mp3" => "audio/mpeg",
        "wav" => "audio/x-wav",
        "flac" => "audio/x-flac",
        "ogg" | "opus" => "audio/ogg",
        "aac" => "audio/aac",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        "tar" => "application/x-tar",
        "rar" => "application/vnd.rar",
        "7z" => "application/x-7z-compressed",
        "dmg" => "application/x-apple-diskimage",
        "xml" => "text/xml",
        "rtf" => "text/rtf",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "wasm" => "application/wasm",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        _ => "",
    };
    if guess.is_empty() {
        "application/octet-stream".to_string()
    } else {
        guess.to_string()
    }
}

// ---------------------------------------------------------------- 其它

/// 12 位十六进制随机串（`uuid.uuid4().hex[:n]` 的替身）。
pub fn rand_hex(n: usize) -> String {
    use std::io::Read;
    let mut buf = vec![0u8; n];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok();
    if !ok {
        let mut seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
            ^ ((std::process::id() as u64) << 32);
        for b in buf.iter_mut() {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            *b = (seed & 0xff) as u8;
        }
    }
    buf.iter().map(|b| format!("{b:02x}")).collect::<String>()[..n].to_string()
}
