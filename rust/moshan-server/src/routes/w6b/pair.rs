// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：`moshan/pair.py` 的两条 HTTP 路由 —— 取手机接入整包（/api/pair）、
//       把接入方案发到微信（/api/pair/send）。
// 不做：`/pair` 那一页 HTML（不在本泳道清单里）；8778 那边任何计算（数据全在它手里）。
//
// 鉴权与别的路由不一样：**不是 auth，也不是回环免令牌** —— Python 的 `_ok()` 要
// `x-token` 或 `?k=` 等于主令牌 **或 view_token**，否则 401 `{"detail":"token 无效"}`。
// 所以这里不能直接复用 `common::peek`（那只认主令牌，会把 view_token 挡在门外）。
//
// 发微信走网关（127.0.0.1:18789）的 WebSocket RPC（`crate::wechat`）：
// 发送在 spawn_blocking 里做，失败只回「网关没接」，不假装发出去了。

use crate::paths::Ctx;
use crate::routes::common::{q, CtxState};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

pub fn router() -> Router<Arc<Ctx>> {
    Router::new()
        .route("/api/pair", get(pair_get))
        .route("/api/pair/send", post(pair_send))
}

const DEV_BASE: &str = "http://127.0.0.1:8778";

/// `_ok(k, x_token)`：主令牌 **或** view_token，空串不算。
fn pair_ok(ctx: &Ctx, headers: &HeaderMap, uri: &Uri) -> bool {
    let x = headers
        .get("x-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let k = q(uri, "k");
    let got = if !x.is_empty() { x } else { k };
    if got.is_empty() {
        return false;
    }
    let view = ctx
        .cfg
        .get("view_token")
        .map(|v| match v {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        })
        .unwrap_or_default();
    got == ctx.token || (!view.is_empty() && got == view)
}

fn unauth() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"detail": "token 无效"})),
    )
        .into_response()
}

// ---------------------------------------------------------------- 取整包

/// `urllib.parse.quote(s, safe="")`
fn quote(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        let c = *b as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
            out.push(c);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

fn strerror(code: i32) -> String {
    // macOS/Linux 要的是 `os.strerror()` 那句纯文案（对拍按字节比）；
    // Windows 没有 libc::strerror，用 std 的错误文案顶上。
    #[cfg(unix)]
    unsafe {
        let p = libc::strerror(code);
        if p.is_null() {
            return format!("OS error {}", code);
        }
        std::ffi::CStr::from_ptr(p).to_string_lossy().to_string()
    }
    #[cfg(not(unix))]
    {
        std::io::Error::from_raw_os_error(code).to_string()
    }
}

/// 一次极简的 HTTP/1.1 GET（只打本机 8778，无 TLS）。
/// 错误文案按 Python urllib 的口径拼（`<urlopen error [Errno 61] Connection refused>`），
/// 因为那串字会原样进 `{"ok": false, "error": "智能设备服务（8778）没起来：…"}`。
fn http_get(url: &str) -> Result<String, String> {
    let rest = url.strip_prefix("http://").ok_or_else(|| "unknown url type".to_string())?;
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().unwrap_or(80)),
        None => (hostport, 80),
    };
    let addr = format!("{}:{}", host, port);
    let sock = std::net::ToSocketAddrs::to_socket_addrs(&addr)
        .ok()
        .and_then(|mut it| it.next());
    let Some(sock) = sock else {
        return Err("<urlopen error [Errno 8] nodename nor servname provided, or not known>".into());
    };
    let mut stream = match TcpStream::connect_timeout(&sock, Duration::from_secs(5)) {
        Ok(s) => s,
        Err(e) => {
            return Err(match e.raw_os_error() {
                Some(code) => format!("<urlopen error [Errno {}] {}>", code, strerror(code)),
                None => "<urlopen error timed out>".to_string(),
            })
        }
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n",
        path, hostport
    );
    if stream.write_all(req.as_bytes()).is_err() {
        return Err("<urlopen error [Errno 32] Broken pipe>".to_string());
    }
    let mut buf: Vec<u8> = Vec::new();
    if let Err(e) = stream.read_to_end(&mut buf) {
        return Err(match e.raw_os_error() {
            Some(code) => format!("<urlopen error [Errno {}] {}>", code, strerror(code)),
            None => "<urlopen error timed out>".to_string(),
        });
    }
    let text = String::from_utf8_lossy(&buf).to_string();
    let (head, body) = match text.find("\r\n\r\n") {
        Some(i) => (&text[..i], &text[i + 4..]),
        None => return Err("<urlopen error [Errno 61] Connection refused>".to_string()),
    };
    let status: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    if !(200..300).contains(&status) {
        let reason = head
            .lines()
            .next()
            .and_then(|l| l.splitn(3, ' ').nth(2))
            .unwrap_or("")
            .to_string();
        return Err(format!("HTTP Error {}: {}", status, reason));
    }
    Ok(body.to_string())
}

