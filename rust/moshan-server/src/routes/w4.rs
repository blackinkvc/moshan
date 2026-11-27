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

/// FastAPI 缺必填查询参数时的 422 形状。
///
/// 前端永远会带 id，但「不带参数直接打」是对拍里必然会出现的一步 ——
/// 与其让它红着，不如把形状照抄。`url` 里的 pydantic 版本跟 requirements.txt 走，
/// 升级 pydantic 时这里一起改（它只出现在错误消息里，不影响任何判定）。
fn missing_query(field: &str) -> Response {
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

pub async fn one(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let Some(id) = q.get("id") else {
        return missing_query("id");
    };
    Json(ledger::full_item(&ctx, id)).into_response()
}

pub async fn img(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let Some(id) = q.get("id") else {
        return missing_query("id");
    };
    let raw = q.get("raw").and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
    let Some(p) = ledger::image_file(&ctx, id, raw != 0) else {
        return http_err(404, "没有这张图");
    };
    if raw != 0 {
        return file_bytes(&p, ledger::mime_of(&p), "private, max-age=60");
    }
    // 与 `/api/thumb` 同一套：宽限制 80..1200，缩不出来就回原图（功能不能断）。
    let w = q
        .get("w")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(400)
        .clamp(80, 1200) as u32;
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
