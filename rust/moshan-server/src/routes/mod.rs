// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：声明按波次迁移的路由处理函数。
// `common` 与各波次常驻；泳道（lane_*）是分批施工期的临时分区，
// 每条泳道用同名特性单独编译，互不阻塞 —— 一条泳道写坏了不影响别人干活。
pub mod common;
pub mod w1;
pub mod w2;
pub mod w3;
pub mod w4;

#[cfg(feature = "lane_w4b")]
pub mod w4b;
#[cfg(feature = "lane_w4c")]
pub mod w4c;
#[cfg(feature = "lane_w5a")]
pub mod w5a;
#[cfg(feature = "lane_w5b")]
pub mod w5b;
#[cfg(feature = "lane_w6a")]
pub mod w6a;
#[cfg(feature = "lane_w6b")]
pub mod w6b;
#[cfg(feature = "lane_w7a")]
pub mod w7a;
#[cfg(feature = "lane_w7b")]
pub mod w7b;
#[cfg(feature = "lane_u")]
pub mod u;
