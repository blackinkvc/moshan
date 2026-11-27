// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：泳道 u 自己的小工具 —— MIME 判定、预览类型、HTML/文件响应、页面注入、Range。
// 不做：不碰共享地基（storage/jsonio/ledger）；这些函数只在本泳道内使用。

#![allow(dead_code)]

use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use std::path::Path;

/// `server.py:_SEC_HEADERS` 里与 body 无关的那几项。页面的安全头照抄，
/// 免得同一个页面走 Rust 应答时少了 CSP（前端行为会不一样）。
pub fn sec_headers() -> Vec<(&'static str, &'static str)> {
    vec![
        ("cache-control", "no-store, max-age=0"),
        ("referrer-policy", "no-referrer"),
        ("x-content-type-options", "nosniff"),
        ("x-frame-options", "DENY"),
        (
            "content-security-policy",
            "default-src 'self'; img-src 'self' data: blob:; media-src 'self' blob:; \
             style-src 'self' 'unsafe-inline'; script-src 'self' 'unsafe-inline'; \
             connect-src 'self'; \
             frame-src 'self' http://127.0.0.1:* http://localhost:* \
             https://127.0.0.1:* https://localhost:*; \
             frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
        ),
    ]
}

fn apply(res: &mut Response, extra: &[(&str, &str)]) {
    let h = res.headers_mut();
    for (k, v) in extra {
        if let (Ok(name), Ok(val)) = (
            header::HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(v),
        ) {
            h.append(name, val);
        }
    }
}

/// HTMLResponse：`text/html; charset=utf-8` + 调用方给的状态码/头。
pub fn html(status: u16, body: impl Into<String>, extra: &[(&str, &str)]) -> Response {
    let mut res = (
        StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
        body.into(),
    )
        .into_response();
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    apply(&mut res, extra);
    res
}

/// PlainTextResponse：`/agent` 用 `text/markdown; charset=utf-8`。
pub fn text(status: u16, body: String, mime: &str) -> Response {
    let mut res = (
        StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
        body,
    )
        .into_response();
    if let Ok(v) = HeaderValue::from_str(mime) {
        res.headers_mut().insert(header::CONTENT_TYPE, v);
    }
    res
}

/// Starlette `FileResponse` 的 Content-Disposition 口径：
/// ASCII 文件名用 `filename="x"`，含非 ASCII 时改成 RFC 5987 的 `filename*=utf-8''…`。
fn disposition(filename: &str) -> String {
    let quoted = quote_url(filename);
    if quoted != filename {
        format!("attachment; filename*=utf-8''{quoted}")
    } else {
        format!("attachment; filename=\"{filename}\"")
    }
}

/// Starlette 的 `quote(safe="/")`：字母数字与 `_.-~` 保留，`/` 保留。
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

/// 直接吐一个文件。`filename` 给 None 时不带 Content-Disposition（`/s/{sid}` 的 `dl=0`）。
/// 支持单段 Range：Starlette 的 FileResponse 也支持，浏览器里 <video> 拖进度要用。
pub fn file_response(
    p: &Path,
    mime: &str,
    cache: Option<&str>,
    filename: Option<&str>,
    range: Option<&str>,
) -> Response {
    let meta = match std::fs::metadata(p) {
        Ok(m) => m,
        Err(_) => return crate::routes::common::http_err(404, "文件不存在"),
    };
    let size = meta.len();
    // 先看 Range：只认 `bytes=a-b` 单段，多段退化成整文件（Starlette 也只做单段）。
    if let Some(raw) = range {
        if let Some((start, end)) = parse_range(raw, size) {
            match read_part(p, start, end) {
                Ok(bytes) => {
                    let mut res = (StatusCode::PARTIAL_CONTENT, bytes).into_response();
                    let h = res.headers_mut();
                    h.insert(header::CONTENT_TYPE, HeaderValue::from_str(mime).unwrap_or(HeaderValue::from_static("application/octet-stream")));
                    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
                    h.insert(header::CONTENT_RANGE, HeaderValue::from_str(&format!("bytes {start}-{end}/{size}")).unwrap());
                    if let Some(c) = cache {
                        if let Ok(v) = HeaderValue::from_str(c) {
                            h.insert(header::CACHE_CONTROL, v);
                        }
                    }
                    if let Some(f) = filename {
                        if let Ok(v) = HeaderValue::from_str(&disposition(f)) {
                            h.insert(header::CONTENT_DISPOSITION, v);
                        }
                    }
                    return res;
                }
                Err(_) => return crate::routes::common::http_err(500, "读取失败"),
            }
        }
        if raw.trim_start().starts_with("bytes=") {
            // 语法对但区间不可满足 → 416，与 Starlette 一致。
            return (
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{size}"))],
                "",
            )
                .into_response();
        }
    }

    let bytes = match std::fs::read(p) {
        Ok(b) => b,
        Err(_) => return crate::routes::common::http_err(404, "文件不存在"),
    };
    let mut res = bytes.into_response();
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(mime).unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Some(c) = cache {
        if let Ok(v) = HeaderValue::from_str(c) {
            h.insert(header::CACHE_CONTROL, v);
        }
    }
    if let Some(f) = filename {
        if let Ok(v) = HeaderValue::from_str(&disposition(f)) {
            h.insert(header::CONTENT_DISPOSITION, v);
        }
    }
    res
}

