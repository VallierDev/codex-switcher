//! Codex Switcher - 用量获取模块
//!
//! 从 OpenAI API 获取 Codex 使用量信息

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::OnceLock;
use std::time::Duration;

/// 进程级共享 reqwest::Client — 整个 quota 刷新链路共用一个连接池，
/// 不再每个账号都跑一次 TLS 握手。30 秒空闲回收，最多 8 个 keep-alive。
fn usage_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(6))
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(8)
            .build()
            .expect("build shared usage reqwest client")
    })
}

/// 前端展示的用量数据
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageDisplay {
    /// 套餐类型
    pub plan_type: String,
    /// 5小时窗口使用百分比
    pub five_hour_used: i32,
    /// 5小时窗口剩余百分比
    pub five_hour_left: i32,
    /// 5小时窗口标签 (如 "5H 限额")
    pub five_hour_label: String,
    /// 5小时重置时间描述
    pub five_hour_reset: String,
    /// 5小时重置时间戳
    pub five_hour_reset_at: Option<i64>,
    /// 周窗口使用百分比
    pub weekly_used: i32,
    /// 周窗口剩余百分比
    pub weekly_left: i32,
    /// 周窗口标签 (如 "周限额")
    pub weekly_label: String,
    /// 周重置时间描述
    pub weekly_reset: String,
    /// 周重置时间戳
    pub weekly_reset_at: Option<i64>,
    /// 额度余额
    pub credits_balance: Option<f64>,
    /// 是否有额度
    pub has_credits: bool,
    /// 主动重置次数（rate_limit_reset_credits.available_count）。None = 接口未返回
    pub reset_credits: Option<i32>,
    /// Token 是否对 CLI 有效 (api.openai.com)
    pub is_valid_for_cli: bool,
}

/// 用量获取器
pub struct UsageFetcher;

impl UsageFetcher {
    /// 从 API 获取用量 (直接使用提供的 Token，不读取 auth.json)
    pub async fn fetch_usage_direct(
        access_token: String,
        account_id: Option<String>,
        refresh_token: Option<String>,
        allow_local_refresh: bool,
    ) -> Result<(UsageDisplay, Option<crate::oauth::TokenResponse>), String> {
        let mut current_token = access_token;
        let mut new_tokens: Option<crate::oauth::TokenResponse> = None;

        let client = usage_client();
        // 与官方 codex CLI 完全同形态的 UA / originator（codex_ua 模块统一构造）。
        let user_agent = crate::codex_ua::codex_user_agent();
        let build_request = |at: &str, aid: &Option<String>| {
            // 12s 是经验值：正常 < 2s，5s+ 已经是慢路径，>12s 基本可以判定为节流/超时。
            // 之前 30s 让 "刷新全部" 的尾延迟被个别慢账号拖很久。
            let mut req = client
                .get("https://chatgpt.com/backend-api/wham/usage")
                .header("Authorization", format!("Bearer {}", at))
                .header("User-Agent", user_agent)
                .header("originator", crate::codex_ua::CODEX_ORIGINATOR)
                .header("Accept", "application/json")
                .timeout(Duration::from_secs(12));
            if let Some(id) = aid {
                req = req.header("ChatGPT-Account-Id", id);
            }
            req
        };

        let mut response = build_request(&current_token, &account_id)
            .send()
            .await
            .map_err(|e| format!("网络请求失败: {}", e))?;

        let mut status = response.status();

        // 如果允许本地刷新，且 401/403 且有 refresh_token，尝试刷新
        // 注意：rt 旋转走 _locked 串行化，同账号并发自动排队不撞 race
        if allow_local_refresh && (status == 401 || status == 403) && refresh_token.is_some() {
            if let Some(ref rt) = refresh_token {
                // 锁 key 优先 account_id（最稳定 = OpenAI workspace id），缺时退到 rt
                // 前缀（rt 跟 OpenAI account 1:1 绑定，唯一性够用，只是切号轮换后失效）
                let lock_key = account_id.clone().unwrap_or_else(|| {
                    format!("rt:{}", &rt[..rt.len().min(16)])
                });
                match crate::oauth::refresh_access_token_locked(&lock_key, rt).await {
                    Ok(token_res) => {
                        current_token = token_res.access_token.clone();
                        new_tokens = Some(token_res);

                        // 重试请求
                        response = build_request(&current_token, &account_id)
                            .send()
                            .await
                            .map_err(|e| format!("刷新后重试失败: {}", e))?;
                        status = response.status();
                    }
                    Err(e) => {
                        let lower = e.to_lowercase();
                        if lower.contains("logged out")
                            || lower.contains("signed in to another account")
                            || lower.contains("invalid_grant")
                        {
                            return Err("ACCOUNT_LOGGED_OUT:您已登出或登录了其他账号，请重新登录"
                                .to_string());
                        }
                    }
                }
            }
        }

        if status == 401 || status == 403 {
            // 读取响应体以检测是否为封号
            let body = response.text().await.unwrap_or_default().to_lowercase();
            let is_banned = body.contains("deactivated")
                || body.contains("banned")
                || body.contains("suspended")
                || body.contains("account_deactivated");

            if is_banned {
                return Err("ACCOUNT_BANNED:该账号已被封禁".to_string());
            }

            if !allow_local_refresh {
                return Err(
                    "当前激活账号访问配额接口返回 401/403；已禁用本地 refresh_token 刷新，请稍后重试或在 Codex 中触发一次请求".to_string(),
                );
            }
            // 如果刷新后仍然 401/403，标记为无效
            return Err("TOKEN_INVALID:授权已失效，请删除该账号后重新登录".to_string());
        }

        let text = response
            .text()
            .await
            .map_err(|e| format!("读取响应失败: {}", e))?;

        let json: Value =
            serde_json::from_str(&text).map_err(|e| format!("解析 JSON 失败: {}", e))?;

        // 检测 200 状态码下的软封号/停用响应，如 {"detail":{"code":"deactivated_workspace"}}
        if let Some(detail_code) = json
            .get("detail")
            .and_then(|d| d.get("code"))
            .and_then(|c| c.as_str())
        {
            let code_lower = detail_code.to_lowercase();
            if code_lower.contains("deactivated")
                || code_lower.contains("banned")
                || code_lower.contains("suspended")
            {
                println!("[Usage] 检测到账号停用: detail.code={}", detail_code);
                return Err("ACCOUNT_BANNED:该账号已被封禁(workspace 已停用)".to_string());
            }
        }

        let display = Self::parse_usage_response(&json)?;

        Ok((display, new_tokens))
    }

