// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：会话/工作量的公共采集层（对齐 moshan/hostwork.py）。
// 放在 crate 顶层而不是某条泳道里，是因为**多条泳道都要读同一份数**：
// `/api/workstat`、`/api/pulse`、`/api/tasks`、`/api/overview`、`/api/agent/brief`、
// `/api/agent/import/scan` 全从这里取。谁各写一份，数字就一定对不上。
// 不做：不做 HTTP 装配；路由在各泳道里调这里的函数。

use crate::paths::Ctx;

/// 空实现占位：尚未接管采集时，返回「没有数据」的统一形态。
/// 迁移期真实的 Python 仍在跑采集，调用方拿到空表也不会崩，只是少几行数。
pub fn empty_note(_ctx: &Ctx) -> bool {
    true
}
