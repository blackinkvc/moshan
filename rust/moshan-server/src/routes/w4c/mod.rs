// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：泳道 w4c 的路由装配。本目录由该泳道独占，与其他泳道并行施工。
// 不做：不改 main.rs / routes/mod.rs / 共享模块（auth、paths、storage、jsonio、
//       ledger、common、w1..w4）—— 那些是所有人共用的地基。

pub mod article;
pub mod dedupe;
pub mod inbox;
pub mod jobs;
pub mod ledgerx;
pub mod link;
pub mod naming;
pub mod py;
pub mod report;

use crate::paths::Ctx;
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use axum::Router;
use std::sync::Arc;

pub fn router() -> Router<Arc<Ctx>> {
    Router::new()
        // ---- 文章（5 条）
        .route("/api/article/fetch", post(article::fetch))
        .route("/api/article/classify", post(article::classify))
        .route("/api/article/list", get(article::list))
        .route("/api/article/status", get(article::status))
        .route("/api/article/topics", get(article::topics))
        // ---- 存图去重（3 条）
        .route("/api/dedupe", get(dedupe::state_route))
        .route("/api/dedupe/run", post(dedupe::run))
        .route("/api/dedupe/restore", post(dedupe::restore))
        // ---- 收件箱（3 条）
        .route("/api/inbox/group", post(inbox::group))
        .route("/api/inbox/name", post(inbox::name))
        .route("/api/inbox/open", get(inbox::open))
        // ---- 存图（2 条）
        .route("/api/link", post(link::create))
        .route("/api/link/status", get(link::status))
        // ---- 使用报告（4 条）
        .route("/api/report", get(report::one))
        .route("/api/report/list", get(report::list))
        .route("/api/report/open", post(report::open))
        .route("/api/report/run", post(report::run_route))
        // ---- 记账扩展（3 条；multipart 上限按 Python 的 64MB 读入上限放宽）
        .route("/api/ledger/upload", post(ledgerx::upload))
        .route("/api/ledger/image", post(ledgerx::image))
        .route("/api/ledger/open", post(ledgerx::open))
        // axum 的默认请求体上限是 2MB，Python 侧 `_read_upload` 的上限是 64MB；
        // 不改这一层的话，几十 MB 的订单截图会在进 handler 之前就被 413。
        .layer(DefaultBodyLimit::max(70 * 1024 * 1024))
}
