//! 活动检测引擎：从会话日志判定每个会话的「信号灯」状态。
//! - 🟢 done    : 最新有意义条目 = assistant end_turn/stop_sequence（Claude）或 task_complete（Codex）
//! - 🟡 waiting : 在提问/等拍板（AskUserQuestion/ExitPlanMode）→ 立即黄灯。
//!               普通 tool_use 卡住默认不判黄灯（长命令会误报），仅在用户开启
//!               stall_to_waiting 时才按「静默够久+文件够久没写」判。
//! 显示哪些灯 / 是否弹通知，由用户设置 show_done/waiting/error 控制（见 config.rs）。
//! - 🔴 error   : 最新 = api_error（Claude）或 turn_aborted（Codex）
//! 纯读日志文本，不监听键盘/屏幕。chip 是「日志状态 + 时间 + dismiss 标记」的纯投影。
//!
//! IO 策略：把「解析文件」（tail 读，IO）和「判定状态」（纯计算，依赖时间）拆开。
//! - 全量 WalkDir 发现活跃文件：只每 DISCOVERY_INTERVAL 做一次（不再每 4 秒）。
//! - 每 4 秒：只对缓存里的活跃文件 stat mtime（廉价），仅 mtime 变化的才重新 tail 解析。
//! - Codex 只扫「今天 + 昨天」日期目录，不再 WalkDir 整棵 sessions 树。
use chrono::{DateTime, Datelike, Local, Utc};
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tauri::{AppHandle, Emitter};
use walkdir::WalkDir;

const ACTIVE_WINDOW_SECS: u64 = 15 * 60; // 只跟踪近 15 分钟改动过的会话
const TICK: Duration = Duration::from_secs(4); // 检测节奏（廉价 stat + 状态更新）
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(30); // 全量 walk 发现新活跃文件的间隔
const TAIL_BYTES: u64 = 96 * 1024; // 只读文件尾部
                                   // 注：waiting 阈值（默认 180s）/ 仍在跑宽限（默认 60s）现由用户设置提供，见 config.rs。

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Chip {
    pub id: String,
    pub tool: String,    // claude | codex
    pub project: String, // 显示名（Claude=aiTitle / Codex=仓库名）
    pub folder: String,  // 仓库文件夹名（cwd 末段），tooltip 用
    pub state: String,   // done | waiting | error
    pub since: String,   // 触发条目 ISO 时间
    pub trigger: String, // 触发条目签名（=since），dismiss 比对用
}

#[derive(Default)]
pub struct ActivityState {
    pub chips: Vec<Chip>,
    pub dismissed: HashMap<String, String>, // session id -> 已 dismiss 的 trigger
}
pub type SharedActivity = Arc<Mutex<ActivityState>>;

pub fn new_shared() -> SharedActivity {
    Arc::new(Mutex::new(ActivityState::default()))
}

// ───────────────────────── 工具函数 ─────────────────────────

fn tail_read(path: &Path, max_bytes: u64) -> Option<String> {
    let mut f = File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(max_bytes);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let mut s = String::from_utf8_lossy(&buf).into_owned();
    if start > 0 {
        if let Some(i) = s.find('\n') {
            s = s[i + 1..].to_string();
        }
    }
    Some(s)
}

fn basename(p: &str) -> String {
    p.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(p)
        .to_string()
}

/// 距某 ISO 时间过去了多少秒。
fn elapsed_secs(ts: &str) -> i64 {
    match DateTime::parse_from_rfc3339(ts) {
        Ok(t) => (Utc::now() - t.with_timezone(&Utc)).num_seconds(),
        Err(_) => 0,
    }
}

