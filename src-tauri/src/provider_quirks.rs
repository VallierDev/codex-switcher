//! Per-provider quirks for chat_completions Relay 上游。
//!
//! 历史背景：每个第三方 OpenAI-兼容 provider 都有自己的小 quirk —— MiMo 要
//! 在 body 里加 `webSearchEnabled: true` 才让 web_search tool 生效，GLM 把
//! 上下文超限报 `1261`，DeepSeek 的 chat_completions 自带又一套 quota 错误
//! 文案。原本这堆判断散在 proxy.rs 里通过 `is_mimo_relay_base` / hardcoded
//! 关键字探测交错调用，每加一个 provider 都要去找散乱的钩子。
//!
//! 这里把 quirk 收口成一个 enum + 几个简单函数：
//! - `detect_provider(base_url)`：从 base_url 推断 provider 身份
//! - `preprocess_chat_body(provider, &mut body)`：发上游前 mutate body
//! - `enhance_error_hint(provider, status, msg)`：把上游 4xx 错误文案翻译成
//!   "用户能照着修"的提示（比如 MiMo Web Search Plugin 未激活时直接给激活 URL）
//!
//! 加新 provider 时：扩 `RelayProvider` enum + 在 `detect_provider` 里加 host
//! 匹配 + 按需在 preprocess / enhance_error_hint 加分支。不动 proxy.rs 的调用结构。

use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelayProvider {
    /// Xiaomi MiMo (token-plan-*.xiaomimimo.com / api.xiaomimimo.com)
    Mimo,
    /// 智谱 GLM (open.bigmodel.cn 普通 paas 或 coding plan)
    Glm,
    /// DeepSeek (api.deepseek.com)
    DeepSeek,
    /// 其它（OpenAI-兼容透传，无 quirk）
    Generic,
}

/// 按 base_url 主机识别 provider。
///
/// 大小写不敏感、子域名包含匹配（`token-plan-sgp.xiaomimimo.com` 也算 MiMo）。
/// 未知/缺省 → `Generic`，不做任何 mutation。
pub fn detect_provider(base_url: Option<&str>) -> RelayProvider {
    let Some(url) = base_url else {
        return RelayProvider::Generic;
    };
    let u = url.to_ascii_lowercase();
    if u.contains("xiaomimimo.com") {
        RelayProvider::Mimo
    } else if u.contains("bigmodel.cn") {
        RelayProvider::Glm
    } else if u.contains("deepseek.com") {
        RelayProvider::DeepSeek
    } else {
        RelayProvider::Generic
    }
}

/// 发上游前 mutate body 的总入口。每个 provider 在这里实现自家的 body 改写
/// 规则。当前只有 MiMo 需要 `webSearchEnabled` 强标记。
pub fn preprocess_chat_body(provider: RelayProvider, body: &mut Vec<u8>) {
    match provider {
        RelayProvider::Mimo => force_mimo_web_search_flag(body),
        RelayProvider::Glm | RelayProvider::DeepSeek | RelayProvider::Generic => {}
    }
}

/// MiMo：codex 端发 web_search 工具时，必须在 body 顶层加
/// `webSearchEnabled: true`，否则 MiMo 即使 tools 数组里有 web_search，也会
/// 直接返 400 "webSearchEnabled is false"。
fn force_mimo_web_search_flag(body: &mut Vec<u8>) {
    let mut value: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return,
    };
    let has_web_search = value
        .get("tools")
        .and_then(|v| v.as_array())
        .map(|tools| {
            tools.iter().any(|tool| {
                tool.get("type").and_then(|v| v.as_str()) == Some("web_search")
                    || tool.get("web_search").is_some()
            })
        })
        .unwrap_or(false);
    if !has_web_search {
        return;
    }
    if let Some(obj) = value.as_object_mut() {
        obj.insert("webSearchEnabled".to_string(), Value::Bool(true));
        if let Ok(out) = serde_json::to_vec(&value) {
            *body = out;
        }
    }
}

