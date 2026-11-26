// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：泳道 w5b 的路由装配。本目录由该泳道独占，与其他泳道并行施工。
// 不做：不改 main.rs / routes/mod.rs / 共享模块（auth、paths、storage、jsonio、
//       ledger、common、w1..w4）—— 那些是所有人共用的地基。
//
// 本泳道 13 条：账单 5、巡检 3、流水 3、模型 2。
// 处理函数按 Python 模块分文件：billing / inspect / flow / model，
// Python 口径的小工具在 util。
//
// `/api/worklog` 两条已移交 w5a：左卡的自动采集（pocket.jsonl / 插件 / 会话）
// 属于 dash.py 的采集层，w5b 只能给出空 items。两条路由同时注册会让 axum 合并
// 直接 panic，所以这里刻意不注册。

pub mod billing;
pub mod flow;
pub mod inspect;
pub mod model;
pub mod util;

use crate::paths::Ctx;
use axum::routing::{get, post};
use axum::Router;
use std::sync::Arc;

pub fn router() -> Router<Arc<Ctx>> {
    Router::new()
        // —— 账单：供应商账号 + 余额/月度账单
        .route("/api/billing", get(billing::billing))
        .route("/api/billing/providers", get(billing::providers_api))
        .route("/api/billing/save", post(billing::save_api))
        .route("/api/billing/del", post(billing::del_api))
        .route("/api/billing/pull", post(billing::pull_api))
        // —— 巡检：报告 / 跑一轮 / 进度
        .route("/api/inspect/report", get(inspect::report))
        .route("/api/inspect/run", post(inspect::run_api))
        .route("/api/inspect/state", get(inspect::state_api))
        // —— 工作流水
        .route("/api/flow", get(flow::get_month).post(flow::post_toggle))
        .route("/api/flow/add", post(flow::post_add))
        // —— 模型：本机在跑什么 / 探一个端点
        .route("/api/llm", get(model::llm_api))
        .route("/api/model/test", post(model::model_test))
}
