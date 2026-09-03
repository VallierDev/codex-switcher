//! Remote Mode — 本机 client 模块
//!
//! 负责向 Server 侧 HTTP API 发起请求。所有函数纯异步，返回 Result。
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::account::Account;

const AUTH_HEADER: &str = "X-Auth-Token";
const DEFAULT_TIMEOUT_SECS: u64 = 10;
const PROBE_TIMEOUT_SECS: u64 = 2;
// 之前 60s：每分钟一次重探，破网情况下每分钟烧一次 2s LAN timeout（即便已经知道 LAN
// 不通）。300s 折衷：缓存命中率高得多，路径切换（家 LAN 回到办公室 WiFi）也只要等 5 min。
const CACHE_TTL_SECS: u64 = 300;

struct UrlCache {
    url: String,
    at: Instant,
}

fn url_cache() -> &'static Mutex<Option<UrlCache>> {
    static CACHE: OnceLock<Mutex<Option<UrlCache>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

fn cached_url() -> Option<String> {
    let guard = url_cache().lock().ok()?;
    let c = guard.as_ref()?;
    if c.at.elapsed() < Duration::from_secs(CACHE_TTL_SECS) {
        Some(c.url.clone())
    } else {
        None
    }
}

fn set_cached_url(url: &str) {
    if let Ok(mut g) = url_cache().lock() {
        *g = Some(UrlCache {
            url: url.to_string(),
            at: Instant::now(),
        });
    }
}

pub fn invalidate_cached_url() {
    if let Ok(mut g) = url_cache().lock() {
        *g = None;
    }
}

async fn probe(url: &str) -> bool {
    // `.no_proxy()` 极重要：Server 永远是 LAN/ZeroTier 私有 IP，必须绕开系统代理。
    // 默认 reqwest 会读 macOS 系统代理（如用户开了 Clash 监听 127.0.0.1:7890）
    // 把所有 HTTP 请求都路由过去，Clash 处理不了 192.168/172.16 私有 IP → timeout。
    let Ok(c) = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(PROBE_TIMEOUT_SECS))
        .build()
    else {
        eprintln!("[remote_client] probe: client builder 失败 url={}", url);
        return false;
    };
    let probe_url = format!("{}/health", trim_url(url));
    let t0 = std::time::Instant::now();
    match c.get(&probe_url).send().await {
        Ok(r) => {
            let ok = r.status().is_success();
            eprintln!(
                "[remote_client] probe {} → HTTP {} ok={} t={:?}",
                probe_url,
                r.status().as_u16(),
                ok,
                t0.elapsed()
            );
            ok
        }
        Err(e) => {
            // 把 reqwest 的完整错误链打出来，能区分 timeout / connect refused /
            // permission denied (macOS Sequoia Local Network) / DNS 等。
            let mut chain = format!("{}", e);
            let mut source = std::error::Error::source(&e);
            while let Some(s) = source {
                chain.push_str(" | caused by: ");
                chain.push_str(&format!("{}", s));
                source = s.source();
            }
            eprintln!(
                "[remote_client] probe {} → ERR ({}) t={:?}",
                probe_url,
                chain,
                t0.elapsed()
            );
            false
        }
    }
}

