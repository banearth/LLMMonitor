//! 后台采集线程：快节奏刷新本地用量，慢节奏拉取额度，合并状态并 emit 给前端。
use crate::state::Shared;
use crate::{auth, quota, usage};
use chrono::Utc;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;
use tauri::{AppHandle, Emitter};

/// 本地用量刷新节奏。
const TICK: Duration = Duration::from_secs(20);
/// 每隔几个 tick 拉一次额度接口（5 × 20s = 100s）。第 0 个 tick 会立即拉。
const QUOTA_EVERY: u32 = 5;

/// `wake` 用于“立即刷新”：前端点 ⟳ 时通过它唤醒本循环并强制拉一次额度。
pub fn run(handle: AppHandle, shared: Shared, wake: Receiver<()>) {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap_or_else(|_| reqwest::blocking::Client::new());

    let mut tick: u32 = 0;
    let mut forced = false;
    loop {
        let do_quota = forced || tick % QUOTA_EVERY == 0;
        refresh(&handle, &shared, &client, do_quota);
        tick = tick.wrapping_add(1);

        match wake.recv_timeout(TICK) {
            Ok(_) => {
                // 手动刷新：清空积压的多次点击，强制下一轮立即拉额度并重置节奏。
                while wake.try_recv().is_ok() {}
                forced = true;
                tick = 0;
            }
            Err(RecvTimeoutError::Timeout) => forced = false,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn refresh(handle: &AppHandle, shared: &Shared, client: &reqwest::blocking::Client, do_quota: bool) {
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
    let snapshot = {
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
        st.clone()
    };

    let _ = handle.emit("state-updated", snapshot);
}
