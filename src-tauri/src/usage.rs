//! Codex Switcher - 用量获取模块
//!
//! 从 OpenAI API 获取 Codex 使用量信息

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::OnceLock;
use std::time::Duration;

/// 进程级共享 reqwest::Client — 整个 quota 刷新链路共用一个连接池，
/// 不再每个账号都跑一次 TLS 握手。30 秒空闲回收，最多 8 个 keep-alive。
///
/// `wham/usage` 不在应用层指定代理，由系统网络层（Clash/TUN、250 透明
/// 接管或环境变量代理）决定实际出口。
pub(crate) fn usage_client() -> &'static reqwest::Client {
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

/// Spark（GPT-5.3-Codex-Spark）独立限额窗口。来自 wham/usage 的
/// `additional_rate_limits[]`（limit_name == "GPT-5.3-Codex-Spark"）。
/// free 号没有 → None。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SparkWindows {
    /// 5小时剩余百分比
    pub five_hour_left: i32,
    pub five_hour_reset: String,
    pub five_hour_reset_at: Option<i64>,
    /// 周剩余百分比
    pub weekly_left: i32,
    pub weekly_reset: String,
    pub weekly_reset_at: Option<i64>,
}

/// OpenAI 临时 Luna Reserve 独立额度窗口。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LunaReserveWindow {
    pub normal_model_slug: String,
    pub allowed: bool,
    pub limit_reached: bool,
    pub used_percent: i32,
    pub reset_after_seconds: Option<i64>,
    pub reset_at: Option<i64>,
}

impl LunaReserveWindow {
    pub fn is_available_for(&self, model: &str) -> bool {
        self.normal_model_slug.eq_ignore_ascii_case(model)
            && self.allowed
            && !self.limit_reached
            && self.used_percent < 100
    }
}

/// 前端展示的用量数据
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesktopUsageGate {
    pub allowed: Option<bool>,
    pub limit_reached: Option<bool>,
    pub spend_limit_reached: Option<bool>,
    pub reason: Option<String>,
}

impl DesktopUsageGate {
    pub fn workspace_blocked(&self) -> bool {
        self.spend_limit_reached == Some(true)
            || self.reason.as_deref().is_some_and(|reason| {
                reason.starts_with("workspace_")
                    && (reason.ends_with("credits_depleted")
                        || reason.ends_with("usage_limit_reached"))
            })
    }