/// 从 primary 和 fallback 中挑一个可用地址，带 300s 缓存。
///
/// 之前是「先串行探 primary（2s timeout）再 fallback」—— 用户网络切到不同子网时
/// LAN 不通，每次缓存过期都白白卡 2s 才用上 ZT。改成 **并行探测**：两条都起，谁先
/// 返 OK 用谁。最坏情况两条都坏，并行等到双方 timeout（仍是 2s 上限，比之前 4s 好）。
pub async fn resolve_base_url(primary: &str, fallback: &str) -> Result<String, String> {
    let p = primary.trim().to_string();
    let f = fallback.trim().to_string();
    if p.is_empty() && f.is_empty() {
        return Err("未配置 Server 地址".to_string());
    }
    // 缓存命中：必须匹配当前配置（防止 settings 改完仍用旧 URL）
    if let Some(c) = cached_url() {
        if c == p || c == f {
            return Ok(c);
        }
    }
    // 只配了一个的简单分支
    if p.is_empty() {
        if probe(&f).await {
            set_cached_url(&f);
            return Ok(f);
        }
        return Err(format!("Server 不可达（fallback={}）", f));
    }
    if f.is_empty() {
        if probe(&p).await {
            set_cached_url(&p);
            return Ok(p);
        }
        return Err(format!("Server 不可达（primary={}）", p));
    }

    // 双地址：并行探测，第一个返 OK 的赢。
    // futures_util::select 要求 future 实现 Unpin，pin_mut 给堆栈固定。
    use futures_util::{
        future::{select, Either},
        pin_mut,
    };
    let p_fut = async {
        let ok = probe(&p).await;
        (p.clone(), ok)
    };
    let f_fut = async {
        let ok = probe(&f).await;
        (f.clone(), ok)
    };
    pin_mut!(p_fut);
    pin_mut!(f_fut);

    let winner = match select(p_fut, f_fut).await {
        Either::Left(((url, true), _)) => Some(url),
        Either::Right(((url, true), _)) => Some(url),
        // 先回来的那条失败了 —— 等另一条
        Either::Left(((_, false), other)) => {
            let (url, ok) = other.await;
            if ok {
                Some(url)
            } else {
                None
            }
        }
        Either::Right(((_, false), other)) => {
            let (url, ok) = other.await;
            if ok {
                Some(url)
            } else {
                None
            }
        }
    };

    if let Some(url) = winner {
        set_cached_url(&url);
        Ok(url)
    } else {
        Err(format!("Server 不可达（primary={}, fallback={}）", p, f))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteHealth {
    pub mode: String,
    pub version: String,
    pub account_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteToken {
    pub auth_json: Value,
    pub refresh_token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteAntigravityToken {
    pub account_id: String,
    pub access_token: String,
    pub project_id: String,
    pub expires_at: Option<String>,
}

/// 从 Mini Server 获取 Google 短期 access token 租约。
/// Server 按需刷新 RT；Client 只在内存中缓存 ST，不写 accounts.json。
pub async fn fetch_antigravity_token(
    base_url: &str,
    secret: &str,
    id: &str,
    force_refresh: bool,
) -> Result<RemoteAntigravityToken, String> {
    let url = format!("{}/accounts/{}/antigravity-token", trim_url(base_url), id);
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|error| error.to_string())?;
    let request = if force_refresh {
        client.post(url)
    } else {
        client.get(url)
    };
    let response = request
        .header(AUTH_HEADER, secret)
        .send()
        .await
        .map_err(|error| format!("Server Google token 租约失败: {error}"))?;
    let status = response.status();
    let body: Value = response.json().await.map_err(|error| error.to_string())?;
    if !status.is_success() {
        // Rolling upgrade: the old Mini build only has the generic token endpoint.
        // Read it once, project the response down to an ST-only in-memory lease, and
        // discard the returned auth document immediately. Never persist its RT locally.
        if !force_refresh
            && matches!(
                status,
                reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::METHOD_NOT_ALLOWED
            )
        {
            let legacy = fetch_token(base_url, secret, id).await?;
            let tokens = legacy
                .auth_json
                .get("tokens")
                .ok_or_else(|| "Legacy Server Google token response has no tokens".to_string())?;
            return Ok(RemoteAntigravityToken {
                account_id: id.to_string(),
                access_token: tokens
                    .get("access_token")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "Legacy Server Google token response has no ST".to_string())?
                    .to_string(),
                project_id: legacy
                    .auth_json
                    .get("project_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        "Legacy Server Google token response has no project_id".to_string()
                    })?
                    .to_string(),
                expires_at: tokens
                    .get("expires_at")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
            });
        }
        return Err(body
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("Server Google token 租约失败")
            .to_string());
    }
    serde_json::from_value(body).map_err(|error| error.to_string())
}

#[derive(Debug, Clone, Deserialize)]
pub struct RemoteQuotaEntry {
    pub id: String,
    #[serde(default)]
    pub cached_quota: Option<crate::account::CachedQuota>,
    #[serde(default)]
    pub antigravity_model_quotas: Option<Value>,
    #[serde(default)]
    pub window_priming: Option<crate::account::WindowPrimingState>,
    #[serde(default)]
    pub is_banned: bool,
    #[serde(default)]
    pub is_token_invalid: bool,
    #[serde(default)]
    pub is_logged_out: bool,
}

fn client() -> Result<Client, String> {
    // 同 probe()：绕开系统代理，避免 Clash 截走 LAN/ZeroTier 流量。
    Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
        .build()
        .map_err(|e| format!("构建 HTTP client 失败: {}", e))
}

fn trim_url(base: &str) -> String {
    base.trim_end_matches('/').to_string()
}

/// 健康检查（无需密钥也可拿到 Server 版本等信息）
pub async fn health(base_url: &str) -> Result<RemoteHealth, String> {
    let url = format!("{}/health", trim_url(base_url));
    let resp = client()?
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("连接 Server 失败: {}", e))?;
    if !resp.status().is_success() {
        return Err(format!("Server /health 返回 {}", resp.status()));
    }
    resp.json::<RemoteHealth>()
        .await
        .map_err(|e| format!("解析 /health 响应失败: {}", e))
}

