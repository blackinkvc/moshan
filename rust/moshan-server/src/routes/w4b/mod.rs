// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：泳道 w4b 的路由装配 —— AI 工作计划 / 每日简报 / 待确认 / 笔记，共 13 条。
// 不做：不改 main.rs / routes/mod.rs / 共享模块（auth、paths、storage、jsonio、
//       ledger、common、w1..w4）—— 那些是所有人共用的地基，改坏了全线停摆。
//
// 顺序即契约：先鉴权（common::peek）→ 再解析查询/请求体 → 再干活。
// 这些接口全是 auth 保护，鉴权不过一律 403，别让 422 抢在 403 前面。

pub(crate) mod brief;
mod dashrs;
mod hostwork;
mod notes;
pub(crate) mod pending;
mod plan;
mod util;

use crate::paths::Ctx;
use crate::routes::common::{self, body_obj, peek, CtxState};
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

pub fn router() -> Router<Arc<Ctx>> {
    Router::new()
        // ---- AI 工作计划
        .route("/api/plan", get(plan_read))
        .route("/api/plan/act", post(plan_act))
        .route("/api/plan/triage", post(plan_triage))
        .route("/api/plan/assign", post(plan_assign))
        // ---- 每日简报
        .route("/api/brief", get(brief_get))
        .route("/api/brief/run", post(brief_run))
        .route("/api/brief/push", post(brief_push))
        // ---- 个人工作台笔记
        .route("/api/notes", get(notes_read).post(notes_write))
        .route("/api/notes/add", post(notes_add))
        // ---- 待确认（存图 / 文章）
        .route("/api/pending", get(pending_read))
        .route("/api/pending/answer", post(pending_answer))
        .route("/api/pending/resolve", post(pending_resolve))
}