/// 返回"用户能照着修"的提示，会被 `normalize_chat_completions_error` 追加
/// 到归一化后的 error message 里。返回 None 表示这个 provider 没什么特别的
/// 用户向提示。
///
/// 给的是**操作可行的提示**（比如激活 URL、配额查看面板链接），不是文学描述。
pub fn enhance_error_hint(provider: RelayProvider, msg_lower: &str) -> Option<&'static str> {
    match provider {
        RelayProvider::Mimo => {
            if msg_lower.contains("websearchenabled is false") {
                return Some(
                    "MiMo Web Search Plugin 未激活；到 https://platform.xiaomimimo.com/#/console/plugin 开通后重新发起请求。",
                );
            }
            if msg_lower.contains("reasoning_content") && msg_lower.contains("must be passed back")
            {
                return Some(
                    "MiMo thinking 模式要求历史里的 assistant 轮回传完整 reasoning_content；本机已通过 encrypted_content round-trip 修复，如仍报此错可能是 codex 版本不支持，升级 codex 到 0.130+。",
                );
            }
            None
        }
        RelayProvider::Glm => {
            if msg_lower.contains("prompt exceeds max length") || msg_lower.contains("1261") {
                return Some(
                    "GLM 上下文超长（错误码 1261）。Codex 收到本错误后会自动 compact 历史并重试，无需手动处理。",
                );
            }
            None
        }
        RelayProvider::DeepSeek => {
            if msg_lower.contains("insufficient_balance") || msg_lower.contains("余额不足") {
                return Some(
                    "DeepSeek 账户余额不足，前往 https://platform.deepseek.com/usage 充值后重试。",
                );
            }
            None
        }
        RelayProvider::Generic => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_mimo_tokenplan() {
        assert_eq!(
            detect_provider(Some("https://token-plan-sgp.xiaomimimo.com/v1")),
            RelayProvider::Mimo
        );
        assert_eq!(
            detect_provider(Some("https://api.xiaomimimo.com/v1")),
            RelayProvider::Mimo
        );
    }

    #[test]
    fn detect_glm_variants() {
        assert_eq!(
            detect_provider(Some("https://open.bigmodel.cn/api/paas/v4")),
            RelayProvider::Glm
        );
        assert_eq!(
            detect_provider(Some("https://open.bigmodel.cn/api/coding/paas/v4")),
            RelayProvider::Glm
        );
    }

    #[test]
    fn detect_deepseek() {
        assert_eq!(
            detect_provider(Some("https://api.deepseek.com/v1")),
            RelayProvider::DeepSeek
        );
    }

    #[test]
    fn detect_unknown_is_generic() {
        assert_eq!(
            detect_provider(Some("https://example.com/v1")),
            RelayProvider::Generic
        );
        assert_eq!(detect_provider(None), RelayProvider::Generic);
    }

    #[test]
    fn mimo_web_search_flag_injected_when_tools_has_websearch() {
        let mut body = br#"{"messages":[],"tools":[{"type":"web_search"}]}"#.to_vec();
        preprocess_chat_body(RelayProvider::Mimo, &mut body);
        let s = String::from_utf8(body).unwrap();
        assert!(
            s.contains("\"webSearchEnabled\":true"),
            "expected flag in: {}",
            s
        );
    }

    #[test]
    fn mimo_web_search_flag_not_injected_without_websearch() {
        let mut body = br#"{"messages":[],"tools":[{"type":"function"}]}"#.to_vec();
        preprocess_chat_body(RelayProvider::Mimo, &mut body);
        let s = String::from_utf8(body).unwrap();
        assert!(!s.contains("webSearchEnabled"), "should not inject: {}", s);
    }

    #[test]
    fn glm_no_body_mutation() {
        let mut body = br#"{"messages":[],"tools":[{"type":"web_search"}]}"#.to_vec();
        let before = body.clone();
        preprocess_chat_body(RelayProvider::Glm, &mut body);
        assert_eq!(
            body, before,
            "GLM body should not be mutated by provider quirks"
        );
    }

    #[test]
    fn enhance_mimo_websearch_disabled() {
        let hint = enhance_error_hint(RelayProvider::Mimo, "websearchenabled is false");
        assert!(hint.is_some());
        assert!(hint.unwrap().contains("Plugin"));
    }

    #[test]
    fn enhance_generic_none() {
        assert!(enhance_error_hint(RelayProvider::Generic, "any message").is_none());
    }
}
