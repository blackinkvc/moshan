// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：泳道 w5e 的路由装配。本目录由该泳道独占。
//
// 本泳道 1 条（对应 moshan/workstat.py）：
//   GET /api/workstat?days=30&day=&fresh=0  —— 每日工作总结那张卡要的全部数据。
//
// 处理顺序照 FastAPI：先鉴权（401/403），再参数校验（422），最后干活。
// 参数校验的形状逐字节照 pydantic v2 的 int_parsing 错误（含 url 那一格）。
use crate::paths::Ctx;
use crate::routes::common::{peek, q, query_map, CtxState};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Json;
use axum::Router;
use serde_json::json;
use std::sync::Arc;

pub mod screen;
pub mod util;
pub mod workstat;

pub fn router() -> Router<Arc<Ctx>> {
    Router::new().route("/api/workstat", get(workstat_api))
}

fn bad_int(field: &str, raw: &str) -> Response {
    let body = json!({
        "detail": [{
            "type": "int_parsing",
            "loc": ["query", field],
            "msg": "Input should be a valid integer, unable to parse string as an integer",
            "input": raw,
            "url": "https://errors.pydantic.dev/2.14/v/int_parsing",
        }]
    });
    (StatusCode::UNPROCESSABLE_ENTITY, Json(body)).into_response()
}

/// pydantic v2 的 `int` 解析口径：去空白、可带正负号、`1_0` 认、`5.0` 认、`5.5` 不认。
fn parse_py_int(s: &str) -> Option<i128> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    let (neg, body) = match t.as_bytes()[0] {
        b'-' => (true, &t[1..]),
        b'+' => (false, &t[1..]),
        _ => (false, t),
    };
    if body.is_empty() {
        return None;
    }
    let (int_part, frac) = match body.split_once('.') {
        Some((a, b)) => (a, Some(b)),
        None => (body, None),
    };
    // 小数部分只允许「全 0」（pydantic 接受 5.0，不接受 5.5）
    if let Some(f) = frac {
        if f.is_empty() || !f.chars().all(|c| c == '0') {
            return None;
        }
    }
    if int_part.is_empty() {
        return None;
    }
    // 下划线只能夹在数字之间
    let chars: Vec<char> = int_part.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if *c == '_' {
            let prev_ok = i > 0 && chars[i - 1].is_ascii_digit();
            let next_ok = i + 1 < chars.len() && chars[i + 1].is_ascii_digit();
            if !prev_ok || !next_ok {
                return None;
            }
        } else if !c.is_ascii_digit() {
            return None;
        }
    }
    let digits: String = chars.into_iter().filter(|c| *c != '_').collect();
    if digits.is_empty() {
        return None;
    }
    let mut v: i128 = 0;
    for c in digits.chars() {
        v = v.saturating_mul(10).saturating_add((c as u8 - b'0') as i128);
    }
    Some(if neg { -v } else { v })
}

fn param_int(uri: &Uri, key: &str, default: i64) -> Result<i64, String> {
    let found = query_map(uri).into_iter().find(|(k, _)| k == key);
    match found {
        None => Ok(default),
        Some((_, raw)) => match parse_py_int(&raw) {
            Some(v) => Ok(v.clamp(i64::MIN as i128, i64::MAX as i128) as i64),
            None => Err(raw),
        },
    }
}

pub async fn workstat_api(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let days = match param_int(&uri, "days", 30) {
        Ok(v) => v,
        Err(raw) => return bad_int("days", &raw),
    };
    let fresh = match param_int(&uri, "fresh", 0) {
        Ok(v) => v,
        Err(raw) => return bad_int("fresh", &raw),
    };
    let day = q(&uri, "day");

    let c = ctx.clone();
    let joined = tokio::task::spawn_blocking(move || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            workstat::view(&c, days, &day, fresh != 0, false)
        }))
    })
    .await;

    match joined {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => {
            let msg = match e.downcast_ref::<String>() {
                Some(s) => s.clone(),
                None => match e.downcast_ref::<&str>() {
                    Some(s) => s.to_string(),
                    None => "统计层崩了".to_string(),
                },
            };
            Json(workstat::error_shape(&format!("PanicError: {}", msg))).into_response()
        }
        Err(e) => Json(workstat::error_shape(&format!("JoinError: {}", e))).into_response(),
    }
}