/// 测试连接+密钥是否正确（会拉 /accounts 看是否 200）
pub async fn test_auth(base_url: &str, secret: &str) -> Result<RemoteHealth, String> {
    let h = health(base_url).await?;
    let url = format!("{}/accounts", trim_url(base_url));
    let resp = client()?
        .get(&url)
        .header(AUTH_HEADER, secret)
        .send()
        .await
        .map_err(|e| format!("连接失败: {}", e))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err("共享密钥不正确".to_string());
    }
    if !resp.status().is_success() {
        return Err(format!("Server /accounts 返回 {}", resp.status()));
    }
    Ok(h)
}

/// 列出 Server 上的所有账号
pub async fn list_accounts(base_url: &str, secret: &str) -> Result<Vec<Account>, String> {
    let url = format!("{}/accounts", trim_url(base_url));
    let resp = client()?
        .get(&url)
        .header(AUTH_HEADER, secret)
        .send()
        .await
        .map_err(|e| format!("请求失败: {}", e))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err("共享密钥不正确".to_string());
    }
    if !resp.status().is_success() {
        return Err(format!("Server 返回 {}", resp.status()));
    }
    let body: Value = resp.json().await.map_err(|e| e.to_string())?;
    let arr = body
        .get("accounts")
        .ok_or("响应缺少 accounts 字段")?
        .clone();
    serde_json::from_value(arr).map_err(|e| format!("反序列化账号列表失败: {}", e))
}

/// Google-only, token-free mirrors. This must not use the generic credential list.
pub async fn list_antigravity_accounts(
    base_url: &str,
    secret: &str,
) -> Result<Vec<Account>, String> {
    let response = client()?
        .get(format!("{}/antigravity/accounts", trim_url(base_url)))
        .header(AUTH_HEADER, secret)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!(
            "Google account sync returned HTTP {}",
            response.status()
        ));
    }
    let body: Value = response.json().await.map_err(|e| e.to_string())?;
    let accounts: Vec<Account> =
        serde_json::from_value(body.get("accounts").cloned().unwrap_or(Value::Null))
            .map_err(|e| e.to_string())?;
    Ok(tokenless_google_accounts(accounts))
}

fn tokenless_google_accounts(mut accounts: Vec<Account>) -> Vec<Account> {
    accounts.retain(|account| account.is_antigravity_oauth());
    // Defense in depth: rebuild the allowed metadata even if Server regresses.
    for account in &mut accounts {
        account.refresh_token = None;
        account.auth_json = serde_json::json!({
            "provider":"antigravity", "email":account.name,
            "project_id":account.auth_json.get("project_id"),
            "model_quotas":account.auth_json.get("model_quotas"),
            "subscription_tier":account.auth_json.get("subscription_tier"),
            "subscription_tier_checked_at":account.auth_json.get("subscription_tier_checked_at"),
        });
    }
    accounts
}

