//! 用户设置持久化：`%APPDATA%\LLMMonitor\settings.json`。
//! load() 失败时透明地返回 Default，save() 失败时静默忽略。
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    // ── 信号灯 ──────────────────────────────────────────────────
    /// 显示哪些信号条（同时决定是否在该状态弹桌面通知）。
    pub show_done: bool,
    pub show_waiting: bool,
    pub show_error: bool,
    /// 普通工具卡住（非提问/计划）是否也判「等你」。默认关——长命令会误报。
    pub stall_to_waiting: bool,
    /// 普通工具静默多久才判「等你」（秒，仅 stall_to_waiting 开启时生效）。
    pub waiting_threshold_secs: i64,
    /// 文件多久没写入才视为「工具真停了」（秒）。
    pub running_grace_secs: i64,
    // ── 外观 ─────────────────────────────────────────────────────
    /// 主窗口透明度，0.3–1.0。
    pub opacity: f64,
    pub main_window_x: Option<i32>,
    pub main_window_y: Option<i32>,
    // ── 额度轮询 ──────────────────────────────────────────────────
    /// 多久没有 token 增长算空闲（秒）。
    pub idle_after_secs: u64,
    // ── 系统 ─────────────────────────────────────────────────────
    pub auto_start: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            show_done: true,
            show_waiting: true,
            show_error: true,
            stall_to_waiting: false,
            waiting_threshold_secs: 180,
            running_grace_secs: 60,
            opacity: 1.0,
            main_window_x: None,
            main_window_y: None,
            idle_after_secs: 300,
            auto_start: false,
        }
    }
}

fn config_path() -> Option<std::path::PathBuf> {
    Some(dirs::config_dir()?.join("LLMMonitor").join("settings.json"))
}

pub fn load() -> Settings {
    config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(s: &Settings) {
    let Some(p) = config_path() else { return };
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(s) {
        let _ = std::fs::write(p, json);
    }
}

pub type SharedSettings = Arc<Mutex<Settings>>;

pub fn new_shared() -> SharedSettings {
    Arc::new(Mutex::new(load()))
}

// ── 开机自启（Windows 注册表） ──────────────────────────────────────

pub fn set_autostart(enable: bool) {
    #[cfg(target_os = "windows")]
    {
        use winreg::{
            enums::{HKEY_CURRENT_USER, KEY_WRITE},
            RegKey,
        };
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        if let Ok(run) =
            hkcu.open_subkey_with_flags(r"Software\Microsoft\Windows\CurrentVersion\Run", KEY_WRITE)
        {
            if enable {
                if let Ok(exe) = std::env::current_exe() {
                    let _ = run.set_value("LLMMonitor", &exe.to_string_lossy().as_ref());
                }
            } else {
                let _ = run.delete_value("LLMMonitor");
            }
        }
    }
    let _ = enable; // suppress unused on non-Windows
}

pub fn is_autostart_enabled() -> bool {
    #[cfg(target_os = "windows")]
    {
        use winreg::{enums::HKEY_CURRENT_USER, RegKey};
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        if let Ok(run) = hkcu.open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Run") {
            return run.get_value::<String, _>("LLMMonitor").is_ok();
        }
    }
    false
}
