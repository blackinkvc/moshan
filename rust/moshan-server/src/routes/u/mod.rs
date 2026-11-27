// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：泳道 u 的路由装配。本目录内的文件由该泳道独占，
//       与其他泳道并行施工，互不修改。
// 不做：不改 main.rs / routes/mod.rs / 共享模块（auth、paths、storage、jsonio、
//       ledger、common、w1..w4）—— 那些是所有人共用的地基，改坏了全线停摆。
//
// 迁了 25 条（27 条清单里扣掉 /fleet、/gray：Python 侧根本没挂载这两条，
// 见 pages.rs 顶部说明）。

use crate::paths::Ctx;
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use axum::Router;
use std::sync::Arc;

pub mod appearance;
pub mod files;
pub mod pages;
pub mod util;
pub mod welcome;
pub mod zipw;

pub fn router() -> Router<Arc<Ctx>> {
    Router::new()
        // ---- 页面
        .route("/", get(pages::index))
        .route("/dash", get(pages::dash))
        .route("/about", get(pages::about))
        .route("/agent", get(pages::agent))
        .route("/hostwork", get(pages::hostwork))
        .route("/pair", get(pages::pair))
        .route("/docs/{name}", get(pages::docs))
        // ---- 文件 / 媒体
        .route("/api/file", get(files::api_file))
        .route("/api/raw", get(files::api_raw))
        .route("/api/thumb", get(files::api_thumb))
        .route("/api/preview", get(files::api_preview))
        .route("/api/icon", get(files::api_icon))
        .route("/api/zip", get(files::api_zip))
        .route("/__appearance.css", get(files::appearance_css))
        .route("/__appearance-bg.jpg", get(files::appearance_bg))
        .route("/__wall/{name}", get(files::appearance_wall))
        // ---- 外观（写）：设置面板的那三个开关。GET 在 main.rs 的 W1 里，
        //      这里只补 POST；同路径不同方法由 axum 合并，不会互相覆盖。
        //      壁纸那条放宽到 20MB —— 前端 dataURL 是 base64（×1.33），
        //      上限 12MB 的原图到这儿已经十六七 MB，默认 2MB 会先被 413 挡掉。
        .route("/api/appearance", post(files::appearance_save))
        .route(
            "/api/appearance/wall",
            post(files::appearance_wall_upload)
                .layer(DefaultBodyLimit::max(20 * 1024 * 1024)),
        )
        .route("/s/{sid}/{*name}", get(files::public_share))
        .route("/s/{sid}/", get(files::public_share_empty))
        // ---- 新手引导
        .route("/welcome", get(welcome::page))
        .route("/welcome/status", get(welcome::status))
        .route(
            "/welcome/pull",
            get(welcome::pull_get).post(welcome::pull_post),
        )
        .route("/welcome/apply", post(welcome::apply))
        .route("/welcome/finish", post(welcome::finish))
        .route("/welcome/gateway/test", post(welcome::gateway_test))
        .route("/welcome/pickdir", post(welcome::pickdir))
}
