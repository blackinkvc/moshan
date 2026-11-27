// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：会话/工作量的公共采集层（对齐 moshan/hostwork.py）。
// 放在 crate 顶层而不是某条泳道里，是因为**多条泳道都要读同一份数**：
// `/api/overview`、`/api/pulse`、`/api/tasks`、`/api/agent/brief`、
// `/api/agent/import/scan` 全从这里取。谁各写一份，数字就一定对不上。
// 不做：不做 HTTP 装配；路由在各泳道里调这里的函数。
//
// 分文件（对齐 Python 的模块边界）：
//   util / platform / procs / disk  —— Python 口径的小工具与本机采样（从 w5a 抄进来，
//                                    跨泳道不能直接引用，保持同口径）
//   sessions                        —— 会话流水解析层（rounds / 每轮请求）
//   agg                             —— 会话增量扫描 + 日聚合缓存（hostwork._scan_file）
//   hourly                          —— 每小时 token 的轻量账
//   machine / spec                  —— 机器指标与机器身份证
//   quota / pocket / price          —— 额度中断、口袋流水、折算
//   clients / overview / pulse / tasks / trae —— 四张视图
//   cherry / billing / prompts / llm —— overview 的四段装饰

pub mod agg;
pub mod billing;
pub mod cherry;
pub mod clients;
pub mod disk;
pub mod hourly;
pub mod machine;
pub mod overview;
pub mod platform;
pub mod pocket;
pub mod price;
pub mod procnames;
pub mod procs;
pub mod procs_state;
pub mod prompts;
pub mod pulse;
pub mod quota;
pub mod sessions;
pub mod spec;
pub mod tasks;
pub mod trae;
pub mod util;


use crate::paths::Ctx;
use serde_json::Value;

/// `/api/rounds` 与 `brief.recent_work` 共用的一份逐轮记录。
/// 带 8 秒内存缓存（与 Python `_ROUNDS_CACHE` 一致），today 档从 7 天那份切。
pub fn rounds_items(ctx: &Ctx, range_key: &str) -> Value {
    tasks::rounds_items_impl(ctx, range_key)
}

/// `/api/overview`
pub fn overview(ctx: &Ctx, range_key: &str, fresh: bool) -> Value {
    overview::gated(ctx, range_key, fresh)
}

/// `/api/pulse`
pub fn pulse(ctx: &Ctx, days: i64, scope: &str) -> Value {
    pulse::pulse(ctx, days, scope)
}

/// `/api/tasks`
pub fn tasks_view(ctx: &Ctx, range_key: &str) -> Value {
    tasks::tasks_view(ctx, range_key)
}

/// `agent_import._hw_stats` 要的「谁在干活」全量（days=0 即全部历史）。
pub fn clients_view(ctx: &Ctx, days: i64) -> Vec<Value> {
    clients::clients_view(ctx, days)
}