    pub fn confirmed_available(&self) -> bool {
        self.allowed == Some(true)
            && self.limit_reached != Some(true)
            && self.spend_limit_reached != Some(true)
            && self.reason.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageDisplay {
    #[serde(default)]
    pub desktop_gate: Option<DesktopUsageGate>,
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
    /// wham/usage primary_window.limit_window_seconds；窗口语义以它为准。
    pub primary_window_seconds: Option<i64>,
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
    /// wham/usage secondary_window.limit_window_seconds。
    pub secondary_window_seconds: Option<i64>,
    /// 额度余额
    pub credits_balance: Option<f64>,
    /// 是否有额度
    pub has_credits: bool,
    /// 主动重置次数（rate_limit_reset_credits.available_count）。None = 接口未返回
    pub reset_credits: Option<i32>,
    /// Spark 独立限额窗口（仅 Pro 等有 Spark 的号；free=None）
    #[serde(default)]
    pub spark: Option<SparkWindows>,
    /// Luna Reserve 独立限额（通常对应 gpt-5.6-luna）。
    #[serde(default)]
    pub luna_reserve: Option<LunaReserveWindow>,
    /// Token 是否对 CLI 有效 (api.openai.com)
    pub is_valid_for_cli: bool,
}

impl UsageDisplay {
    /// 只有明确拿到正数余额时，才把账号视为可用余额号。
    pub fn has_spendable_credits(&self) -> bool {
        self.credits_balance
            .is_some_and(|balance| balance.is_finite() && balance > 0.0)
    }

    /// 套餐窗口仍可用；free/unknown 没有可靠周窗口时只看主窗口。
    pub fn has_rate_limit_quota(&self) -> bool {
        if self
            .desktop_gate
            .as_ref()
            .is_some_and(DesktopUsageGate::workspace_blocked)
        {
            return false;
        }
        let plan = self.plan_type.to_lowercase();
        let is_free = plan == "free" || plan == "unknown";
        if is_free {
            self.five_hour_left > 0
        } else {
            self.five_hour_left > 0 && self.weekly_left > 0
        }
    }

    pub fn has_usable_quota(&self) -> bool {
        if self
            .desktop_gate
            .as_ref()
            .is_some_and(DesktopUsageGate::workspace_blocked)
        {
            return false;
        }
        self.has_rate_limit_quota() || self.has_spendable_credits()
    }
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
        // seat 级唯一锁键（= store 账号 id）。team 多成员共用同一 chatgpt_account_id，
        // 必须用 seat id 当 rt 刷新锁键，才能跟 proxy / 后台 / scheduler 走同一把锁。
        refresh_lock_key: Option<String>,
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

        // 上游 429/5xx 是瞬时熔断（biscuit_baker_circuit_open / connection reset），
        // 不是账号坏了。最多再试 2 次（共 3 次），指数退避 1s/2s。
        let mut response = None;
        let mut status = reqwest::StatusCode::INTERNAL_SERVER_ERROR;
        let mut last_net_err: Option<String> = None;
        for attempt in 0..3u32 {
            match build_request(&current_token, &account_id).send().await {
                Ok(resp) => {
                    status = resp.status();
                    // 瞬时上游错误：读掉 body 后退避重试（401/403 留给下面鉴权分支）
                    let transient = matches!(status.as_u16(), 429 | 500 | 502 | 503 | 504);
                    if transient && attempt < 2 {
                        let _ = resp.text().await;
                        let wait_ms = 1000u64 * (1u64 << attempt);
                        eprintln!(
                            "[Usage] wham/usage HTTP {}，{}ms 后重试 ({}/3)",
                            status.as_u16(),
                            wait_ms,
                            attempt + 2
                        );
                        tokio::time::sleep(Duration::from_millis(wait_ms)).await;
                        continue;
                    }
                    response = Some(resp);
                    break;
                }
                Err(e) => {
                    last_net_err = Some(format!("网络请求失败: {}", e));
                    if attempt < 2 {
                        let wait_ms = 1000u64 * (1u64 << attempt);
                        eprintln!(
                            "[Usage] wham/usage 网络错误，{}ms 后重试 ({}/3): {}",
                            wait_ms,
                            attempt + 2,
                            e
                        );
                        tokio::time::sleep(Duration::from_millis(wait_ms)).await;
                        continue;
                    }
                }
            }
        }
        let mut response = match response {
            Some(r) => r,
            None => {
                return Err(last_net_err.unwrap_or_else(|| "wham/usage 请求失败".to_string()));
            }
        };
        status = response.status();

        // 如果允许本地刷新，且 401/403 且有 refresh_token，尝试刷新
        // 注意：rt 旋转走 _locked 串行化，同账号并发自动排队不撞 race
        if allow_local_refresh && (status == 401 || status == 403) && refresh_token.is_some() {
            if let Some(ref rt) = refresh_token {
                // 锁 key 必须 seat 级唯一（= store 账号 id），保证「同一 seat 的 rt 旋转」
                // 跟 proxy / 后台 QuotaRefresh / scheduler 走同一把锁串行化。
                // 绝不能用 chatgpt_account_id：team 多成员共用同一 workspace id，会让不同
                // seat 的刷新撞不到同一把锁 → 跨路径并发刷同一 rt → refresh_token_reused →
                // 整条 rt 家族被 OpenAI 吊销而死号。seat key 缺失才退到 account_id / rt 前缀。
                let lock_key = refresh_lock_key
                    .clone()
                    .filter(|s| !s.is_empty())
                    .or_else(|| account_id.clone())
                    .unwrap_or_else(|| format!("rt:{}", &rt[..rt.len().min(16)]));
                match crate::oauth::refresh_access_token_locked(&lock_key, rt).await {
                    Ok(token_res) => {
                        current_token = token_res.access_token.clone();
                        new_tokens = Some(token_res);

                        // 重试请求（同样带 5xx 退避）
                        let mut retried = None;
                        for attempt in 0..3u32 {
                            match build_request(&current_token, &account_id).send().await {
                                Ok(resp) => {
                                    status = resp.status();
                                    let transient =
                                        matches!(status.as_u16(), 429 | 500 | 502 | 503 | 504);
                                    if transient && attempt < 2 {
                                        let _ = resp.text().await;
                                        tokio::time::sleep(Duration::from_millis(
                                            1000u64 * (1u64 << attempt),
                                        ))
                                        .await;
                                        continue;
                                    }
                                    retried = Some(resp);
                                    break;
                                }
                                Err(e) => {
                                    if attempt == 2 {
                                        return Err(format!("刷新后重试失败: {}", e));
                                    }
                                    tokio::time::sleep(Duration::from_millis(
                                        1000u64 * (1u64 << attempt),
                                    ))
                                    .await;
                                }
                            }
                        }
                        response = retried.ok_or_else(|| "刷新后重试失败".to_string())?;
                        status = response.status();
                    }
                    Err(e) => {
                        let lower = e.to_lowercase();
                        // 终态（rt 真失效 / session 结束）→ 标"需重新登录"
                        if lower.contains("logged out")
                            || lower.contains("signed in to another account")
                            || lower.contains("invalid_grant")
                            || lower.contains("refresh_token_invalidated")
                            || lower.contains("refresh_token_expired")
                            || lower.contains("session has ended")
                            || lower.contains("session_expired")
                        {
                            return Err("ACCOUNT_LOGGED_OUT:您已登出或登录了其他账号，请重新登录"
                                .to_string());
                        }
                        // 瞬时（refresh_token_reused 轮换冲突 / 网络抖动 / 边缘节流）：
                        // 绝不翻 is_token_invalid。早退一个非终态错误（不含 TOKEN_INVALID
                        // 前缀），交给下一轮配额刷新 / proxy 自愈。
                        return Err(format!(
                            "TOKEN_REFRESH_TRANSIENT:刷新瞬时失败(reused/网络)，不标记失效: {}",
                            e
                        ));
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

        // 非 2xx 绝不能当成功：OpenAI 间歇 429/500/503 时 body 仍是 JSON
        // （如 {"error":{"code":"biscuit_baker_service_me_circuit_open"},"status":503}），
        // 旧逻辑会落到 parse_usage_response → plan_type=unknown / 剩余 100% / 重置「未知」，
        // 再经 Mini Mac remote refresh 写回 cached_quota，把好额度盖成假数据。
        if !status.is_success() {
            let preview: String = text.chars().take(240).collect();
            let code = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|j| {
                    j.pointer("/error/code")
                        .and_then(|c| c.as_str())
                        .map(|s| s.to_string())
                        .or_else(|| {
                            j.get("status")
                                .and_then(|s| s.as_u64())
                                .map(|n| n.to_string())
                        })
                })
                .unwrap_or_else(|| "unknown".to_string());
            return Err(format!(
                "USAGE_UPSTREAM_HTTP_{}: wham/usage 上游异常 (code={}): {}",
                status.as_u16(),
                code,
                preview
            ));
        }

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

        // 200 但其实是错误壳（偶发 CDN/网关把 error 包成 200）
        if json.get("plan_type").and_then(|v| v.as_str()).is_none()
            && json.get("rate_limit").is_none()
        {
            if let Some(err) = json.get("error") {
                let preview: String = err.to_string().chars().take(200).collect();
                return Err(format!(
                    "USAGE_UPSTREAM_ERROR_BODY: wham/usage 返回错误体且无额度字段: {}",
                    preview
                ));
            }
            let preview: String = text.chars().take(200).collect();
            return Err(format!(
                "USAGE_EMPTY_BODY: wham/usage 200 但缺少 plan_type/rate_limit: {}",
                preview
            ));
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
        let reason = json
            .get("rate_limit_reached_type")
            .filter(|value| !value.is_null())
            .or_else(|| rate_limit.and_then(|r| r.get("rate_limit_reached_type")))
            .and_then(|value| {
                value
                    .as_str()
                    .or_else(|| value.get("type").and_then(Value::as_str))
            })
            .map(str::to_string);
        let spend_limit_reached = json
            .pointer("/spend_control/reached")
            .and_then(Value::as_bool);
        let desktop_gate = (rate_limit.is_some_and(Value::is_object)
            || reason.is_some()
            || spend_limit_reached.is_some())
        .then(|| DesktopUsageGate {
            allowed: rate_limit
                .and_then(|value| value.get("allowed"))
                .and_then(Value::as_bool),
            limit_reached: rate_limit
                .and_then(|value| value.get("limit_reached"))
                .and_then(Value::as_bool),
            spend_limit_reached,
            reason,
        });

        // 解析 5 小时窗口 (Primary)
        let primary_val = rate_limit.and_then(|r| r.get("primary_window"));
        let (p_used, p_reset, p_label, p_reset_at, p_window_seconds) =
            Self::parse_window(primary_val, "5H 限额");

        // 解析周窗口 (Secondary)
        let secondary_val = rate_limit.and_then(|r| r.get("secondary_window"));
        let (s_used, s_reset, s_label, s_reset_at, s_window_seconds) =
            Self::parse_window(secondary_val, "周限额");

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

        // Spark 独立限额：additional_rate_limits[] 里 limit_name == GPT-5.3-Codex-Spark
        let spark = json
            .get("additional_rate_limits")
            .and_then(|a| a.as_array())
            .and_then(|arr| {
                arr.iter().find(|e| {
                    e.get("limit_name")
                        .and_then(|n| n.as_str())
                        .map(|n| n.eq_ignore_ascii_case("GPT-5.3-Codex-Spark"))
                        .unwrap_or(false)
                })
            })
            .and_then(|e| e.get("rate_limit"))
            .map(|rl| {
                let (p_used, p_reset, _l, p_at, _) =
                    Self::parse_window(rl.get("primary_window"), "5H 限额");
                let (s_used, s_reset, _l2, s_at, _) =
                    Self::parse_window(rl.get("secondary_window"), "周限额");
                SparkWindows {
                    five_hour_left: 100 - p_used,
                    five_hour_reset: p_reset,
                    five_hour_reset_at: p_at,
                    weekly_left: 100 - s_used,
                    weekly_reset: s_reset,
                    weekly_reset_at: s_at,
                }
            });

        let luna_reserve = json
            .get("additional_rate_limits")
            .and_then(|a| a.as_array())
            .and_then(|arr| {
                arr.iter().find(|e| {
                    e.get("limit_name")
                        .and_then(|n| n.as_str())
                        .map(|n| n.eq_ignore_ascii_case("gpt-reserve"))
                        .unwrap_or(false)
                })
            })
            .and_then(|e| {
                let rl = e.get("rate_limit")?;
                let (used, _reset, _label, reset_at, _) =
                    Self::parse_window(rl.get("primary_window"), "Luna Reserve");
                Some(LunaReserveWindow {
                    normal_model_slug: e
                        .get("normal_model_slug")
                        .and_then(|v| v.as_str())
                        .unwrap_or("gpt-5.6-luna")
                        .to_string(),
                    allowed: rl.get("allowed").and_then(|v| v.as_bool()).unwrap_or(false),
                    limit_reached: rl
                        .get("limit_reached")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(true),
                    used_percent: used,
                    reset_after_seconds: rl
                        .get("primary_window")
                        .and_then(|w| w.get("reset_after_seconds"))
                        .and_then(Self::parse_number)
                        .map(|v| v as i64),
                    reset_at,
                })
            });

        Ok(UsageDisplay {
            desktop_gate,
            plan_type,
            five_hour_used: p_used,
            five_hour_left: 100 - p_used,
            five_hour_label: p_label,
            five_hour_reset: p_reset,
            five_hour_reset_at: p_reset_at,
            primary_window_seconds: p_window_seconds,
            weekly_used: s_used,
            weekly_left: 100 - s_used,
            weekly_label: s_label,
            weekly_reset: s_reset,
            weekly_reset_at: s_reset_at,
            secondary_window_seconds: s_window_seconds,
            credits_balance,
            has_credits: has_credits || unlimited,
            reset_credits,
            spark,
            luna_reserve,
            is_valid_for_cli: true,
        })
    }

    /// 解析窗口数据
    fn parse_window(
        window: Option<&Value>,
        default_label: &str,
    ) -> (i32, String, String, Option<i64>, Option<i64>) {
        let window = match window {
            Some(w) => w,
            None => return (0, "未知".to_string(), default_label.to_string(), None, None),
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

        (
            used_percent,
            reset_str,
            label,
            reset_at,
            (limit_window_seconds > 0).then_some(limit_window_seconds),
        )
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
        let base = base_url.trim_end_matches('/');
        let url = if base.ends_with("/v1") {
            format!("{}/usage", base)
        } else {
            format!("{}/v1/usage", base)
        };
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

        let windows = body
            .get("windows")
            .and_then(|value| serde_json::from_value(value.clone()).ok())
            .unwrap_or_default();

        Ok(crate::account::RelayUsageCache {
            windows,
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
            return Err(format!(
                "HTTP {} @ {} → {}",
                "subscription", sub_url, body_preview
            ));
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
            body.get("total_usage")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0)
        } else {
            0.0
        };
        let remaining_usd = (soft_limit_usd - total_usage_cents / 100.0).max(0.0);

        Ok(crate::account::RelayUsageCache {
            windows: Vec::new(),
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
        let url = if base.ends_with("/v1") {
            format!("{}/usage", base)
        } else {
            format!("{}/v1/usage", base)
        };
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
            windows: Vec::new(),
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
            windows: Vec::new(),
            remaining: remaining_pct,
            unit: "% MiMo Credits".to_string(),
            is_active: remaining_pct > 0.0,
            next_reset_at,
            updated_at: chrono::Utc::now(),
        })
    }

    /// StepFun Step Plan quota：平台控制台的 Dashboard RPC。
    ///
    /// Step Plan 的 API Key 只负责模型调用；官方额度接口
    /// `QueryStepPlanRateLimit` 走 platform.stepfun.com 的网页登录态，使用
    /// `Oasis-Token`。这里接受两种输入：直接粘贴 token，或粘贴包含
    /// `Oasis-Token=...` 的 Cookie header。不会把该 token 放进模型请求。
    pub async fn fetch_relay_usage_stepfun_plan(
        console_token_or_cookie: &str,
    ) -> Result<crate::account::RelayUsageCache, String> {
        let token =
            Self::normalize_stepfun_console_token(console_token_or_cookie).ok_or_else(|| {
                "StepFun 额度查询需要 platform.stepfun.com 的 Oasis-Token".to_string()
            })?;
        let url = "https://platform.stepfun.com/api/step.openapi.devcenter.Dashboard/QueryStepPlanRateLimit";
        let client = reqwest::Client::new();
        let resp = client
            .post(url)
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .header("Oasis-Token", token)
            .header("Oasis-AppID", "10300")
            .header("Oasis-Platform", "web")
            .header("Origin", "https://platform.stepfun.com")
            .header("Referer", "https://platform.stepfun.com/plan-subscribe")
            .timeout(std::time::Duration::from_secs(15))
            .json(&serde_json::json!({}))
            .send()
            .await
            .map_err(|e| format!("StepFun 额度请求失败: {}", e))?;
        let status = resp.status();
        let body: Value = resp
            .json()
            .await
            .map_err(|e| format!("StepFun 额度 JSON 解析失败: {}", e))?;
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(
                "StepFun 控制台 Token 已失效，请重新登录 platform.stepfun.com 后复制 Oasis-Token"
                    .to_string(),
            );
        }
        if !status.is_success() {
            let desc = Self::stepfun_field(&body, "desc", "desc")
                .and_then(Value::as_str)
                .unwrap_or("上游拒绝了额度查询");
            return Err(format!("HTTP {} @ {} → {}", status.as_u16(), url, desc));
        }
        Self::parse_stepfun_rate_limit_response(&body)
    }

    fn normalize_stepfun_console_token(raw: &str) -> Option<String> {
        let mut text = raw.trim();
        let lower = text.to_ascii_lowercase();
        if let Some(idx) = lower.find("cookie:") {
            text = &text[idx + "cookie:".len()..];
        } else if let Some(idx) = lower.find("oasis-token:") {
            text = &text[idx + "oasis-token:".len()..];
        }
        if let Some(line) = text.lines().next() {
            text = line;
        }
        let text = text
            .trim()
            .trim_matches(|c| c == '\'' || c == '"' || c == '`' || c == '\\');
        for pair in text.split(';') {
            let Some((name, value)) = pair.trim().split_once('=') else {
                continue;
            };
            if name.trim().eq_ignore_ascii_case("Oasis-Token") {
                let value = value.trim().trim_matches(|c| c == '\'' || c == '"');
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
        if !text.contains('=') && !text.is_empty() {
            return Some(text.to_string());
        }
        None
    }

    fn stepfun_field<'a>(value: &'a Value, snake: &str, camel: &str) -> Option<&'a Value> {
        value.get(snake).or_else(|| value.get(camel))
    }

    fn stepfun_timestamp(value: Option<&Value>) -> Option<i64> {
        let timestamp = value.and_then(Self::parse_number)?.round() as i64;
        if timestamp > 20_000_000_000 {
            Some(timestamp / 1000)
        } else {
            Some(timestamp)
        }
    }

    fn stepfun_rate_percent(value: Option<&Value>) -> Option<f64> {
        let rate = Self::parse_number(value?)?;
        if !rate.is_finite() {
            return None;
        }
        Some(if rate <= 1.0 { rate * 100.0 } else { rate }.clamp(0.0, 100.0))
    }

    fn parse_stepfun_rate_limit_response(
        body: &Value,
    ) -> Result<crate::account::RelayUsageCache, String> {
        use crate::account::RelayQuotaWindow;

        let credit_limit =
            Self::stepfun_field(body, "plan_credit_rate_limit", "planCreditRateLimit");
        let mut windows = Vec::new();
        if let Some(buckets) = credit_limit
            .and_then(|value| Self::stepfun_field(value, "credit_buckets", "creditBuckets"))
            .and_then(Value::as_array)
        {
            for bucket in buckets {
                let total = Self::stepfun_field(bucket, "credit_total", "creditTotal")
                    .and_then(Self::parse_number);
                let residual = Self::stepfun_field(bucket, "credit_residual", "creditResidual")
                    .and_then(Self::parse_number);
                let remaining_percent = match (total, residual) {
                    (Some(total), Some(residual)) if total > 0.0 => {
                        Some((residual / total * 100.0).clamp(0.0, 100.0))
                    }
                    _ => None,
                };
                let bucket_type = Self::stepfun_field(bucket, "type", "type")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let label = if bucket_type.contains("TOPUP") || bucket_type == "2" {
                    "加油包 Credit"
                } else {
                    "订阅 Credit"
                };
                let reset_at = Self::stepfun_timestamp(
                    Self::stepfun_field(bucket, "next_reset_at", "nextResetAt")
                        .or_else(|| Self::stepfun_field(bucket, "expire_at", "expireAt")),
                );
                if remaining_percent.is_some() {
                    windows.push(RelayQuotaWindow {
                        label: label.to_string(),
                        remaining_percent,
                        reset_at,
                    });
                }
            }
        }

        if windows.is_empty() {
            if let Some(rate) = Self::stepfun_rate_percent(credit_limit.and_then(|value| {
                Self::stepfun_field(
                    value,
                    "subscription_credit_left_rate",
                    "subscriptionCreditLeftRate",
                )
            })) {
                windows.push(RelayQuotaWindow {
                    label: "订阅 Credit".to_string(),
                    remaining_percent: Some(rate),
                    reset_at: Self::stepfun_timestamp(credit_limit.and_then(|value| {
                        Self::stepfun_field(
                            value,
                            "subscription_credit_reset_time",
                            "subscriptionCreditResetTime",
                        )
                    })),
                });
            }
            if let Some(rate) = Self::stepfun_rate_percent(credit_limit.and_then(|value| {
                Self::stepfun_field(value, "topup_credit_left_rate", "topupCreditLeftRate")
            })) {
                windows.push(RelayQuotaWindow {
                    label: "加油包 Credit".to_string(),
                    remaining_percent: Some(rate),
                    reset_at: None,
                });
            }
        }

        if windows.is_empty() {
            let five_hour = Self::stepfun_rate_percent(Self::stepfun_field(
                body,
                "five_hour_usage_left_rate",
                "fiveHourUsageLeftRate",
            ));
            let weekly = Self::stepfun_rate_percent(Self::stepfun_field(
                body,
                "weekly_usage_left_rate",
                "weeklyUsageLeftRate",
            ));
            if let Some(rate) = five_hour {
                windows.push(RelayQuotaWindow {
                    label: "5H".to_string(),
                    remaining_percent: Some(rate),
                    reset_at: Self::stepfun_timestamp(Self::stepfun_field(
                        body,
                        "five_hour_usage_reset_time",
                        "fiveHourUsageResetTime",
                    )),
                });
            }
            if let Some(rate) = weekly {
                windows.push(RelayQuotaWindow {
                    label: "7D".to_string(),
                    remaining_percent: Some(rate),
                    reset_at: Self::stepfun_timestamp(Self::stepfun_field(
                        body,
                        "weekly_usage_reset_time",
                        "weeklyUsageResetTime",
                    )),
                });
            }
        }

        let remaining = windows
            .iter()
            .filter_map(|window| window.remaining_percent)
            .reduce(f64::min)
            .ok_or_else(|| {
                let desc = Self::stepfun_field(body, "desc", "desc")
                    .and_then(Value::as_str)
                    .unwrap_or("响应中没有可识别的 Step Plan 额度字段");
                format!("StepFun 额度解析失败: {}", desc)
            })?;
        let next_reset_at = windows.iter().filter_map(|window| window.reset_at).min();
        let is_active = windows
            .iter()
            .any(|window| window.remaining_percent.is_some_and(|value| value > 0.0));
        Ok(crate::account::RelayUsageCache {
            windows,
            remaining,
            unit: "% Step Plan Credit".to_string(),
            is_active,
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

    let resp = req
        .send()
        .await
        .map_err(|e| format!("网络请求失败: {}", e))?;
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

/// 主动重置（消耗一次 rate_limit_reset_credit）的结果，回传给前端。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResetCreditResult {
    /// true = 上游确实重置了至少一个窗口（code == "reset"）
    pub ok: bool,
    pub status_code: u16,
    /// 上游 code：reset / nothing_to_reset / no_credit / already_redeemed / http_error / unknown
    pub code: String,
    /// 实际被重置的窗口数（reset 时 > 0）
    pub windows_reset: i64,
    /// 翻成人话的提示文案
    pub message: String,
    /// 上游回传「实际被消耗的那条 credit id」（成功时 `credit.id`）。
    /// 客户端无法定向消耗——烧哪条由服务端决定，这里仅用于回显「烧掉的是哪条」。
    pub consumed_credit_id: Option<String>,
    /// 上游原始响应（成功/失败都带上，方便前端兜底）
    pub upstream_raw: String,
}

/// 一条可用的「主动重置次数」。来自 `GET wham/rate-limit-reset-credits` 的
/// `credits[]`（只取 `status == "available"`）。所有 credit 功能等价（都是
/// 「Full reset (Weekly + 5 hr)」），唯一区别是到期时间——不用就作废。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResetCreditItem {
    /// 上游唯一 id；仅用于回显「实际消耗的是哪条」，不能用于定向消耗。
    pub id: String,
    /// 到期时间（Unix 秒）。None = 上游没给/解析失败。
    pub expires_at: Option<i64>,
    /// 获得时间（Unix 秒）。
    pub granted_at: Option<i64>,
    /// 标题，如 "Full reset (Weekly + 5 hr)"。
    pub title: String,
    /// 来源人话，如 "邀请 amazing8078 获得" / "Codex 赠送"。
    pub source: String,
}

/// 把 ISO8601 / RFC3339（如 `2026-07-18T00:34:00.259879Z`）转成 Unix 秒。
fn parse_iso8601_to_unix(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp())
}

/// 把一条 credit 的来源翻成人话。优先从 description 抽出被邀请邮箱，
/// 否则按 profile_user_id（"Codex Team" / "@handle"）兜底。
fn reset_credit_source(profile_user_id: &str, description: &str) -> String {
    // 邀请获得：description 形如 "...for inviting amazing8078@gmail.com"
    if let Some(idx) = description.find("for inviting ") {
        let rest = &description[idx + "for inviting ".len()..];
        let token = rest
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_end_matches('.');
        let handle = token.split('@').next().unwrap_or(token);
        if !handle.is_empty() {
            return format!("邀请 {} 获得", handle);
        }
    }
    if profile_user_id.eq_ignore_ascii_case("Codex Team") {
        return "Codex 赠送".to_string();
    }
    if let Some(h) = profile_user_id.strip_prefix('@') {
        if !h.is_empty() {
            return format!("邀请 {} 获得", h);
        }
    }
    if !profile_user_id.is_empty() {
        return profile_user_id.to_string();
    }
    "未知来源".to_string()
}

/// 拉取该账号所有「可用」的主动重置次数，按到期时间升序（最早在前）返回。
/// `GET /backend-api/wham/rate-limit-reset-credits`（只读、不消耗）。
/// 走 quota 同一条出口（codex UA + originator + ChatGPT-Account-Id）。
pub async fn list_reset_credits(
    access_token: &str,
    account_id: Option<&str>,
) -> Result<Vec<ResetCreditItem>, String> {
    let client = usage_client();
    let mut req = client
        .get("https://chatgpt.com/backend-api/wham/rate-limit-reset-credits")
        .header("Authorization", format!("Bearer {}", access_token))
        .header("User-Agent", crate::codex_ua::codex_user_agent())
        .header("originator", crate::codex_ua::CODEX_ORIGINATOR)
        .header("Accept", "application/json")
        .timeout(Duration::from_secs(30));
    if let Some(id) = account_id {
        if !id.is_empty() {
            req = req.header("ChatGPT-Account-Id", id);
        }
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("网络请求失败: {}", e))?;
    let status = resp.status();
    let raw = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(match status.as_u16() {
            401 => "token 失效（401），请刷新或重新登录".to_string(),
            403 => "需要验证（403）".to_string(),
            429 => "请求过于频繁（429），稍后再试".to_string(),
            c => format!("上游返回 HTTP {}", c),
        });
    }

    let json: Value = serde_json::from_str(&raw).map_err(|_| "上游返回非 JSON".to_string())?;
    if !json.get("credits").is_some_and(Value::is_array) {
        return Err("上游未返回有效的重置银行明细，当前次数未知（不代表已清空）".to_string());
    }
    let mut items: Vec<ResetCreditItem> = json
        .get("credits")
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|c| {
                    c.get("status")
                        .and_then(|s| s.as_str())
                        .map(|s| s == "available")
                        .unwrap_or(false)
                })
                .map(|c| {
                    let id = c
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let expires_at = c
                        .get("expires_at")
                        .and_then(|v| v.as_str())
                        .and_then(parse_iso8601_to_unix);
                    let granted_at = c
                        .get("granted_at")
                        .and_then(|v| v.as_str())
                        .and_then(parse_iso8601_to_unix);
                    let title = c
                        .get("title")
                        .and_then(|v| v.as_str())
                        .unwrap_or("Rate limit reset")
                        .to_string();
                    let profile = c
                        .get("profile_user_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let desc = c.get("description").and_then(|v| v.as_str()).unwrap_or("");
                    ResetCreditItem {
                        id,
                        expires_at,
                        granted_at,
                        title,
                        source: reset_credit_source(profile, desc),
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    // 最早到期在前（无到期时间的沉底）——服务端通常按这个顺序消耗。
    items.sort_by(|a, b| match (a.expires_at, b.expires_at) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });

    Ok(items)
}

/// 消耗一次「主动重置次数」立即重置已耗尽的限额窗口：
/// `POST /backend-api/wham/rate-limit-reset-credits/consume`，body `{"redeem_request_id": "<uuid>"}`。
///
/// `redeem_request_id` 是幂等键 —— 同一个 id 重发只会兑现一次（上游回 `already_redeemed`），
/// 所以每次按钮点击都生成一把新的 UUID。响应形如：
/// `{"code":"reset|nothing_to_reset|no_credit|already_redeemed","windows_reset":N}`。
///
/// 走 quota 同一条出口（`usage_client` + codex UA + originator + ChatGPT-Account-Id），
/// 保证带 token 的请求跟该账号平时的 quota 查询走同一个 egress（Pro 号同 IP 约束）。
pub async fn consume_reset_credit(
    access_token: &str,
    account_id: Option<&str>,
) -> Result<ResetCreditResult, String> {
    let redeem_request_id = uuid::Uuid::new_v4().to_string();
    let body = serde_json::json!({ "redeem_request_id": redeem_request_id });

    let client = usage_client();
    let mut req = client
        .post("https://chatgpt.com/backend-api/wham/rate-limit-reset-credits/consume")
        .header("Authorization", format!("Bearer {}", access_token))
        .header("User-Agent", crate::codex_ua::codex_user_agent())
        .header("originator", crate::codex_ua::CODEX_ORIGINATOR)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .timeout(Duration::from_secs(30))
        .json(&body);
    if let Some(id) = account_id {
        if !id.is_empty() {
            req = req.header("ChatGPT-Account-Id", id);
        }
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("网络请求失败: {}", e))?;
    let status = resp.status();
    let raw = resp.text().await.unwrap_or_default();
    let parsed = serde_json::from_str::<Value>(&raw).ok();

    if !status.is_success() {
        // 上游 detail / error.message 优先，缺失时按状态码给一句人话
        let detail = parsed.as_ref().and_then(|v| {
            v.get("detail")
                .and_then(|d| d.as_str())
                .or_else(|| {
                    v.get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(|m| m.as_str())
                })
                .map(|s| s.to_string())
        });
        let message = detail.unwrap_or_else(|| match status.as_u16() {
            401 => "token 失效（401），请刷新或重新登录".to_string(),
            403 => "需要验证（403）".to_string(),
            429 => "请求过于频繁（429），稍后再试".to_string(),
            _ => format!("上游返回 HTTP {}", status.as_u16()),
        });
        return Ok(ResetCreditResult {
            ok: false,
            status_code: status.as_u16(),
            code: "http_error".to_string(),
            windows_reset: 0,
            message,
            consumed_credit_id: None,
            upstream_raw: raw,
        });
    }

    let code = parsed
        .as_ref()
        .and_then(|v| v.get("code"))
        .and_then(|c| c.as_str())
        .unwrap_or("unknown")
        .to_string();
    let windows_reset = parsed
        .as_ref()
        .and_then(|v| v.get("windows_reset"))
        .and_then(|w| w.as_i64())
        .unwrap_or(0);
    // 上游回传实际烧掉的那条 credit id（`credit.id`）——客户端选不了，只能回显。
    let consumed_credit_id = parsed
        .as_ref()
        .and_then(|v| v.get("credit"))
        .and_then(|c| c.get("id"))
        .and_then(|i| i.as_str())
        .map(|s| s.to_string());
    let ok = code == "reset";
    let message = match code.as_str() {
        "reset" => format!("已重置 {} 个限额窗口", windows_reset),
        "nothing_to_reset" => "当前没有可重置的限额（额度还没用到上限）".to_string(),
        "no_credit" => "没有可用的主动重置次数了".to_string(),
        "already_redeemed" => "该重置请求已经兑现过了".to_string(),
        other => format!("上游返回：{}", other),
    };

    Ok(ResetCreditResult {
        ok,
        status_code: status.as_u16(),
        code,
        windows_reset,
        message,
        consumed_credit_id,
        upstream_raw: raw,
    })
}

/// Default wakeup model for a minimal window-prime request.
/// Keep this compatible with the ChatGPT-authenticated Codex Responses endpoint.
pub const DEFAULT_WAKEUP_MODEL: &str = "gpt-6-luna";

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
    let prompt = if prompt.trim().is_empty() {
        "你好"
    } else {
        prompt.trim()
    };
    let model = if model.trim().is_empty() {
        DEFAULT_WAKEUP_MODEL
    } else {
        model.trim()
    };
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

    let resp = req
        .send()
        .await
        .map_err(|e| format!("网络请求失败: {}", e))?;
    let status = resp.status();
    // A failed or truncated SSE read must not be treated as a completed prime.
    let raw = resp.text().await.map_err(|e| format!("Codex response read failed: {e}"))?;

    if status.is_success() {
        let completed = wakeup_stream_completed(&raw);
        Ok(WakeupResult {
            ok: completed,
            status_code: status.as_u16(),
            model: model.to_string(),
            prompt: prompt.to_string(),
            reply: if completed { parse_wakeup_reply(&raw) } else { String::new() },
            error: (!completed).then(|| "Codex returned HTTP success without a completed model response".to_string()),
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

/// The HTTP status alone does not mean that the streamed inference succeeded.
fn wakeup_stream_completed(sse: &str) -> bool {
    let mut completed = false;
    for line in sse.lines() {
        let Some(payload) = line.trim_start().strip_prefix("data:") else { continue };
        let Ok(value) = serde_json::from_str::<Value>(payload.trim()) else { continue };
        match value.get("type").and_then(Value::as_str) {
            Some("response.completed") => {
                completed = value.get("response")
                    .and_then(|r| r.get("status"))
                    .and_then(Value::as_str)
                    .is_some_and(|status| status == "completed");
            }
            Some("response.failed" | "response.incomplete" | "error") => return false,
            _ => {}
        }
    }
    completed
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
        let Ok(v) = serde_json::from_str::<Value>(payload) else {
            continue;
        };
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
    fn wakeup_uses_current_lightweight_model() {
        assert_eq!(DEFAULT_WAKEUP_MODEL, "gpt-6-luna");
    }

    #[test]
    fn window_prime_requires_completed_response() {
        assert!(wakeup_stream_completed("data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n"));
        assert!(!wakeup_stream_completed("data: {\"type\":\"response.created\"}\n"));
        assert!(!wakeup_stream_completed("data: {\"type\":\"response.failed\"}\n"));
        assert!(!wakeup_stream_completed("data: {\"type\":\"response.completed\",\"response\":{\"status\":\"failed\"}}\n"));
        assert!(!wakeup_stream_completed("data: {\"type\":\"response.completed\"}\n"));
        assert!(!wakeup_stream_completed("data: {\"type\":\"response.completed\",\"response\":{}}\n"));
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
    fn luna_reserve_is_parsed_as_an_independent_model_window() {
        let body = json!({
            "plan_type": "plus",
            "rate_limit": {
                "primary_window": {"used_percent": 100, "limit_window_seconds": 18000},
                "secondary_window": {"used_percent": 67, "limit_window_seconds": 604800}
            },
            "additional_rate_limits": [{
                "limit_name": "gpt-reserve",
                "normal_model_slug": "gpt-5.6-luna",
                "rate_limit": {
                    "allowed": true,
                    "limit_reached": false,
                    "primary_window": {"used_percent": 3, "reset_after_seconds": 604631}
                }
            }]
        });
        let usage = UsageFetcher::parse_usage_response(&body).unwrap();
        let reserve = usage.luna_reserve.unwrap();
        assert!(reserve.is_available_for("gpt-5.6-luna"));
        assert!(!reserve.is_available_for("gpt-5.5"));
    }

    #[test]
    fn credits_balance_is_parsed_and_can_be_used_as_fallback() {
        let body = json!({
            "plan_type": "plus",
            "rate_limit": {
                "primary_window": {"used_percent": 100, "limit_window_seconds": 18000},
                "secondary_window": {"used_percent": 100, "limit_window_seconds": 604800}
            },
            "credits": {"has_credits": true, "balance": 1000}
        });
        let mut usage = UsageFetcher::parse_usage_response(&body).unwrap();
        assert_eq!(usage.credits_balance, Some(1000.0));
        assert!(usage.has_credits);
        assert!(usage.has_spendable_credits());
        assert!(usage.has_usable_quota());
        assert!(!usage.has_rate_limit_quota());

        usage.credits_balance = Some(0.0);
        assert!(!usage.has_spendable_credits());
        assert!(!usage.has_usable_quota());
    }

    #[test]
    fn workspace_hard_stop_overrides_positive_windows_and_credits() {
        for reason in [
            serde_json::json!("workspace_owner_credits_depleted"),
            serde_json::json!({"type":"workspace_member_usage_limit_reached"}),
        ] {
            let body = serde_json::json!({"plan_type":"business", "rate_limit_reached_type":reason,
                "rate_limit":{"allowed":false,"limit_reached":true,
                    "primary_window":{"used_percent":0},"secondary_window":{"used_percent":0}},
                "credits":{"has_credits":true,"balance":100}});
            let usage = UsageFetcher::parse_usage_response(&body).unwrap();
            assert!(usage.desktop_gate.as_ref().unwrap().workspace_blocked());
            assert!(!usage.has_usable_quota());
            let cached = crate::cached_quota_from_usage(&usage);
            assert!(!cached.has_usable_quota());
        }
    }

    #[test]
    fn desktop_gate_requires_explicit_availability() {
        let known = UsageFetcher::parse_usage_response(&serde_json::json!({"plan_type":"plus",
            "rate_limit":{"allowed":true,"limit_reached":false}}))
        .unwrap();
        assert!(known.desktop_gate.unwrap().confirmed_available());
        let unknown = UsageFetcher::parse_usage_response(&serde_json::json!({"plan_type":"plus",
            "rate_limit":{"primary_window":{"used_percent":0}}}))
        .unwrap();
        assert!(!unknown.desktop_gate.unwrap().confirmed_available());
    }

    #[test]
    fn spend_control_and_nested_reason_are_preserved() {
        let blocked =
            UsageFetcher::parse_usage_response(&serde_json::json!({"plan_type":"business",
            "rate_limit":{"allowed":true,"limit_reached":false},"spend_control":{"reached":true}}))
            .unwrap();
        assert!(blocked.desktop_gate.as_ref().unwrap().workspace_blocked());
        assert!(!blocked.desktop_gate.unwrap().confirmed_available());
        let nested =
            UsageFetcher::parse_usage_response(&serde_json::json!({"plan_type":"business",
            "rate_limit_reached_type":null,"rate_limit":{"allowed":false,"limit_reached":true,
                "rate_limit_reached_type":"workspace_owner_credits_depleted"}}))
            .unwrap();
        assert!(nested.desktop_gate.unwrap().workspace_blocked());
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
            Some(1_777_939_199)
        );
    }

    #[test]
    fn stepfun_token_normalizer_accepts_raw_token_or_cookie() {
        assert_eq!(
            UsageFetcher::normalize_stepfun_console_token("raw-step-token"),
            Some("raw-step-token".to_string())
        );
        assert_eq!(
            UsageFetcher::normalize_stepfun_console_token(
                "Cookie: foo=bar; Oasis-Token=console-token; baz=qux"
            ),
            Some("console-token".to_string())
        );
        assert!(UsageFetcher::normalize_stepfun_console_token("Cookie: foo=bar").is_none());
    }

    #[test]
    fn stepfun_rate_limit_parser_reads_credit_buckets() {
        let body = json!({
            "planCreditRateLimit": {
                "creditBuckets": [
                    {"type": "SUBSCRIPTION", "creditTotal": 400000000, "creditResidual": 300000000, "nextResetAt": 1790000000},
                    {"type": "TOPUP", "creditTotal": "160000000", "creditResidual": "80000000", "expireAt": 1791000000}
                ]
            }
        });
        let cache = UsageFetcher::parse_stepfun_rate_limit_response(&body).unwrap();
        assert_eq!(cache.windows.len(), 2);
        assert_eq!(cache.windows[0].label, "订阅 Credit");
        assert_eq!(cache.windows[0].remaining_percent, Some(75.0));
        assert_eq!(cache.windows[1].label, "加油包 Credit");
        assert_eq!(cache.windows[1].remaining_percent, Some(50.0));
        assert_eq!(cache.remaining, 50.0);
        assert_eq!(cache.next_reset_at, Some(1790000000));
    }

    #[test]
    fn stepfun_rate_limit_parser_falls_back_to_5h_and_weekly() {
        let body = json!({
            "fiveHourUsageLeftRate": 0.8,
            "fiveHourUsageResetTime": 1790000000000i64,
            "weeklyUsageLeftRate": 35,
            "weeklyUsageResetTime": 1791000000
        });
        let cache = UsageFetcher::parse_stepfun_rate_limit_response(&body).unwrap();
        assert_eq!(cache.windows[0].label, "5H");
        assert_eq!(cache.windows[0].remaining_percent, Some(80.0));
        assert_eq!(cache.windows[0].reset_at, Some(1790000000));
        assert_eq!(cache.windows[1].label, "7D");
        assert_eq!(cache.windows[1].remaining_percent, Some(35.0));
        assert_eq!(cache.remaining, 35.0);
    }
}
