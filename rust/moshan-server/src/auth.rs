// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：令牌校验，三通道与 `server.py:auth()` 完全一致（header → URL 参数 → Cookie）。
// 不做：不做来源过滤（guard_network）。那是独立一层，属于 W2 文件层的前置项，
//       这里若顺手加上，会让 52 条公开端点在局域网里变 401。

use crate::paths::Ctx;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// 常量时间比较：普通 `==` 会在第一个不同字节处返回，理论上可按耗时逐字节试出令牌。
/// 令牌就在本机回环上，风险很低，但这是**所有**受保护端点的唯一闸门，不省这点代码。
fn eq_ct(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

fn query_k(uri: &Uri) -> Option<String> {
    let q = uri.query()?;
    for pair in q.split('&') {
        let (k, v) = match pair.split_once('=') {
            Some(kv) => kv,
            None => continue,
        };
        if k == "k" {
            return Some(percent_decode(v));
        }
    }
    None
}

/// 只解 %XX 与 '+'：令牌是 URL-safe 字符集，不需要完整的表单解码。
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(b) => {
                        out.push(b);
                        i += 3;
                    }
                    None => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn cookie_k(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    for part in raw.split(';') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix("moshan_k=") {
            return Some(v.to_string());
        }
    }
    None
}

pub fn supplied(headers: &HeaderMap, uri: &Uri) -> String {
    headers
        .get("x-token")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string())
        .or_else(|| query_k(uri))
        .or_else(|| cookie_k(headers))
        .unwrap_or_default()
}

pub fn token_ok(ctx: &Ctx, headers: &HeaderMap, uri: &Uri) -> bool {
    let s = supplied(headers, uri);
    !s.is_empty() && eq_ct(&s, &ctx.token)
}

/// FastAPI 的 `HTTPException(401, "token 无效")` 渲染成 `{"detail":"无效"}`。
/// 形状必须照搬：前端 `api()` 读的是 `detail` 字段。
pub fn deny() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "detail": "token 无效" })),
    )
        .into_response()
}
