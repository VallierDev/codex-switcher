use crate::oauth;
use base64::{engine::general_purpose, Engine as _};
use rand::{rng, RngCore};
use std::sync::Mutex;
use std::sync::OnceLock;
use tauri::{AppHandle, Emitter};
use tauri_plugin_opener::OpenerExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::{Duration, Instant};
use url::Url;

/// 使用 OnceLock 代替 lazy_static 存储 OAuth 流程中的临时数据
static PENDING_LOGIN: OnceLock<Mutex<Option<PendingLogin>>> = OnceLock::new();
static CALLBACK_TASK: OnceLock<Mutex<Option<tokio::task::JoinHandle<()>>>> = OnceLock::new();

fn get_pending_login() -> &'static Mutex<Option<PendingLogin>> {
    PENDING_LOGIN.get_or_init(|| Mutex::new(None))
}

fn get_callback_task() -> &'static Mutex<Option<tokio::task::JoinHandle<()>>> {
    CALLBACK_TASK.get_or_init(|| Mutex::new(None))
}

struct PendingLogin {
    pkce: oauth::PkceCodes,
    port: u16,
    state: String,
}

/// 生成与官方一致的 state (Base64 编码的32字节随机数)
fn generate_state() -> String {
    let mut bytes = [0u8; 32];
    rng().fill_bytes(&mut bytes);
    general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// 官方固定端口
const DEFAULT_PORT: u16 = 1455;

/// 准备 OAuth 流程并返回授权 URL
///
/// `open_browser=Some(false)` 时不调用系统默认浏览器，前端可以把返回的 URL 拷贝到剪贴板，
/// 用户自行粘贴到目标浏览器里完成授权。回调监听仍然启动，所以授权完成后流程与"直接点击登录"一致。
#[tauri::command]
pub async fn start_oauth_login(
    app_handle: AppHandle,
    open_browser: Option<bool>,
) -> Result<String, String> {
    // 1. 如果有旧回调任务，先中止，避免同一进程重复占用固定端口
    if let Ok(mut task_slot) = get_callback_task().lock() {
        if let Some(task) = task_slot.take() {
            task.abort();
        }
    }

    // 等待端口从旧任务释放
    tokio::time::sleep(Duration::from_millis(100)).await;

    let listener = TcpListener::bind(format!("127.0.0.1:{}", DEFAULT_PORT))
        .await
        .map_err(|e| {
            format!(
                "无法绑定本地端口 {}: {}。请关闭占用该端口的进程后重试。",
                DEFAULT_PORT, e
            )
        })?;
    let port = DEFAULT_PORT;

    // 2. 生成 PKCE 和 State (与官方一致)
    let pkce = oauth::generate_pkce();
    let state = generate_state();
    let redirect_uri = format!("http://localhost:{}/auth/callback", port);

    // 3. 构造授权 URL (与官方完全一致: 手动拼接, 不对特殊字符编码)
    let qs = format!(
        "response_type=code&client_id={}&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&id_token_add_organizations=true&codex_cli_simplified_flow=true&state={}&originator=codex_cli_rs",
        oauth::CLIENT_ID,
        redirect_uri,
        "openid profile email offline_access",
        pkce.code_challenge,
        state
    );

    let auth_url = format!("{}?{}", oauth::AUTH_URL, qs);

    // 4. 保存状态，开启监听任务
    {
        let mut pending = get_pending_login()
            .lock()
            .map_err(|_| "登录流程状态锁异常")?;
        *pending = Some(PendingLogin {
            pkce: pkce.clone(),
            port,
            state: state.clone(),
        });
    }

    // 5. 启动异步监听
    let app_handle_clone = app_handle.clone();
    let handle = tokio::spawn(async move {
        handle_callback(listener, app_handle_clone, state).await;
    });
    if let Ok(mut task_slot) = get_callback_task().lock() {
        *task_slot = Some(handle);
    }

    // 6. 打开浏览器（除非前端显式要求"只拿 URL 不开浏览器"）
    if open_browser.unwrap_or(true) {
        let _ = app_handle.opener().open_url(&auth_url, None::<String>);
    }

    Ok(auth_url)
}

/// 监听回调
async fn handle_callback(listener: TcpListener, app_handle: AppHandle, expected_state: String) {
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            eprintln!("[OAuth] 回调监听超时，未收到有效授权码");
            return;
        }

        let accepted = match tokio::time::timeout(remaining, listener.accept()).await {
            Ok(result) => result,
            Err(_) => {
                eprintln!("[OAuth] 回调监听超时，未收到有效授权码");
                return;
            }
        };

        let (mut socket, _) = match accepted {
            Ok(sock) => sock,
            Err(e) => {
                eprintln!("[OAuth] 监听回调连接失败: {}", e);
                continue;
            }
        };

        let mut buffer = [0; 4096];
        let n = match socket.read(&mut buffer).await {
            Ok(n) => n,
            Err(e) => {
                eprintln!("[OAuth] 读取回调请求失败: {}", e);
                continue;
            }
        };
        if n == 0 {
            continue;
        }
        let request = String::from_utf8_lossy(&buffer[..n]);

        if let Some(code) = extract_oauth_code_from_request(&request, &expected_state) {
            // 发送成功 HTML 并通知前端
            let response = "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\r\n\
                <html><body><h1>授权成功</h1><p>已成功连接 OpenAI，你可以关闭此窗口并回到应用。</p>\
                <script>setTimeout(() => window.close(), 3000)</script></body></html>";
            let _ = socket.write_all(response.as_bytes()).await;

            if let Err(e) = app_handle.emit("oauth-callback-received", code) {
                eprintln!("发送 oauth-callback-received 事件失败: {}", e);
            }
            return;
        }

        let response = "HTTP/1.1 400 Bad Request\r\n\r\n授权失败: State 校验不通过或参数缺失";
        let _ = socket.write_all(response.as_bytes()).await;
    }
}

