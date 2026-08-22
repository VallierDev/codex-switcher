//! Session affinity（会话亲和性）+ Evidence-based stickiness（证据驱动黏性）
//!
//! 目的：保住 OpenAI 那头的 prompt cache —— 同一个会话的连续请求尽量打回同一个账号，
//! 因为 OpenAI 的 prompt cache key 是按 organization/account 隔离的，切号 = cache 全失。
//!
//! 设计参考：
//! - CLIProxyAPI sdk/cliproxy/auth/selector.go::SessionAffinitySelector —— 多源 ID 提取 + TTL
//! - labring/aiproxy core/relay/plugin/cachefollow —— "看到 cached_tokens>0 才记忆账号"
//!
//! 实现要点：
//! 1. session_key 提取优先级：prompt_cache_key > previous_response_id > Session_id 头 >
//!    (model + 首条 user input 哈希)
//! 2. 只在 **响应里 cached_tokens > 0** 时才记 session_key → account 绑定（evidence-based）
//! 3. 路由时若绑定账号健康且非当前 → 静默切号；否则用当前账号
//! 4. 账号被标 banned / quota 耗尽时，连同它的所有 session 绑定一起作废

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 单条绑定的 TTL（无新命中刷新就过期）
const BINDING_TTL: Duration = Duration::from_secs(3600); // 1 小时

/// 同一个 session_key 在 `RECENT_DEBOUNCE` 内不重复记账（避免 hot loop 反复写）
const RECENT_DEBOUNCE: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
struct BindingEntry {
    account_id: String,
    /// 首次绑定时间
    first_bound_at: Instant,
    /// 最后一次"看到 cached_tokens>0"的时间（也是 TTL 刷新点）
    last_hit_at: Instant,
    /// 累计命中次数
    hit_count: u32,
    /// 累计 cached_tokens
    total_cached_tokens: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionBindingSnapshot {
    pub session_key: String,
    pub account_id: String,
    pub age_secs: u64,
    pub hit_count: u32,
    pub total_cached_tokens: i64,
}

pub struct SessionAffinity {
    bindings: Mutex<HashMap<String, BindingEntry>>,
}

impl SessionAffinity {
    pub fn new() -> Self {
        Self {
            bindings: Mutex::new(HashMap::new()),
        }
    }

    /// 按 session_key 查健康绑定。account_filter 用于排除被标记 banned/exhausted 的号。
    pub fn lookup<F>(&self, session_key: &str, account_filter: F) -> Option<String>
    where
        F: Fn(&str) -> bool,
    {
        let mut g = self.bindings.lock().ok()?;
        let entry = g.get(session_key)?.clone();
        if entry.last_hit_at.elapsed() > BINDING_TTL {
            g.remove(session_key);
            return None;
        }
        if !account_filter(&entry.account_id) {
            // 账号已不健康 —— 清掉这条 binding，让上层重新走选号
            g.remove(session_key);
            return None;
        }
        Some(entry.account_id)
    }

    /// 记录一次响应完成（response.completed），把 session 黏到当前号上。
    ///
    /// 原本是 "evidence-based"：只在 `cached_tokens > 0` 时才记。但 codex 的
    /// `prompt_cache_key = thread_id` 跨 turn 稳定（codex-rs/core/src/client.rs:742
    /// 已验证），首轮必然 cold cache `cached_tokens=0`；如果非要等到第二轮看见命中
    /// 才记 binding，那中间任何一次 quota 抖动 / 切号都会把这个会话推到别的号上，
    /// 第二轮的"命中证据"永远等不到。
    ///
    /// 改成：响应一完成就记 binding（不要求 cache 命中），让后续请求能稳定回到
    /// 同一个号建/复用 cache。`cached_tokens` 仅用作统计累加。
    pub fn record_cache_hit(&self, session_key: &str, account_id: &str, cached_tokens: i64) {
        let mut g = match self.bindings.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        match g.get_mut(session_key) {
            Some(e) if e.account_id == account_id => {
                // debounce：30s 内的重复命中只刷 last_hit_at + 计数
                if e.last_hit_at.elapsed() >= RECENT_DEBOUNCE {
                    e.last_hit_at = Instant::now();
                }
                e.hit_count = e.hit_count.saturating_add(1);
                if cached_tokens > 0 {
                    e.total_cached_tokens = e.total_cached_tokens.saturating_add(cached_tokens);
                }
            }
            _ => {
                // 切到新账号 / 第一次记 → 覆盖（也可以保留两槽 stable+recent 但先简单实现）
                g.insert(
                    session_key.to_string(),
                    BindingEntry {
                        account_id: account_id.to_string(),
                        first_bound_at: Instant::now(),
                        last_hit_at: Instant::now(),
                        hit_count: 1,
                        total_cached_tokens: cached_tokens.max(0),
                    },
                );
            }
        }
    }

