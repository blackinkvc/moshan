// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：泳道 w6b 的路由装配。本目录由该泳道独占，与其他泳道并行施工。
// 不做：不改 main.rs / routes/mod.rs / 共享模块（auth、paths、storage、jsonio、
//       ledger、common、w1..w4）—— 那些是所有人共用的地基。
//
// 本泳道搬的是「Agent 协作组 + 一组杂项」29 条：
//   Agent 主体    schema / usage / event / read / note / trae·note / probe / brief
//   Agent 导入    import·detect / import / import·scan / config·scan
//                 store·refresh / store·stats
//   Agent 联动    link·prompt / link·scan / link·import / link·install / link·report
//   杂项          chat / intake / parse-event / pick·dir / plugins·add /
//                 plugins·remove / rescan / reveal / sort / pair / pair·send

pub mod agent;
pub mod bus;
pub mod config;
pub mod import;
pub mod link;
pub mod misc;
pub mod pair;
pub mod store;
pub mod util;

use crate::paths::Ctx;
use axum::body::Bytes;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub fn router() -> Router<Arc<Ctx>> {
    Router::new()
        .merge(agent::router())
        .merge(import::router())
        .merge(config::router())
        .merge(store::router())
        .merge(link::router())
        .merge(misc::router())
        .merge(pair::router())
}

/// `await request.json()` 里 `ValueError` 那一支：读不出来就当没给。
///
/// 注意与 `parse_json_strict` 的分工：这里的上层都写成
/// `except ValueError: body = {}`（或直接给业务错误），所以坏 JSON 与空体
/// 的结果一样，都是 `None`。
pub(crate) fn parse_json_opt(b: &Bytes) -> Option<Value> {
    if b.is_empty() {
        return None;
    }
    serde_json::from_slice::<Value>(b).ok()
}

/// `p.write_text(json.dumps(v, ensure_ascii=False, indent=n))`
pub(crate) fn write_json_indent(p: &Path, v: &Value, indent: usize) -> std::io::Result<()> {
    std::fs::write(p, crate::jsonio::dumps(v, indent))
}

/// `dash._root(cfg)`：`allowed_roots[0]`，空则收件箱的上级。
///
/// 与 `Ctx::allowed_roots` 的兜底一致（那里配置空时已退回 library_dir）。
pub(crate) fn data_root(ctx: &Ctx) -> PathBuf {
    // 整个 w6b（Agent 总线、工作台插件、pair/transfer 的记录）都是运行数据，
    // 一条媒体路径都没有 —— 所以这里直接给运行根，别再回落 media。
    ctx.runtime_dir.clone()
}

// ---------------------------------------------------------------- FastAPI 形状

/// FastAPI 缺必填 body 时的 422。
pub(crate) fn missing_body() -> Response {
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

/// body 是合法 JSON 但不是对象：pydantic 报 `dict_type`。
pub(crate) fn dict_type_body(v: &Value) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({"detail": [{
            "type": "dict_type",
            "loc": ["body"],
            "msg": "Input should be a valid dictionary",
            "input": v,
            "url": "https://errors.pydantic.dev/2.14/v/dict_type",
        }]})),
    )
        .into_response()
}

/// JSON 坏掉：pydantic 报 `json_invalid`（文案对照与 w4b 同一套）。
pub(crate) fn json_invalid_body(e: &serde_json::Error) -> Response {
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

/// `body: dict` 必填的接口：缺体 / 非对象 / 坏 JSON 各自 422。
pub(crate) fn parse_body(raw: &Bytes) -> Result<Value, Response> {
    if raw.is_empty() {
        return Err(missing_body());
    }
    match serde_json::from_slice::<Value>(raw) {
        Ok(Value::Object(map)) => Ok(Value::Object(map)),
        Ok(Value::Null) => Err(missing_body()),
        Ok(v) => Err(dict_type_body(&v)),
        Err(e) => Err(json_invalid_body(&e)),
    }
}

/// `PlainTextResponse(txt, media_type=...)`
pub(crate) fn plain_text(code: u16, body: String, media: &'static str) -> Response {
    (
        StatusCode::from_u16(code).unwrap_or(StatusCode::OK),
        [(axum::http::header::CONTENT_TYPE, media)],
        body,
    )
        .into_response()
}
