// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：泳道 w5d —— 后台调度器（心跳/采样/巡检/OCR/趋势/报告）+ 手动触发路由。
// 不做：不做采集口径本身，数据取自 crate::hostwork；运行态统一放 crate::live。
//
// 路由只注册别处没有的两条：
//   POST /api/trends/run  —— w5a 只做了 GET /api/trends，写族一直转发给 Python；
//   GET  /api/heartbeat   —— /api/heartbeat/run 在 w7a，只读快照归这里。
// OCR 的 GET/POST 由 w7a 注册，重复注册 axum 会 panic，所以不碰。
//
// 调度开关：`MOSHAN_NO_SCHED=1` 时**只关自动循环**，手动路由照常可用；
// /api/ping 不受影响（main.rs 在监听之后才调 spawn，本函数里也不做阻塞初始化）。

pub mod brief_push;
pub mod heartbeat;
pub mod ocr;
pub mod report;
pub mod trends;
pub mod util;

use crate::paths::Ctx;
use axum::routing::{get, post};
use axum::Router;
use std::sync::Arc;

pub fn router() -> Router<Arc<Ctx>> {
    Router::new()
        .route("/api/trends/run", post(trends::run_route))
        .route("/api/heartbeat", get(heartbeat::state_route))
}

fn env_flag(key: &str) -> bool {
    match std::env::var(key) {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            !v.is_empty() && v != "0" && v != "false" && v != "no"
        }
        Err(_) => false,
    }
}

/// `paths.data_root()` 的 Rust 版：allowed_roots[0]，退化到资料库。
fn data_root(ctx: &Ctx) -> std::path::PathBuf {
    let p = util::data_root(&ctx.cfg);
    if p == std::path::Path::new(".") || p.as_os_str().is_empty() {
        ctx.library_dir.clone()
    } else {
        p
    }
}

/// 启动后台循环。监听端口已经在跑之后才调用，慢启动不能拖住 `/api/ping`。
pub fn spawn(ctx: Arc<Ctx>) {
    if env_flag("MOSHAN_NO_SCHED") {
        eprintln!(
            "[moshan-rs] MOSHAN_NO_SCHED=1：已关闭 心跳/报告采样/OCR/趋势/进程采样 自动循环（手动路由仍可用）"
        );
        return;
    }

    // 顺序照 Python 的 startup：心跳 -> OCR -> 报告 -> 趋势；进程采样是 hostwork 的
    // 常驻线程，这里只是把它拉起来（它自己保持 10 秒节奏与 进程.jsonl 落盘）。
    heartbeat::start(ctx.clone());
    ocr::start(ctx.clone());
    report::start(ctx.clone());
    trends::start(ctx.clone());

    // 简报的定时推送（8–11 点、60 秒一跳）与待定问句的补发（2 秒一跳）。
    // 两者都只碰微信网关，出错自己吞；`MOSHAN_NO_PUSH` 只关简报那条。
    #[cfg(feature = "lane_w4b")]
    {
        brief_push::start(ctx.clone());
        crate::routes::w4b::pending::start(ctx.clone());
    }

    let own = data_root(&ctx).join("_本机工作");
    // start_at 会同步采第一笔（几百毫秒），放到线程里，别让 serve 等它。
    std::thread::Builder::new()
        .name("moshan-sampler".into())
        .spawn(move || {
            crate::hostwork::procs_state::start_at(own);
        })
        .ok();
}
