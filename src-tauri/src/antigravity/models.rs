use serde::Serialize;

/// Antigravity model metadata required by Codex's picker and request router.
///
/// IDs intentionally match the upstream route names. Display names are UI-only;
/// the router must always use `id` for execution.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct AntigravityModel {
    pub id: &'static str,
    pub display_name: &'static str,
    pub description: &'static str,
    pub context_length: u64,
    pub max_completion_tokens: u64,
    pub input_modalities: &'static [&'static str],
    pub output_modalities: &'static [&'static str],
    pub thinking_levels: &'static [&'static str],
}

// Codex 0.149 的模型目录枚举当前只接受 text/image/audio；先只公开
// 已完成端到端适配的 text + image，避免一个未知 modality 使整份目录失效。
const TEXT_IMAGE: &[&str] = &["text", "image"];
const TEXT_OUTPUT: &[&str] = &["text"];
const FOUR_THINKING_LEVELS: &[&str] = &["minimal", "low", "medium", "high"];

const MODELS: &[AntigravityModel] = &[
    AntigravityModel {
        id: "gemini-3.7-flash-high",
        display_name: "Gemini 3.7 Flash (Antigravity)",
        description: "Gemini 3.7 Flash through Google Antigravity OAuth",
        context_length: 1_048_576,
        max_completion_tokens: 65_536,
        input_modalities: TEXT_IMAGE,
        output_modalities: TEXT_OUTPUT,
        thinking_levels: FOUR_THINKING_LEVELS,
    },
    AntigravityModel {
        id: "gemini-3.6-flash-high",
        display_name: "Gemini 3.6 Flash (Antigravity)",
        description: "Gemini 3.6 Flash through Google Antigravity OAuth",
        context_length: 1_048_576,
        max_completion_tokens: 65_536,
        input_modalities: TEXT_IMAGE,
        output_modalities: TEXT_OUTPUT,
        thinking_levels: FOUR_THINKING_LEVELS,
    },
    AntigravityModel {
        id: "gemini-pro-agent",
        display_name: "Gemini 3.1 Pro High (Antigravity)",
        description: "Gemini 3.1 Pro High through Google Antigravity OAuth",
        context_length: 1_048_576,
        max_completion_tokens: 65_536,
        input_modalities: TEXT_IMAGE,
        output_modalities: TEXT_OUTPUT,
        thinking_levels: &["high"],
    },
    AntigravityModel {
        id: "gemini-3.1-pro-low",
        display_name: "Gemini 3.1 Pro Low (Antigravity)",
        description: "Gemini 3.1 Pro Low through Google Antigravity OAuth",
        context_length: 1_048_576,
        max_completion_tokens: 65_536,
        input_modalities: TEXT_IMAGE,
        output_modalities: TEXT_OUTPUT,
        thinking_levels: &["low"],
    },
];

pub fn models() -> &'static [AntigravityModel] {
    MODELS
}

pub fn find_model(id: &str) -> Option<&'static AntigravityModel> {
    MODELS.iter().find(|model| model.id == id)
}

pub fn is_antigravity_model(id: &str) -> bool {
    find_model(id).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_only_declared_antigravity_models() {
        assert!(is_antigravity_model("gemini-3.7-flash-high"));
        assert!(!is_antigravity_model("gemini-3.7-flash"));
        assert!(!is_antigravity_model("gpt-5.6-sol"));
    }

    #[test]
    fn model_ids_are_unique() {
        let mut ids = std::collections::BTreeSet::new();
        for model in models() {
            assert!(ids.insert(model.id), "duplicate model id: {}", model.id);
        }
    }
}
