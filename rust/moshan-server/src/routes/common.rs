// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：泳道共用的四件小事 —— 鉴权闸门、两种错误形状、请求体取值、查询串解码。
// 不做：不做业务判断。这些函数只有一处实现，是为了让六条泳道的「拒绝形状」
//       不可能各写一套：`{"detail": ...}` 与 `{"ok": false, "error": ...}` 是两种
//       前端契约，混用一次就要修一周。
//
// 泳道还没用到的函数在单特性编译时是死的，属正常，不必逐个 `#[allow]`。
#![allow(dead_code)]

use crate::auth;
use crate::paths::Ctx;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use std::sync::Arc;

pub type CtxState = State<Arc<Ctx>>;

/// FastAPI `HTTPException(code, msg)` 的默认形状。
pub fn http_err(code: u16, msg: &str) -> Response {
    (
        StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_REQUEST),
        Json(json!({ "detail": msg })),
    )
        .into_response()
}

/// 业务层自己 `return {"ok": False, "error": ...}` 的形状（HTTP 仍是 200）。
pub fn ok_err(msg: &str) -> Json<Value> {
    Json(json!({ "ok": false, "error": msg }))
}

/// 鉴权不通过时返回拒绝响应；通过返回 None。
pub fn peek(ctx: &Ctx, headers: &HeaderMap, uri: &Uri) -> Option<Response> {
    if auth::token_ok(ctx, headers, uri) {
        None
    } else {
        Some(auth::deny())
    }
}

/// `body: dict` 的参数：缺体/空体都当 `{}`，与 FastAPI 给 `dict` 的缺省一致。
pub fn body_obj(body: Option<Json<Value>>) -> Value {
    match body.map(|j| j.0) {
        Some(v) if v.is_object() => v,
        _ => json!({}),
    }
}

pub fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("")
}

pub fn i(v: &Value, key: &str) -> i64 {
    match v.get(key) {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::Bool(b)) => *b as i64,
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

pub fn b(v: &Value, key: &str) -> bool {
    match v.get(key) {
        Some(Value::Bool(x)) => *x,
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0) != 0,
        Some(Value::String(s)) => !s.is_empty() && s != "0" && s != "false",
        _ => false,
    }
}

/// `?k=v&k2=v2` 解码成表。`+` 按表单口径还原成空格 —— FastAPI 也是这么解的，
/// 路径里空格被编成 `%20`、短语里被编成 `+`，两种都得认。
pub fn query_map(uri: &Uri) -> Vec<(String, String)> {
    let Some(q) = uri.query() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        out.push((dec(k), dec(v)));
    }
    out
}

fn dec(s: &str) -> String {
    let plus = s.replace('+', " ");
    percent_encoding::percent_decode_str(&plus)
        .decode_utf8_lossy()
        .into_owned()
}

/// 取第一个同名参数；缺省返回 `""`。
pub fn q(uri: &Uri, key: &str) -> String {
    query_map(uri)
        .into_iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
        .unwrap_or_default()
}

/// FastAPI `?x=1` 声明成 `int` 时的口径：空串用缺省，非数字 422。
/// 这里只在真要报错的地方用，返回 Err 时调用方自己决定形状。
pub fn q_int(uri: &Uri, key: &str, default: i64) -> Result<i64, ()> {
    let raw = q(uri, key);
    if raw.trim().is_empty() {
        return Ok(default);
    }
    raw.trim().parse::<i64>().map_err(|_| ())
}
