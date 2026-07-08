//! 前后端共享的状态契约。序列化为 camelCase 给前端。
use serde::Serialize;
use std::sync::{Arc, Mutex};

/// 单个限流窗口（5 小时 或 7 天）。
#[derive(Serialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct Window {
    /// 剩余额度百分比 0..100。
    pub remaining: f64,
    /// 重置时间，ISO 8601；拿不到则为 None。
    pub resets_at: Option<String>,
}

/// 一个厂商（Claude 或 Codex）的完整状态。
#[derive(Serialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct Provider {
    /// 本机是否检测到该工具的凭证。
    pub present: bool,
    /// 凭证是否有效（false = 需重新登录）。
    pub logged_in: bool,
    /// 5 小时滚动窗口。
    pub five_hour: Option<Window>,
    /// 7 天窗口。
    pub seven_day: Option<Window>,
    /// 今日累计 token（含 cache）。
    pub today_tokens: Option<u64>,
    /// 今日 API 等价花费（美元）。
    pub today_cost: Option<f64>,
    /// 额度数据是否陈旧（上次同步失败，展示的是历史值）。
    pub stale: bool,
    /// 最近一次错误信息（用于提示）。
    pub error: Option<String>,
    /// 接口明确表示当前额度不可用（耗尽/未订阅）。仅由实时同步更新。
    pub blocked: bool,
    /// 套餐类型，如 "free"/"plus"/"pro"；拿不到为 None。
    pub plan: Option<String>,
}

/// 整个应用对外暴露的状态。
#[derive(Serialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct AppState {
    pub claude: Provider,
    pub codex: Provider,
    /// 最近一次成功同步额度的时间，ISO 8601。
    pub last_sync: Option<String>,
}

/// 线程间共享的句柄。
pub type Shared = Arc<Mutex<AppState>>;

pub fn new_shared() -> Shared {
    Arc::new(Mutex::new(AppState::default()))
}

/// 供前端“立即刷新”命令唤醒采集线程的发送端。
pub struct RefreshTx(pub Mutex<std::sync::mpsc::Sender<()>>);
