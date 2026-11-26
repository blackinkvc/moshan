// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：桌面入口。真实逻辑在 lib.rs，这里只做 Windows 的窗口子系统开关。
// 不做：不放任何业务代码——移动端编译时 main 会被替换成 Kotlin 入口。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    moshan_desktop_lib::run()
}
