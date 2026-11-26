// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：泳道 w6a 的路由装配（虫巢 22 条 + apps 7 条，共 29 条）。
// 不做：不改 main.rs / routes/mod.rs / 共享模块 —— 那些是所有人共用的地基。

pub mod apps;
pub mod fleet;
pub mod perms;
pub mod util;

use crate::paths::Ctx;
use axum::extract::{DefaultBodyLimit, Request};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use std::sync::Arc;

/// `/api/fleet/report` 的体积闸门。
///
/// 位置和 Python 的 `@app.middleware("http")` 一样：在**读 body 之前**按
/// Content-Length 拦。写在 handler 里就变成「先把 1MB 读完再拒绝」——
/// 响应字节一样，但连接行为不同（Python 那边是没读完就回话 + Connection: close）。
async fn cap_report(req: Request, next: Next) -> Response {
    if req.uri().path() == "/api/fleet/report" {
        if let Some(cl) = perms::content_length(req.headers()) {
            if cl > perms::MAX_REPORT_BYTES {
                let mut res = (
                    axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                    axum::Json(serde_json::json!({
                        "ok": false,
                        "error": format!("上报体积超过 {} 字节", perms::MAX_REPORT_BYTES),
                    })),
                )
                    .into_response();
                res.headers_mut().insert(
                    axum::http::header::CONNECTION,
                    axum::http::HeaderValue::from_static("close"),
                );
                return res;
            }
        }
    }
    next.run(req).await
}

/// apps 七条：主服务挂的就是这一组（Python 的 `server.py` 里 `/api/apps` 是挂着的）。
pub fn router() -> Router<Arc<Ctx>> {
    Router::new()
        // ---- 工作台入口卡片（主令牌，三通道）
        .route("/api/apps", get(apps::list))
        .route("/api/apps/add", post(apps::add))
        .route("/api/apps/launch", post(apps::launch))
        .route("/api/apps/preview", post(apps::preview))
        .route("/api/apps/update", post(apps::update))
        .route("/api/apps/remove", post(apps::remove))
        .route("/api/apps/icon", get(apps::icon))
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
}

/// 虫巢 22 条：**主服务不挂**。Python 的 `server.py` 里 `fleetmod.register()` 早已
/// 被注释，虫巢是独立服务（:`8781`）。主服务若默认挂上，`/api/fleet/*` 会从 404
/// 变成 200 —— 那不是「迁完了」，是把一套独立服务塞进另一个进程。
/// 要跑虫巢角色时用 `MOSHAN_ROLE=nest` 启动，由 main.rs 决定挂不挂。
pub fn fleet_router() -> Router<Arc<Ctx>> {
    Router::new()
        // ---- 虫巢：agent 侧（只认 fleet_token）
        .route("/api/fleet/report", post(fleet::report_route))
        .route("/api/fleet/poll", post(fleet::poll_route))
        .route("/api/fleet/result", post(fleet::result_route))
        .route("/api/fleet/software", post(fleet::software_report_route))
        .route("/api/fleet/task/progress", post(fleet::task_progress_route))
        .route("/api/fleet/task/result", post(fleet::task_result_route))
        .route("/api/fleet/agent/latest", get(fleet::agent_latest_route))
        .route(
            "/api/fleet/agent/download/{version}",
            get(fleet::agent_download_route),
        )
        .route("/api/fleet/pkg/{name}", get(fleet::pkg_get_route))
        .route("/api/fleet/prune", post(fleet::prune_route))
        // ---- 虫巢：主端侧（本机免令牌，远端要 view_token）
        .route("/api/fleet/unbind", post(fleet::unbind_route))
        .route("/api/fleet/command", post(fleet::command_route))
        .route("/api/fleet/commands", get(fleet::commands_route))
        .route("/api/fleet/overview", get(fleet::overview_route))
        .route("/api/fleet/access", post(fleet::access_route))
        .route("/api/fleet/software", get(fleet::software_route))
        .route("/api/fleet/task", post(fleet::task_route))
        .route("/api/fleet/tasks", get(fleet::tasks_route))
        .route("/api/fleet/agent/releases", get(fleet::releases_route))
        .route("/api/fleet/pkg/register", post(fleet::pkg_register_route))
        .route("/api/fleet/pkg", get(fleet::pkg_list_route))
        .route("/api/fleet/agent/publish", post(fleet::publish_route))
        // 上报体积闸门是**中间件位置**的：超过 1MB 要在解析 body 之前拦，
        // 所以这一层必须比 axum 默认的 2MB 宽，真正的 413 由 cap_report 自己回
        // （文案与 Connection: close 都照 Python 的，axum 自带 413 不是那个形状）。
        .layer(middleware::from_fn(cap_report))
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
}
