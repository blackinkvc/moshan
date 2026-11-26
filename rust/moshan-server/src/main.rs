// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：装配服务端。已迁路由由 Rust 直接应答，其余原样转发给上游 Python。
// 不做：不建目录、不初始化数据根、不启动后台调度（这些随波次逐个接手，
//       W1 阶段保持"零副作用"，才能与 Python 逐字节对拍）。

mod auth;
mod paths;
mod proxy;
mod routes;

use axum::routing::get;
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