    /// 从 Value 解析用量数据
    fn parse_usage_response(json: &Value) -> Result<UsageDisplay, String> {
        let plan_type = json
            .get("plan_type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        let rate_limit = json.get("rate_limit");

        // 解析 5 小时窗口 (Primary)
        let primary_val = rate_limit.and_then(|r| r.get("primary_window"));
        let (p_used, p_reset, p_label, p_reset_at) = Self::parse_window(primary_val, "5H 限额");

        // 解析周窗口 (Secondary)
        let secondary_val = rate_limit.and_then(|r| r.get("secondary_window"));
        let (s_used, s_reset, s_label, s_reset_at) = Self::parse_window(secondary_val, "周限额");

        // 解析额度
        let credits = json.get("credits");
        let has_credits = credits
            .and_then(|c| c.get("has_credits"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let unlimited = credits
            .and_then(|c| c.get("unlimited"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let credits_balance = credits
            .and_then(|c| c.get("balance"))
            .and_then(Self::parse_number);

        // 主动重置次数：rate_limit_reset_credits.available_count
        let reset_credits = json
            .get("rate_limit_reset_credits")
            .and_then(|r| r.get("available_count"))
            .and_then(Self::parse_number)
            .map(|f| f as i32);

        Ok(UsageDisplay {
            plan_type,
            five_hour_used: p_used,
            five_hour_left: 100 - p_used,
            five_hour_label: p_label,
            five_hour_reset: p_reset,
            five_hour_reset_at: p_reset_at,
            weekly_used: s_used,
            weekly_left: 100 - s_used,
            weekly_label: s_label,
            weekly_reset: s_reset,
            weekly_reset_at: s_reset_at,
            credits_balance,
            has_credits: has_credits || unlimited,
            reset_credits,
            is_valid_for_cli: true,
        })
    }

    /// 解析窗口数据
    fn parse_window(
        window: Option<&Value>,
        default_label: &str,
    ) -> (i32, String, String, Option<i64>) {
        let window = match window {
            Some(w) => w,
            None => return (0, "未知".to_string(), default_label.to_string(), None),
        };

        // 关键修复：使用 f64 解析百分比，然后四舍五入
        let used_percent = window
            .get("used_percent")
            .and_then(Self::parse_number)
            .map(|f| f.round() as i32)
            .unwrap_or(0);

        let reset_at = window
            .get("reset_at")
            .and_then(Self::parse_number)
            .map(|f| f as i64);

        let limit_window_seconds = window
            .get("limit_window_seconds")
            .and_then(Self::parse_number)
            .map(|f| f as i64)
            .unwrap_or(0);

        // 动态计算标签
        let label = if limit_window_seconds > 0 {
            Self::get_limits_label(limit_window_seconds)
        } else {
            default_label.to_string()
        };

        let reset_str = if let Some(ts) = reset_at {
            if ts > 0 {
                Self::format_reset(ts)
            } else {
                "未知".to_string()
            }
        } else {
            // 尝试使用 reset_after_seconds
            let reset_after = window
                .get("reset_after_seconds")
                .or_else(|| window.get("reset_after_sec"))
                .and_then(Self::parse_number)
                .map(|f| f as i64)
                .unwrap_or(0);
            if reset_after > 0 {
                Self::format_duration(reset_after)
            } else {
                "未知".to_string()
            }
        };

        (used_percent, reset_str, label, reset_at)
    }

    /// 根据窗口秒数获取人类可读标签
    fn get_limits_label(seconds: i64) -> String {
        const SECS_PER_HOUR: i64 = 3600;
        const SECS_PER_DAY: i64 = 24 * SECS_PER_HOUR;
        const SECS_PER_WEEK: i64 = 7 * SECS_PER_DAY;

        if seconds <= SECS_PER_HOUR * 5 + 600 {
            "5H 限额".to_string()
        } else if seconds <= SECS_PER_DAY + 600 {
            "24H 限额".to_string()
        } else if seconds <= SECS_PER_WEEK + 3600 {
            "周限额".to_string()
        } else {
            format!("{}H 限额", (seconds + 3599) / 3600)
        }
    }

    /// 解析数字（支持字符串和数字）
    fn parse_number(v: &Value) -> Option<f64> {
        match v {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.parse().ok(),
            _ => None,
        }
    }

    /// 解析整数（支持字符串和数字）
    fn parse_int(v: &Value) -> Option<i32> {
        match v {
            Value::Number(n) => n.as_i64().map(|i| i as i32),
            Value::String(s) => s.parse().ok(),
            _ => None,
        }
    }

    /// 格式化重置时间（时间戳）
    fn format_reset(reset_at: i64) -> String {
        use chrono::{TimeZone, Utc};

        if reset_at == 0 {
            return "未知".to_string();
        }

        let reset_time = Utc
            .timestamp_opt(reset_at, 0)
            .single()
            .unwrap_or_else(Utc::now);
        let now = Utc::now();

        let duration = reset_time.signed_duration_since(now);
        Self::format_chrono_duration(duration)
    }

    /// 格式化持续时间（秒）
    fn format_duration(seconds: i64) -> String {
        let hours = seconds / 3600;
        let minutes = (seconds % 3600) / 60;

        if hours > 24 {
            let days = hours / 24;
            format!("{}天后重置", days)
        } else if hours > 0 {
            format!("{}小时{}分钟后重置", hours, minutes)
        } else if minutes > 0 {
            format!("{}分钟后重置", minutes)
        } else {
            "即将重置".to_string()
        }
    }

    /// 格式化 chrono Duration
    fn format_chrono_duration(duration: chrono::Duration) -> String {
        let hours = duration.num_hours();
        let minutes = duration.num_minutes() % 60;

        if hours > 24 {
            let days = hours / 24;
            format!("{}天后重置", days)
        } else if hours > 0 {
            format!("{}小时{}分钟后重置", hours, minutes.abs())
        } else if minutes > 0 {
            format!("{}分钟后重置", minutes)
        } else {
            "即将重置".to_string()
        }
    }

    /// 中转站 OpenAI 兼容 usage：`GET {base}/v1/usage` with `Authorization: Bearer <key>`
    ///
    /// 字段优先级（cc-switch 通用模板兼容）：
    /// - remaining: `remaining` / `quota.remaining` / `balance`
    /// - unit:      `unit` / `quota.unit` / 默认 `"USD"`
    /// - is_active: `is_active` / `isValid` / 默认 `true`
    pub async fn fetch_relay_usage_openai_compat(
        base_url: &str,
        api_key: &str,
    ) -> Result<crate::account::RelayUsageCache, String> {
        let url = format!("{}/v1/usage", base_url.trim_end_matches('/'));
        let client = reqwest::Client::new();
        let resp = client
            .get(&url)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Accept", "application/json")
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| format!("usage 请求失败: {}", e))?;
        let status = resp.status();
        if !status.is_success() {
            // 带上 URL + body 前 200 字节，方便定位（如 GLM 401 会返回中文"令牌过期"）
            let body_preview = resp
                .text()
                .await
                .map(|s| s.chars().take(200).collect::<String>())
                .unwrap_or_default();
            return Err(format!(
                "HTTP {} @ {} → {}",
                status.as_u16(),
                url,
                body_preview
            ));
        }
        let body: Value = resp
            .json()
            .await
            .map_err(|e| format!("usage JSON 解析失败: {}", e))?;

        let remaining = body
            .get("remaining")
            .and_then(|v| v.as_f64())
            .or_else(|| {
                body.get("quota")
                    .and_then(|q| q.get("remaining"))
                    .and_then(|v| v.as_f64())
            })
            .or_else(|| body.get("balance").and_then(|v| v.as_f64()))
            .ok_or_else(|| "上游响应缺 remaining/balance 字段".to_string())?;

        let unit = body
            .get("unit")
            .and_then(|v| v.as_str())
            .or_else(|| {
                body.get("quota")
                    .and_then(|q| q.get("unit"))
                    .and_then(|v| v.as_str())
            })
            .unwrap_or("USD")
            .to_string();

        let is_active = body
            .get("is_active")
            .and_then(|v| v.as_bool())
            .or_else(|| body.get("isValid").and_then(|v| v.as_bool()))
            .unwrap_or(true);

        Ok(crate::account::RelayUsageCache {
            remaining,
            unit,
            is_active,
            next_reset_at: None,
            updated_at: chrono::Utc::now(),
        })
    }

    /// new-api 风格的 dashboard usage：
    ///   `GET <base>/v1/dashboard/billing/subscription` →
    ///       `{ soft_limit_usd, hard_limit_usd, access_until }`（与 OpenAI SDK 对齐）
    ///   `GET <base>/v1/dashboard/billing/usage` →
    ///       `{ total_usage }`（单位 0.01 美元）
    ///
    /// 覆盖：PinCC / PackyCode / AICodeMirror / 自建 new-api 等所有基于
    /// QuantumNous/new-api 的中转。`Authorization: Bearer <sk-key>` 即可。
    pub async fn fetch_relay_usage_new_api_dashboard(
        base_url: &str,
        api_key: &str,
    ) -> Result<crate::account::RelayUsageCache, String> {
        let base = base_url.trim_end_matches('/');
        let client = reqwest::Client::new();

        let sub_url = format!("{}/v1/dashboard/billing/subscription", base);
        let sub_resp = client
            .get(&sub_url)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Accept", "application/json")
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| format!("subscription 请求失败: {}", e))?;
        if !sub_resp.status().is_success() {
            let body_preview = sub_resp
                .text()
                .await
                .map(|s| s.chars().take(200).collect::<String>())
                .unwrap_or_default();
            return Err(format!("HTTP {} @ {} → {}", "subscription", sub_url, body_preview));
        }
        let sub_body: Value = sub_resp
            .json()
            .await
            .map_err(|e| format!("subscription JSON 解析失败: {}", e))?;
        let soft_limit_usd = sub_body
            .get("soft_limit_usd")
            .and_then(|v| v.as_f64())
            .ok_or_else(|| "subscription 缺 soft_limit_usd 字段".to_string())?;

        let usage_url = format!("{}/v1/dashboard/billing/usage", base);
        let usage_resp = client
            .get(&usage_url)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Accept", "application/json")
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| format!("usage 请求失败: {}", e))?;
        let total_usage_cents = if usage_resp.status().is_success() {
            let body: Value = usage_resp
                .json()
                .await
                .map_err(|e| format!("usage JSON 解析失败: {}", e))?;
            body.get("total_usage").and_then(|v| v.as_f64()).unwrap_or(0.0)
        } else {
            0.0
        };
        let remaining_usd = (soft_limit_usd - total_usage_cents / 100.0).max(0.0);

        Ok(crate::account::RelayUsageCache {
            remaining: remaining_usd,
            unit: "USD".to_string(),
            is_active: remaining_usd > 0.0,
            next_reset_at: None,
            updated_at: chrono::Utc::now(),
        })
    }

    /// 自动探测中转站的 usage fetcher：依次尝试 new-api / openai_compat。
    /// 命中返回 fetcher 名（写回 `account.relay_usage_preset` 持久化），
    /// 都失败返回 None（用户看到"不拉取"）。
    ///
    /// 只发 GET 请求，不会改上游状态；4xx 不算命中（key 无效另当别论）。
    pub async fn probe_relay_usage_preset(base_url: &str, api_key: &str) -> Option<String> {
        let base = base_url.trim_end_matches('/');
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .ok()?;

        // 1) new-api 风格：/v1/dashboard/billing/subscription
        let url = format!("{}/v1/dashboard/billing/subscription", base);
        if let Ok(resp) = client
            .get(&url)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Accept", "application/json")
            .send()
            .await
        {
            if resp.status().is_success() {
                if let Ok(v) = resp.json::<Value>().await {
                    if v.get("soft_limit_usd").is_some() || v.get("hard_limit_usd").is_some() {
                        return Some("new_api_dashboard".to_string());
                    }
                }
            }
        }

        // 2) sub2api / 通用 OpenAI 兼容：/v1/usage
        let url = format!("{}/v1/usage", base);
        if let Ok(resp) = client
            .get(&url)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Accept", "application/json")
            .send()
            .await
        {
            if resp.status().is_success() {
                if let Ok(v) = resp.json::<Value>().await {
                    let has_field = v.get("remaining").is_some()
                        || v.get("balance").is_some()
                        || v.pointer("/quota/remaining").is_some();
                    if has_field {
                        return Some("openai_compat".to_string());
                    }
                }
            }
        }

        None
    }

    /// GLM / 智谱 monitor quota：`GET https://<host>/api/monitor/usage/quota/limit` with Bearer。
    ///
    /// 输入 `base_url` 通常是 OpenAI 兼容根（如 `https://open.bigmodel.cn/api/paas/v4`），
    /// 这里只取 origin，再拼 `/api/monitor/usage/quota/limit`。
    ///
    /// 响应：
    /// ```json
    /// {"code":200,"data":{"limits":[
    ///   {"type":"TIME_LIMIT","percentage":30,"remaining":0.7,"nextResetTime":1234567890000},
    ///   {"type":"TOKENS_LIMIT","percentage":50}
    /// ]}}
    /// ```
    /// 我们把 TOKENS_LIMIT 的 `100 - percentage` 当 `remaining`，单位 `"% tokens"`。
    pub async fn fetch_relay_usage_glm_zhipu(
        base_url: &str,
        api_key: &str,
    ) -> Result<crate::account::RelayUsageCache, String> {
        let origin = url::Url::parse(base_url)
            .ok()
            .and_then(|u| {
                u.host_str().map(|h| {
                    let scheme = u.scheme();
                    let port = u.port().map(|p| format!(":{}", p)).unwrap_or_default();
                    format!("{}://{}{}", scheme, h, port)
                })
            })
            .ok_or_else(|| format!("无法从 base_url 解析 origin: {}", base_url))?;

        let url = format!("{}/api/monitor/usage/quota/limit", origin);
        let client = reqwest::Client::new();
        let resp = client
            .get(&url)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Accept", "application/json")
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| format!("usage 请求失败: {}", e))?;
        let status = resp.status();
        if !status.is_success() {
            let body_preview = resp
                .text()
                .await
                .map(|s| s.chars().take(200).collect::<String>())
                .unwrap_or_default();
            return Err(format!(
                "HTTP {} @ {} → {}",
                status.as_u16(),
                url,
                body_preview
            ));
        }
        let body: Value = resp
            .json()
            .await
            .map_err(|e| format!("usage JSON 解析失败: {}", e))?;

        let code = body.get("code").and_then(|v| v.as_i64()).unwrap_or(0);
        if code != 200 {
            let msg = body
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("non-200 code");
            return Err(format!("GLM code={} msg={}", code, msg));
        }

        // 从 limits 数组里捞 TOKENS_LIMIT 的 percentage
        let tokens_pct = body
            .get("data")
            .and_then(|d| d.get("limits"))
            .and_then(|v| v.as_array())
            .and_then(|arr| {
                arr.iter().find_map(|lim| {
                    let kind = lim.get("type").and_then(|v| v.as_str())?;
                    if kind == "TOKENS_LIMIT" {
                        lim.get("percentage").and_then(|v| v.as_f64())
                    } else {
                        None
                    }
                })
            });

        // 计算 remaining 百分比（用 tokens 维度；优先级：TOKENS_LIMIT > TIME_LIMIT > 100）
        let used_pct = tokens_pct
            .or_else(|| {
                body.get("data")
                    .and_then(|d| d.get("limits"))
                    .and_then(|v| v.as_array())
                    .and_then(|arr| {
                        arr.iter()
                            .find_map(|lim| lim.get("percentage").and_then(|v| v.as_f64()))
                    })
            })
            .unwrap_or(0.0);
        let remaining_pct = (100.0 - used_pct).max(0.0);

        // 取 nextResetTime（毫秒时间戳）→ Unix 秒
        let next_reset_at = body
            .get("data")
            .and_then(|d| d.get("limits"))
            .and_then(|v| v.as_array())
            .and_then(|arr| {
                arr.iter()
                    .find_map(|lim| lim.get("nextResetTime").and_then(|v| v.as_i64()))
            })
            .map(|ms| ms / 1000);

        Ok(crate::account::RelayUsageCache {
            remaining: remaining_pct,
            unit: "%".to_string(),
            is_active: remaining_pct > 0.0,
            next_reset_at,
            updated_at: chrono::Utc::now(),
        })
    }

    /// Xiaomi MiMo Token Plan usage：MiMo 当前没有公开的 tp-key 配额接口。
    ///
    /// 这里复用网页登录态 Cookie 访问控制台接口：
    /// - GET https://platform.xiaomimimo.com/api/v1/tokenPlan/usage
    /// - GET https://platform.xiaomimimo.com/api/v1/tokenPlan/detail
    ///
    /// `usage.monthUsage.items[0].percent` 是已用比例（0.0505 = 5.05%），
    /// RelayUsageCache.remaining 仍按现有 UI 语义保存"剩余百分比"。
    pub async fn fetch_relay_usage_mimo_token_plan(
        cookie_header: &str,
    ) -> Result<crate::account::RelayUsageCache, String> {
        let cookie = Self::normalize_mimo_cookie_header(cookie_header)
            .ok_or_else(|| "MiMo Cookie 缺少 api-platform_serviceToken 或 userId".to_string())?;

        let client = reqwest::Client::new();
        let usage_url = "https://platform.xiaomimimo.com/api/v1/tokenPlan/usage";
        let detail_url = "https://platform.xiaomimimo.com/api/v1/tokenPlan/detail";

        let usage_body = Self::fetch_mimo_console_json(&client, usage_url, &cookie).await?;
        let detail_body = Self::fetch_mimo_console_json(&client, detail_url, &cookie)
            .await
            .ok();

        let code = usage_body.get("code").and_then(|v| v.as_i64()).unwrap_or(0);
        if code != 0 {
            let msg = usage_body
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("non-zero code");
            return Err(format!("MiMo usage code={} msg={}", code, msg));
        }

        let item = usage_body
            .get("data")
            .and_then(|d| d.get("monthUsage"))
            .and_then(|m| m.get("items"))
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .ok_or_else(|| "MiMo usage 响应缺 monthUsage.items".to_string())?;

        let used = item.get("used").and_then(Self::parse_number).unwrap_or(0.0);
        let limit = item
            .get("limit")
            .and_then(Self::parse_number)
            .unwrap_or(0.0);
        let used_pct_fraction = item
            .get("percent")
            .and_then(Self::parse_number)
            .or_else(|| {
                usage_body
                    .get("data")
                    .and_then(|d| d.get("monthUsage"))
                    .and_then(|m| m.get("percent"))
                    .and_then(Self::parse_number)
            })
            .or_else(|| {
                if limit > 0.0 {
                    Some(used / limit)
                } else {
                    None
                }
            })
            .ok_or_else(|| "MiMo usage 响应缺 percent/used/limit".to_string())?;

        let used_pct = if used_pct_fraction <= 1.0 {
            used_pct_fraction * 100.0
        } else {
            used_pct_fraction
        };
        let remaining_pct = (100.0 - used_pct).clamp(0.0, 100.0);

        let next_reset_at = detail_body
            .as_ref()
            .and_then(|body| body.get("data"))
            .and_then(|data| data.get("currentPeriodEnd"))
            .and_then(|v| v.as_str())
            .and_then(Self::parse_mimo_period_end);

        Ok(crate::account::RelayUsageCache {
            remaining: remaining_pct,
            unit: "% MiMo Credits".to_string(),
            is_active: remaining_pct > 0.0,
            next_reset_at,
            updated_at: chrono::Utc::now(),
        })
    }

    async fn fetch_mimo_console_json(
        client: &reqwest::Client,
        url: &str,
        cookie: &str,
    ) -> Result<Value, String> {
        let resp = client
            .get(url)
            .header("Accept", "application/json, text/plain, */*")
            .header("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8")
            .header("Cookie", cookie)
            .header("Origin", "https://platform.xiaomimimo.com")
            .header(
                "Referer",
                "https://platform.xiaomimimo.com/#/console/balance",
            )
            .header("x-timeZone", "Asia/Shanghai")
            .header(
                "User-Agent",
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36",
            )
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| format!("MiMo usage 请求失败: {}", e))?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err("MiMo 登录态已失效，请重新登录后复制 Cookie".to_string());
        }
        if status == reqwest::StatusCode::FORBIDDEN {
            return Err("MiMo Cookie 无效或权限不足".to_string());
        }
        if !status.is_success() {
            let body_preview = resp
                .text()
                .await
                .map(|s| s.chars().take(200).collect::<String>())
                .unwrap_or_default();
            return Err(format!(
                "HTTP {} @ {} → {}",
                status.as_u16(),
                url,
                body_preview
            ));
        }
        resp.json()
            .await
            .map_err(|e| format!("MiMo usage JSON 解析失败: {}", e))
    }

    fn normalize_mimo_cookie_header(raw: &str) -> Option<String> {
        let mut text = raw.trim();
        let lower = text.to_ascii_lowercase();
        if let Some(idx) = lower.find("cookie:") {
            text = &text[idx + "cookie:".len()..];
        }
        if let Some(line) = text.lines().next() {
            text = line;
        }
        let text = text
            .trim()
            .trim_matches(|c| c == '\'' || c == '"' || c == '`' || c == '\\');

        let known = [
            "api-platform_ph",
            "api-platform_serviceToken",
            "api-platform_slh",
            "userId",
        ];
        let required = ["api-platform_serviceToken", "userId"];
        let mut values = std::collections::BTreeMap::<String, String>::new();
        for pair in text.split(';') {
            let Some((name, value)) = pair.trim().split_once('=') else {
                continue;
            };
            let name = name.trim();
            let value = value.trim().trim_matches(|c| c == '\'' || c == '"');
            if known.contains(&name) && !value.is_empty() {
                values.insert(name.to_string(), value.to_string());
            }
        }
        if !required.iter().all(|name| values.contains_key(*name)) {
            return None;
        }
        Some(
            values
                .into_iter()
                .map(|(name, value)| format!("{}={}", name, value))
                .collect::<Vec<_>>()
                .join("; "),
        )
    }

    fn parse_mimo_period_end(value: &str) -> Option<i64> {
        chrono::NaiveDateTime::parse_from_str(value.trim(), "%Y-%m-%d %H:%M:%S")
            .ok()
            .map(|dt| dt.and_utc().timestamp())
    }
}