fn extract_oauth_code_from_request(request: &str, expected_state: &str) -> Option<String> {
    let first_line = request.lines().next().unwrap_or("");
    let parts: Vec<&str> = first_line.split_whitespace().collect();
    if parts.len() <= 1 {
        return None;
    }

    let callback_url = format!("http://localhost{}", parts[1]);
    let url = Url::parse(&callback_url).ok()?;
    let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();

    let code = params.get("code")?;
    let state = params.get("state")?;
    if state != expected_state {
        return None;
    }

    Some(code.to_string())
}

/// 手动粘贴回调链接或裸 code 提交。适用于浏览器没能跳回本机监听端口（走了代理、被防火墙拦截等）。
/// 接受的 input 形式：
/// - 完整 URL: `http://localhost:1455/auth/callback?code=XXX&state=YYY`
/// - query 串:  `?code=XXX&state=YYY` 或 `code=XXX&state=YYY`
/// - 裸 code（不推荐，不做 state 校验）
#[tauri::command]
pub async fn submit_oauth_callback(app_handle: AppHandle, input: String) -> Result<(), String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("回调链接不能为空".to_string());
    }

    // 尝试按 URL 解析；失败则按 query 串处理；都失败就当作裸 code
    let (code_opt, state_opt) = parse_callback_input(trimmed);

    let Some(code) = code_opt else {
        return Err("未能从输入中解析出 code 参数".to_string());
    };

    // 有 state 就校验；没 state 的裸 code 也接受（用户自己承担风险）
    if let Some(ref provided_state) = state_opt {
        let expected = {
            let guard = get_pending_login()
                .lock()
                .map_err(|_| "登录流程状态锁异常")?;
            guard.as_ref().map(|p| p.state.clone())
        };
        match expected {
            Some(expected) if expected != *provided_state => {
                return Err("state 校验不通过：这个回调链接不属于本次登录流程".to_string());
            }
            None => {
                return Err("登录流程已过期或未启动，请先点击『立即登录 OpenAI』".to_string());
            }
            _ => {}
        }
    }

    // 停掉后端 HTTP 监听，避免它再接收一个回调
    if let Ok(mut slot) = get_callback_task().lock() {
        if let Some(task) = slot.take() {
            task.abort();
        }
    }

    // 走跟 HTTP 监听完全相同的路径：把 code 丢到前端
    app_handle
        .emit("oauth-callback-received", code)
        .map_err(|e| format!("派发 oauth-callback-received 失败: {}", e))?;
    Ok(())
}

