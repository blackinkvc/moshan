// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：泳道 w5a 的路由装配。本目录由该泳道独占，与其他泳道并行施工。
// 不做：不改 main.rs / routes/mod.rs / 共享模块（auth、paths、storage、jsonio、
//       ledger、common、w1..w4）—— 那些是所有人共用的地基。
//
// 本泳道 16 条：工作台读数（dash/status/sys/model/overview/trends/rounds/pulse/
// workstat/hourly/tasks/sources/plugincards/worklog ×2）。
// 处理函数按 Python 模块分文件：dash / worklog / sessions / hostwork / cards /
// modeluse / procs / disk / heartbeat / images，Python 口径的小工具在 util，
// 平台能力（mach、libproc、statvfs、sqlite）在 platform。
//
// **只挂真迁完的**：没迁的（overview / pulse / workstat / tasks /
// POST /api/trends/run）不注册，请求会走 main.rs 的 fallback 转给 Python 上游。
// 挂一个半成品上去，页面拿到的是看起来对、其实差几行的数 —— 比 404 糟得多。
//
// 已迁（11 / 16 条）：
//   GET  /api/dash  /api/status  /api/sys  /api/model  /api/trends
//   GET  /api/rounds  /api/sources  /api/plugincards  /api/hourly  /api/worklog
//   POST /api/worklog
// 未迁（5 条，走上游）：/api/overview /api/pulse /api/workstat /api/tasks
//   /api/trends/run —— 都压在 hostwork.py（4359 行）那套会话日聚合缓存上
//   （agg.days / act / hw / utt，带 17 版解析缓存），外加 quota / cherry / billing /
//   price 四个子系统；tasks 还要 trae_work.py（1317 行）的会话索引导出。
//   迁一半只会给出「看起来对、其实差几行」的数，比 404 糟得多。

pub mod cards;
pub mod dash;
pub mod disk;
pub mod heartbeat;
pub mod hostwork;
pub mod hourly;
pub mod images;
pub mod modeluse;
pub mod platform;
pub mod procs;
pub mod sessions;
pub mod util;
pub mod worklog;

use crate::paths::Ctx;
use axum::routing::get;
use axum::Router;
use std::sync::Arc;

pub fn router() -> Router<Arc<Ctx>> {
    Router::new()
        // —— 工作台主视图
        .route("/api/dash", get(dash::dash))
        .route("/api/status", get(cards::status))
        .route("/api/sys", get(procs::sys_now))
        .route("/api/model", get(modeluse::model_state))
        // —— 本机工作（采集层）里已迁完的三条
        .route("/api/rounds", get(hostwork::rounds_api))
        .route("/api/sources", get(hostwork::sources_api))
        .route("/api/hourly", get(hourly::hourly_api))
        .route("/api/overview", get(hostwork::overview_api))
        .route("/api/pulse", get(hostwork::pulse_api))
        .route("/api/tasks", get(hostwork::tasks_api))
        // —— AI 新闻
        .route("/api/trends", get(dash::trends_state))
        // —— 插件卡片
        .route("/api/plugincards", get(cards::plugincards))
        // —— 工作记录（读 + 补记）
        .route(
            "/api/worklog",
            get(worklog::worklog_read).post(worklog::worklog_add),
        )
}