/// 默认 referral_key —— ChatGPT 推荐邀请的常驻邀请类型。
pub const DEFAULT_REFERRAL_KEY: &str = "codex_referral_persistent_invite";

/// 单条邀请结果（对应上游 invites[] 元素）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InviteLink {
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub referral_id: String,
    #[serde(default)]
    pub invite_url: String,
}

/// 发送邀请的整体结果，回传给前端展示
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InviteResult {
    pub ok: bool,
    pub status_code: u16,
    pub emails: Vec<String>,
    pub invites: Vec<InviteLink>,
    /// 上游拒绝的邮箱（如重复邀请 → "This person already has a referral sent to them"）
    #[serde(default)]
    pub failed_emails: Vec<String>,
    /// 上游附带的提示文案（失败原因等）
    #[serde(default)]
    pub message: Option<String>,
    /// 上游原始响应（成功/失败都带上，方便前端兜底展示）
    pub upstream_raw: String,
}

/// 调用 ChatGPT 推荐邀请接口：`POST /backend-api/wham/referrals/invite`。
///
/// 复用 quota 的同一条出口（`usage_client` + codex UA），保证带 token 的请求
/// 跟该账号平时的 quota 查询走同一个 egress —— 这对 Pro 号尤其重要：
/// 同一 token 从不同国家 IP 发请求会被 OpenAI 直接 token_invalidated。
pub async fn send_referral_invite(
    access_token: &str,
    account_id: Option<&str>,
    emails: &[String],
    referral_key: Option<&str>,
) -> Result<InviteResult, String> {
    if emails.is_empty() {
        return Err("至少需要 1 个邀请邮箱".to_string());
    }

    let referral_key = referral_key
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_REFERRAL_KEY);

    let body = serde_json::json!({
        "referral_key": referral_key,
        "emails": emails,
    });

    let client = usage_client();
    let mut req = client
        .post("https://chatgpt.com/backend-api/wham/referrals/invite")
        .header("Authorization", format!("Bearer {}", access_token))
        .header("User-Agent", crate::codex_ua::codex_user_agent())
        .header("originator", crate::codex_ua::CODEX_ORIGINATOR)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .timeout(Duration::from_secs(45))
        .json(&body);
    if let Some(id) = account_id {
        if !id.is_empty() {
            req = req.header("ChatGPT-Account-Id", id);
        }
    }

    let resp = req.send().await.map_err(|e| format!("网络请求失败: {}", e))?;
    let status = resp.status();
    let raw = resp.text().await.unwrap_or_default();

    let parsed = serde_json::from_str::<Value>(&raw).ok();
    let invites = parsed
        .as_ref()
        .and_then(|v| v.get("invites").cloned())
        .and_then(|v| serde_json::from_value::<Vec<InviteLink>>(v).ok())
        .unwrap_or_default();

    // failed_emails / message 既可能在顶层，也可能嵌在 detail 对象里（403 那种）。
    // 取一个"既看顶层又看 detail"的视图。
    let detail_obj = parsed.as_ref().and_then(|v| v.get("detail"));
    let pick = |key: &str| -> Option<Value> {
        parsed
            .as_ref()
            .and_then(|v| v.get(key))
            .or_else(|| detail_obj.and_then(|d| d.get(key)))
            .cloned()
    };

    let failed_emails = pick("failed_emails")
        .as_ref()
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| e.as_str().map(|s| s.to_string()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    // message：顶层 message → detail.message → detail 本身是字符串
    let message = pick("message")
        .and_then(|m| m.as_str().map(|s| s.to_string()))
        .or_else(|| detail_obj.and_then(|d| d.as_str()).map(|s| s.to_string()));

    // ok = HTTP 2xx 且没有任何被拒邮箱
    let ok = status.is_success() && failed_emails.is_empty();

    Ok(InviteResult {
        ok,
        status_code: status.as_u16(),
        emails: emails.to_vec(),
        invites,
        failed_emails,
        message,
        upstream_raw: raw,
    })
}

