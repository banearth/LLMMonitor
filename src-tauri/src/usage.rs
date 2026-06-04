//! 扫描本地 Claude Code 会话日志，统计“今日”token 与 API 等价花费。
//! 只读 ~/.claude/projects 下近期改动过的 jsonl，按 (msgId, requestId) 去重。
use chrono::{DateTime, Local};
use serde_json::Value;
use std::collections::HashSet;
use std::time::{Duration, SystemTime};
use walkdir::WalkDir;

/// 各模型每百万 token 单价（美元）。来源参考公开定价，用于估算等价成本。
struct Price {
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write_5m: f64,
    cache_write_1h: f64,
}

/// 按模型 id 子串匹配单价；未知模型回退到 Sonnet 档。
fn price_for(model: &str) -> Price {
    let m = model.to_ascii_lowercase();
    if m.contains("opus") {
        Price { input: 15.0, output: 75.0, cache_read: 1.5, cache_write_5m: 18.75, cache_write_1h: 30.0 }
    } else if m.contains("haiku") {
        Price { input: 1.0, output: 5.0, cache_read: 0.10, cache_write_5m: 1.25, cache_write_1h: 2.0 }
    } else {
        // sonnet 及未知
        Price { input: 3.0, output: 15.0, cache_read: 0.30, cache_write_5m: 3.75, cache_write_1h: 6.0 }
    }
}

#[derive(Default)]
pub struct UsageTotals {
    pub tokens: u64,
    pub cost: f64,
}

fn f64_at(v: &Value, key: &str) -> f64 {
    v.get(key).and_then(|x| x.as_f64()).unwrap_or(0.0)
}

/// 扫描今日（本地时区）Claude 用量。失败返回 None。
pub fn scan_claude_today() -> Option<UsageTotals> {
    let root = dirs::home_dir()?.join(".claude").join("projects");
    if !root.exists() {
        return None;
    }

    let today = Local::now().date_naive();
    // 仅扫描近 36 小时内改动过的文件——今日数据只会落在最近写过的文件里。
    let cutoff = SystemTime::now() - Duration::from_secs(36 * 3600);

    let mut totals = UsageTotals::default();
    let mut seen: HashSet<String> = HashSet::new();

    for entry in WalkDir::new(&root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| e.path().extension().map(|x| x == "jsonl").unwrap_or(false))
    {
        if let Ok(meta) = entry.metadata() {
            if let Ok(modified) = meta.modified() {
                if modified < cutoff {
                    continue;
                }
            }
        }
        let Ok(content) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        for line in content.lines() {
            // 廉价预筛：没有 usage 的行直接跳过。
            if !line.contains("\"usage\"") {
                continue;
            }
            let Ok(o) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if o.get("type").and_then(|x| x.as_str()) != Some("assistant") {
                continue;
            }
            // 时间过滤：转本地日期 == 今天。
            let Some(ts) = o.get("timestamp").and_then(|x| x.as_str()) else {
                continue;
            };
            let Ok(dt) = DateTime::parse_from_rfc3339(ts) else {
                continue;
            };
            if dt.with_timezone(&Local).date_naive() != today {
                continue;
            }
            let Some(msg) = o.get("message") else { continue };
            let Some(usage) = msg.get("usage") else { continue };

            // 去重：同一条消息可能在续接会话里重复出现。
            let msg_id = msg.get("id").and_then(|x| x.as_str()).unwrap_or("");
            let req_id = o.get("requestId").and_then(|x| x.as_str()).unwrap_or("");
            if !msg_id.is_empty() || !req_id.is_empty() {
                let key = format!("{msg_id}|{req_id}");
                if !seen.insert(key) {
                    continue;
                }
            }

            let model = msg.get("model").and_then(|x| x.as_str()).unwrap_or("");
            let p = price_for(model);

            let input = f64_at(usage, "input_tokens");
            let output = f64_at(usage, "output_tokens");
            let cache_read = f64_at(usage, "cache_read_input_tokens");
            let cache_create_total = f64_at(usage, "cache_creation_input_tokens");

            // cache 写入细分 5m / 1h（价格不同）；无细分则全按 5m。
            let (w5m, w1h) = match usage.get("cache_creation") {
                Some(c) => (
                    f64_at(c, "ephemeral_5m_input_tokens"),
                    f64_at(c, "ephemeral_1h_input_tokens"),
                ),
                None => (cache_create_total, 0.0),
            };

            let cost = (input * p.input
                + output * p.output
                + cache_read * p.cache_read
                + w5m * p.cache_write_5m
                + w1h * p.cache_write_1h)
                / 1_000_000.0;

            totals.cost += cost;
            totals.tokens +=
                (input + output + cache_read + cache_create_total).max(0.0) as u64;
        }
    }

    Some(totals)
}

