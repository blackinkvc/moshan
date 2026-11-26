// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：装配服务端。已迁路由由 Rust 直接应答，其余原样转发给上游 Python。
// 不做：不建目录、不初始化数据根、不启动后台调度（这些随波次逐个接手，
//       W1 阶段保持"零副作用"，才能与 Python 逐字节对拍）。

mod auth;
mod jsonio;
mod ledger;
mod paths;
mod proxy;
mod routes;
mod storage;

use axum::routing::{get, post};
use axum::Router;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    let ctx = Arc::new(paths::Ctx::load());

    // 默认 8400，刻意避开 Python 的 8321/8322/8323：对拍时两个服务要能同时起。
    let port: u16 = std::env::var("MOSHAN_RS_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8400);

    let app = Router::new()
        // ---- W1：契约地基（4 条，依赖零扇出，响应与运行环境无关）
        .route("/api/ping", get(routes::w1::ping))
        .route("/api/versions", get(routes::w1::versions))
        .route("/api/about", get(routes::w1::about))
        .route("/api/appearance", get(routes::w1::appearance))
        // ---- W2：文件层只读（5 条）。路径语义（链接/越界/面包屑）全在 storage.rs
        .route("/api/files", get(routes::w2::files))
        .route("/api/browse", get(routes::w2::browse))
        .route("/api/usage", get(routes::w2::usage))
        .route("/api/trash", get(routes::w2::trash))
        .route("/api/shares", get(routes::w2::shares))
        // ---- W3：文件层写族（8 条）。判定链顺序与 Python 完全一致
        .route("/api/mkdir", post(routes::w3::mkdir))
        .route("/api/rename", post(routes::w3::rename))
        .route("/api/move", post(routes::w3::move_))
        .route("/api/delete", post(routes::w3::delete))
        .route("/api/trash/restore", post(routes::w3::trash_restore))
        .route("/api/trash/purge", post(routes::w3::trash_purge))
        .route("/api/share", post(routes::w3::share_create))
        .route("/api/shares/{sid}", axum::routing::delete(routes::w3::share_revoke))
        // ---- W4：记账账本（6 条；上传那两条等 multipart 基建）
        .route("/api/ledger", get(routes::w4::all))
        .route("/api/ledger/item", get(routes::w4::one))
        .route("/api/ledger/img", get(routes::w4::img))
        .route("/api/ledger/update", post(routes::w4::update))
        .route("/api/ledger/remove", post(routes::w4::remove))
        // ---- 其余 216 条：镜像给 Python，行为完全不变
        .fallback(proxy::forward)
        .with_state(ctx.clone());

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[moshan-rs] 绑定 {addr} 失败：{e}");
            std::process::exit(1);
        }
    };

    eprintln!(
        "[moshan-rs] 监听 http://{addr}  上游={}  状态目录={}",
        ctx.upstream.as_deref().unwrap_or("（无，未迁路由将 404）"),
        ctx.state_dir.display()
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .expect("server");
}