fn parse_range(raw: &str, size: u64) -> Option<(u64, u64)> {
    let s = raw.trim();
    let rest = s.strip_prefix("bytes=")?;
    if rest.contains(',') {
        return None;
    }
    let (a, b) = rest.split_once('-')?;
    if a.trim().is_empty() {
        // `bytes=-N`：最后 N 字节
        let n: u64 = b.trim().parse().ok()?;
        if n == 0 || size == 0 {
            return None;
        }
        let start = size.saturating_sub(n);
        return Some((start, size - 1));
    }
    let start: u64 = a.trim().parse().ok()?;
    let end: u64 = if b.trim().is_empty() {
        size.checked_sub(1)?
    } else {
        b.trim().parse().ok()?
    };
    if start > end || start >= size {
        return None;
    }
    Some((start, end.min(size - 1)))
}

fn read_part(p: &Path, start: u64, end: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(p)?;
    f.seek(SeekFrom::Start(start))?;
    let mut buf = vec![0u8; (end - start + 1) as usize];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

pub fn range_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_string())
}

// ---------------------------------------------------------------- MIME

/// `storage.mime_of()`：先查 MIME_EXTRA，再退回 `mimetypes.guess_type`。
/// 只影响 Content-Type 头，但和 Python 列在同一张表里更不容易漏。
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
    // mimetypes.guess_type 的常见项（macOS 的 mime.types 与这里一致）。
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

// ---------------------------------------------------------------- 预览类型

const PREVIEW_IMAGE: &[&str] =
    &["jpg", "jpeg", "png", "gif", "webp", "heic", "heif", "bmp", "svg", "ico"];
const PREVIEW_VIDEO: &[&str] = &["mp4", "mov", "m4v", "webm", "mkv", "avi"];
const PREVIEW_AUDIO: &[&str] = &["mp3", "m4a", "wav", "aac", "flac", "ogg", "opus"];
const TEXT_EXTS: &[&str] = &[
    "txt", "md", "markdown", "json", "csv", "tsv", "log", "yaml", "yml", "toml", "ini", "py",
    "js", "ts", "jsx", "tsx", "html", "css", "scss", "sh", "sql", "xml", "conf", "gitignore",
    "env",
];

/// `storage.preview_kind()`：与 `Path(name).suffix.lower().lstrip(".")` 同一口径。
pub fn preview_kind(p: &Path) -> &'static str {
    let ext = p
        .extension()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let e = ext.as_str();
    if PREVIEW_IMAGE.contains(&e) {
        return "image";
    }
    if PREVIEW_VIDEO.contains(&e) {
        return "video";
    }
    if PREVIEW_AUDIO.contains(&e) {
        return "audio";
    }
    if e == "pdf" {
        return "pdf";
    }
    if TEXT_EXTS.contains(&e) && p.is_file() {
        if let Ok(m) = std::fs::metadata(p) {
            if m.len() <= 2 * 1024 * 1024 {
                return "text";
            }
        }
    }
    "none"
}

// ---------------------------------------------------------------- 其它

/// `shutil.which()` 的可用版本：PATH 逐段找可执行普通文件。
pub fn which(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(name);
        if crate::hostwork::platform::is_exec_file(&cand) {
            return Some(cand.to_string_lossy().to_string());
        }
    }
    None
}

/// 本机 Tailscale 地址：先看网卡里有没有 100.x，再退回 `tailscale ip -4`。
/// 页面上用于「连不上时自己切 100.x 明文口」。
///
/// 与 `tls::tailscale_ip()` 同一份实现 —— 两个模块各写一遍会漂移
/// （之前 Windows 的 `tailscale.exe` 路径只写在 tls 那边）。
pub fn tailscale_ip() -> Option<String> {
    crate::tls::tailscale_ip()
}


/// Python 的 `round()`（银行家舍入）；appearance 的十六进制换算要用。
pub fn py_round(x: f64) -> f64 {
    let f = x.floor();
    let diff = x - f;
    if (diff - 0.5).abs() < 1e-12 {
        if (f as i64) % 2 == 0 {
            f
        } else {
            f + 1.0
        }
    } else {
        x.round()
    }
}
