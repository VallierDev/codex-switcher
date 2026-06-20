use base64::{engine::general_purpose, Engine as _};
use rand::{rng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;

/// OpenAI 官方授权常量 (参考 codex-main)
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const AUTH_URL: &str = "https://auth.openai.com/oauth/authorize";
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

/// 进程级共享 reqwest::Client：连接池可复用，避免每次 quota 刷新都跑 TLS 握手。
/// 给整个请求加了一个 connect+request 双限，避免少数账号让 /oauth/token 无限期挂起
/// （之前 heydsoneicke@gmail.com 的 "刷新不回来" 就是这个 case）。
fn token_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(6))
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(8)
            .build()
            .expect("build shared oauth reqwest client")
    })
}

/// PKCE 相关的代码
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PkceCodes {
    pub code_verifier: String,
    pub code_challenge: String,
}

/// 生成 PKCE 代码对 (与官方一致: 64字节)
pub fn generate_pkce() -> PkceCodes {
    let mut bytes = [0u8; 64];
    rng().fill_bytes(&mut bytes);

    // 生成 verifier (Base64URL 编码)
    let code_verifier = general_purpose::URL_SAFE_NO_PAD.encode(bytes);

    // 生成 challenge (SHA256 哈希后 Base64URL 编码)
    let mut hasher = Sha256::new();
    hasher.update(code_verifier.as_bytes());
    let code_challenge = general_purpose::URL_SAFE_NO_PAD.encode(hasher.finalize());

    PkceCodes {
        code_verifier,
        code_challenge,
    }
}

/// 令牌响应结构
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
    pub expires_in: Option<u64>,
}

/// 用户信息预提取 (通过解析 id_token)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserInfo {
    pub email: String,
    pub account_id: Option<String>,
}

/// 使用授权码交换访问令牌 (与官方一致: 手动拼接请求体)
pub async fn exchange_code(
    code: &str,
    redirect_uri: &str,
    code_verifier: &str,
) -> Result<TokenResponse, String> {
    // 官方格式: 手动拼接字符串
    let body = format!(
        "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&code_verifier={}",
        urlencoding::encode(code),
        urlencoding::encode(redirect_uri),
        urlencoding::encode(CLIENT_ID),
        urlencoding::encode(code_verifier)
    );

    let response = token_client()
        .post(TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        // 真 codex 的 token 请求走 default_client，必带 originator + UA；对齐之。
        .header("User-Agent", crate::codex_ua::codex_user_agent())
        .header("originator", crate::codex_ua::CODEX_ORIGINATOR)
        .timeout(Duration::from_secs(20))
        .body(body)
        .send()
        .await
        .map_err(|e| format!("请求令牌失败: {}", e))?;

    if !response.status().is_success() {
        let error_body = response.text().await.unwrap_or_default();
        return Err(format!("OpenAI 返回错误: {}", error_body));
    }

    response
        .json::<TokenResponse>()
        .await
        .map_err(|e| format!("解析令牌响应失败: {}", e))
}

/// per-account rt 锁。OpenAI 的 rt 是单次使用 + 轮换：同账号并发 refresh 会让一方
/// 拿到 `refresh_token_reused`，rt 链断掉 → 死号。这把锁保证同账号同时只有一次
/// `refresh_access_token` 在飞，串行化所有调用点（Server 内的 quota_refresh /
/// keepalive / anchor / fetch_token / UI 触发）。**所有 rt 旋转都应该走 `_locked`
/// 版本**，裸 `refresh_access_token` 只留给 OAuth 初次换码（无 race，账号还没建）。
fn rt_lock_for(account_id: &str) -> Arc<AsyncMutex<()>> {
    static LOCKS: OnceLock<StdMutex<HashMap<String, Arc<AsyncMutex<()>>>>> = OnceLock::new();
    let map = LOCKS.get_or_init(|| StdMutex::new(HashMap::new()));
    let mut g = map.lock().expect("rt-locks map poisoned");
    g.entry(account_id.to_string())
        .or_insert_with(|| Arc::new(AsyncMutex::new(())))
        .clone()
}

/// 锁保护版的 `refresh_access_token`：同 account_id 强串行。
/// 任何 Server 端调 OpenAI rotate rt 的地方都该用这个；裸版本只留给 OAuth 初次换码。
pub async fn refresh_access_token_locked(
    account_id: &str,
    refresh_token: &str,
) -> Result<TokenResponse, String> {
    let lock = rt_lock_for(account_id);
    let _guard = lock.lock().await;
    refresh_access_token(refresh_token).await
}

/// 使用刷新令牌获取新访问令牌
pub async fn refresh_access_token(refresh_token: &str) -> Result<TokenResponse, String> {
    let params = [
        ("grant_type", "refresh_token"),
        ("client_id", CLIENT_ID),
        ("refresh_token", refresh_token),
        ("scope", "openid profile email offline_access"),
    ];

    // 关键：之前没 timeout，OpenAI 边缘把这个账号 hang 住时整条 quota 刷新永久卡死。
    // 15s 是经验值：正常 < 1s 完成，10s+ 基本可以判定为边缘节流/限流。
    //
    // 网络重试：Server 经 192.168.2.250→38 出口到 auth.openai.com，send 层抖动
    // （"error sending request" / 超时 / 连接重置）是常态。send 失败说明请求多半没到
    // OpenAI、rt 没被消耗，重试是安全的；且能避免上层把网络抖动误判成 token 失效。
    // 只对 send 层错误重试，HTTP 已返回（含 invalid_grant 拒绝）一律不重试。
    let mut last_err = String::new();
    let mut response = None;
    for attempt in 0..3 {
        match token_client()
            .post(TOKEN_URL)
            .header("Content-Type", "application/x-www-form-urlencoded")
            // 真 codex 的 token 请求走 default_client，必带 originator + UA；对齐之。
            .header("User-Agent", crate::codex_ua::codex_user_agent())
            .header("originator", crate::codex_ua::CODEX_ORIGINATOR)
            .timeout(Duration::from_secs(15))
            .form(&params)
            .send()
            .await
        {
            Ok(r) => {
                response = Some(r);
                break;
            }
            Err(e) => {
                last_err = format!("刷新令牌失败: {}", e);
                if attempt < 2 {
                    // 200ms / 600ms 退避，避开瞬时抖动
                    tokio::time::sleep(Duration::from_millis(200 * (attempt as u64 * 2 + 1)))
                        .await;
                }
            }
        }
    }
    let response = response.ok_or(last_err)?;

    if !response.status().is_success() {
        let error_body = response.text().await.unwrap_or_default();
        return Err(format!("刷新令牌被拒绝: {}", error_body));
    }

    response
        .json::<TokenResponse>()
        .await
        .map_err(|e| format!("解析刷新响应失败: {}", e))
}

/// 从 ID Token 中提取用户信息 (JWT 解析)
pub fn parse_user_info(id_token: &str) -> Option<UserInfo> {
    let parts: Vec<&str> = id_token.split('.').collect();
    if parts.len() < 2 {
        return None;
    }

    let payload = general_purpose::URL_SAFE_NO_PAD.decode(parts[1]).ok()?;
    let json: serde_json::Value = serde_json::from_slice(&payload).ok()?;

    let email = json.get("email")?.as_str()?.to_string();

    // 从 OpenAI 特有的 claims 中获取 account_id
    let account_id = json
        .get("https://api.openai.com/auth")
        .and_then(|v| v.get("chatgpt_account_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    Some(UserInfo { email, account_id })
}
