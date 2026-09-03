use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QuotaWindow {
    pub remaining_fraction: f64,
    pub reset_time: Option<String>,
}

impl QuotaWindow {
    fn is_available(&self, now: DateTime<Utc>) -> bool {
        self.remaining_fraction > 0.000_001
            || self
                .reset_time
                .as_deref()
                .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                .is_some_and(|reset| reset.with_timezone(&Utc) <= now)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ModelQuota {
    pub remaining_fraction: f64,
    pub reset_time: Option<String>,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub five_hour: Option<QuotaWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly: Option<QuotaWindow>,
}

impl ModelQuota {
    pub fn is_available(&self, now: DateTime<Utc>) -> bool {
        if self
            .five_hour
            .as_ref()
            .is_some_and(|window| !window.is_available(now))
            || self
                .weekly
                .as_ref()
                .is_some_and(|window| !window.is_available(now))
        {
            return false;
        }
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
    let user_agent = super::native::request_user_agent(client).await;
    let response = client
        .post(super::native::FETCH_MODELS_URL)
        .bearer_auth(access_token)
        .header("content-type", "application/json")
        .header("user-agent", &user_agent)
        .json(&serde_json::json!({"project": project_id}))
        .timeout(std::time::Duration::from_secs(30))
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
    let mut quotas = parse_model_quotas(&body, Utc::now().to_rfc3339())?;
    // The model catalog reports only one effective quota. Window details come
    // from the native summary API; never infer 5H/7D from a reset countdown.
    let summary = client
        .post(super::native::QUOTA_SUMMARY_URL)
        .bearer_auth(access_token)
        .header("user-agent", &user_agent)
        .json(&serde_json::json!({"project":project_id}))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await;
    match summary {
        Ok(response) if response.status().is_success() => {
            if let Ok(body) = response.json::<Value>().await {
                attach_group_windows(&mut quotas, &body);
            }
        }
        Ok(response) => eprintln!(
            "[GoogleQuota] window summary unavailable: HTTP {}",
            response.status()
        ),
        Err(_) => eprintln!("[GoogleQuota] window summary unavailable; keeping model quota only"),
    }
    Ok(quotas)
}

fn attach_group_windows(quotas: &mut HashMap<String, ModelQuota>, summary: &Value) {
    let mut buckets = HashMap::new();
    for group in summary
        .get("groups")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        for bucket in group
            .get("buckets")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let (Some(id), Some(fraction)) = (
                bucket.get("bucketId").and_then(Value::as_str),
                bucket.get("remainingFraction").and_then(Value::as_f64),
            ) else {
                continue;
            };
            let window = bucket.get("window").and_then(Value::as_str);
            if !matches!(
                (id, window),
                ("gemini-5h" | "3p-5h", Some("5h"))
                    | ("gemini-weekly" | "3p-weekly", Some("weekly"))
            ) {
                continue;
            }
            buckets.insert(
                id,
                QuotaWindow {
                    remaining_fraction: fraction,
                    reset_time: bucket
                        .get("resetTime")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                },
            );
        }
    }
    for (model, quota) in quotas {
        let group = if model.starts_with("gemini-") && !model.contains("-image") {
            "gemini"
        } else if model.starts_with("claude-") || model.starts_with("gpt-oss-") {
            "3p"
        } else {
            continue;
        };
        quota.five_hour = buckets.get(format!("{group}-5h").as_str()).cloned();
        quota.weekly = buckets.get(format!("{group}-weekly").as_str()).cloned();
    }
}

fn parse_model_quotas(body: &Value, now: String) -> Result<HashMap<String, ModelQuota>, String> {
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
                    ..ModelQuota::default()
                },
            );
        }
    }
    if quotas.is_empty() {
        return Err("Google 未返回模型额度，请稍后重试；这不表示额度为 0".to_string());
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
        ..ModelQuota::default()
    });
    entry.remaining_fraction = 0.0;
    entry.updated_at = Utc::now().to_rfc3339();
    write_model_quotas(auth_json, &quotas);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_summary_keeps_five_hour_and_weekly_independent() {
        let mut quotas = parse_model_quotas(
            &serde_json::json!({"models":{
                "gemini-3.8-flash-high":{"quotaInfo":{"remainingFraction":1.0}},
                "claude-sonnet-4-6":{"quotaInfo":{"remainingFraction":1.0}},
                "gemini-3.1-flash-image":{"quotaInfo":{"remainingFraction":1.0}}
            }}),
            "now".into(),
        )
        .unwrap();
        attach_group_windows(
            &mut quotas,
            &serde_json::json!({"groups":[{"buckets":[
                {"bucketId":"gemini-5h","window":"5h","remainingFraction":1.0},
                {"bucketId":"gemini-weekly","window":"weekly","remainingFraction":0.74},
                {"bucketId":"3p-5h","window":"5h","remainingFraction":1.0},
                {"bucketId":"3p-weekly","window":"weekly","remainingFraction":0.0,"resetTime":"2099-01-01T00:00:00Z"}
            ]}]}),
        );
        let gemini = &quotas["gemini-3.8-flash-high"];
        assert_eq!(gemini.five_hour.as_ref().unwrap().remaining_fraction, 1.0);
        assert_eq!(gemini.weekly.as_ref().unwrap().remaining_fraction, 0.74);
        assert!(gemini.is_available(Utc::now()));
        assert!(!quotas["claude-sonnet-4-6"].is_available(Utc::now()));
        assert!(quotas["gemini-3.1-flash-image"].five_hour.is_none());
        let mut auth = serde_json::json!({});
        write_model_quotas(&mut auth, &quotas);
        assert_eq!(read_model_quotas(&auth), quotas);
    }

    #[test]
    fn reset_countdown_does_not_invent_a_window_type() {
        let quotas = parse_model_quotas(&serde_json::json!({"models":{
            "gemini-3.8-flash-high":{"quotaInfo":{"remainingFraction":0.5,"resetTime":"2099-01-01T00:00:00Z"}}
        }}), "now".into()).unwrap();
        assert!(quotas["gemini-3.8-flash-high"].five_hour.is_none());
        assert!(quotas["gemini-3.8-flash-high"].weekly.is_none());
    }

    #[test]
    fn weekly_only_accounts_do_not_get_a_fabricated_five_hour_quota() {
        let mut quotas = parse_model_quotas(
            &serde_json::json!({"models":{
                "gemini-3.8-flash-high":{"quotaInfo":{"remainingFraction":0.6}}
            }}),
            "now".into(),
        )
        .unwrap();
        attach_group_windows(
            &mut quotas,
            &serde_json::json!({"groups":[{"buckets":[
                {"bucketId":"gemini-weekly","window":"weekly","remainingFraction":0.6},
                {"bucketId":"gemini-5h","window":"5h","remainingFraction":null}
            ]}]}),
        );
        assert!(quotas["gemini-3.8-flash-high"].five_hour.is_none());
        assert_eq!(
            quotas["gemini-3.8-flash-high"]
                .weekly
                .as_ref()
                .unwrap()
                .remaining_fraction,
            0.6
        );
    }

    #[test]
    fn quota_refresh_distinguishes_missing_data_from_zero_quota() {
        assert!(parse_model_quotas(&serde_json::json!({"models": {}}), "now".into()).is_err());
        let quotas = parse_model_quotas(
            &serde_json::json!({
                "models": {
                    "empty-model": {"quotaInfo": {"remainingFraction": 0.0, "resetTime": "later"}},
                    "full-model": {"quotaInfo": {"remainingFraction": 1.0}}
                }
            }),
            "now".into(),
        )
        .unwrap();
        assert_eq!(quotas["empty-model"].remaining_fraction, 0.0);
        assert_eq!(quotas["empty-model"].reset_time.as_deref(), Some("later"));
        assert_eq!(quotas["full-model"].updated_at, "now");
    }

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
