use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use url::Url;

const AUTH_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
const USERINFO_ENDPOINT: &str = "https://www.googleapis.com/oauth2/v2/userinfo?alt=json";
const CLOUD_CODE_ENDPOINT: &str = "https://cloudcode-pa.googleapis.com";
const DAILY_CLOUD_CODE_ENDPOINT: &str = "https://daily-cloudcode-pa.googleapis.com";
const API_VERSION: &str = "v1internal";

const SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
    "https://www.googleapis.com/auth/cclog",
    "https://www.googleapis.com/auth/experimentsandconfigs",
];

/// OAuth client identity. Environment overrides allow deployment with a
/// Codex-Switcher-owned Google client while retaining explicit CPA-compatible
/// behavior for installations that opt into it.
#[derive(Debug, Clone)]
pub struct OAuthClientConfig {
    pub client_id: String,
    pub client_secret: String,
}

impl OAuthClientConfig {
    pub fn from_environment() -> Self {
        Self {
            client_id: std::env::var("CODEX_SWITCHER_ANTIGRAVITY_CLIENT_ID")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .or_else(|| {
                    option_env!("CODEX_SWITCHER_ANTIGRAVITY_CLIENT_ID").map(ToOwned::to_owned)
                })
                .unwrap_or_default(),
            client_secret: std::env::var("CODEX_SWITCHER_ANTIGRAVITY_CLIENT_SECRET")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .or_else(|| {
                    option_env!("CODEX_SWITCHER_ANTIGRAVITY_CLIENT_SECRET").map(ToOwned::to_owned)
                })
                .unwrap_or_default(),
        }
    }

    fn validate(&self) -> Result<(), String> {
        if self.client_id.trim().is_empty() || self.client_secret.trim().is_empty() {
            return Err(
                "Google OAuth client is not configured; set CODEX_SWITCHER_ANTIGRAVITY_CLIENT_ID and CODEX_SWITCHER_ANTIGRAVITY_CLIENT_SECRET"
                    .to_string(),
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AntigravityCredential {
    pub email: String,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: DateTime<Utc>,
    pub project_id: String,
}

impl AntigravityCredential {
    pub fn to_auth_json(&self) -> Value {
        json!({
            "provider": "antigravity",
            "email": self.email,
            "project_id": self.project_id,
            "tokens": {
                "access_token": self.access_token,
                "refresh_token": self.refresh_token,
                "expires_at": self.expires_at.to_rfc3339(),
            },
            "last_refresh": Utc::now().to_rfc3339(),
        })
    }
}

pub fn build_authorize_url(
    config: &OAuthClientConfig,
    redirect_uri: &str,
    state: &str,
) -> Result<String, String> {
    config.validate()?;
    let mut url = Url::parse(AUTH_ENDPOINT).map_err(|e| e.to_string())?;
    url.query_pairs_mut()
        .append_pair("access_type", "offline")
        .append_pair("client_id", &config.client_id)
        .append_pair("prompt", "consent")
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", &SCOPES.join(" "))
        .append_pair("state", state);
    Ok(url.into())
}

pub async fn exchange_code(
    client: &reqwest::Client,
    config: &OAuthClientConfig,
    code: &str,
    redirect_uri: &str,
) -> Result<TokenResponse, String> {
    post_token_form(
        client,
        config,
        &[
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("grant_type", "authorization_code"),
        ],
    )
    .await
}

pub async fn refresh_access_token(
    client: &reqwest::Client,
    config: &OAuthClientConfig,
    refresh_token: &str,
) -> Result<TokenResponse, String> {
    post_token_form(
        client,
        config,
        &[
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
        ],
    )
    .await
}

async fn post_token_form(
    client: &reqwest::Client,
    config: &OAuthClientConfig,
    fields: &[(&str, &str)],
) -> Result<TokenResponse, String> {
    config.validate()?;
    let mut form = vec![
        ("client_id", config.client_id.as_str()),
        ("client_secret", config.client_secret.as_str()),
    ];
    form.extend_from_slice(fields);
    let response = client
        .post(TOKEN_ENDPOINT)
        .form(&form)
        .send()
        .await
        .map_err(|e| format!("Google OAuth token request failed: {e}"))?;
    let status = response.status();
    let body = response.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!(
            "Google OAuth token request failed with HTTP {status}"
        ));
    }
    serde_json::from_str(&body).map_err(|e| format!("invalid Google OAuth response: {e}"))
}

pub async fn fetch_email(client: &reqwest::Client, access_token: &str) -> Result<String, String> {
    let response = client
        .get(USERINFO_ENDPOINT)
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|e| format!("Google userinfo request failed: {e}"))?;
    let status = response.status();
    let body: Value = response.json().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("Google userinfo request failed with HTTP {status}"));
    }
    body.get("email")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|email| !email.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| "Google userinfo response did not contain an email".to_string())
}