/// 默认唤醒模型（实测 ChatGPT 账号 codex responses 接口当前接受 gpt-5.5；
/// gpt-5-codex / gpt-5.3-codex 等已被该端点拒绝）。
pub const DEFAULT_WAKEUP_MODEL: &str = "gpt-5.5";

/// 唤醒结果，回传给前端
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WakeupResult {
    /// true = 模型正常回了话（HTTP 2xx）
    pub ok: bool,
    pub status_code: u16,
    pub model: String,
    /// 发出去的唤醒词
    pub prompt: String,
    /// 模型回复（成功时）
    pub reply: String,
    /// 失败摘要（usage_limit_reached / needs_verification / token_invalid …）
    pub error: Option<String>,
}

/// 用账号 token 模拟官方 codex CLI，向 `chatgpt.com/backend-api/codex/responses`
/// 发一句最小对话（默认「你好」）。等价于 cockpit 的「唤醒/账户检测」：
/// 200=活着且回话，429=配额耗尽，403=需验证，401=token 失效。
/// 走 quota 同一条出口（Pro 号同 IP 约束见 [[pro_account_multi_region_token_invalidated]]）。
pub async fn send_wakeup(
    access_token: &str,
    account_id: Option<&str>,
    prompt: &str,
    model: &str,
) -> Result<WakeupResult, String> {
    let prompt = if prompt.trim().is_empty() { "你好" } else { prompt.trim() };
    let model = if model.trim().is_empty() { DEFAULT_WAKEUP_MODEL } else { model.trim() };
    let session_id = uuid::Uuid::new_v4().to_string();

    let body = serde_json::json!({
        "model": model,
        "instructions": "You are a helpful assistant.",
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": prompt }],
        }],
        "tools": [],
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        "reasoning": { "effort": "low", "summary": "auto" },
        "store": false,
        "stream": true,
        "include": ["reasoning.encrypted_content"],
        "prompt_cache_key": session_id,
    });

    let client = usage_client();
    let mut req = client
        .post("https://chatgpt.com/backend-api/codex/responses")
        .header("Authorization", format!("Bearer {}", access_token))
        .header("OpenAI-Beta", "responses=experimental")
        .header("originator", crate::codex_ua::CODEX_ORIGINATOR)
        .header("User-Agent", crate::codex_ua::codex_user_agent())
        .header("session_id", &session_id)
        .header("Accept", "text/event-stream")
        .header("Content-Type", "application/json")
        .timeout(Duration::from_secs(60))
        .json(&body);
    if let Some(id) = account_id {
        if !id.is_empty() {
            req = req.header("ChatGPT-Account-Id", id);
        }
    }

    let resp = req.send().await.map_err(|e| format!("网络请求失败: {}", e))?;
    let status = resp.status();
    let raw = resp.text().await.unwrap_or_default();

    if status.is_success() {
        Ok(WakeupResult {
            ok: true,
            status_code: status.as_u16(),
            model: model.to_string(),
            prompt: prompt.to_string(),
            reply: parse_wakeup_reply(&raw),
            error: None,
        })
    } else {
        Ok(WakeupResult {
            ok: false,
            status_code: status.as_u16(),
            model: model.to_string(),
            prompt: prompt.to_string(),
            reply: String::new(),
            error: Some(parse_wakeup_error(&raw, status.as_u16())),
        })
    }
}

