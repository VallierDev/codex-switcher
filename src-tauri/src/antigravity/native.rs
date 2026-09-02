use reqwest::Client;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

pub const GENERATE_URL: &str =
    "https://daily-cloudcode-pa.googleapis.com/v1internal:generateContent";
pub const STREAM_URL: &str =
    "https://daily-cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse";
pub const GOOGLE_API_CLIENT: &str = "gl-node/22.21.1";

const FALLBACK_VERSION: &str = "2.9.1";
const VERSION_MANIFEST_URL: &str =
    "https://antigravity-hub-auto-updater-974169037036.us-central1.run.app/manifest/latest-arm64-mac.yml";
const VERSION_TTL: Duration = Duration::from_secs(6 * 60 * 60);

static VERSION_CACHE: LazyLock<tokio::sync::RwLock<Option<(String, Instant)>>> =
    LazyLock::new(|| tokio::sync::RwLock::new(None));

pub fn build_http_client() -> Result<Client, String> {
    Client::builder()
        .http1_only()
        .pool_max_idle_per_host(100)
        .pool_idle_timeout(Duration::from_secs(10 * 60))
        .build()
        .map_err(|error| format!("build Antigravity HTTP/1.1 client: {error}"))
}

pub async fn request_user_agent(client: &Client) -> String {
    let version = latest_version(client).await;
    format!("antigravity/hub/{version} darwin/arm64")
}

pub async fn onboard_user_agent(client: &Client) -> String {
    format!(
        "{} google-api-nodejs-client/10.3.0",
        request_user_agent(client).await
    )
}

async fn latest_version(client: &Client) -> String {
    {
        let cache = VERSION_CACHE.read().await;
        if let Some((version, expires_at)) = cache.as_ref() {
            if *expires_at > Instant::now() {
                return version.clone();
            }
        }
    }

    let fetched = fetch_latest_version(client)
        .await
        .unwrap_or_else(|| FALLBACK_VERSION.to_string());
    *VERSION_CACHE.write().await = Some((fetched.clone(), Instant::now() + VERSION_TTL));
    fetched
}

async fn fetch_latest_version(client: &Client) -> Option<String> {
    let response = client
        .get(VERSION_MANIFEST_URL)
        .header("user-agent", "electron-builder")
        .header("cache-control", "no-cache")
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body = response.text().await.ok()?;
    body.lines().find_map(|line| {
        let version = line.trim().strip_prefix("version:")?.trim();
        is_semver(version).then(|| version.to_string())
    })
}

fn is_semver(value: &str) -> bool {
    let parts: Vec<_> = value.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.chars().all(|ch| ch.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_antigravity_versions() {
        assert!(is_semver("2.9.1"));
        assert!(is_semver("12.3.40"));
        assert!(!is_semver("2.9"));
        assert!(!is_semver("2.9.1-beta"));
    }
}
