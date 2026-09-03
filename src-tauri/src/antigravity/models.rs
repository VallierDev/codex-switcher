use serde::Serialize;
use std::collections::BTreeSet;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AntigravityModel {
    pub id: String,
    pub display_name: String,
    pub description: String,
    pub context_length: u64,
    pub max_completion_tokens: u64,
    pub input_modalities: Vec<&'static str>,
    pub output_modalities: Vec<&'static str>,
    pub thinking_levels: Vec<&'static str>,
}

pub fn is_public_model_id(id: &str) -> bool {
    let supported_family =
        id.starts_with("gemini-") || id.starts_with("claude-") || id.starts_with("gpt-oss-");
    supported_family
        && id.len() <= 160
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        && !id.contains("-image")
        && !id.starts_with("chat_")
        && !id.starts_with("tab_")
}

pub fn catalog_from_live_ids<I, S>(ids: I) -> Vec<AntigravityModel>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let unique: BTreeSet<String> = ids
        .into_iter()
        .map(|id| id.as_ref().to_string())
        .filter(|id| is_public_model_id(id))
        .collect();
    unique
        .into_iter()
        .filter_map(|id| model_for_id(&id))
        .collect()
}

pub fn model_for_id(id: &str) -> Option<AntigravityModel> {
    if !is_public_model_id(id) {
        return None;
    }
    let is_claude = id.starts_with("claude-");
    let display_name = humanize_model_id(id);
    let thinking_levels = if id.ends_with("-low") || id.ends_with("-extra-low") {
        vec!["low"]
    } else if id.ends_with("-medium") {
        vec!["medium"]
    } else if id.ends_with("-high") || id.ends_with("-thinking") {
        vec!["high"]
    } else {
        vec!["minimal", "low", "medium", "high"]
    };
    Some(AntigravityModel {
        id: id.to_string(),
        display_name: format!("{display_name} (Antigravity)"),
        description: format!("{display_name} through Google Antigravity OAuth"),
        context_length: if is_claude { 200_000 } else { 1_048_576 },
        max_completion_tokens: 65_536,
        input_modalities: vec!["text", "image"],
        output_modalities: vec!["text"],
        thinking_levels,
    })
}

fn humanize_model_id(id: &str) -> String {
    let mut words: Vec<String> = Vec::new();
    for part in id.split('-') {
        if !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()) {
            if let Some(last) = words.last_mut() {
                if last.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
                    last.push('.');
                    last.push_str(part);
                    continue;
                }
            }
        }
        words.push(part.to_string());
    }
    words
        .iter()
        .map(String::as_str)
        .map(|part| match part {
            "gemini" => "Gemini".to_string(),
            "claude" => "Claude".to_string(),
            "gpt" => "GPT".to_string(),
            "oss" => "OSS".to_string(),
            "opus" => "Opus".to_string(),
            "sonnet" => "Sonnet".to_string(),
            "pro" => "Pro".to_string(),
            "flash" => "Flash".to_string(),
            "thinking" => "Thinking".to_string(),
            "high" => "High".to_string(),
            "medium" => "Medium".to_string(),
            "low" => "Low".to_string(),
            "tiered" => "Tiered".to_string(),
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_catalog_adds_claude_and_future_gemini_automatically() {
        let models = catalog_from_live_ids([
            "claude-sonnet-4-6",
            "claude-opus-4-6-thinking",
            "gemini-3.9-flash-high",
            "chat_20706",
            "tab_flash_lite_preview",
            "gemini-3.1-flash-image",
        ]);
        let ids: Vec<_> = models.iter().map(|model| model.id.as_str()).collect();
        assert!(ids.contains(&"claude-sonnet-4-6"));
        assert!(ids.contains(&"claude-opus-4-6-thinking"));
        assert!(ids.contains(&"gemini-3.9-flash-high"));
        assert!(!ids.contains(&"gemini-3.8-flash"));
        assert!(!ids.contains(&"chat_20706"));
        assert!(!ids.contains(&"tab_flash_lite_preview"));
        assert!(!ids.contains(&"gemini-3.1-flash-image"));
    }

    #[test]
    fn claude_gets_conservative_codex_metadata() {
        let model = model_for_id("claude-sonnet-4-6").unwrap();
        assert_eq!(model.display_name, "Claude Sonnet 4.6 (Antigravity)");
        assert_eq!(model.context_length, 200_000);
        assert!(is_public_model_id("gemini-3.8-flash"));
        assert!(!is_public_model_id("gpt-5.6-sol"));
        assert!(catalog_from_live_ids(std::iter::empty::<&str>()).is_empty());
    }

    #[test]
    fn refreshed_ids_replace_old_models_without_static_seeds() {
        let old = catalog_from_live_ids(["gemini-3.7-flash-high"]);
        let new = catalog_from_live_ids(["gemini-3.8-flash-high", "gemini-3.8-flash-high"]);
        assert_eq!(old.len(), 1);
        assert_eq!(new.len(), 1);
        assert_eq!(new[0].id, "gemini-3.8-flash-high");
        assert!(!new.iter().any(|model| model.id == old[0].id));
    }
}
