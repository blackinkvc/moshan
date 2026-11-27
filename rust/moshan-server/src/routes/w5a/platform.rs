// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：w5a 泳道对「平台能力」的门面。
//
// 这里原来是 `hostwork/platform.rs` 的一份**同构副本**（mach/libproc/statvfs 全抄了一遍），
// 两处各修各的，很快就分叉了：w5a 的 `proc_bsdinfo` 少了 pbi_name→pbi_comm 兜底、
// `run_cmd` 没走读线程（大输出会死锁）、`sqlite_rows` 用的是会死锁的那条。
// 第 5 项做三平台时索性只留一份实现，这里改成纯转发 —— 形状不变，调用点一行不用改。
//
// 已确认 w5a 里没有「只在本文件有」的函数：cards / dash / disk / images / modeluse /
// sessions / procs 用到的 mem_total / vm_counts / page_size / cpu_ticks / cpu_count /
// list_pids / proc_taskinfo / proc_bsdinfo / proc_path / port_open / run_cmd_full /
// hostname_short / disk_usage / sqlite_rows 全在共享版里，签名一致。

pub use crate::hostwork::platform::*;