/// 文件最后一次写入距今多少秒（越大=越久没动）。
fn mtime_age_secs(mtime: SystemTime) -> i64 {
    SystemTime::now()
        .duration_since(mtime)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn recently_modified(p: &Path, cutoff: SystemTime) -> bool {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .map(|t| t >= cutoff)
        .unwrap_or(false)
}

/// 从文件头部找 `"key":"..."` 的第一个值（会话元信息多在头部）。
fn head_find(path: &Path, key: &str) -> Option<String> {
    let mut f = File::open(path).ok()?;
    let mut buf = vec![0u8; 65536];
    let n = f.read(&mut buf).ok()?;
    let s = String::from_utf8_lossy(&buf[..n]);
    let pat = format!("\"{key}\":\"");
    let i = s.find(&pat)? + pat.len();
    let rest = &s[i..];
    let j = rest.find('"')?;
    Some(rest[..j].replace("\\\\", "\\"))
}

fn cwd_from_head(path: &Path) -> Option<String> {
    head_find(path, "cwd")
}

fn codex_session_id_from_head(path: &Path) -> Option<String> {
    head_find(path, "id")
}

fn clean_title(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

fn read_codex_title_map() -> Option<HashMap<String, String>> {
    let path = dirs::home_dir()?
        .join(".codex")
        .join("llmmonitor-session-titles.json");
    let txt = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<HashMap<String, String>>(txt.trim_start_matches('\u{feff}')).ok()
}

fn codex_title_map_mtime() -> Option<SystemTime> {
    let path = dirs::home_dir()?
        .join(".codex")
        .join("llmmonitor-session-titles.json");
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn read_claude_title_map() -> Option<HashMap<String, String>> {
    let path = dirs::home_dir()?
        .join(".claude")
        .join("llmmonitor-session-titles.json");
    let txt = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<HashMap<String, String>>(txt.trim_start_matches('\u{feff}')).ok()
}

fn claude_title_map_mtime() -> Option<SystemTime> {
    let path = dirs::home_dir()?
        .join(".claude")
        .join("llmmonitor-session-titles.json");
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn claude_title_override(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_string_lossy().to_string();
    read_claude_title_map()?
        .get(&stem)
        .and_then(|title| clean_title(title))
}

fn codex_title_override(path: &Path) -> Option<String> {
    let titles = read_codex_title_map()?;
    codex_title_from_map(path, &titles)
}

fn codex_title_from_map(path: &Path, titles: &HashMap<String, String>) -> Option<String> {
    let stem = path.file_stem()?.to_string_lossy().to_string();
    if let Some(title) = codex_session_id_from_head(path)
        .as_deref()
        .and_then(|id| titles.get(id))
        .and_then(|title| clean_title(title))
    {
        return Some(title);
    }

    titles.get(&stem).and_then(|title| clean_title(title))
}

fn folder_fallback(path: &Path) -> String {
    path.parent()
        .and_then(|p| p.file_name())
        .map(|s| {
            let n = s.to_string_lossy();
            n.rsplit('-').next().unwrap_or(&n).to_string()
        })
        .unwrap_or_else(|| "?".into())
}

// ───────────────── 解析（IO）与判定（纯计算）分离 ─────────────────

/// 会话「最新有意义条目」的种类。Working = 用户输入/工具结果/仍在推进，不出灯。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Done,
    ToolUse, // 普通工具（Bash 等），可能长跑 → 走阈值判 waiting
    Asking,  // 在向你提问/等拍板（AskUserQuestion/ExitPlanMode 等）→ 立即 waiting
    Error,
    Working,
}

/// 该 assistant 消息是否在「阻塞等用户决定」的工具上（提问 / 计划批准）。
fn is_user_blocking_tool(msg: &Value) -> bool {
    msg.get("content")
        .and_then(|c| c.as_array())
        .map(|blocks| {
            blocks.iter().any(|b| {
                b.get("type").and_then(|t| t.as_str()) == Some("tool_use")
                    && matches!(
                        b.get("name").and_then(|n| n.as_str()),
                        Some("AskUserQuestion") | Some("ExitPlanMode") | Some("EnterPlanMode")
                    )
            })
        })
        .unwrap_or(false)
}

/// 一次 tail 解析的结果（只在文件 mtime 变化时才重算，缓存复用）。
#[derive(Clone)]
struct Parsed {
    id: String,
    tool: &'static str,
    project: String,
    folder: String,
    kind: Kind,
    last_ts: String, // 最新有意义条目的 ISO 时间
}

#[derive(Clone, Copy, PartialEq)]
enum Src {
    Claude,
    Codex,
}

fn parse_file(src: Src, path: &Path) -> Option<Parsed> {
    match src {
        Src::Claude => parse_claude(path),
        Src::Codex => parse_codex(path),
    }
}

fn parse_claude(path: &Path) -> Option<Parsed> {
    let content = tail_read(path, TAIL_BYTES)?;
    let mut kind = Kind::Working;
    let mut last_ts = String::new();
    let mut have = false;
    let mut cwd: Option<String> = None;
    let mut ai_title: Option<String> = None;

    for line in content.lines() {
        if line.len() < 2 {
            continue;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if cwd.is_none() {
            if let Some(c) = v.get("cwd").and_then(|x| x.as_str()) {
                cwd = Some(c.to_string());
            }
        }
        if v.get("type").and_then(|x| x.as_str()) == Some("ai-title") {
            if let Some(t) = v.get("aiTitle").and_then(|x| x.as_str()) {
                if !t.is_empty() {
                    ai_title = Some(t.to_string());
                }
            }
            continue;
        }
        let ts = v
            .get("timestamp")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();

        if line.contains("\"isApiErrorMessage\":true") || line.contains("\"subtype\":\"api_error\"")
        {
            kind = Kind::Error;
            last_ts = ts;
            have = true;
            continue;
        }
        match v.get("type").and_then(|x| x.as_str()) {
            Some("assistant") => {
                let msg = v.get("message");
                let stop = msg
                    .and_then(|m| m.get("stop_reason"))
                    .and_then(|x| x.as_str());
                kind = match stop {
                    Some("end_turn") | Some("stop_sequence") => Kind::Done,
                    Some("tool_use") => {
                        if msg.map(is_user_blocking_tool).unwrap_or(false) {
                            Kind::Asking
                        } else {
                            Kind::ToolUse
                        }
                    }
                    _ => Kind::Working,
                };
                last_ts = ts;
                have = true;
            }
            Some("user") => {
                kind = Kind::Working;
                last_ts = ts;
                have = true;
            }
            _ => {}
        }
    }

    if !have {
        return None;
    }
    let folder = cwd
        .as_deref()
        .map(basename)
        .unwrap_or_else(|| folder_fallback(path));
    let project = claude_title_override(path)
        .or(ai_title)
        .or_else(|| head_find(path, "aiTitle"))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| folder.clone());
    let id = path.file_stem()?.to_string_lossy().to_string();
    Some(Parsed {
        id,
        tool: "claude",
        project,
        folder,
        kind,
        last_ts,
    })
}

fn parse_codex(path: &Path) -> Option<Parsed> {
    let content = tail_read(path, TAIL_BYTES)?;
    let mut last_ev: Option<String> = None;
    let mut last_ts = String::new();

    for line in content.lines() {
        if !line.contains("event_msg") {
            continue;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("type").and_then(|x| x.as_str()) != Some("event_msg") {
            continue;
        }
        if let Some(pt) = v.pointer("/payload/type").and_then(|x| x.as_str()) {
            if matches!(
                pt,
                "task_complete" | "turn_aborted" | "task_started" | "user_message"
            ) {
                last_ev = Some(pt.to_string());
                last_ts = v
                    .get("timestamp")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
            }
        }
    }

    let kind = match last_ev.as_deref()? {
        "task_complete" => Kind::Done,
        "turn_aborted" => Kind::Error,
        _ => Kind::Working, // task_started / user_message
    };
    let folder = cwd_from_head(path)
        .as_deref()
        .map(basename)
        .unwrap_or_else(|| "codex".into());
    let project = codex_title_override(path).unwrap_or_else(|| folder.clone());
    let id = path.file_stem()?.to_string_lossy().to_string();
    Some(Parsed {
        id,
        tool: "codex",
        project,
        folder,
        kind,
        last_ts,
    })
}

/// 纯判定：由解析结果 + 文件 mtime（+ 当前时间）算出是否出灯。无 IO。
///
/// waiting 规则（降低误报）：仅当 tool_use 条目已静默 ≥ WAITING_THRESHOLD_SECS
/// 且文件已 ≥ RUNNING_GRACE_SECS 没有任何写入，才判为「等你」。
/// 这样长工具运行（文件近期仍在写、或还没到阈值）不会立刻黄灯；
/// 只有真的长时间无任何后续才黄灯；user 继续输入则 kind=Working，直接不出灯。
fn decide(
    p: &Parsed,
    mtime: SystemTime,
    stall_enabled: bool,
    waiting_threshold: i64,
    running_grace: i64,
) -> Option<Chip> {
    let state = match p.kind {
        Kind::Done => "done",
        Kind::Error => "error",
        Kind::Working => return None,
        // 提问/计划批准 → 一定是等你，立即黄灯。
        Kind::Asking => "waiting",
        // 普通工具卡住：默认不判黄灯（长命令会误报）；仅当用户开启才按阈值判。
        Kind::ToolUse => {
            if stall_enabled
                && elapsed_secs(&p.last_ts) >= waiting_threshold
                && mtime_age_secs(mtime) >= running_grace
            {
                "waiting"
            } else {
                return None;
            }
        }
    };
    Some(Chip {
        id: p.id.clone(),
        tool: p.tool.into(),
        project: p.project.clone(),
        folder: p.folder.clone(),
        state: state.into(),
        since: p.last_ts.clone(),
        trigger: p.last_ts.clone(),
    })
}

// ───────────────────────── 扫描器（带缓存）─────────────────────────

struct Entry {
    src: Src,
    /// 上次解析时的文件 mtime；mtime 没变就不重读。
    mtime: SystemTime,
    parsed: Option<Parsed>,
}

struct Scanner {
    active: HashMap<PathBuf, Entry>,
    last_discovery: Option<Instant>,
    codex_title_mtime: Option<Option<SystemTime>>,
    claude_title_mtime: Option<Option<SystemTime>>,
    settings: crate::config::SharedSettings,
}

impl Scanner {
    fn new(settings: crate::config::SharedSettings) -> Self {
        Self {
            active: HashMap::new(),
            last_discovery: None,
            codex_title_mtime: None,
            claude_title_mtime: None,
            settings,
        }
    }

    fn codex_titles_changed(&mut self) -> bool {
        let current = codex_title_map_mtime();
        let changed = self
            .codex_title_mtime
            .map(|previous| previous != current)
            .unwrap_or(false);
        self.codex_title_mtime = Some(current);
        changed
    }

    fn claude_titles_changed(&mut self) -> bool {
        let current = claude_title_map_mtime();
        let changed = self
            .claude_title_mtime
            .map(|previous| previous != current)
            .unwrap_or(false);
        self.claude_title_mtime = Some(current);
        changed
    }
}

/// Codex 只看「今天 + 昨天」两个日期目录，避免 walk 整棵 sessions 树。
fn codex_date_dirs(sroot: &Path) -> Vec<PathBuf> {
    let today = Local::now().date_naive();
    let yesterday = today.pred_opt().unwrap_or(today);
    [today, yesterday]
        .iter()
        .map(|d| {
            sroot
                .join(format!("{:04}", d.year()))
                .join(format!("{:02}", d.month()))
                .join(format!("{:02}", d.day()))
        })
        .collect()
}

impl Scanner {
    /// 全量 walk，把近期改动的文件并入活跃集合（低频调用）。
    fn discover(&mut self) {
        let cutoff = SystemTime::now() - Duration::from_secs(ACTIVE_WINDOW_SECS);
        let Some(home) = dirs::home_dir() else {
            return;
        };

        let add = |root: &Path, src: Src, active: &mut HashMap<PathBuf, Entry>| {
            if !root.exists() {
                return;
            }
            for e in WalkDir::new(root)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file())
                .filter(|e| e.path().extension().map(|x| x == "jsonl").unwrap_or(false))
            {
                if recently_modified(e.path(), cutoff) {
                    active.entry(e.path().to_path_buf()).or_insert(Entry {
                        src,
                        mtime: SystemTime::UNIX_EPOCH, // 触发首次解析
                        parsed: None,
                    });
                }
            }
        };

        add(
            &home.join(".claude").join("projects"),
            Src::Claude,
            &mut self.active,
        );
        let sroot = home.join(".codex").join("sessions");
        for d in codex_date_dirs(&sroot) {
            add(&d, Src::Codex, &mut self.active);
        }
    }

    /// 每 4 秒一次：必要时发现；对活跃文件 stat，仅 mtime 变化的重读；judging 全跑。
    fn tick(&mut self) -> Vec<Chip> {
        let due = self
            .last_discovery
            .map(|t| t.elapsed() >= DISCOVERY_INTERVAL)
            .unwrap_or(true);
        if due {
            self.discover();
            self.last_discovery = Some(Instant::now());
        }

        // 每 tick 从设置读阈值/开关（用户修改后立即生效）
        let (stall, wt, rg) = {
            let s = self.settings.lock().unwrap();
            (
                s.stall_to_waiting,
                s.waiting_threshold_secs,
                s.running_grace_secs,
            )
        };
        let codex_titles_changed = self.codex_titles_changed();
        let claude_titles_changed = self.claude_titles_changed();

        let cutoff = SystemTime::now() - Duration::from_secs(ACTIVE_WINDOW_SECS);
        let mut chips = Vec::new();
        let mut remove = Vec::new();

        for (path, e) in self.active.iter_mut() {
            let meta = match std::fs::metadata(path) {
                Ok(m) => m,
                Err(_) => {
                    remove.push(path.clone());
                    continue;
                }
            };
            let mtime = meta.modified().unwrap_or_else(|_| SystemTime::now());
            if mtime < cutoff {
                remove.push(path.clone()); // 不再活跃
                continue;
            }
            if mtime != e.mtime
                || (matches!(e.src, Src::Codex) && codex_titles_changed)
                || (matches!(e.src, Src::Claude) && claude_titles_changed)
            {
                e.parsed = parse_file(e.src, path); // 仅在文件确有变化时才读
                e.mtime = mtime;
            }
            if let Some(p) = &e.parsed {
                if let Some(c) = decide(p, mtime, stall, wt, rg) {
                    chips.push(c);
                }
            }
        }
        for p in remove {
            self.active.remove(&p);
        }

        // 待办(error/waiting)靠左固定、不被裁；done 排其后；同级按时间早→晚
        chips.sort_by(|a, b| {
            prio(&b.state)
                .cmp(&prio(&a.state))
                .then(a.since.cmp(&b.since))
        });
        chips
    }
}

// ───────────────────────── 输出 ─────────────────────────

fn prio(s: &str) -> u8 {
    match s {
        "error" => 3,
        "waiting" => 2,
        "done" => 1,
        _ => 0,
    }
}

fn sig(c: &Chip) -> String {
    format!("{}|{}|{}", c.id, c.state, c.trigger)
}

/// 当前要给前端的信号条 = 候选集里没被 dismiss 的。
/// （关闭的颜色其 chip 会被自动 dismiss，所以这里只看 dismiss 即可。）
pub fn visible_chips(activity: &SharedActivity) -> Vec<Chip> {
    let st = activity.lock().unwrap();
    let dismissed = &st.dismissed;
    st.chips
        .iter()
        .filter(|c| {
            dismissed
                .get(&c.id)
                .map(|d| d != &c.trigger)
                .unwrap_or(true)
        })
        .cloned()
        .collect()
}

/// 把某颜色当前的 chip 全部标记已处理（用户关掉该颜色通知时调用）。
/// 之后开回来也不会复活——只有全新的完成才会再出现。
pub fn mute_color(activity: &SharedActivity, state: &str) {
    let mut st = activity.lock().unwrap();
    let targets: Vec<(String, String)> = st
        .chips
        .iter()
        .filter(|c| c.state == state)
        .map(|c| (c.id.clone(), c.trigger.clone()))
        .collect();
    for (id, trig) in targets {
        st.dismissed.insert(id, trig);
    }
}

fn notify(handle: &AppHandle, c: &Chip) {
    use tauri_plugin_notification::NotificationExt;
    let (icon, label) = match c.state.as_str() {
        "done" => ("✅", "完成"),
        "waiting" => ("⏸", "等你处理"),
        "error" => ("⚠️", "出错/中断"),
        _ => ("", ""),
    };
    let tool = if c.tool == "claude" {
        "Claude"
    } else {
        "Codex"
    };
    let _ = handle
        .notification()
        .builder()
        .title(format!("{icon} {tool} · {}", c.project))
        .body(label)
        .show();
}

/// 后台活动检测循环：扫描 → 过滤 dismiss → 检测新增弹 toast → emit。
pub fn run(handle: AppHandle, shared: SharedActivity, settings: crate::config::SharedSettings) {
    let mut scanner = Scanner::new(settings.clone());
    let mut first = true;
    let mut prev: HashSet<String> = HashSet::new();
    loop {
        let computed = scanner.tick();
        // 关闭的颜色：当前及之后新出现的 chip 都自动 dismiss（订阅式语义——
        // 关了不再出现、当前也清掉；开回来不复活旧的，只显示全新的）。
        let (sd, sw, se) = {
            let c = settings.lock().unwrap();
            (c.show_done, c.show_waiting, c.show_error)
        };
        let visible: Vec<Chip> = {
            let mut st = shared.lock().unwrap();
            for c in &computed {
                let muted = match c.state.as_str() {
                    "done" => !sd,
                    "waiting" => !sw,
                    "error" => !se,
                    _ => false,
                };
                if muted {
                    st.dismissed.insert(c.id.clone(), c.trigger.clone());
                }
            }
            let visible = computed
                .iter()
                .filter(|c| {
                    st.dismissed
                        .get(&c.id)
                        .map(|d| d != &c.trigger)
                        .unwrap_or(true)
                })
                .cloned()
                .collect::<Vec<_>>();
            st.chips = computed;
            visible
        };

        if !first {
            for c in &visible {
                if !prev.contains(&sig(c)) {
                    notify(&handle, c);
                }
            }
        }
        first = false;
        prev = visible.iter().map(sig).collect();

        let _ = handle.emit("chips-updated", &visible);
        std::thread::sleep(TICK);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_rollout(name: &str, session_id: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "llmmonitor-test-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let line = format!(
            r#"{{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{{"id":"{session_id}","cwd":"C:\\repo\\Client"}}}}"#
        );
        std::fs::write(&path, line).unwrap();
        path
    }

    fn parsed(kind: Kind, ts: &str) -> Parsed {
        Parsed {
            id: "x".into(),
            tool: "claude",
            project: "p".into(),
            folder: "p".into(),
            kind,
            last_ts: ts.into(),
        }
    }

    #[test]
    fn done_shows() {
        let c = decide(
            &parsed(Kind::Done, "2020-01-01T00:00:00Z"),
            SystemTime::now(),
            true,
            180,
            60,
        );
        assert_eq!(c.unwrap().state, "done");
    }

    #[test]
    fn error_shows() {
        let c = decide(
            &parsed(Kind::Error, "2020-01-01T00:00:00Z"),
            SystemTime::now(),
            true,
            180,
            60,
        );
        assert_eq!(c.unwrap().state, "error");
    }

    #[test]
    fn working_hidden() {
        let c = decide(
            &parsed(Kind::Working, "2020-01-01T00:00:00Z"),
            SystemTime::now(),
            true,
            180,
            60,
        );
        assert!(c.is_none());
    }

    #[test]
    fn asking_is_immediate_waiting() {
        // 在提问（AskUserQuestion）→ 即使时间戳就是现在、文件刚写，也立即黄灯
        let now_ts = Utc::now().to_rfc3339();
        let c = decide(
            &parsed(Kind::Asking, &now_ts),
            SystemTime::now(),
            true,
            180,
            60,
        );
        assert_eq!(c.unwrap().state, "waiting");
    }

    #[test]
    fn codex_title_map_prefers_session_id_then_rollout_stem() {
        let path = temp_rollout("rollout-test.jsonl", "session-1");
        let mut titles = HashMap::new();
        titles.insert("rollout-test".to_string(), "Stem Title".to_string());
        titles.insert("session-1".to_string(), "Session Title".to_string());
        assert_eq!(
            codex_title_from_map(&path, &titles).as_deref(),
            Some("Session Title")
        );

        titles.remove("session-1");
        assert_eq!(
            codex_title_from_map(&path, &titles).as_deref(),
            Some("Stem Title")
        );

        titles.insert("session-1".to_string(), "   ".to_string());
        assert_eq!(
            codex_title_from_map(&path, &titles).as_deref(),
            Some("Stem Title")
        );

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn tooluse_recent_not_waiting() {
        let now_ts = Utc::now().to_rfc3339();
        let c = decide(
            &parsed(Kind::ToolUse, &now_ts),
            SystemTime::now(),
            true,
            180,
            60,
        );
        assert!(c.is_none());
    }

    #[test]
    fn tooluse_stale_but_recent_write_not_waiting() {
        let old = (Utc::now() - chrono::Duration::seconds(600)).to_rfc3339();
        let c = decide(
            &parsed(Kind::ToolUse, &old),
            SystemTime::now(),
            true,
            180,
            60,
        );
        assert!(c.is_none());
    }

    #[test]
    fn tooluse_stale_and_quiet_waiting() {
        let old = (Utc::now() - chrono::Duration::seconds(600)).to_rfc3339();
        let quiet_mtime = SystemTime::now() - Duration::from_secs(120);
        let c = decide(&parsed(Kind::ToolUse, &old), quiet_mtime, true, 180, 60);
        assert_eq!(c.unwrap().state, "waiting");
    }
}