pub async fn fetch_project_id(
    client: &reqwest::Client,
    access_token: &str,
) -> Result<String, String> {
    let endpoint = format!("{CLOUD_CODE_ENDPOINT}/{API_VERSION}:loadCodeAssist");
    let response = client
        .post(endpoint)
        .bearer_auth(access_token)
        .header("user-agent", "antigravity/2.11.0")
        .json(&json!({"metadata": {"ideType": "ANTIGRAVITY"}}))
        .send()
        .await
        .map_err(|e| format!("Antigravity project discovery failed: {e}"))?;
    let status = response.status();
    let body: Value = response.json().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!(
            "Antigravity project discovery failed with HTTP {status}"
        ));
    }
    if let Some(project_id) = extract_project_id(&body) {
        return Ok(project_id);
    }
    let tier_id = body
        .get("allowedTiers")
        .and_then(Value::as_array)
        .and_then(|tiers| {
            tiers
                .iter()
                .find(|tier| tier.get("isDefault").and_then(Value::as_bool) == Some(true))
        })
        .and_then(|tier| tier.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("free-tier");
    onboard_user(client, access_token, tier_id).await
}

async fn onboard_user(
    client: &reqwest::Client,
    access_token: &str,
    tier_id: &str,
) -> Result<String, String> {
    let endpoint = format!("{DAILY_CLOUD_CODE_ENDPOINT}/{API_VERSION}:onboardUser");
    for _ in 0..5 {
        let response = client
            .post(&endpoint)
            .bearer_auth(access_token)
            .header("user-agent", "antigravity/2.11.0")
            .header("x-goog-api-client", "gl-node/22.0.0 antigravity/2.11.0")
            .json(&json!({
                "tier_id": tier_id,
                "metadata": {
                    "ide_type": "ANTIGRAVITY",
                    "ide_version": "2.11.0",
                    "ide_name": "antigravity"
                }
            }))
            .send()
            .await
            .map_err(|e| format!("Antigravity onboarding failed: {e}"))?;
        let status = response.status();
        let body: Value = response.json().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!("Antigravity onboarding failed with HTTP {status}"));
        }
        if body.get("done").and_then(Value::as_bool) == Some(true) {
            if let Some(project_id) = body.get("response").and_then(extract_project_id) {
                return Ok(project_id);
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    Err("Antigravity onboarding did not return a project_id".to_string())
}

fn extract_project_id(value: &Value) -> Option<String> {
    for key in ["cloudaicompanionProject", "projectId", "project"] {
        match value.get(key) {
            Some(Value::String(id)) if !id.trim().is_empty() => return Some(id.trim().to_string()),
            Some(Value::Object(object)) => {
                if let Some(id) = object
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
                {
                    return Some(id.to_string());
                }
            }
            _ => {}
        }
    }
    None
}

pub async fn complete_credential(
    client: &reqwest::Client,
    config: &OAuthClientConfig,
    code: &str,
    redirect_uri: &str,
) -> Result<AntigravityCredential, String> {
    let tokens = exchange_code(client, config, code, redirect_uri).await?;
    let refresh_token = tokens.refresh_token.clone().ok_or_else(|| {
        "Google OAuth did not return a refresh_token; revoke consent and retry".to_string()
    })?;
    let email = fetch_email(client, &tokens.access_token).await?;
    let project_id = fetch_project_id(client, &tokens.access_token).await?;
    Ok(AntigravityCredential {
        email,
        access_token: tokens.access_token,
        refresh_token,
        expires_at: Utc::now() + Duration::seconds(tokens.expires_in),
        project_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_url_contains_offline_consent_and_scopes() {
        let config = OAuthClientConfig {
            client_id: "client".into(),
            client_secret: "secret".into(),
        };
        let url = build_authorize_url(&config, "http://localhost:51121/oauth-callback", "state-1")
            .unwrap();
        let parsed = Url::parse(&url).unwrap();
        let query: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(
            query.get("access_type").map(String::as_str),
            Some("offline")
        );
        assert_eq!(query.get("prompt").map(String::as_str), Some("consent"));
        assert!(query.get("scope").unwrap().contains("cloud-platform"));
    }

    #[test]
    fn extracts_project_id_from_supported_shapes() {
        assert_eq!(
            extract_project_id(&json!({"projectId":"p1"})).as_deref(),
            Some("p1")
        );
        assert_eq!(
            extract_project_id(&json!({"cloudaicompanionProject":{"id":"p2"}})).as_deref(),
            Some("p2")
        );
    }
}
