mod auth;
mod collector;
mod quota;
mod state;
mod usage;

use state::{AppState, RefreshTx, Shared};
use std::sync::Mutex;
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Manager, WindowEvent,
};

/// 前端首帧拉取一次当前状态（事件之外的兜底）。
#[tauri::command]
fn get_state(state: tauri::State<Shared>) -> AppState {
    state.lock().unwrap().clone()
}

/// 前端点击 ⟳ 触发：唤醒采集线程立即刷新一次（含额度网络拉取）。
#[tauri::command]
fn refresh_now(tx: tauri::State<RefreshTx>) {
    let _ = tx.0.lock().unwrap().send(());
}

/// 切换悬浮窗显示 / 隐藏。
fn toggle_window(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        if w.is_visible().unwrap_or(false) {
            let _ = w.hide();
        } else {
            let _ = w.show();
            let _ = w.set_focus();
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let shared: Shared = state::new_shared();
    let (wake_tx, wake_rx) = std::sync::mpsc::channel::<()>();

    tauri::Builder::default()
        .manage(shared.clone())
        .manage(RefreshTx(Mutex::new(wake_tx)))
        .invoke_handler(tauri::generate_handler![get_state, refresh_now])
        .setup(move |app| {
            // 后台采集线程
            let handle = app.handle().clone();
            std::thread::spawn(move || collector::run(handle, shared, wake_rx));

            // 系统托盘：左键切换显隐；右键菜单显示/隐藏、立即刷新、退出。
            let item_toggle = MenuItem::with_id(app, "toggle", "显示 / 隐藏", true, None::<&str>)?;
            let item_refresh = MenuItem::with_id(app, "refresh", "立即刷新", true, None::<&str>)?;
            let item_quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&item_toggle, &item_refresh, &item_quit])?;

            let mut tray = TrayIconBuilder::new()
                .tooltip("LLMMonitor")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "toggle" => toggle_window(app),
                    "refresh" => {
                        if let Some(tx) = app.try_state::<RefreshTx>() {
                            let _ = tx.0.lock().unwrap().send(());
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        toggle_window(tray.app_handle());
                    }
                });
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            tray.build(app)?;

            Ok(())
        })
        // 点 ✕ / 关闭：不退出，而是隐藏到托盘（后台常驻）。真正退出走托盘“退出”。
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
