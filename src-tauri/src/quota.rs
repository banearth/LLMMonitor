//! 慢轮询两家“额度”接口，解析为统一的 Window。
//! 接口均为非公开接口，解析做了防御性容错。
use crate::auth::{ClaudeCreds, CodexCreds};
use crate::state::Window;
use anyhow::{anyhow, Result};
use chrono::{TimeZone, Utc};
use serde_json::Value;

/// Claude Code 用的固定 UA —— 缺它会撞激进的 429 限流桶。
const CLAUDE_UA: &str = "claude-code/2.0.0 (external, cli)";
const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const CODEX_ORIGINATOR: &str = "codex_cli_rs";

fn codex_user_agent() -> String {
    let version_path = dirs::home_dir().map(|h| h.join(".codex").join("version.json"));
    let version = version_path
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|txt| serde_json::from_str::<Value>(&txt).ok())
        .and_then(|v| {
            v.get("latest_version")
                .and_then(|x| x.as_str())
                .map(String::from)
        })
        .unwrap_or_else(|| "0.140.0".to_string());

    format!("{CODEX_ORIGINATOR}/{version}")
}

/// 抓取结果。`logged_in=false` 表示 token 失效（401）。
pub struct QuotaResult {
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
    pub logged_in: bool,
}

/// 接口返回的 utilization / used_percent 都是 0–100 的百分比，直接用。
/// （注意：不能把 ≤1 当成 0..1 小数 ×100，否则 1% 会被误算成 100%。）
fn remaining_from_util(util: f64) -> f64 {
    (100.0 - util).clamp(0.0, 100.0)
}

fn epoch_to_iso(secs: i64) -> Option<String> {
    Utc.timestamp_opt(secs, 0)
        .single()
        .map(|dt| dt.to_rfc3339())
}

// ─────────────────────────── Claude ───────────────────────────

pub fn fetch_claude(
    client: &reqwest::blocking::Client,
    creds: &ClaudeCreds,
) -> Result<QuotaResult> {
    let resp = client
        .get("https://api.anthropic.com/api/oauth/usage")
        .header("Authorization", format!("Bearer {}", creds.access_token))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("User-Agent", CLAUDE_UA)
        .header("Content-Type", "application/json")
        .send()?;

    let status = resp.status();
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Ok(QuotaResult {
            five_hour: None,
            seven_day: None,
            logged_in: false,
        });
    }
    if !status.is_success() {
        return Err(anyhow!("Claude usage HTTP {}", status.as_u16()));
    }

    let v: Value = resp.json()?;

    // {"five_hour":{"utilization":23.0,"resets_at":"...ISO..."}, "seven_day":{...}}
    let parse = |key: &str| -> Option<Window> {
        let o = v.get(key)?;
        if !o.is_object() {
            return None;
        }
        let util = o.get("utilization").and_then(|x| x.as_f64())?;
        Some(Window {
            remaining: remaining_from_util(util),
            resets_at: o
                .get("resets_at")
                .and_then(|x| x.as_str())
                .map(String::from),
        })
    };

    Ok(QuotaResult {
        five_hour: parse("five_hour"),
        seven_day: parse("seven_day"),
        logged_in: true,
    })
}

// ─────────────────────────── Codex ───────────────────────────

pub fn fetch_codex(client: &reqwest::blocking::Client, creds: &CodexCreds) -> Result<QuotaResult> {
    let mut req = client
        .get(CODEX_USAGE_URL)
        .header("Authorization", format!("Bearer {}", creds.access_token))
        .header("User-Agent", codex_user_agent())
        .header("originator", CODEX_ORIGINATOR)
        .header("Content-Type", "application/json");
    if let Some(acct) = &creds.account_id {
        req = req.header("ChatGPT-Account-Id", acct.clone());
    }

    let resp = req.send()?;
    let status = resp.status();
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Ok(QuotaResult {
            five_hour: None,
            seven_day: None,
            logged_in: false,
        });
    }
    if !status.is_success() {
        return Err(anyhow!("{CODEX_USAGE_URL} HTTP {}", status.as_u16()));
    }

    let v: Value = resp.json()?;
    Ok(parse_codex_value(&v))
}

/// 同时给“网络响应”和“本地缓存 usage-limits.json”复用的解析。
/// 形状：{"rate_limit":{"primary_window":{"used_percent":0,"reset_at":<epoch s>}, "secondary_window":{...}}}
/// 也兼容把 rate_limit 直接放在顶层、或字段名为 utilization / resets_at(ISO) 的情况。
pub fn parse_codex_value(v: &Value) -> QuotaResult {
    let rl = v.get("rate_limit").unwrap_or(v);

    let parse = |key: &str| -> Option<Window> {
        let o = rl.get(key)?;
        if !o.is_object() {
            return None;
        }
        // 百分比：used_percent 优先，其次 utilization。
        let used = o
            .get("used_percent")
            .or_else(|| o.get("utilization"))
            .and_then(|x| x.as_f64())?;
        // 重置时间：reset_at(epoch 秒) 优先，其次 resets_at / reset_at 的 ISO 字符串。
        let resets_at = o
            .get("reset_at")
            .and_then(|x| x.as_i64())
            .and_then(epoch_to_iso)
            .or_else(|| {
                o.get("resets_at")
                    .or_else(|| o.get("reset_at"))
                    .and_then(|x| x.as_str())
                    .map(String::from)
            });
        Some(Window {
            remaining: remaining_from_util(used),
            resets_at,
        })
    };

    QuotaResult {
        five_hour: parse("primary_window"),
        seven_day: parse("secondary_window"),
        logged_in: true,
    }
}

/// 优先读 Codex 守护进程写的缓存 `~/.codex/usage-limits.json`，避免打网络。
pub fn read_codex_cache() -> Option<QuotaResult> {
    let path = dirs::home_dir()?.join(".codex").join("usage-limits.json");
    let txt = std::fs::read_to_string(&path).ok()?;
    let v: Value = serde_json::from_str(&txt).ok()?;
    let r = parse_codex_value(&v);
    if r.five_hour.is_some() || r.seven_day.is_some() {
        Some(r)
    } else {
        None
    }
}