#[cfg(test)]
mod google_mirror_tests {
    use super::*;

    #[test]
    fn incoming_mirrors_cannot_carry_tokens_or_codex_accounts() {
        let mut store = crate::account::AccountStore::default();
        let google = store.add_antigravity_account("example@gmail.com".into(), serde_json::json!({
            "project_id":"p", "model_quotas":{"gemini-test":{"remaining_fraction":0.9}},
            "subscription_tier":"pro", "subscription_tier_checked_at":"now",
            "access_token":"top-secret-st", "tokens":{"access_token":"secret-st", "refresh_token":"secret-rt"},
        }), None);
        let mut codex = google.clone();
        codex.kind = crate::account::AccountKind::ChatgptOauth;
        let mirrors = tokenless_google_accounts(vec![google, codex]);
        assert_eq!(mirrors.len(), 1);
        assert!(mirrors[0].refresh_token.is_none());
        assert_eq!(mirrors[0].auth_json["project_id"], "p");
        assert_eq!(mirrors[0].auth_json["subscription_tier"], "pro");
        assert_eq!(mirrors[0].auth_json["subscription_tier_checked_at"], "now");
        assert_eq!(
            mirrors[0].auth_json["model_quotas"]["gemini-test"]["remaining_fraction"],
            0.9
        );
        let encoded = serde_json::to_string(&mirrors).unwrap();
        assert!(!encoded.contains("secret-st"));
        assert!(!encoded.contains("secret-rt"));
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpsertOutcome {
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub upserted: String,
    #[serde(default)]
    pub quota_refreshed: bool,
    #[serde(default)]
    pub quota_error: Option<String>,
}

/// 上传（upsert）单个账号到 Server。Server 会在写入后尝试刷新一次额度。
pub async fn upsert_account(
    base_url: &str,
    secret: &str,
    account: &Account,
) -> Result<UpsertOutcome, String> {
    let url = format!("{}/accounts", trim_url(base_url));
    let payload = serde_json::json!({ "account": account });
    // 上传 + 服务端刷额度 可能 10+ 秒，临时给 45s 超时；
    // `.no_proxy()` 极重要：Server 是 LAN/ZeroTier 私有 IP，不绕过 macOS 系统代理（Clash）
    // 会被截走 → 502 Bad Gateway。同 client() 设置。
    let c = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(45))
        .build()
        .map_err(|e| format!("创建 HTTP 客户端失败: {}", e))?;
    let resp = c
        .post(&url)
        .header(AUTH_HEADER, secret)
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(&payload).map_err(|e| e.to_string())?)
        .send()
        .await
        .map_err(|e| format!("POST 失败: {}", e))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err("共享密钥不正确".to_string());
    }
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Server 返回 {}: {}", status, body));
    }
    resp.json::<UpsertOutcome>()
        .await
        .map_err(|e| format!("解析 upsert 响应失败: {}", e))
}

/// Client 模式把 Google OAuth code 交给权威 Server 完成交换。
/// 返回值是去除 token 的账号镜像，真实 refresh token 只落 Server。
pub async fn complete_antigravity_oauth(
    base_url: &str,
    secret: &str,
    code: &str,
    redirect_uri: &str,
) -> Result<Account, String> {
    let url = format!("{}/antigravity/oauth/complete", trim_url(base_url));
    let response = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| format!("创建 HTTP 客户端失败: {e}"))?
        .post(url)
        .header(AUTH_HEADER, secret)
        .json(&serde_json::json!({"code": code, "redirect_uri": redirect_uri}))
        .send()
        .await
        .map_err(|e| format!("Server Google OAuth 请求失败: {e}"))?;
    let status = response.status();
    let body: Value = response.json().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(body
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("Server Google OAuth 失败")
            .to_string());
    }
    serde_json::from_value(
        body.get("account")
            .cloned()
            .ok_or_else(|| "Server OAuth 响应缺少 account".to_string())?,
    )
    .map_err(|e| format!("解析 Google 账号镜像失败: {e}"))
}