    /// 该账号是否有任何未过期的 session binding。
    /// 用于 `should_preemptive_switch` —— 当前号正承载活跃会话时不抢切，保住 cache。
    pub fn has_active_binding_to(&self, account_id: &str) -> bool {
        let g = match self.bindings.lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        g.values()
            .any(|e| e.account_id == account_id && e.last_hit_at.elapsed() <= BINDING_TTL)
    }

    /// 账号被标 banned / quota 0 时，作废所有指向它的 binding，让下次同 session 重新选号。
    pub fn invalidate_account(&self, account_id: &str) {
        if let Ok(mut g) = self.bindings.lock() {
            g.retain(|_, e| e.account_id != account_id);
        }
    }

    /// 清掉所有过期 binding（懒清理时调用，不强制）
    pub fn gc(&self) {
        if let Ok(mut g) = self.bindings.lock() {
            g.retain(|_, e| e.last_hit_at.elapsed() <= BINDING_TTL);
        }
    }

    pub fn snapshot(&self) -> Vec<SessionBindingSnapshot> {
        let Ok(g) = self.bindings.lock() else {
            return Vec::new();
        };
        g.iter()
            .map(|(k, e)| SessionBindingSnapshot {
                session_key: k.clone(),
                account_id: e.account_id.clone(),
                age_secs: e.first_bound_at.elapsed().as_secs(),
                hit_count: e.hit_count,
                total_cached_tokens: e.total_cached_tokens,
            })
            .collect()
    }
}

impl Default for SessionAffinity {
    fn default() -> Self {
        Self::new()
    }
}

// ────────────────────────────────────────────────────────────────
// Session key 提取
// ────────────────────────────────────────────────────────────────

/// 从请求体 + headers 里提取一个稳定的 session_key。
/// 优先级（codex 常见顺序）：
///   1. body.prompt_cache_key —— codex CLI / 新 SDK 显式提供
///   2. body.previous_response_id —— Responses API 链式上下文
///   3. headers.Session_id / X-Session-Id —— 部分 client 用
///   4. (model + 首段 input 文本前 256 字符的哈希) —— 兜底，对非链式请求也能聚到一起
pub fn extract_session_key(body: &[u8], headers: &reqwest::header::HeaderMap) -> Option<String> {
    // 4 路兜底，前 3 路要解 JSON
    let json: Option<serde_json::Value> = serde_json::from_slice(body).ok();

    if let Some(v) = json.as_ref() {
        if let Some(s) = v.get("prompt_cache_key").and_then(|x| x.as_str()) {
            if !s.is_empty() {
                return Some(format!("pck:{s}"));
            }
        }
        if let Some(s) = v.get("previous_response_id").and_then(|x| x.as_str()) {
            if !s.is_empty() {
                return Some(format!("prev:{s}"));
            }
        }
    }

    for h in &["session_id", "session-id", "x-session-id"] {
        if let Some(v) = headers.get(*h).and_then(|v| v.to_str().ok()) {
            if !v.is_empty() {
                return Some(format!("hdr:{v}"));
            }
        }
    }

    if let Some(v) = json {
        let model = v.get("model").and_then(|x| x.as_str()).unwrap_or("");
        // input 可能是 string 或 array<{type, content}>
        let input_text = match v.get("input") {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Array(arr)) => arr
                .first()
                .and_then(|m| {
                    m.get("content").and_then(|c| match c {
                        serde_json::Value::String(s) => Some(s.clone()),
                        serde_json::Value::Array(parts) => parts
                            .iter()
                            .filter_map(|p| {
                                p.get("text")
                                    .and_then(|t| t.as_str())
                                    .map(|s| s.to_string())
                            })
                            .next(),
                        _ => None,
                    })
                })
                .unwrap_or_default(),
            _ => String::new(),
        };
        if !input_text.is_empty() {
            let prefix: String = input_text.chars().take(256).collect();
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            std::hash::Hash::hash(&model, &mut hasher);
            std::hash::Hash::hash(&prefix, &mut hasher);
            let h = std::hash::Hasher::finish(&hasher);
            return Some(format!("hash:{h:016x}"));
        }
    }

    None
}
