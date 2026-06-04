mod activity;
mod auth;
mod collector;
mod quota;
mod state;
mod usage;

use activity::{Chip, SharedActivity};
use state::{AppState, RefreshTx, Shared};
use std::sync::Mutex;
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Emitter, Manager, WindowEvent,
};

/// 前端首帧拉取一次额度状态。
#[tauri::command]
fn get_state(state: tauri::State<Shared>) -> AppState {
    state.lock().unwrap().clone()
}

/// 前端首帧拉取一次当前信号灯。
#[tauri::command]
fn get_chips(state: tauri::State<SharedActivity>) -> Vec<Chip> {
    state.lock().unwrap().chips.clone()
}

/// 点 ⟳ 立即刷新额度。
#[tauri::command]
fn refresh_now(tx: tauri::State<RefreshTx>) {
    let _ = tx.0.lock().unwrap().send(());
}

/// 前端 hover 清除某个信号灯：记下 dismiss 标记并立即移除 + 通知前端。
#[tauri::command]
fn dismiss_chip(
    app: tauri::AppHandle,
    state: tauri::State<SharedActivity>,
    id: String,
    trigger: String,
) {
    let chips = {
        let mut st = state.lock().unwrap();
        st.dismissed.insert(id.clone(), trigger.clone());
        st.chips.retain(|c| !(c.id == id && c.trigger == trigger));
        st.chips.clone()
    };
    let _ = app.emit("chips-updated", &chips);
}

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
    let activity_shared: SharedActivity = activity::new_shared();
    let (wake_tx, wake_rx) = std::sync::mpsc::channel::<()>();

    let shared_for_thread = shared.clone();
    let activity_for_thread = activity_shared.clone();

    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .manage(shared)
        .manage(activity_shared)
        .manage(RefreshTx(Mutex::new(wake_tx)))
        .invoke_handler(tauri::generate_handler![
            get_state,
            get_chips,
            refresh_now,
            dismiss_chip
        ])
        .setup(move |app| {
            // 额度采集线程
            let h1 = app.handle().clone();
            std::thread::spawn(move || collector::run(h1, shared_for_thread, wake_rx));
            // 活动检测线程（信号隧道）
            let h2 = app.handle().clone();
            std::thread::spawn(move || activity::run(h2, activity_for_thread));

            // 系统托盘
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
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