// ─────────────────────────── Codex ───────────────────────────

/// Codex 走 ChatGPT 订阅、无逐 token 计费；这里用 GPT-5.x 公开 API 单价做“等价成本”估算。
struct CodexPrice {
    input: f64,
    cached_input: f64,
    output: f64,
}
fn codex_price() -> CodexPrice {
    CodexPrice { input: 1.25, cached_input: 0.125, output: 10.0 }
}

/// 扫描今日（本地时区）Codex 用量。
/// 数据来自 ~/.codex/sessions/年/月/日/rollout-*.jsonl 里的 token_count 事件，
/// `total_token_usage` 是会话累计值，用“按序差分、只累加今天的增量”来统计今日量。
pub fn scan_codex_today() -> Option<UsageTotals> {
    let root = dirs::home_dir()?.join(".codex").join("sessions");
    if !root.exists() {
        return None;
    }

    let today = Local::now().date_naive();
    let cutoff = SystemTime::now() - Duration::from_secs(36 * 3600);
    let price = codex_price();
    let mut totals = UsageTotals::default();

    for entry in WalkDir::new(&root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| e.path().extension().map(|x| x == "jsonl").unwrap_or(false))
    {
        if let Ok(meta) = entry.metadata() {
            if let Ok(modified) = meta.modified() {
                if modified < cutoff {
                    continue;
                }
            }
        }
        let Ok(content) = std::fs::read_to_string(entry.path()) else {
            continue;
        };

        // 单个会话内按时间顺序跟踪累计值，对“今天”的事件累加其增量。
        let mut prev_in = 0f64;
        let mut prev_cached = 0f64;
        let mut prev_out = 0f64;

        for line in content.lines() {
            if !line.contains("token_count") {
                continue;
            }
            let Ok(o) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let payload = o.get("payload");
            let is_tc = payload
                .and_then(|p| p.get("type"))
                .and_then(|x| x.as_str())
                == Some("token_count");
            if !is_tc {
                continue;
            }
            let Some(tot) = payload
                .and_then(|p| p.get("info"))
                .and_then(|i| i.get("total_token_usage"))
            else {
                continue;
            };

            let cum_in = f64_at(tot, "input_tokens");
            let cum_cached = f64_at(tot, "cached_input_tokens");
            let cum_out = f64_at(tot, "output_tokens");

            let d_in = (cum_in - prev_in).max(0.0);
            let d_cached = (cum_cached - prev_cached).max(0.0);
            let d_out = (cum_out - prev_out).max(0.0);
            prev_in = cum_in;
            prev_cached = cum_cached;
            prev_out = cum_out;

            let is_today = o
                .get("timestamp")
                .and_then(|x| x.as_str())
                .and_then(|ts| DateTime::parse_from_rfc3339(ts).ok())
                .map(|dt| dt.with_timezone(&Local).date_naive() == today)
                .unwrap_or(false);
            if !is_today {
                continue;
            }

            let non_cached_in = (d_in - d_cached).max(0.0);
            totals.tokens += (d_in + d_out) as u64;
            totals.cost += (non_cached_in * price.input
                + d_cached * price.cached_input
                + d_out * price.output)
                / 1_000_000.0;
        }
    }

    Some(totals)
}