fn parse_callback_input(input: &str) -> (Option<String>, Option<String>) {
    // 1) 完整 URL
    if let Ok(url) = Url::parse(input) {
        let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        if let Some(code) = params.get("code") {
            return (Some(code.clone()), params.get("state").cloned());
        }
    }
    // 2) 以 ? 开头或形如 code=xxx&state=yyy 的 query 串
    let stripped = input.trim_start_matches('?');
    if stripped.contains('=') {
        let fake = format!("http://x/?{}", stripped);
        if let Ok(url) = Url::parse(&fake) {
            let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
            if let Some(code) = params.get("code") {
                return (Some(code.clone()), params.get("state").cloned());
            }
        }
    }
    // 3) 裸 code：只要不含空白就认作 code
    if !input.chars().any(char::is_whitespace) {
        return (Some(input.to_string()), None);
    }
    (None, None)
}

/// macOS 端剪贴板写入：webview 的 `navigator.clipboard.writeText` 在跨过 await
/// 后会丢失 user-gesture，触发 NotAllowedError；改走 pbcopy 通过 Tauri IPC 写入，
/// 不依赖 user gesture，也避开 webview 权限提示。
#[tauri::command]
pub async fn copy_to_clipboard(text: String) -> Result<(), String> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new("pbcopy")
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| format!("无法启动 pbcopy: {}", e))?;

    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| "pbcopy stdin 不可写".to_string())?;
        stdin
            .write_all(text.as_bytes())
            .map_err(|e| format!("写入 pbcopy 失败: {}", e))?;
    }

    let status = child
        .wait()
        .map_err(|e| format!("等待 pbcopy 退出失败: {}", e))?;
    if !status.success() {
        return Err(format!("pbcopy 返回非零: {:?}", status.code()));
    }
    Ok(())
}

/// 最后一步：使用捕获到的 Code 交换 Token (由前端触发)
#[tauri::command]
pub async fn complete_oauth_login(code: String) -> Result<oauth::TokenResponse, String> {
    // 提取所需数据并立即释放锁，避免跨 await 持有 MutexGuard
    let (code_verifier, port) = {
        let mut pending_lock = get_pending_login().lock().map_err(|_| "锁被污染")?;
        let pending = pending_lock.take().ok_or("登录流程已过期或未启动")?;
        (pending.pkce.code_verifier, pending.port)
    };

    let redirect_uri = format!("http://localhost:{}/auth/callback", port);

    oauth::exchange_code(&code, &redirect_uri, &code_verifier).await
}

#[cfg(test)]
mod tests {
    use super::extract_oauth_code_from_request;

    #[test]
    fn extract_code_success_when_state_matches() {
        let req = "GET /auth/callback?code=abc123&state=s1 HTTP/1.1\r\nHost: localhost\r\n\r\n";
        assert_eq!(
            extract_oauth_code_from_request(req, "s1"),
            Some("abc123".to_string())
        );
    }

    #[test]
    fn extract_code_returns_none_when_state_mismatch() {
        let req = "GET /auth/callback?code=abc123&state=s2 HTTP/1.1\r\nHost: localhost\r\n\r\n";
        assert_eq!(extract_oauth_code_from_request(req, "s1"), None);
    }

    #[test]
    fn extract_code_returns_none_when_invalid_request_line() {
        let req = "INVALID\r\nHost: localhost\r\n\r\n";
        assert_eq!(extract_oauth_code_from_request(req, "s1"), None);
    }
}
