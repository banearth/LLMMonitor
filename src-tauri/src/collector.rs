//! 后台采集线程：
//! - 本地用量（今日 token/花费）：每 TICK(20s) 扫一次（本地 IO），用来刷新底部数字 + 探测活动。
//! - 额度接口（5h/7天）：自适应轮询 ——
//!     · 活跃（最近在烧 token）→ 每 ACTIVE_INTERVAL(100s) 刷一次，看着剩余往下掉；
//!     · 空闲 → 不按表轮询，直接睡到下一个 reset 时间点再刷（响应里带 resetsAt，空闲时唯一会变的就是 reset）；
//!     · 从空闲恢复 / 手动⟳ / 启动 → 立即刷。
use crate::state::Shared;
use crate::{auth, quota, usage};
use chrono::{DateTime, Utc};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime};
use tauri::{AppHandle, Emitter};

const TICK: Duration = Duration::from_secs(20); // 本地用量刷新 / 活动探测节奏
const ACTIVE_INTERVAL: Duration = Duration::from_secs(100); // 活跃期额度刷新间隔
const IDLE_AFTER: Duration = Duration::from_secs(300); // 多久没 token 增长算空闲
const RESET_BUFFER: Duration = Duration::from_secs(15); // reset 后缓冲再刷，确保已跳变
const IDLE_FALLBACK: Duration = Duration::from_secs(600); // 拿不到 reset 时间时的兜底心跳

