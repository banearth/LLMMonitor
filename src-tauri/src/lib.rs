mod activity;
mod auth;
mod collector;
mod config;
mod quota;
mod state;
mod usage;

use activity::{Chip, SharedActivity};
use config::{Settings, SharedSettings};
use state::{AppState, RefreshTx, Shared};
use std::sync::Mutex;
use tauri::{
    menu::{CheckMenuItem, Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Emitter, Manager, WindowEvent,
};

// ── 命令 ──────────────────────────────────────────────────────────────

#[tauri::command]
fn get_state(state: tauri::State<Shared>) -> AppState {
    state.lock().unwrap().clone()
}

#[tauri::command]
fn get_chips(state: tauri::State<SharedActivity>) -> Vec<Chip> {
    activity::visible_chips(state.inner())
}

#[tauri::command]
fn refresh_now(tx: tauri::State<RefreshTx>) {
    let _ = tx.0.lock().unwrap().send(());
}

#[tauri::command]
fn dismiss_chip(
    app: tauri::AppHandle,
    state: tauri::State<SharedActivity>,
    id: String,
    trigger: String,
) {
    {
        let mut st = state.lock().unwrap();
        st.dismissed.insert(id.clone(), trigger.clone());
    }
    let chips = activity::visible_chips(state.inner());
    let _ = app.emit("chips-updated", &chips);
}

#[tauri::command]
fn get_settings(s: tauri::State<SharedSettings>) -> Settings {
    s.lock().unwrap().clone()
}

#[tauri::command]
fn save_settings(
    app: tauri::AppHandle,
    s: tauri::State<SharedSettings>,
    activity: tauri::State<SharedActivity>,
    new: Settings,
) {
    let opacity = new.opacity.clamp(0.3, 1.0);
    let auto_start = new.auto_start;
    // 记录旧的显示开关 + 新值，用于检测「开→关」
    let (old_d, old_w, old_e) = {
        let g = s.lock().unwrap();
        (g.show_done, g.show_waiting, g.show_error)
    };
    let (new_d, new_w, new_e) = (new.show_done, new.show_waiting, new.show_error);
    {
        let mut guard = s.lock().unwrap();
        *guard = new;
        guard.opacity = opacity;
    }
    config::save(&s.lock().unwrap());

    // 颜色从开→关：把当前该颜色的信号条全部标记已处理（清掉且不复活）
    if old_d && !new_d {
        activity::mute_color(activity.inner(), "done");
    }
    if old_w && !new_w {
        activity::mute_color(activity.inner(), "waiting");
    }
    if old_e && !new_e {
        activity::mute_color(activity.inner(), "error");
    }

    let _ = app.emit("apply-opacity", opacity);
    config::set_autostart(auto_start);
    // 立即刷新信号条（关掉的颜色当场消失）
    let chips = activity::visible_chips(activity.inner());
    let _ = app.emit("chips-updated", &chips);
}

// ── 窗口辅助 ──────────────────────────────────────────────────────────

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

fn open_settings(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("settings") {
        let _ = w.show();
        let _ = w.set_focus();
    }
}

// ── 入口 ─────────────────────────────────────────────────────────────

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let shared: Shared = state::new_shared();
    let activity_shared: SharedActivity = activity::new_shared();
    let settings: SharedSettings = config::new_shared();
    let (wake_tx, wake_rx) = std::sync::mpsc::channel::<()>();

    let shared_c = shared.clone();
    let activity_c = activity_shared.clone();
    let settings_c = settings.clone();
    let settings_a = settings.clone();

    tauri::Builder::default()
        // 单实例锁：再次启动时聚焦已有窗口、不开第二个（必须最先注册）
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_notification::init())
        .manage(shared)
        .manage(activity_shared)
        .manage(settings)
        .manage(RefreshTx(Mutex::new(wake_tx)))
        .invoke_handler(tauri::generate_handler![
            get_state,
            get_chips,
            refresh_now,
            dismiss_chip,
            get_settings,
            save_settings,
        ])
        .setup(move |app| {
            // 采集线程
            let h1 = app.handle().clone();
            std::thread::spawn(move || collector::run(h1, shared_c, settings_c, wake_rx));
            // 活动检测线程
            let h2 = app.handle().clone();
            std::thread::spawn(move || activity::run(h2, activity_c, settings_a));

            // 启动时同步自启状态（透明度由前端加载时读 get_settings 自行应用）
            {
                let s = app.state::<SharedSettings>();
                let should = s.lock().unwrap().auto_start;
                if should != config::is_autostart_enabled() {
                    config::set_autostart(should);
                }
            }

            // 系统托盘
            let auto_on = app.state::<SharedSettings>().lock().unwrap().auto_start;
            let item_toggle  = MenuItem::with_id(app, "toggle",   "显示 / 隐藏", true, None::<&str>)?;
            let item_settings= MenuItem::with_id(app, "settings", "设置",        true, None::<&str>)?;
            let item_auto    = CheckMenuItem::with_id(app, "autostart", "开机自启", true, auto_on, None::<&str>)?;
            let item_refresh = MenuItem::with_id(app, "refresh",  "立即刷新",     true, None::<&str>)?;
            let item_quit    = MenuItem::with_id(app, "quit",     "退出",         true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&item_toggle, &item_settings, &item_auto, &item_refresh, &item_quit])?;

            let auto_item = item_auto.clone();
            let mut tray = TrayIconBuilder::new()
                .tooltip("LLMMonitor")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(move |app, event| match event.id.as_ref() {
                    "toggle"   => toggle_window(app),
                    "settings" => open_settings(app),
                    "refresh"  => {
                        if let Some(tx) = app.try_state::<RefreshTx>() {
                            let _ = tx.0.lock().unwrap().send(());
                        }
                    }
                    "autostart" => {
                        // 以设置为真理：翻转 → 同步勾选与注册表
                        let new_state = if let Some(s) = app.try_state::<SharedSettings>() {
                            let mut g = s.lock().unwrap();
                            g.auto_start = !g.auto_start;
                            config::save(&g);
                            g.auto_start
                        } else {
                            false
                        };
                        let _ = auto_item.set_checked(new_state);
                        config::set_autostart(new_state);
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
        // 关闭按钮：主窗口 → 隐藏到托盘；设置窗口 → 隐藏（保留状态）
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
