// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：泳道 w5a 的路由装配。本目录由该泳道独占，与其他泳道并行施工。
// 不做：不改 main.rs / routes/mod.rs / 共享模块（auth、paths、storage、jsonio、
//       ledger、common、w1..w4）—— 那些是所有人共用的地基。

use crate::paths::Ctx;
use axum::Router;
use std::sync::Arc;

pub fn router() -> Router<Arc<Ctx>> {
    Router::new()
}