/// `fetch(did, q)`：向 8778 要一整包；没起来就如实说。
fn fetch(did: &str, qs: &str) -> Value {
    let mut url = format!("{}/api/mobile/pair", DEV_BASE);
    let mut parts: Vec<String> = Vec::new();
    if !did.is_empty() {
        parts.push(format!("did={}", quote(did)));
    }
    if !qs.is_empty() {
        parts.push(format!("q={}", quote(qs)));
    }
    if !parts.is_empty() {
        url.push('?');
        url.push_str(&parts.join("&"));
    }
    match http_get(&url) {
        Ok(body) => match serde_json::from_str::<Value>(&body) {
            Ok(v) => v,
            Err(e) => json!({"ok": false,
                             "error": format!("智能设备服务（8778）没起来：{}",
                                              crate::routes::w6b::util::cut(&e.to_string(), 90))}),
        },
        Err(e) => json!({"ok": false,
                         "error": format!("智能设备服务（8778）没起来：{}",
                                          crate::routes::w6b::util::cut(&e, 90))}),
    }
}

// ---------------------------------------------------------------- 拼消息

/// `compose(data)`：发到微信的那条消息。
fn compose(data: &Value) -> String {
    if !crate::routes::w6b::util::truthy(data.get("ok").unwrap_or(&Value::Null)) {
        let err = crate::routes::w6b::util::s_or(
            data.get("error").unwrap_or(&Value::Null),
        );
        return format!(
            "手机接入：{}",
            if err.is_empty() { "取不到地址".to_string() } else { err }
        );
    }
    let dev = crate::routes::w6b::util::s_or(
        data.get("device")
            .and_then(|d| d.get("name"))
            .unwrap_or(&Value::Null),
    );
    let url = crate::routes::w6b::util::s_or(data.get("phone_url").unwrap_or(&Value::Null));
    if url.is_empty() {
        return "手机接入：没探到可用地址（局域网和 Tailscale 都不通），先在电脑上看一眼网络。"
            .to_string();
    }
    let head = format!(
        "【手机接入】{}",
        if dev.is_empty() { "选好这台手机".to_string() } else { dev }
    );
    let mut tail = "打开后电量自动读本机、屏幕时间带上今天已报的值，点一下「上报」就行；想连按钮都不点，把地址加到主屏幕（结尾带 go=1 打开即报）。".to_string();
    let enabled = data.get("enabled").map(crate::routes::w6b::util::truthy).unwrap_or(false);
    if !enabled {
        tail = format!(
            "⚠️ 上报通道现在是关的（智能设备 → 接入设备 → 手机实时上报），开了这条链接才打得开。{}",
            tail
        );
    }
    format!("{}\n{}\n{}", head, url, tail)
}

// ---------------------------------------------------------------- 路由

async fn pair_get(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if !pair_ok(&ctx, &headers, &uri) {
        return unauth();
    }
    let did = q(&uri, "did");
    let qs = q(&uri, "q");
    let out = tokio::task::spawn_blocking(move || fetch(&did, &qs))
        .await
        .unwrap_or_else(|_| json!({"ok": false, "error": "智能设备服务（8778）没起来：RuntimeError"}));
    Json(out).into_response()
}

async fn pair_send(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    if !pair_ok(&ctx, &headers, &uri) {
        return unauth();
    }
    let b = match crate::routes::w6b::parse_body(&body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let mut text = crate::routes::w6b::util::s_or(b.get("text").unwrap_or(&Value::Null))
        .trim()
        .to_string();
    if text.is_empty() {
        let did = crate::routes::w6b::util::s_or(b.get("did").unwrap_or(&Value::Null));
        let qs = crate::routes::w6b::util::s_or(b.get("q").unwrap_or(&Value::Null));
        let data = tokio::task::spawn_blocking(move || fetch(&did, &qs))
            .await
            .unwrap_or_else(|_| json!({"ok": false, "error": "取不到地址"}));
        text = compose(&data);
    }
    // 真发：网关不通 / 发送失败都只回「网关没接」，与 Python `send()` 的 False 同形。
    // 握手要 200~800ms，丢进阻塞线程，别占 tokio 工作线程。
    let text_for_send = text.clone();
    let ok = tokio::task::spawn_blocking(move || crate::wechat::send(&text_for_send))
        .await
        .unwrap_or(false);
    Json(json!({
        "ok": ok,
        "text": text,
        "error": if ok { "" } else { "网关没接（微信助手没跑？）" },
    }))
    .into_response()
}
