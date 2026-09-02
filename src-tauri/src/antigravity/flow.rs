use super::oauth::{self, AntigravityCredential, OAuthClientConfig};
use base64::{engine::general_purpose, Engine as _};
use rand::{rng, RngCore};
use std::sync::{Mutex, OnceLock};
use tauri::{AppHandle, Emitter};
use tauri_plugin_opener::OpenerExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::{Duration, Instant};
use url::Url;

const CALLBACK_PORT: u16 = 51_121;
const CALLBACK_PATH: &str = "/oauth-callback";

struct PendingLogin {
    redirect_uri: String,
}

static PENDING_LOGIN: OnceLock<Mutex<Option<PendingLogin>>> = OnceLock::new();
static CALLBACK_TASK: OnceLock<Mutex<Option<tokio::task::JoinHandle<()>>>> = OnceLock::new();

fn pending_login() -> &'static Mutex<Option<PendingLogin>> {
    PENDING_LOGIN.get_or_init(|| Mutex::new(None))
}

fn callback_task() -> &'static Mutex<Option<tokio::task::JoinHandle<()>>> {
    CALLBACK_TASK.get_or_init(|| Mutex::new(None))
}

fn generate_state() -> String {
    let mut bytes = [0u8; 32];
    rng().fill_bytes(&mut bytes);
    general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[tauri::command]
pub async fn start_antigravity_oauth_login(
    app_handle: AppHandle,
    open_browser: Option<bool>,
) -> Result<String, String> {
    if let Ok(mut slot) = callback_task().lock() {
        if let Some(task) = slot.take() {
            task.abort();
        }
    }
    tokio::time::sleep(Duration::from_millis(100)).await;

    let listener = TcpListener::bind(("127.0.0.1", CALLBACK_PORT))
        .await
        .map_err(|e| format!("无法绑定 Antigravity OAuth 回调端口 {CALLBACK_PORT}: {e}"))?;
    let redirect_uri = format!("http://localhost:{CALLBACK_PORT}{CALLBACK_PATH}");
    let state = generate_state();
    let config = OAuthClientConfig::from_environment();
    let auth_url = oauth::build_authorize_url(&config, &redirect_uri, &state)?;

    *pending_login().lock().map_err(|_| "登录流程状态锁异常")? =
        Some(PendingLogin { redirect_uri });

    let app = app_handle.clone();
    let handle = tokio::spawn(async move {
        handle_callback(listener, app, state).await;
    });
    if let Ok(mut slot) = callback_task().lock() {
        *slot = Some(handle);
    }

    if open_browser.unwrap_or(true) {
        let _ = app_handle.opener().open_url(&auth_url, None::<String>);
    }
    Ok(auth_url)
}

async fn handle_callback(listener: TcpListener, app_handle: AppHandle, expected_state: String) {
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return;
        }
        let accepted = match tokio::time::timeout(remaining, listener.accept()).await {
            Ok(Ok(value)) => value,
            Ok(Err(_)) => continue,
            Err(_) => return,
        };
        let (mut socket, _) = accepted;
        let mut buffer = [0u8; 8192];
        let Ok(size) = socket.read(&mut buffer).await else {
            continue;
        };
        let request = String::from_utf8_lossy(&buffer[..size]);
        if let Some(code) = extract_code(&request, &expected_state) {
            let response = "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<h1>Google 授权成功</h1><p>可以关闭此窗口并返回 Codex Switcher。</p>";
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = app_handle.emit("antigravity-oauth-callback-received", code);
            return;
        }
        let _ = socket
            .write_all(b"HTTP/1.1 400 Bad Request\r\n\r\nInvalid OAuth callback")
            .await;
    }
}

fn extract_code(request: &str, expected_state: &str) -> Option<String> {
    let request_target = request.lines().next()?.split_whitespace().nth(1)?;
    let url = Url::parse(&format!("http://localhost{request_target}")).ok()?;
    if url.path() != CALLBACK_PATH {
        return None;
    }
    let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    (query.get("state")?.as_str() == expected_state).then(|| query.get("code").cloned())?
}

pub async fn complete_oauth_login(code: String) -> Result<AntigravityCredential, String> {
    let redirect_uri = take_pending_redirect_uri()?;
    let config = OAuthClientConfig::from_environment();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(45))
        .build()
        .map_err(|e| e.to_string())?;
    oauth::complete_credential(&client, &config, &code, &redirect_uri).await
}

pub fn take_pending_redirect_uri() -> Result<String, String> {
    pending_login()
        .lock()
        .map_err(|_| "登录流程状态锁异常")?
        .take()
        .map(|pending| pending.redirect_uri)
        .ok_or_else(|| "Antigravity 登录流程已过期或未启动".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_requires_expected_path_and_state() {
        let request = "GET /oauth-callback?code=abc&state=s1 HTTP/1.1\r\nHost: localhost\r\n\r\n";
        assert_eq!(extract_code(request, "s1").as_deref(), Some("abc"));
        assert_eq!(extract_code(request, "s2"), None);
        assert_eq!(
            extract_code("GET /other?code=abc&state=s1 HTTP/1.1\r\n\r\n", "s1"),
            None
        );
    }
}
