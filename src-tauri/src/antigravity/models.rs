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
    pub default_thinking_level: &'static str,
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
    // Suffixes are upstream defaults, not a restriction on selectable effort.
    // Gemini accepts an explicit thinkingLevel even on a *-high model ID.
    let thinking_levels = if id.starts_with("gpt-oss-") {
        vec!["medium"]
    } else if id.starts_with("gemini-3.6-")
        || id.starts_with("gemini-3.5-")
        || id.starts_with("gemini-3-flash")
    {
        vec!["minimal", "low", "medium", "high"]
    } else {
        vec!["low", "medium", "high"]
    };
    let default_thinking_level = if id.ends_with("-extra-low") {
        "minimal"
    } else if id.ends_with("-low") {
        "low"
    } else if id.ends_with("-medium") {
        "medium"
    } else {
        "high"
    };
    Some(AntigravityModel {
        id: id.to_string(),
        display_name: format!("{display_name} (Antigravity)"),
        description: format!("{display_name} through Google Antigravity OAuth"),
        context_length: if is_claude { 200_000 } else { 1_048_576 },
        // Older Gemini routes advertise 65,535, not 65,536. Use the lower
        // compatible ceiling so adding thinking config doesn't cause HTTP 400.
        max_completion_tokens: if is_claude {
            64_000
        } else if id.starts_with("gpt-oss-") {
            32_768
        } else {
            65_535
        },
        input_modalities: vec!["text", "image"],
        output_modalities: vec!["text"],
        thinking_levels,
        default_thinking_level,
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

fn model_family(id: &str) -> &str {
    for suffix in [
        "-extra-low",
        "-thinking",
        "-tiered",
        "-medium",
        "-high",
        "-low",
    ] {
        if let Some(family) = id.strip_suffix(suffix) {
            return family;
        }
    }
    id
}

fn preset_priority(id: &str) -> u8 {
    if id.ends_with("-high") {
        0
    } else if id == model_family(id) {
        1
    } else if id.ends_with("-thinking") {
        2
    } else if id.ends_with("-tiered") {
        3
    } else if id.ends_with("-medium") {
        4
    } else {
        5
    }
}

fn family_rank(id: &str) -> u8 {
    if id.starts_with("gemini-") {
        0
    } else if id.starts_with("claude-") {
        1
    } else {
        2
    }
}

fn version_numbers(id: &str) -> Vec<u32> {
    id.split(|c: char| !c.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse().ok())
        .collect()
}

/// One visible entry per model, with effort selected independently. The ID stays
/// a real advertised upstream ID; hidden presets remain available to old threads.
pub fn grouped_display_models(models: &[AntigravityModel]) -> Vec<AntigravityModel> {
    let mut selected = std::collections::BTreeMap::<String, AntigravityModel>::new();
    for model in models {
        let family = model_family(&model.id).to_string();
        let replace = selected.get(&family).is_none_or(|old| {
            (preset_priority(&model.id), &model.id) < (preset_priority(&old.id), &old.id)
        });
        if replace {
            selected.insert(family, model.clone());
        }
    }
    let mut grouped: Vec<_> = selected
        .into_iter()
        .map(|(family, mut model)| {
            let name = humanize_model_id(&family);
            model.display_name = format!("{name} (Antigravity)");
            model.description = format!(
                "{name} through Google Antigravity OAuth; choose reasoning effort separately"
            );
            model
        })
        .collect();
    grouped.sort_by(|a, b| {
        family_rank(&a.id)
            .cmp(&family_rank(&b.id))
            .then_with(|| version_numbers(&b.id).cmp(&version_numbers(&a.id)))
            .then_with(|| model_family(&a.id).cmp(model_family(&b.id)))
    });
    grouped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_menu_groups_effort_presets_and_orders_newest_first() {
        let raw = catalog_from_live_ids([
            "gemini-3.7-flash-low",
            "gemini-3.8-flash-tiered",
            "gemini-3.8-flash-high",
            "gemini-3.8-flash-low",
            "gemini-3.8-flash-medium",
            "gemini-3.10-flash-high",
            "gemini-3.9-flash-high",
            "claude-opus-4-6-thinking",
            "gpt-oss-120b-medium",
        ]);
        let grouped = grouped_display_models(&raw);
        assert_eq!(grouped.len(), 6);
        assert_eq!(grouped[0].id, "gemini-3.10-flash-high");
        assert_eq!(grouped[1].id, "gemini-3.9-flash-high");
        assert_eq!(grouped[2].id, "gemini-3.8-flash-high");
        assert_eq!(grouped[2].display_name, "Gemini 3.8 Flash (Antigravity)");
        assert_eq!(grouped[2].thinking_levels, vec!["low", "medium", "high"]);
        assert_eq!(raw.len(), 9); // Raw presets stay available for existing threads.
    }

    #[test]
    fn presets_keep_defaults_but_allow_effort_selection() {
        for (id, default) in [
            ("gemini-3.7-flash-high", "high"),
            ("gemini-3.7-flash-low", "low"),
            ("gemini-3.7-flash-medium", "medium"),
            ("claude-opus-4-6-thinking", "high"),
            ("claude-sonnet-4-6", "high"),
        ] {
            let model = model_for_id(id).unwrap();
            assert_eq!(model.thinking_levels, vec!["low", "medium", "high"]);
            assert_eq!(model.default_thinking_level, default);
        }
    }

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