/// 删除 Server 上指定账号
pub async fn delete_account(base_url: &str, secret: &str, id: &str) -> Result<(), String> {
    let url = format!("{}/accounts/{}", trim_url(base_url), id);
    let resp = client()?
        .delete(&url)
        .header(AUTH_HEADER, secret)
        .send()
        .await
        .map_err(|e| format!("DELETE 失败: {}", e))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err("共享密钥不正确".to_string());
    }
    if !resp.status().is_success() && resp.status() != reqwest::StatusCode::NOT_FOUND {
        return Err(format!("Server 返回 {}", resp.status()));
    }
    Ok(())
}

/// 拉取 Server 上所有账号的配额数据（client 模式下替代本地 quota_refresh）
pub async fn refresh_antigravity_quota(
    base_url: &str,
    secret: &str,
    id: &str,
) -> Result<std::collections::HashMap<String, crate::antigravity::quota::ModelQuota>, String> {
    let response = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(100))
        .build()
        .map_err(|error| error.to_string())?
        .post(format!(
            "{}/accounts/{}/antigravity-quota",
            trim_url(base_url),
            id
        ))
        .header(AUTH_HEADER, secret)
        .send()
        .await
        .map_err(|error| format!("Google 额度查询失败: {error}"))?;
    let status = response.status();
    let body: Value = response.json().await.map_err(|error| error.to_string())?;
    if !status.is_success() {
        return Err(body
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("Server Google 额度查询失败")
            .to_string());
    }
    serde_json::from_value(
        body.get("model_quotas")
            .cloned()
            .ok_or("响应缺少模型额度")?,
    )
    .map_err(|error| error.to_string())
}

pub async fn fetch_all_quota(
    base_url: &str,
    secret: &str,
) -> Result<Vec<RemoteQuotaEntry>, String> {
    let url = format!("{}/quotas", trim_url(base_url));
    let resp = client()?
        .get(&url)
        .header(AUTH_HEADER, secret)
        .send()
        .await
        .map_err(|e| format!("GET /quotas 失败: {}", e))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err("共享密钥不正确".to_string());
    }
    if !resp.status().is_success() {
        return Err(format!("Server 返回 {}", resp.status()));
    }
    let body: Value = resp.json().await.map_err(|e| e.to_string())?;
    let arr = body.get("quotas").ok_or("响应缺少 quotas 字段")?.clone();
    serde_json::from_value(arr).map_err(|e| format!("反序列化 quotas 失败: {}", e))
}

#[derive(Debug, Clone, Deserialize)]
pub struct RemoteCurrent {
    pub current: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub cached_quota: Option<crate::account::CachedQuota>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RemoteSwitchOutcome {
    #[serde(default)]
    pub switched: bool,
    #[serde(default)]
    pub stale: bool,
    #[serde(default)]
    pub exhausted: bool,
    #[serde(default)]
    pub current: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub earliest_reset_at: Option<i64>,
}

pub async fn get_current(base_url: &str, secret: &str) -> Result<RemoteCurrent, String> {
    let url = format!("{}/current", trim_url(base_url));
    let resp = client()?
        .get(&url)
        .header(AUTH_HEADER, secret)
        .send()
        .await
        .map_err(|e| format!("GET /current 失败: {}", e))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err("共享密钥不正确".to_string());
    }
    if !resp.status().is_success() {
        return Err(format!("Server 返回 {}", resp.status()));
    }
    resp.json::<RemoteCurrent>()
        .await
        .map_err(|e| format!("解析 /current 响应失败: {}", e))
}

pub async fn request_switch(
    base_url: &str,
    secret: &str,
    from: Option<&str>,
    reason: &str,
) -> Result<RemoteSwitchOutcome, String> {
    let url = format!("{}/switch", trim_url(base_url));
    let body = serde_json::json!({ "from": from, "reason": reason });
    let resp = client()?
        .post(&url)
        .header(AUTH_HEADER, secret)
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(&body).map_err(|e| e.to_string())?)
        .send()
        .await
        .map_err(|e| format!("POST /switch 失败: {}", e))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err("共享密钥不正确".to_string());
    }
    if !resp.status().is_success() {
        return Err(format!("Server 返回 {}", resp.status()));
    }
    resp.json::<RemoteSwitchOutcome>()
        .await
        .map_err(|e| format!("解析 /switch 响应失败: {}", e))
}

