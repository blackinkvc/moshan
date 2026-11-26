// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：泳道 w4b 的路由装配。本目录内的文件由该泳道独占，
//       与其他泳道并行施工，互不修改。
// 不做：不改 main.rs / routes/mod.rs / 共享模块（auth、paths、storage、jsonio、
//       ledger、common、w1..w4）—— 那些是所有人共用的地基，改坏了全线停摆。

use crate::paths::Ctx;
use axum::Router;
use std::sync::Arc;

/// 未接入任何路由时的空表。加路由只需在后面继续 `.route(...)`。
pub fn router() -> Router<Arc<Ctx>> {
    Router::new()
}