/// FastAPI 缺必填 body 时的 422 形状（与 w4 的 missing_query 同一套文案）。
fn missing_body() -> Response {
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

/// body 是合法 JSON 但不是对象：pydantic 报 `dict_type`，input 原样带回去。
fn dict_type_body(v: &Value) -> Response {
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

/// JSON 本身坏掉：pydantic 报 `json_invalid`，loc 是出错字符位置。
/// CPython 的解析器文案与 serde 不同，这里按最常见的几类做对照，
/// 冷门坏法（多行、转义）会落到 "Expecting value"。
fn json_invalid_body(e: &serde_json::Error) -> Response {
    let text = e.to_string();
    let msg = if text.contains("key must be a string") || text.contains("EOF while parsing an object") {
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

/// `body: dict` 参数：缺体 / 非对象 / 坏 JSON 各自 422，与 FastAPI 同口径。
///
/// 注意这里收的是原始字节，不是 `Option<Json<Value>>` —— axum 的 Json 提取器
/// 遇到空体会在进处理函数之前就回 400，抢在 FastAPI 的 422 前面。
fn parse_body(raw: &Bytes) -> Result<Value, Response> {
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

/// FastAPI 未捕获异常时的默认 500：纯文本 `Internal Server Error`。
fn internal_error() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        [(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "Internal Server Error",
    )
        .into_response()
}

/// `date.fromisoformat` 能认的形态（空串 = 今天，交给下游）。
fn is_iso_day(day: &str) -> bool {
    if day.is_empty() {
        return true;
    }
    let p = |f: &str| chrono::NaiveDate::parse_from_str(day, f).is_ok();
    p("%Y-%m-%d")
        || (day.len() == 8 && day.bytes().all(|b| b.is_ascii_digit()) && p("%Y%m%d"))
}

/// FastAPI `int` 查询参数非法时的 422 形状。
fn bad_query(field: &str, input: &str) -> Response {
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

// ---------------------------------------------------------------- 计划

async fn plan_read(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let ctx2 = ctx.clone();
    let out = tokio::task::spawn_blocking(move || plan::payload_ctx(&ctx2))
        .await
        .unwrap_or_else(|_| {
            json!({"ok": false, "error": "RuntimeError: 计划读取失败", "projects": [], "inbox": []})
        });
    Json(out).into_response()
}

async fn plan_act(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let body = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let ctx2 = ctx.clone();
    let out = tokio::task::spawn_blocking(move || {
        let root = plan::root_of(&ctx2);
        let mut r = plan::act(&ctx2, &root, &body);
        if plan::is_ok_view(&r) {
            if let Some(o) = r.as_object_mut() {
                // Python 的 dict.pop 保序；serde_json 开了 preserve_order 后
                // `Map::remove` 是 swap_remove（把末位搬到这个位置），键序会变。
                // 必须用 shift_remove，否则 import 的响应会以 parsed 打头。
                o.shift_remove("doc");
                let view = plan::payload(&ctx2, &root);
                o.insert("view".to_string(), view);
            }
        }
        r
    })
    .await
    .unwrap_or_else(|_| json!({"ok": false, "error": "RuntimeError: 计划写入失败"}));
    Json(out).into_response()
}

async fn plan_triage(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let b = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let ids: Vec<String> = b
        .get("ids")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .map(util::pystr)
                .filter(|s| !s.trim().is_empty())
                .collect()
        })
        .unwrap_or_default();
    let root = plan::root_of(&ctx);
    let mut items = plan::candidates(&root, &ids);
    let mut out = json!({"ok": true, "items": items.clone(), "ai": false, "note": ""});
    if !util::truthy(util::get(&b, "ai")) {
        return Json(out).into_response();
    }
    let weak: Vec<Value> = items
        .iter()
        .filter(|x| util::as_f64(util::get(util::get(x, "hint"), "score")).unwrap_or(0.0) < 0.45)
        .cloned()
        .collect();
    if weak.is_empty() {
        if let Some(o) = out.as_object_mut() {
            o.insert("note".into(), json!("代码已经能归位，不用问模型"));
        }
        return Json(out).into_response();
    }
    let (ran, model, note) = plan::ai_assign(&ctx, &root, &mut items).await;
    if let Some(o) = out.as_object_mut() {
        o.insert("items".into(), json!(items));
        o.insert("ai".into(), json!(ran));
        o.insert("note".into(), json!(note));
        if ran {
            o.insert("model".into(), json!(model));
        }
    }
    Json(out).into_response()
}

async fn plan_assign(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let b = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let items: Vec<Value> = b
        .get("items")
        .and_then(|x| x.as_array())
        .map(|a| a.iter().filter(|x| x.is_object()).cloned().collect())
        .unwrap_or_default();
    if items.is_empty() {
        return Json(json!({"ok": false, "error": "没有要落库的条目"})).into_response();
    }
    let ctx2 = ctx.clone();
    let out = tokio::task::spawn_blocking(move || {
        let root = plan::root_of(&ctx2);
        let mut r = plan::apply_ai_assign(&ctx2, &root, &items);
        let view = plan::payload(&ctx2, &root);
        if let Some(o) = r.as_object_mut() {
            o.insert("view".to_string(), view);
        }
        r
    })
    .await
    .unwrap_or_else(|_| json!({"ok": false, "error": "RuntimeError: 落库失败"}));
    Json(out).into_response()
}

// ---------------------------------------------------------------- 简报

async fn brief_get(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let day = q.get("day").cloned().unwrap_or_default();
    // Python 侧 build() 里 date.fromisoformat(day) 抛 ValueError，FastAPI 回 500 纯文本
    if !is_iso_day(&day) {
        return internal_error();
    }
    let force_raw = q.get("force").cloned().unwrap_or_default();
    let force = if force_raw.trim().is_empty() {
        0
    } else {
        match force_raw.trim().parse::<i64>() {
            Ok(v) => v,
            Err(_) => return bad_query("force", &force_raw),
        }
    };
    let ctx2 = ctx.clone();
    let out = tokio::task::spawn_blocking(move || {
        let mut data = brief::build(&ctx2, &day, force != 0);
        let days = brief::days_for(&ctx2, 14);
        if let Some(o) = data.as_object_mut() {
            o.insert("days".to_string(), json!(days));
        }
        data
    })
    .await
    .unwrap_or_else(|_| json!({"ok": false, "error": "RuntimeError: 简报生成失败"}));
    Json(out).into_response()
}

async fn brief_run(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let b = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let day = util::s_or(util::get(&b, "day"));
    if !is_iso_day(&day) {
        return internal_error();
    }
    let push = util::truthy(util::get(&b, "push"));
    let ctx2 = ctx.clone();
    let out = tokio::task::spawn_blocking(move || {
        let mut data = brief::build(&ctx2, &day, true);
        let days = brief::days_for(&ctx2, 14);
        let snapshot = data.clone();
        if let Some(o) = data.as_object_mut() {
            o.insert("days".to_string(), json!(days));
            if push {
                let p = brief::push(&snapshot, false);
                o.insert("push".to_string(), p);
            }
        }
        data
    })
    .await
    .unwrap_or_else(|_| json!({"ok": false, "error": "RuntimeError: 简报重建失败"}));
    Json(out).into_response()
}

async fn brief_push(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let b = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let day = util::s_or(util::get(&b, "day"));
    if !is_iso_day(&day) {
        return internal_error();
    }
    let ctx2 = ctx.clone();
    let out = tokio::task::spawn_blocking(move || {
        let data = brief::build(&ctx2, &day, false);
        brief::push(&data, false)
    })
    .await
    .unwrap_or_else(|_| json!({"ok": false, "error": "RuntimeError: 推送失败"}));
    Json(out).into_response()
}

// ---------------------------------------------------------------- 笔记

async fn notes_read(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let ctx2 = ctx.clone();
    let out = tokio::task::spawn_blocking(move || notes::read_notes(&ctx2))
        .await
        .unwrap_or_else(|_| {
            json!({"todos": [], "events": [], "memo": [], "diary": {}, "updated_at": "",
                   "error": "RuntimeError: handler panicked"})
        });
    Json(out).into_response()
}

async fn notes_write(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    // FastAPI 的 `body: dict` 是必填：缺体/非对象一律 422
    let body = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let ctx2 = ctx.clone();
    let out = tokio::task::spawn_blocking(move || notes::write_notes(&ctx2, &body))
        .await
        .unwrap_or_else(|_| json!({"ok": false, "error": "RuntimeError: 保存失败"}));
    Json(out).into_response()
}

async fn notes_add(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let b = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let ctx2 = ctx.clone();
    let out = tokio::task::spawn_blocking(move || notes::add_note(&ctx2, &b))
        .await
        .unwrap_or_else(|_| json!({"ok": false, "error": "RuntimeError: 追加失败"}));
    Json(out).into_response()
}

// ---------------------------------------------------------------- 待确认

async fn pending_read(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let ctx2 = ctx.clone();
    let out = tokio::task::spawn_blocking(move || {
        let items = pending::open_items(&ctx2);
        let n = items.len();
        json!({"count": n, "items": items})
    })
    .await
    .unwrap_or_else(|_| json!({"count": 0, "items": [], "error": "RuntimeError: handler panicked"}));
    Json(out).into_response()
}

async fn pending_answer(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let b = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let text = util::s_or(util::get(&b, "text")).trim().to_string();
    if text.is_empty() {
        return common::http_err(400, "缺少内容");
    }
    let by = {
        let v = util::s_or(util::get(&b, "by"));
        if v.is_empty() {
            "回复".to_string()
        } else {
            v
        }
    };
    let ctx2 = ctx.clone();
    let out = tokio::task::spawn_blocking(move || pending::answer(&ctx2, &text, &by))
        .await
        .unwrap_or_else(|_| json!({"matched": false, "reason": "not_an_answer"}));
    Json(out).into_response()
}

async fn pending_resolve(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let p = match parse_body(&body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let pid = util::s_or(util::get(&p, "id"));
    let mode = util::s_or(util::get(&p, "mode"));
    if pid.is_empty() {
        return common::http_err(400, "缺少 id");
    }
    let by = {
        let v = util::s_or(util::get(&p, "by"));
        if v.is_empty() {
            "选".to_string()
        } else {
            v
        }
    };
    let ctx2 = ctx.clone();
    let out = tokio::task::spawn_blocking(move || pending::resolve(&ctx2, &pid, &mode, &by))
        .await
        .unwrap_or_else(|_| json!({"ok": false, "error": "RuntimeError: 处理失败"}));
    if !util::truthy(util::get(&out, "ok")) {
        let msg = util::s_or(util::get(&out, "error"));
        let msg = if msg.is_empty() {
            "没处理成".to_string()
        } else {
            msg
        };
        return common::http_err(404, &msg);
    }
    Json(out).into_response()
}

#[allow(dead_code)]
fn _body_obj_kept(v: Option<Json<Value>>) -> Value {
    body_obj(v)
}