/// 列出 Server 已安装的 skill 目录名
pub async fn list_remote_skills(base_url: &str, secret: &str) -> Result<Vec<String>, String> {
    let url = format!("{}/skills", trim_url(base_url));
    let resp = client()?
        .get(&url)
        .header(AUTH_HEADER, secret)
        .send()
        .await
        .map_err(|e| format!("GET /skills 失败: {}", e))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err("共享密钥不正确".to_string());
    }
    if !resp.status().is_success() {
        return Err(format!("Server 返回 {}", resp.status()));
    }
    let body: Value = resp.json().await.map_err(|e| e.to_string())?;
    let arr = body.get("skills").ok_or("响应缺少 skills 字段")?.clone();
    serde_json::from_value(arr).map_err(|e| format!("反序列化 skills 失败: {}", e))
}

/// 将一个 skill zip 推送到 Server
pub async fn upload_skill(
    base_url: &str,
    secret: &str,
    name: &str,
    zip_bytes: Vec<u8>,
) -> Result<(), String> {
    let url = format!(
        "{}/skills/upload?name={}",
        trim_url(base_url),
        url_encode(name)
    );
    let resp = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| format!("构建 HTTP client 失败: {}", e))?
        .post(&url)
        .header(AUTH_HEADER, secret)
        .header("Content-Type", "application/zip")
        .body(zip_bytes)
        .send()
        .await
        .map_err(|e| format!("POST /skills/upload 失败: {}", e))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err("共享密钥不正确".to_string());
    }
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Server 返回 {}: {}", status, body));
    }
    Ok(())
}

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// 请 Server 对某账号执行一次额度刷新，返回最新 UsageDisplay。
/// client 模式下本机不持 token，刷新必须由 Server 完成。
pub async fn refresh_account_quota(
    base_url: &str,
    secret: &str,
    id: &str,
) -> Result<crate::usage::UsageDisplay, String> {
    let url = format!("{}/accounts/{}/refresh", trim_url(base_url), id);
    // oauth refresh + /usage 可能 15s+，给 45s 超时
    // `.no_proxy()` 同 upsert_account：Server 是 LAN/ZeroTier 私有 IP
    let c = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(45))
        .build()
        .map_err(|e| format!("创建 HTTP 客户端失败: {}", e))?;
    let resp = c
        .post(&url)
        .header(AUTH_HEADER, secret)
        .send()
        .await
        .map_err(|e| format!("POST /accounts/{}/refresh 失败: {}", id, e))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err("共享密钥不正确".to_string());
    }
    let status = resp.status();
    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("解析刷新响应失败: {}", e))?;
    if !status.is_success() {
        let err = body
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("未知错误")
            .to_string();
        return Err(err);
    }
    let usage = body.get("usage").cloned().ok_or("响应缺少 usage 字段")?;
    let parsed: crate::usage::UsageDisplay =
        serde_json::from_value(usage).map_err(|e| format!("反序列化 usage 失败: {}", e))?;
    // 防御：旧版 Server 会把上游 503/空体「解析」成 plan_type=unknown + 100%/「未知」。
    // 那是假额度，绝不能写进本机 cached_quota 覆盖真实数据。Server 升级后此分支不应再触发。
    let plan = parsed.plan_type.to_lowercase();
    let looks_empty = (plan == "unknown" || plan.is_empty())
        && parsed.five_hour_reset == "未知"
        && parsed.weekly_reset == "未知"
        && parsed.five_hour_reset_at.is_none()
        && parsed.weekly_reset_at.is_none();
    if looks_empty {
        return Err(
            "USAGE_EMPTY_FROM_SERVER: Server 返回了空额度(unknown/未知)，可能是上游 503 被旧版误解析；已拒绝写缓存，请升级 Mini Mac 端或稍后重试"
                .to_string(),
        );
    }
    Ok(parsed)
}

