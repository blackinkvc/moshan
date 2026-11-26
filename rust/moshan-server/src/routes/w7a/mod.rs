// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：泳道 w7a 的路由装配。本目录由该泳道独占，与其他泳道并行施工。
// 不做：不改 main.rs / routes/mod.rs / 共享模块（auth、paths、storage、jsonio、
//       ledger、common、w1..w4）—— 那些是所有人共用的地基。
//
// 本泳道 24 条：第三方插件 10、插件包 8、灰测 6、OCR 2、心跳 1（手动触发）。
// 按 Python 模块分文件：ext / pack / gray / ocr / heartbeat，Python 口径的工具在 util。

pub mod ext;
pub mod gray;
pub mod heartbeat;
pub mod ocr;
pub mod pack;
pub mod util;

use crate::paths::Ctx;
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use axum::Router;
use std::sync::Arc;

/// 灰测六条：Python 的 `server.py` 从未 import/注册 `gray.register()` ——
/// 这套接口在 Python 里其实是 404。迁完之后默认**继续不挂**，用
/// `MOSHAN_GRAY=1` 启动才启用：灰测是独立机器的活，默认挂进主服务会让
/// `/api/gray/status` 从 404 变 200，前端与监控的判据都会跟着变。
pub fn gray_router() -> Router<Arc<Ctx>> {
    Router::new()
        // ---- 灰测任务机
        .route("/api/gray/status", get(gray::status))
        .route("/api/gray/qr", post(gray::qr))
        .route("/api/gray/run", post(gray::run))
        .route("/api/gray/task/{tid}", get(gray::task))
        .route("/api/gray/stop", post(gray::stop))
        .route("/api/gray/images", post(gray::images))
}

pub fn router() -> Router<Arc<Ctx>> {
    Router::new()
        // ---- 第三方插件（10 条）
        .route("/api/ext/list", get(ext::list))
        .route("/api/ext/preview", post(ext::preview))
        .route("/api/ext/add", post(ext::add))
        .route("/api/ext/install", post(ext::install))
        .route("/api/ext/start", post(ext::start))
        .route("/api/ext/stop", post(ext::stop))
        .route("/api/ext/spec", get(ext::spec_route))
        .route("/api/ext/prompt", get(ext::prompt_route))
        .route("/api/ext/parse", post(ext::parse))
        .route("/api/ext/compose", post(ext::compose))
        // ---- 插件包（8 条）
        .route("/api/pack/list", get(pack::list))
        .route("/api/pack/preview", post(pack::preview))
        .route("/api/pack/install", post(pack::install))
        .route("/api/pack/install-files", post(pack::install_files_api))
        .route("/api/pack/install-url", post(pack::install_url_api))
        .route("/api/pack/install-path", post(pack::install_path_api))
        .route("/api/pack/uninstall", post(pack::uninstall))
        .route("/api/pack/upgrade", post(pack::upgrade))
        // ---- OCR + 心跳（3 条）
        .route("/api/ocr", get(ocr::state_route))
        .route("/api/ocr/run", post(ocr::run_route))
        .route("/api/heartbeat/run", post(heartbeat::run))
        // 上传类：插件包上限 256MB、灰测图片上限由 max_upload_mb（默认 500MB）决定。
        // axum 默认 2MB，不改会在进 handler 前就 413。
        .layer(DefaultBodyLimit::max(600 * 1024 * 1024))
}
