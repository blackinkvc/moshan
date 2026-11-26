// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：W4 记账账本（6 条：整体/单条/图片/改/删）。
// 不做：不做 `/api/ledger/upload` 与 `/api/ledger/image` —— 那两条要吃 multipart
//       并起 Swift 工具做 OCR/裁图，等上传基建落定后一起迁。

use crate::auth;
use crate::ledger;
use crate::paths::Ctx;
use crate::storage;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;

type CtxState = State<Arc<Ctx>>;

fn http_err(code: u16, msg: &str) -> Response {
    (
        StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_REQUEST),
        Json(json!({ "detail": msg })),
    )
        .into_response()
}

fn peek(ctx: &Ctx, headers: &HeaderMap, uri: &Uri) -> Option<Response> {
    if auth::token_ok(ctx, headers, uri) {
        None
    } else {
        Some(auth::deny())
    }
}

/// 直接回一段文件内容。账本图/后续 `/api/raw` 都用它。
/// 只做 `<img>` 需要的那几项头：类型、长度、缓存。Range 留给真正的下载接口。
pub fn file_bytes(p: &Path, mime: &str, cache: &str) -> Response {
    match std::fs::read(p) {
        Ok(bytes) => {
            let mut res = bytes.into_response();
            let h = res.headers_mut();
            if let Ok(v) = HeaderValue::from_str(mime) {
                h.insert(header::CONTENT_TYPE, v);
            }
            if let Ok(v) = HeaderValue::from_str(cache) {
                h.insert(header::CACHE_CONTROL, v);
            }
            res
        }
        Err(_) => http_err(404, "文件不存在"),
    }
}

pub async fn all(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    Json(ledger::payload(&ctx)).into_response()
}

#[derive(Deserialize)]
pub struct IdQ {
    #[serde(default)]
    id: String,
    #[serde(default = "default_w")]
    w: i64,
    #[serde(default)]
    raw: i64,
}

fn default_w() -> i64 {
    400
}

pub async fn one(State(ctx): CtxState, headers: HeaderMap, uri: Uri, Query(q): Query<IdQ>) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    Json(ledger::full_item(&ctx, &q.id)).into_response()
}

pub async fn img(State(ctx): CtxState, headers: HeaderMap, uri: Uri, Query(q): Query<IdQ>) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let Some(p) = ledger::image_file(&ctx, &q.id, q.raw != 0) else {
        return http_err(404, "没有这张图");
    };
    if q.raw != 0 {
        return file_bytes(&p, ledger::mime_of(&p), "private, max-age=60");
    }
    // 与 `/api/thumb` 同一套：宽限制 80..1200，缩不出来就回原图（功能不能断）。
    let w = q.w.clamp(80, 1200) as u32;
    match storage::make_thumb(&p, w) {
        Some(t) => file_bytes(&t, "image/jpeg", "private, max-age=600"),
        None => file_bytes(&p, ledger::mime_of(&p), "private, max-age=60"),
    }
}

pub async fn update(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: Option<Json<Value>>,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let body = body.map(|j| j.0).unwrap_or(Value::Null);
    let id = body.get("id").map(ledger::py_str).unwrap_or_default();
    // `body.get("patch") or {}`：null / 缺失都当空补丁
    let patch = body.get("patch").cloned().unwrap_or(json!({}));
    Json(ledger::update(&ctx, &id, &patch)).into_response()
}

pub async fn remove(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: Option<Json<Value>>,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let body = body.map(|j| j.0).unwrap_or(Value::Null);
    let id = body.get("id").map(ledger::py_str).unwrap_or_default();
    Json(ledger::remove(&ctx, &id)).into_response()
}
