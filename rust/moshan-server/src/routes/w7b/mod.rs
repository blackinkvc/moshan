// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：泳道 w7b 的路由装配 —— 「自更新 + 传输队列 + 更新推送」共 18 条。
// 不做：不改 main.rs / routes/mod.rs / 共享模块（auth、paths、storage、jsonio、
//       ledger、common、w1..w4）—— 那些是所有人共用的地基。
//
// 鉴权口径（两条，别混）：
//   · `/api/update/check|mode|state|download|apply`：about.py 自己的 `_token_ok`，
//     失败是 401 `{"ok": false, "error": "需要访问令牌"}`；
//   · `/api/update/push|status|rollback`：`server.py:auth_agent`，回环来源直接放行。
//     本进程只绑 127.0.0.1，所以这里不校验（同 w6b 的 agent_gate）。
//   · `/api/updates` 与 `/api/transfer*`：`server.py:auth`，走 common::peek。

pub mod download;
pub mod pluginpack;
pub mod pushupd;
pub mod transfer;
pub mod update;
pub mod updater;
pub mod util;

use crate::paths::Ctx;
use crate::routes::common::CtxState;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use axum::Json;
use serde_json::{json, Value};
use std::sync::Arc;

const PUSH_MAX_BYTES: u64 = 64 * 1048576 * 2;

pub fn router() -> Router<Arc<Ctx>> {
    Router::new()
        // ---- 自更新（9 条）
        .route("/api/update/check", post(update::check))
        .route("/api/update/mode", post(update::mode))
        .route("/api/update/state", get(update::state))
        .route("/api/update/download", post(download::download))
        .route("/api/update/apply", post(update::apply))
        .route("/api/update/push", post(push_route))
        .route("/api/update/status", get(status_route))
        .route("/api/update/rollback", post(rollback_route))
        .route("/api/updates", get(update::updates))
        // ---- 传输队列（6 条）
        .route("/api/transfer", get(transfer::list))
        .route("/api/transfer/add", post(transfer::add_route))
        .route("/api/transfer/upload", post(transfer::upload))
        .route("/api/transfer/repush", post(transfer::repush))
        .route("/api/transfer/del", post(transfer::del))
        .route("/api/transfer/file", get(transfer::file))
        // axum 默认 2MB：`/api/update/push` 允许 128MB 的 base64 JSON，
        // `/api/transfer/upload` 单文件上限是配置的 max_upload_mb（默认 500MB），
        // 一次还能多选。不放宽这一层，请求会在进 handler 前就被 413。
        .layer(DefaultBodyLimit::max(1100 * 1024 * 1024))
}

/// `POST /api/update/push`：主机把程序包推过来。
async fn push_route(
    State(ctx): CtxState,
    headers: HeaderMap,
    raw: Bytes,
) -> Response {
    util::agent_gate();
    let clen = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("0");
    if let Ok(n) = clen.parse::<u64>() {
        if n > PUSH_MAX_BYTES {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(json!({"ok": false, "error": "包太大（超过 128MB）"})),
            )
                .into_response();
        }
    }
    let body: Value = if raw.is_empty() {
        return util::json_status(400, json!({"ok": false, "error": "body 不是 JSON"}));
    } else {
        match serde_json::from_slice(&raw) {
            Ok(v) => v,
            Err(_) => {
                return util::json_status(400, json!({"ok": false, "error": "body 不是 JSON"}))
            }
        }
    };
    if !body.is_object() {
        return util::json_status(400, json!({"ok": false, "error": "body 要是对象"}));
    }
    match pushupd::push(&ctx, &body) {
        Ok(res) => {
            if util::py_truthy(res.get("ok")) {
                Json(res).into_response()
            } else {
                util::json_status(400, res)
            }
        }
        Err(pushupd::PushError::TooBig(msg)) => {
            util::json_status(413, json!({"ok": false, "error": msg}))
        }
        Err(pushupd::PushError::Other(msg)) => {
            util::json_status(500, json!({"ok": false, "error": format!("收包异常：{msg}")}))
        }
    }
}

/// `GET /api/update/status`：推送更新落到哪一版、在哪个目录、历史包。
async fn status_route(State(ctx): CtxState, _headers: HeaderMap, _uri: Uri) -> Response {
    util::agent_gate();
    Json(pushupd::status(&ctx)).into_response()
}

/// `POST /api/update/rollback`：回到上一版。
async fn rollback_route(State(ctx): CtxState, _headers: HeaderMap, raw: Bytes) -> Response {
    util::agent_gate();
    let mut restart = true;
    if !raw.is_empty() {
        if let Ok(v) = serde_json::from_slice::<Value>(&raw) {
            if let Value::Object(m) = &v {
                if let Some(r) = m.get("restart") {
                    restart = util::py_truthy(Some(r));
                }
            }
        }
    }
    let res = pushupd::rollback(&ctx, restart);
    if util::py_truthy(res.get("ok")) {
        Json(res).into_response()
    } else {
        util::json_status(400, res)
    }
}
