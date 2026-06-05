//! 读取本机 Claude / Codex 的登录凭证。
//! 纯本地读取，不修改任何凭证文件。
use base64::Engine as _;
use std::path::PathBuf;

/// 解析 JWT 的 exp 声明（秒）。无需验签，只用于「是否过期」判断。
fn parse_jwt_exp(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v.get("exp").and_then(|e| e.as_i64())
}

fn home() -> Option<PathBuf> {
    dirs::home_dir()
}

/// Claude Code 凭证：`~/.claude/.credentials.json` 的 `claudeAiOauth`。
#[derive(Clone, Debug)]
pub struct ClaudeCreds {
    pub access_token: String,
    #[allow(dead_code)]
    pub refresh_token: Option<String>,
    /// 过期时间（unix 毫秒）。
    pub expires_at: Option<i64>,
}

impl ClaudeCreds {
    /// token 是否已过期（expiresAt 是毫秒时间戳）。未知则视为未过期。
    pub fn is_expired(&self) -> bool {
        self.expires_at
            .map(|ms| ms < chrono::Utc::now().timestamp_millis())
            .unwrap_or(false)
    }
}

/// 返回值含义：
/// - `Ok(None)`  → 本机未安装 / 未登录（无凭证文件或无 token）
/// - `Ok(Some)`  → 读到凭证
pub fn read_claude() -> Option<ClaudeCreds> {
    let path = home()?.join(".claude").join(".credentials.json");
    let txt = std::fs::read_to_string(&path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&txt).ok()?;
    let o = v.get("claudeAiOauth")?;
    let access = o.get("accessToken")?.as_str()?.to_string();
    if access.is_empty() {
        return None;
    }
    Some(ClaudeCreds {
        access_token: access,
        refresh_token: o.get("refreshToken").and_then(|x| x.as_str()).map(String::from),
        expires_at: o.get("expiresAt").and_then(|x| x.as_i64()),
    })
}

/// 是否存在 `~/.claude` 目录（判断该工具是否被使用过）。
pub fn claude_present() -> bool {
    home()
        .map(|h| h.join(".claude").exists())
        .unwrap_or(false)
}

/// Codex 凭证：`~/.codex/auth.json`，token 可能在顶层或嵌套 `tokens` 对象里。
#[derive(Clone, Debug)]
pub struct CodexCreds {
    pub access_token: String,
    pub account_id: Option<String>,
    #[allow(dead_code)]
    pub refresh_token: Option<String>,
}

impl CodexCreds {
    /// access_token 是 JWT，从 exp 声明（秒）判断是否已过期。未知则视为未过期。
    pub fn is_expired(&self) -> bool {
        parse_jwt_exp(&self.access_token)
            .map(|exp| exp < chrono::Utc::now().timestamp())
            .unwrap_or(false)
    }
}

pub fn read_codex() -> Option<CodexCreds> {
    let path = home()?.join(".codex").join("auth.json");
    let txt = std::fs::read_to_string(&path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&txt).ok()?;

    // tokens 既可能在 v.tokens 下，也可能直接在顶层。
    let t = v.get("tokens").filter(|x| x.is_object()).unwrap_or(&v);

    let access = t
        .get("access_token")
        .or_else(|| v.get("access_token"))
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    if access.is_empty() {
        return None;
    }
    let account_id = t
        .get("account_id")
        .or_else(|| v.get("account_id"))
        .and_then(|x| x.as_str())
        .map(String::from);
    let refresh_token = t
        .get("refresh_token")
        .or_else(|| v.get("refresh_token"))
        .and_then(|x| x.as_str())
        .map(String::from);

    Some(CodexCreds {
        access_token: access,
        account_id,
        refresh_token,
    })
}

pub fn codex_present() -> bool {
    home()
        .map(|h| h.join(".codex").exists())
        .unwrap_or(false)
}
