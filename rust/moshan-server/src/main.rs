// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：装配服务端。已迁路由由 Rust 直接应答，其余原样转发给上游 Python。
// 不做：不建目录、不初始化数据根、不启动后台调度（这些随波次逐个接手，
//       W1 阶段保持"零副作用"，才能与 Python 逐字节对拍）。

mod auth;
mod boot;
mod hostwork;
mod jsonio;
mod ledger;
mod live;
mod paths;
mod proxy;
mod routes;
mod storage;

use axum::routing::{get, post};
use axum::Router;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    // 端口三种来源，优先级从高到低：
    //   1. `MOSHAN_RS_PORT`：测试/多实例/对拍显式指定，此时**不动配置**；
    //   2. `MOSHAN_NO_BOOT=1`：只读配置里的 local_port，缺省 8400；
    //   3. 自举：建目录、发令牌、选空闲端口、写回 config.json（独立运行的默认路径）。
    //
    // 自举必须跑在 `Ctx::load()` 之前：它写的就是 `Ctx` 要读的那份配置。
    let explicit: Option<u16> = std::env::var("MOSHAN_RS_PORT").ok().and_then(|v| v.parse().ok());
    let booted = if explicit.is_none() && !env_flag("MOSHAN_NO_BOOT") {
        Some(boot::prepare())
    } else {
        None
    };

    let mut loaded = paths::Ctx::load();
    let port: u16 = explicit
        .or(booted)
        .or_else(|| {
            loaded.cfg
                .get("local_port")
                .and_then(|v| v.as_u64())
                .map(|v| v as u16)
        })
        .unwrap_or(8400);

    // 自举时配置里的 local_port 就是自己 —— 若还拿它当上游，
    // 未迁路由会转发给自己，一路递归把线程吃光。这里把「上游=自己」直接作废。
    if let Some(up) = loaded.upstream.clone() {
        let me = format!("127.0.0.1:{port}");
        if up.contains(&me) {
            eprintln!("[moshan-rs] 上游指向自身（{up}），已禁用转发");
            loaded.upstream = None;
        }
    }
    let ctx = Arc::new(loaded);

    // 泳道全关时 `mut` 用不上，这是迁移期的正常状态，不必报警。
    #[allow(unused_mut)]
    let mut app = Router::new()
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
        .route("/api/upload", post(routes::w3::upload))
        // ---- W4：记账账本（6 条；上传那两条等 multipart 基建）
        .route("/api/ledger", get(routes::w4::all))
        .route("/api/ledger/item", get(routes::w4::one))
        .route("/api/ledger/img", get(routes::w4::img))
        .route("/api/ledger/update", post(routes::w4::update))
        .route("/api/ledger/remove", post(routes::w4::remove));

    // ---- 泳道：分批施工时各开一个特性，单独编译单独对拍。
    //      全部迁完后把这些 cfg 拆掉、直接 merge 即可。
    #[cfg(feature = "lane_u")]
    {
        app = app.merge(routes::u::router());
    }
    #[cfg(feature = "lane_w4b")]
    {
        app = app.merge(routes::w4b::router());
    }
    #[cfg(feature = "lane_w4c")]
    {
        app = app.merge(routes::w4c::router());
    }
    #[cfg(feature = "lane_w5a")]
    {
        app = app.merge(routes::w5a::router());
    }
    #[cfg(feature = "lane_w5b")]
    {
        app = app.merge(routes::w5b::router());
    }
    #[cfg(feature = "lane_w6a")]
    {
        app = app.merge(routes::w6a::router());
    }
    #[cfg(feature = "lane_w6b")]
    {
        app = app.merge(routes::w6b::router());
    }
    #[cfg(feature = "lane_w5d")]
    {
        app = app.merge(routes::w5d::router());
    }
    #[cfg(feature = "lane_w5e")]
    {
        app = app.merge(routes::w5e::router());
    }
    #[cfg(feature = "lane_w7a")]
    {
        app = app.merge(routes::w7a::router());
    }
    #[cfg(feature = "lane_w7b")]
    {
        app = app.merge(routes::w7b::router());
    }

    // ---- 角色开关：有两套接口在 Python 里本来就不是主服务挂的，
    //      默认跟着 Python 保持「不挂」，免得迁完之后 404 变 200。
    let role = std::env::var("MOSHAN_ROLE").unwrap_or_default();
    let nest = role == "nest" || env_flag("MOSHAN_FLEET");
    let gray = env_flag("MOSHAN_GRAY");
    #[cfg(feature = "lane_w6a")]
    if nest {
        app = app.merge(routes::w6a::fleet_router());
    }
    #[cfg(feature = "lane_w7a")]
    if gray {
        app = app.merge(routes::w7a::gray_router());
    }
    #[cfg(not(feature = "lane_w6a"))]
    let _ = nest;
    #[cfg(not(feature = "lane_w7a"))]
    let _ = gray;

    // ---- 其余未迁路由：镜像给 Python，行为完全不变
    let app = app.fallback(proxy::forward).with_state(ctx.clone());

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

    // 后台调度器：心跳/采样/巡检/OCR 各自按 Python 的节奏跑。
    // 放在监听之后起，是为了让 `/api/ping` 立刻可用（探针不看调度器）。
    #[cfg(feature = "lane_w5d")]
    routes::w5d::spawn(ctx.clone());

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .expect("server");
}

/// `MOSHAN_*` 这类开关只认「非空且不是 0/false」——与 Python 侧读环境变量的口径一致。
fn env_flag(key: &str) -> bool {
    match std::env::var(key) {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            !v.is_empty() && v != "0" && v != "false" && v != "no"
        }
        Err(_) => false,
    }
}