/// 从 SSE 流里抠出 assistant 回复：优先拼 `response.output_text.delta` 的增量，
/// 退化到 `response.output_text.done` 的整段 text。
fn parse_wakeup_reply(sse: &str) -> String {
    let mut acc = String::new();
    let mut done_text: Option<String> = None;
    for line in sse.lines() {
        let line = line.trim_start();
        let payload = match line.strip_prefix("data:") {
            Some(p) => p.trim(),
            None => continue,
        };
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(payload) else { continue };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("response.output_text.delta") => {
                if let Some(d) = v.get("delta").and_then(|d| d.as_str()) {
                    acc.push_str(d);
                }
            }
            Some("response.output_text.done") => {
                if let Some(t) = v.get("text").and_then(|t| t.as_str()) {
                    done_text = Some(t.to_string());
                }
            }
            _ => {}
        }
    }
    let out = if !acc.trim().is_empty() {
        acc
    } else {
        done_text.unwrap_or_default()
    };
    out.trim().to_string()
}

/// 把非 2xx 响应体翻成简短人话。
fn parse_wakeup_error(raw: &str, status: u16) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(raw) {
        // {"error":{"type":"usage_limit_reached","message":"..."}}
        if let Some(err) = v.get("error") {
            let ty = err.get("type").and_then(|t| t.as_str()).unwrap_or("");
            let msg = err.get("message").and_then(|m| m.as_str()).unwrap_or("");
            if ty == "usage_limit_reached" {
                return "配额已耗尽（usage_limit_reached）".to_string();
            }
            if !msg.is_empty() {
                return msg.to_string();
            }
            if !ty.is_empty() {
                return ty.to_string();
            }
        }
        // {"detail":"..."}
        if let Some(detail) = v.get("detail").and_then(|d| d.as_str()) {
            return detail.to_string();
        }
    }
    match status {
        401 => "token 失效（401）".to_string(),
        403 => "需要验证或提交保证书（403）".to_string(),
        429 => "配额已耗尽（429）".to_string(),
        _ => format!("上游返回 HTTP {}", status),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse_relay_response(body: Value) -> Result<(f64, String, bool), String> {
        // 单元测试只验证字段优先级，不实际打网络
        let remaining = body
            .get("remaining")
            .and_then(|v| v.as_f64())
            .or_else(|| {
                body.get("quota")
                    .and_then(|q| q.get("remaining"))
                    .and_then(|v| v.as_f64())
            })
            .or_else(|| body.get("balance").and_then(|v| v.as_f64()))
            .ok_or_else(|| "missing".to_string())?;
        let unit = body
            .get("unit")
            .and_then(|v| v.as_str())
            .or_else(|| {
                body.get("quota")
                    .and_then(|q| q.get("unit"))
                    .and_then(|v| v.as_str())
            })
            .unwrap_or("USD")
            .to_string();
        let is_active = body
            .get("is_active")
            .and_then(|v| v.as_bool())
            .or_else(|| body.get("isValid").and_then(|v| v.as_bool()))
            .unwrap_or(true);
        Ok((remaining, unit, is_active))
    }

    #[test]
    fn relay_usage_top_level_fields() {
        let body = json!({"remaining": 12.5, "unit": "USD", "is_active": true});
        let (r, u, a) = parse_relay_response(body).unwrap();
        assert_eq!(r, 12.5);
        assert_eq!(u, "USD");
        assert!(a);
    }

    #[test]
    fn relay_usage_nested_quota() {
        let body = json!({"quota": {"remaining": 8.0, "unit": "CNY"}, "isValid": false});
        let (r, u, a) = parse_relay_response(body).unwrap();
        assert_eq!(r, 8.0);
        assert_eq!(u, "CNY");
        assert!(!a);
    }

    #[test]
    fn relay_usage_balance_alias_with_default_unit() {
        let body = json!({"balance": 1.23});
        let (r, u, a) = parse_relay_response(body).unwrap();
        assert_eq!(r, 1.23);
        assert_eq!(u, "USD"); // 默认
        assert!(a); // 默认
    }

    #[test]
    fn relay_usage_missing_remaining_errors() {
        let body = json!({"unit": "USD"});
        assert!(parse_relay_response(body).is_err());
    }

    fn parse_glm_quota(body: Value) -> Option<f64> {
        // 模拟 fetch_relay_usage_glm_zhipu 的解析步骤（不打网络）
        let code = body.get("code").and_then(|v| v.as_i64()).unwrap_or(0);
        if code != 200 {
            return None;
        }
        let used_pct = body
            .get("data")
            .and_then(|d| d.get("limits"))
            .and_then(|v| v.as_array())
            .and_then(|arr| {
                arr.iter().find_map(|lim| {
                    let kind = lim.get("type").and_then(|v| v.as_str())?;
                    if kind == "TOKENS_LIMIT" {
                        lim.get("percentage").and_then(|v| v.as_f64())
                    } else {
                        None
                    }
                })
            })?;
        Some((100.0 - used_pct).max(0.0))
    }

    #[test]
    fn glm_quota_picks_tokens_limit_percentage() {
        let body = json!({
            "code": 200,
            "data": {
                "limits": [
                    {"type": "TIME_LIMIT", "percentage": 30, "remaining": 0.7},
                    {"type": "TOKENS_LIMIT", "percentage": 25}
                ]
            }
        });
        let pct = parse_glm_quota(body).unwrap();
        assert_eq!(pct, 75.0); // 100 - 25
    }

    #[test]
    fn glm_quota_skips_when_code_not_200() {
        let body = json!({"code": 401, "message": "unauthorized"});
        assert!(parse_glm_quota(body).is_none());
    }

    #[test]
    fn mimo_cookie_normalizer_keeps_required_console_cookies() {
        let raw =
            "Cookie: ignored=x; userId=123; api-platform_serviceToken=svc; api-platform_ph=ph";
        let normalized = UsageFetcher::normalize_mimo_cookie_header(raw).unwrap();
        assert_eq!(
            normalized,
            "api-platform_ph=ph; api-platform_serviceToken=svc; userId=123"
        );
    }

    #[test]
    fn mimo_cookie_normalizer_rejects_missing_auth_cookie() {
        assert!(UsageFetcher::normalize_mimo_cookie_header("Cookie: userId=123").is_none());
    }

    #[test]
    fn mimo_period_end_parser_reads_console_timestamp() {
        assert_eq!(
            UsageFetcher::parse_mimo_period_end("2026-05-04 23:59:59"),
            Some(1_778_025_599)
        );
    }
}
