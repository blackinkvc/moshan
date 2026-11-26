// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：把尚未迁移的路由原样转发给上游（迁移期 = Python 服务）。
// 不做：不改写响应、不补默认值、不记录业务字段。转发必须"透明"，
//       否则对拍时分不清差异是迁移引入的还是转发引入的。

use crate::paths::Ctx;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

/// 逐跳首部不能转发：它们描述的是"这一段连接"，转过去会让上游按错误的帧格式解析。
fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name,
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

pub async fn forward(State(ctx): State<Arc<Ctx>>, req: Request) -> Response {
    let Some(base) = ctx.upstream.clone() else {
        // 全部迁完之后这里就是"路由不存在"，与 FastAPI 的 404 形状保持一致。
        return (
            StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({ "detail": "Not Found" })),
        )
            .into_response();
    };

    let (parts, body) = req.into_parts();
    let pq = parts
        .uri
        .path_and_query()
        .map(|x| x.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    let url = format!("{base}{pq}");

    let method = match reqwest::Method::from_bytes(parts.method.as_str().as_bytes()) {
        Ok(m) => m,
        Err(_) => return (StatusCode::BAD_REQUEST, "bad method").into_response(),
    };

    let mut upstream_headers = reqwest::header::HeaderMap::new();
    for (k, v) in parts.headers.iter() {
        let name = k.as_str().to_ascii_lowercase();
        if is_hop_by_hop(&name) || name == "host" {
            continue;
        }
        if let (Ok(n), Ok(val)) = (
            reqwest::header::HeaderName::from_bytes(k.as_str().as_bytes()),
            reqwest::header::HeaderValue::from_bytes(v.as_bytes()),
        ) {
            upstream_headers.append(n, val);
        }
    }

    let client = reqwest::Client::builder()
        .build()
        .expect("reqwest client");
    let upstream_body = reqwest::Body::wrap_stream(body.into_data_stream());
    let sent = client
        .request(method, &url)
        .headers(upstream_headers)
        .body(upstream_body)
        .send()
        .await;

    let resp = match sent {
        Ok(r) => r,
        Err(e) => {
            // 上游不可用要让调用方看见原因，别伪装成 200 空体——前端会当成"没有数据"。
            return (
                StatusCode::BAD_GATEWAY,
                axum::Json(serde_json::json!({
                    "ok": false,
                    "error": format!("上游不可用：{e}"),
                    "upstream": base,
                })),
            )
                .into_response();
        }
    };

    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut out = HeaderMap::new();
    for (k, v) in resp.headers().iter() {
        let name = k.as_str().to_ascii_lowercase();
        if is_hop_by_hop(&name) {
            continue;
        }
        if let Ok(val) = HeaderValue::from_bytes(v.as_bytes()) {
            out.append(header::HeaderName::from_bytes(k.as_str().as_bytes()).unwrap(), val);
        }
    }

    let stream = resp.bytes_stream();
    let mut res = Body::from_stream(stream).into_response();
    *res.status_mut() = status;
    *res.headers_mut() = out;
    res
}
