use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

const FETCH_MODELS_URL: &str =
    "https://cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelQuota {
    pub remaining_fraction: f64,
    pub reset_time: Option<String>,
    pub updated_at: String,
}

impl ModelQuota {
    pub fn is_available(&self, now: DateTime<Utc>) -> bool {
        if self.remaining_fraction > 0.000_001 {
            return true;
        }
        self.reset_time
            .as_deref()
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|reset| reset.with_timezone(&Utc) <= now)
            .unwrap_or(false)
    }
}

pub async fn fetch_model_quotas(
    client: &reqwest::Client,
    access_token: &str,
    project_id: &str,
) -> Result<HashMap<String, ModelQuota>, String> {
    let response = client
        .post(FETCH_MODELS_URL)
        .bearer_auth(access_token)
        .header("content-type", "application/json")
        .header("user-agent", "antigravity/2.11.0")
        .json(&serde_json::json!({"project": project_id}))
        .send()
        .await
        .map_err(|error| format!("Antigravity quota request failed: {error}"))?;
    let status = response.status();
    let body: Value = response.json().await.map_err(|error| error.to_string())?;
    if !status.is_success() {
        return Err(format!(
            "Antigravity quota request failed with HTTP {status}"
        ));
    }
    let now = Utc::now().to_rfc3339();
    let mut quotas = HashMap::new();
    if let Some(models) = body.get("models").and_then(Value::as_object) {
        for (model_id, model) in models {
            let Some(quota) = model.get("quotaInfo") else {
                continue;
            };
            let Some(remaining_fraction) = quota.get("remainingFraction").and_then(Value::as_f64)
            else {
                continue;
            };
            quotas.insert(
                model_id.clone(),
                ModelQuota {
                    remaining_fraction,
                    reset_time: quota
                        .get("resetTime")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                    updated_at: now.clone(),
                },
            );
        }
    }
    Ok(quotas)
}

pub fn read_model_quotas(auth_json: &Value) -> HashMap<String, ModelQuota> {
    auth_json
        .get("model_quotas")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default()
}

/// Candidate score for one `(account, model)` pair.
/// Missing cache is optimistically usable until the first live quota refresh;
/// an explicitly exhausted, not-yet-reset model is excluded without disabling
/// the same account's other models.
pub fn model_candidate_score(auth_json: &Value, model_id: &str, now: DateTime<Utc>) -> Option<f64> {
    match read_model_quotas(auth_json).get(model_id) {
        Some(quota) if quota.is_available(now) => Some(quota.remaining_fraction.max(0.0)),
        Some(_) => None,
        None => Some(1.0),
    }
}

pub fn write_model_quotas(auth_json: &mut Value, quotas: &HashMap<String, ModelQuota>) {
    if let (Some(object), Ok(value)) = (auth_json.as_object_mut(), serde_json::to_value(quotas)) {
        object.insert("model_quotas".to_string(), value);
    }
}

pub fn mark_model_exhausted(auth_json: &mut Value, model_id: &str) {
    let mut quotas = read_model_quotas(auth_json);
    let entry = quotas.entry(model_id.to_string()).or_insert(ModelQuota {
        remaining_fraction: 0.0,
        reset_time: None,
        updated_at: Utc::now().to_rfc3339(),
    });
    entry.remaining_fraction = 0.0;
    entry.updated_at = Utc::now().to_rfc3339();
    write_model_quotas(auth_json, &quotas);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exhausted_model_does_not_disable_other_models() {
        let mut auth = serde_json::json!({});
        mark_model_exhausted(&mut auth, "gemini-3.7-flash-high");
        let quotas = read_model_quotas(&auth);
        assert!(!quotas["gemini-3.7-flash-high"].is_available(Utc::now()));
        assert!(!quotas.contains_key("gemini-pro-agent"));
        assert_eq!(
            model_candidate_score(&auth, "gemini-3.7-flash-high", Utc::now()),
            None
        );
        assert_eq!(
            model_candidate_score(&auth, "gemini-pro-agent", Utc::now()),
            Some(1.0)
        );
    }

    #[test]
    fn model_scores_are_independent_between_accounts() {
        let now = Utc::now();
        let mut first = serde_json::json!({});
        let second = serde_json::json!({
            "model_quotas": {
                "gemini-3.7-flash-high": {
                    "remaining_fraction": 0.42,
                    "reset_time": null,
                    "updated_at": now.to_rfc3339()
                }
            }
        });
        mark_model_exhausted(&mut first, "gemini-3.7-flash-high");
        assert_eq!(
            model_candidate_score(&first, "gemini-3.7-flash-high", now),
            None
        );
        assert_eq!(
            model_candidate_score(&second, "gemini-3.7-flash-high", now),
            Some(0.42)
        );
    }
}
