//! 活动检测引擎：从会话日志判定每个会话的「信号灯」状态。
//! - 🟢 done    : 最新有意义条目 = assistant end_turn/stop_sequence（Claude）或 task_complete（Codex）
//! - 🟡 waiting : 最新 = tool_use 且卡住超阈值（Claude）
//! - 🔴 error   : 最新 = api_error（Claude）或 turn_aborted（Codex）
//! 纯读日志文本，不监听键盘/屏幕。chip 是「日志状态 + dismiss 标记」的纯投影。
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use tauri::{AppHandle, Emitter};
use walkdir::WalkDir;

const ACTIVE_WINDOW_SECS: u64 = 15 * 60; // 只看近 15 分钟改动过的会话
const YELLOW_THRESHOLD_SECS: i64 = 45; // tool_use 卡住判定阈值
const TAIL_BYTES: u64 = 96 * 1024; // 只读文件尾部
const TICK: Duration = Duration::from_secs(4); // 检测节奏

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

fn elapsed_secs(ts: &str) -> i64 {
    match DateTime::parse_from_rfc3339(ts) {
        Ok(t) => (Utc::now() - t.with_timezone(&Utc)).num_seconds(),
        Err(_) => 0,
    }
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

// ───────────────────────── Claude ─────────────────────────

fn classify_claude(path: &Path) -> Option<Chip> {
    let content = tail_read(path, TAIL_BYTES)?;
    let mut last_kind: Option<&str> = None;
    let mut last_ts = String::new();
    let mut last_stop: Option<String> = None;
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
        // AI 生成的会话标题（比文件夹名好读得多）
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
            last_kind = Some("error");
            last_ts = ts;
            last_stop = None;
            continue;
        }
        match v.get("type").and_then(|x| x.as_str()) {
            Some("assistant") => {
                last_kind = Some("assistant");
                last_ts = ts;
                last_stop = v
                    .get("message")
                    .and_then(|m| m.get("stop_reason"))
                    .and_then(|x| x.as_str())
                    .map(String::from);
            }
            Some("user") => {
                last_kind = Some("user");
                last_ts = ts;
                last_stop = None;
            }
            _ => {}
        }
    }

    let state = match last_kind? {
        "error" => "error",
        "assistant" => match last_stop.as_deref() {
            Some("end_turn") | Some("stop_sequence") => "done",
            Some("tool_use") if elapsed_secs(&last_ts) > YELLOW_THRESHOLD_SECS => "waiting",
            _ => return None,
        },
        _ => return None, // user => 工作中
    };

    let folder = cwd
        .as_deref()
        .map(basename)
        .unwrap_or_else(|| folder_fallback(path));
    let project = ai_title
        .or_else(|| head_find(path, "aiTitle"))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| folder.clone());
    let id = path.file_stem()?.to_string_lossy().to_string();
    Some(Chip {
        id,
        tool: "claude".into(),
        project,
        folder,
        state: state.into(),
        since: last_ts.clone(),
        trigger: last_ts,
    })
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

// ───────────────────────── Codex ─────────────────────────

fn classify_codex(path: &Path) -> Option<Chip> {
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

    let state = match last_ev.as_deref()? {
        "task_complete" => "done",
        "turn_aborted" => "error",
        _ => return None, // task_started / user_message => 工作中
    };
    let project = cwd_from_head(path)
        .as_deref()
        .map(basename)
        .unwrap_or_else(|| "codex".into());
    let id = path.file_stem()?.to_string_lossy().to_string();
    Some(Chip {
        id,
        tool: "codex".into(),
        folder: project.clone(),
        project,
        state: state.into(),
        since: last_ts.clone(),
        trigger: last_ts,
    })
}

// ───────────────────────── 汇总 ─────────────────────────

fn prio(s: &str) -> u8 {
    match s {
        "error" => 3,
        "waiting" => 2,
        "done" => 1,
        _ => 0,
    }
}

fn compute_chips() -> Vec<Chip> {
    let mut out = Vec::new();
    let cutoff = SystemTime::now() - Duration::from_secs(ACTIVE_WINDOW_SECS);
    let Some(home) = dirs::home_dir() else {
        return out;
    };

    let scan = |root: std::path::PathBuf, f: &dyn Fn(&Path) -> Option<Chip>, out: &mut Vec<Chip>| {
        if !root.exists() {
            return;
        }
        for e in WalkDir::new(&root)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .filter(|e| e.path().extension().map(|x| x == "jsonl").unwrap_or(false))
        {
            if !recently_modified(e.path(), cutoff) {
                continue;
            }
            if let Some(c) = f(e.path()) {
                out.push(c);
            }
        }
    };

    scan(home.join(".claude").join("projects"), &classify_claude, &mut out);
    scan(home.join(".codex").join("sessions"), &classify_codex, &mut out);

    // 待办(error/waiting)靠左固定、不被裁；done 排其后；同级按时间早→晚（早的更靠左）
    out.sort_by(|a, b| {
        prio(&b.state)
            .cmp(&prio(&a.state))
            .then(a.since.cmp(&b.since))
    });
    out
}

fn sig(c: &Chip) -> String {
    format!("{}|{}|{}", c.id, c.state, c.trigger)
}

fn notify(handle: &AppHandle, c: &Chip) {
    use tauri_plugin_notification::NotificationExt;
    let (icon, label) = match c.state.as_str() {
        "done" => ("✅", "完成"),
        "waiting" => ("⏸", "等你处理"),
        "error" => ("⚠️", "出错/中断"),
        _ => ("", ""),
    };
    let tool = if c.tool == "claude" { "Claude" } else { "Codex" };
    let _ = handle
        .notification()
        .builder()
        .title(format!("{icon} {tool} · {}", c.project))
        .body(label)
        .show();
}

/// 后台活动检测循环：算 chip → 过滤 dismiss → 检测新增弹 toast → emit。
pub fn run(handle: AppHandle, shared: SharedActivity) {
    let mut first = true;
    let mut prev: HashSet<String> = HashSet::new();
    loop {
        let computed = compute_chips();
        let visible: Vec<Chip> = {
            let mut st = shared.lock().unwrap();
            let visible: Vec<Chip> = computed
                .into_iter()
                .filter(|c| st.dismissed.get(&c.id).map(|d| d != &c.trigger).unwrap_or(true))
                .collect();
            st.chips = visible.clone();
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
