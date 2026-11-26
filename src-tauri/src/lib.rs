// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：组装 Tauri 应用；桌面端负责拉起本机服务并把窗口导航到它。
// 不做：不承载业务逻辑，所有数据仍由 Python 服务提供（网页版与桌面版共用同一端口）。

#[cfg(desktop)]
mod paths;
#[cfg(desktop)]
mod sidecar;

use tauri::Manager;

pub fn run() {
    let app = tauri::Builder::default();

    // 桌面：起服务 → 等到真的应答 → 把窗口导航过去。
    #[cfg(desktop)]
    let app = app.setup(|app| {
        let handle = app.handle().clone();
        std::thread::spawn(move || match sidecar::boot() {
            Ok(url) => {
                if let Some(win) = handle.get_webview_window("main") {
                    if let Ok(parsed) = url.parse::<tauri::Url>() {
                        let _ = win.navigate(parsed);
                    }
                }
            }
            Err(e) => {
                // 失败也要让人看见原因：白窗口什么信息都不给，是最难排查的形态。
                let payload = serde_json::to_string(&e)
                    .unwrap_or_else(|_| r#"{"stage":"boot","message":"启动失败","log_tail":""}"#.to_string());
                if let Some(win) = handle.get_webview_window("main") {
                    let _ = win.eval(&format!(
                        "window.__moshanFail && window.__moshanFail({payload})"
                    ));
                }
            }
        });
        Ok(())
    });

    // 移动端：不拉 sidecar（Android 跑不了桌面二进制），窗口停在 connect.html，
    // 由用户填写已配对的桌面机地址。业务页面由远端服务提供，壳只做窗口。
    #[cfg(mobile)]
    let app = app.setup(|_app| Ok(()));

    app.build(tauri::generate_context!())
        .expect("failed to build Moshan shell")
        .run(|_app, event| {
            #[cfg(desktop)]
            {
                if let tauri::RunEvent::Exit = event {
                    sidecar::shutdown();
                }
            }
            #[cfg(not(desktop))]
            {
                let _ = event;
            }
        });
}