pub fn run(handle: AppHandle, shared: Shared, wake: Receiver<()>) {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap_or_else(|_| reqwest::blocking::Client::new());

    let mut next_quota_at = SystemTime::now(); // 启动即到期 → 立即拉
    let mut force = true; // 启动强制拉一次
    let mut prev_tokens: Option<u64> = None;
    let mut last_active = Instant::now();
    let mut was_active = true;

    loop {
        let now = SystemTime::now();
        let do_quota = force || now >= next_quota_at;
        force = false;

        let total = refresh(&handle, &shared, &client, do_quota);

        // 活动探测：今日 token 增长 = 在烧额度
        let grew = prev_tokens.map(|p| total > p).unwrap_or(false);
        prev_tokens = Some(total);
        if grew {
            last_active = Instant::now();
        }
        let active = last_active.elapsed() < IDLE_AFTER;

        if do_quota {
            // 刚拉过 → 根据活跃与否安排下次
            next_quota_at = schedule_next(&shared, active, now);
        } else if active && !was_active {
            // 从空闲恢复、但本 tick 没到点 → 提前到下一 tick 立即拉
            next_quota_at = now;
        }
        was_active = active;

        match wake.recv_timeout(TICK) {
            Ok(_) => {
                while wake.try_recv().is_ok() {} // 合并多次点击
                force = true; // 手动⟳ → 立即拉
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// 解析 ISO 时间为 SystemTime。
fn parse_iso(s: &str) -> Option<SystemTime> {
    let dt: DateTime<Utc> = DateTime::parse_from_rfc3339(s).ok()?.with_timezone(&Utc);
    let secs = dt.timestamp();
    if secs < 0 {
        return None;
    }
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs as u64))
}

/// 取所有窗口里最早的 reset 时间（5h/7天，Claude+Codex）。
fn earliest_reset(shared: &Shared) -> Option<SystemTime> {
    let st = shared.lock().unwrap();
    [
        &st.claude.five_hour,
        &st.claude.seven_day,
        &st.codex.five_hour,
        &st.codex.seven_day,
    ]
    .into_iter()
    .filter_map(|w| w.as_ref().and_then(|x| x.resets_at.as_deref()))
    .filter_map(parse_iso)
    .min()
}

/// 决定下次拉额度的时间点。
fn schedule_next(shared: &Shared, active: bool, now: SystemTime) -> SystemTime {
    let reset = earliest_reset(shared).map(|r| r + RESET_BUFFER);
    if active {
        // 活跃：每 100s 一次，但若 reset 更近则取 reset（极少见）
        let periodic = now + ACTIVE_INTERVAL;
        match reset {
            Some(r) if r < periodic => r,
            _ => periodic,
        }
    } else {
        // 空闲：直接睡到下一个 reset；拿不到 reset 则用兜底心跳
        reset.unwrap_or(now + IDLE_FALLBACK)
    }
}

/// 刷新一次：本地用量必刷；额度按 do_quota。返回今日 token 总量（用于活动探测）。
fn refresh(
    handle: &AppHandle,
    shared: &Shared,
    client: &reqwest::blocking::Client,
    do_quota: bool,
) -> u64 {
    // ---- 锁外完成所有 IO（文件 + 网络）----
    let claude_present = auth::claude_present();
    let codex_present = auth::codex_present();
    let claude_creds = auth::read_claude();
    let codex_creds = auth::read_codex();
    let claude_usage = usage::scan_claude_today();
    let codex_usage = usage::scan_codex_today();

    let claude_q = if do_quota {
        claude_creds.as_ref().map(|c| quota::fetch_claude(client, c))
    } else {
        None
    };
    let codex_net = if do_quota {
        codex_creds.as_ref().map(|c| quota::fetch_codex(client, c))
    } else {
        None
    };
    let codex_cache = if do_quota { quota::read_codex_cache() } else { None };

    // ---- 单次持锁写回 ----
    let (snapshot, total) = {
        let mut st = shared.lock().unwrap();
        let mut synced = false;

        // ===== Claude =====
        st.claude.present = claude_present;
        if let Some(u) = claude_usage {
            st.claude.today_tokens = Some(u.tokens);
            st.claude.today_cost = Some(u.cost);
        }
        if claude_creds.is_none() {
            st.claude.logged_in = false;
        }
        if let Some(res) = claude_q {
            match res {
                Ok(r) => {
                    st.claude.logged_in = r.logged_in;
                    if r.logged_in {
                        if r.five_hour.is_some() {
                            st.claude.five_hour = r.five_hour;
                        }
                        if r.seven_day.is_some() {
                            st.claude.seven_day = r.seven_day;
                        }
                        st.claude.stale = false;
                        st.claude.error = None;
                        synced = true;
                    } else {
                        st.claude.error = Some("需在终端重新登录 claude".into());
                    }
                }
                Err(e) => {
                    st.claude.stale = true;
                    st.claude.error = Some(format!("同步失败: {e}"));
                }
            }
        }

        // ===== Codex（优先网络成功，退回本地缓存）=====
        st.codex.present = codex_present;
        if let Some(u) = codex_usage {
            st.codex.today_tokens = Some(u.tokens);
            st.codex.today_cost = Some(u.cost);
        }
        if codex_creds.is_none() {
            st.codex.logged_in = false;
        }
        if do_quota && codex_creds.is_some() {
            let mut applied = false;
            if let Some(Ok(r)) = &codex_net {
                if r.logged_in && (r.five_hour.is_some() || r.seven_day.is_some()) {
                    st.codex.logged_in = true;
                    if r.five_hour.is_some() {
                        st.codex.five_hour = r.five_hour.clone();
                    }
                    if r.seven_day.is_some() {
                        st.codex.seven_day = r.seven_day.clone();
                    }
                    st.codex.stale = false;
                    st.codex.error = None;
                    synced = true;
                    applied = true;
                } else if !r.logged_in {
                    st.codex.logged_in = false;
                    st.codex.error = Some("需在终端重新登录 codex".into());
                    applied = true;
                }
            }
            if !applied {
                if let Some(r) = &codex_cache {
                    st.codex.logged_in = true;
                    if r.five_hour.is_some() {
                        st.codex.five_hour = r.five_hour.clone();
                    }
                    if r.seven_day.is_some() {
                        st.codex.seven_day = r.seven_day.clone();
                    }
                    st.codex.stale = false;
                    st.codex.error = None;
                    synced = true;
                    applied = true;
                }
            }
            if !applied {
                if let Some(Err(e)) = &codex_net {
                    st.codex.stale = true;
                    st.codex.error = Some(format!("同步失败: {e}"));
                }
            }
        }

        if synced {
            st.last_sync = Some(Utc::now().to_rfc3339());
        }
        let total = st.claude.today_tokens.unwrap_or(0) + st.codex.today_tokens.unwrap_or(0);
        (st.clone(), total)
    };

    let _ = handle.emit("state-updated", snapshot);
    total
}