/// solo 模式心跳：通知 Server "本机在接管全部保活"。
/// Server 收到后 TTL 内会跳过自己的 quota_refresh 循环，避免双端同时 refresh 撞 rotate。
pub async fn send_solo_heartbeat(
    base_url: &str,
    secret: &str,
    ttl_secs: i64,
) -> Result<(), String> {
    let url = format!("{}/solo/heartbeat", trim_url(base_url));
    let body = serde_json::json!({ "ttl_secs": ttl_secs });
    let resp = client()?
        .post(&url)
        .header(AUTH_HEADER, secret)
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(&body).map_err(|e| e.to_string())?)
        .send()
        .await
        .map_err(|e| format!("心跳失败: {}", e))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err("共享密钥不正确".to_string());
    }
    if !resp.status().is_success() {
        return Err(format!("Server 返回 {}", resp.status()));
    }
    Ok(())
}

/// 客户端切号同步：告诉 Server 本机刚切到了 new_id。
///
/// `apply_to_disk` 区分两种语义：
/// - `true`（client 模式）：Server 也跟着切——写 ~/.codex/auth.json，让 Server 那台机器的
///   codex 也用同一个号工作。两台机器永远在同一账号上，cache 一致、协作顺畅。
/// - `false`（solo 模式）：Server 仅更新 current 指针归档，**不写盘**。Server 那边可能
///   独立跑别的任务，不能被本机劫持。
pub async fn push_solo_switch(
    base_url: &str,
    secret: &str,
    new_id: &str,
    apply_to_disk: bool,
) -> Result<(), String> {
    let url = format!("{}/solo/current", trim_url(base_url));
    let body = serde_json::json!({ "current": new_id, "apply_to_disk": apply_to_disk });
    let resp = client()?
        .post(&url)
        .header(AUTH_HEADER, secret)
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(&body).map_err(|e| e.to_string())?)
        .send()
        .await
        .map_err(|e| format!("push /solo/current 失败: {}", e))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err("共享密钥不正确".to_string());
    }
    if !resp.status().is_success() {
        return Err(format!("Server 返回 {}", resp.status()));
    }
    Ok(())
}

/// 拉取指定账号最新的 auth_json（Server 侧保活已保证新鲜）
pub async fn fetch_token(base_url: &str, secret: &str, id: &str) -> Result<RemoteToken, String> {
    let url = format!("{}/accounts/{}/token", trim_url(base_url), id);
    let resp = client()?
        .get(&url)
        .header(AUTH_HEADER, secret)
        .send()
        .await
        .map_err(|e| format!("GET token 失败: {}", e))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err("共享密钥不正确".to_string());
    }
    if !resp.status().is_success() {
        return Err(format!("Server 返回 {}", resp.status()));
    }
    resp.json::<RemoteToken>()
        .await
        .map_err(|e| format!("解析 token 响应失败: {}", e))
}

/// 让 Server 就地强制刷新指定账号的 access_token，返回刷新后的 auth_json。
/// 手机锚保活用:client 拉到的锚 token 快过期时打这个,Server 走单刷新者 locked_fresh
/// 路径刷新(不新增 reused 源),client 再把返回的新 token 写盘。
pub async fn refresh_token_now(
    base_url: &str,
    secret: &str,
    id: &str,
) -> Result<RemoteToken, String> {
    let url = format!("{}/accounts/{}/refresh-token", trim_url(base_url), id);
    let resp = client()?
        .post(&url)
        .header(AUTH_HEADER, secret)
        .send()
        .await
        .map_err(|e| format!("POST refresh-token 失败: {}", e))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err("共享密钥不正确".to_string());
    }
    if !resp.status().is_success() {
        return Err(format!("Server 返回 {}", resp.status()));
    }
    resp.json::<RemoteToken>()
        .await
        .map_err(|e| format!("解析 refresh-token 响应失败: {}", e))
}
