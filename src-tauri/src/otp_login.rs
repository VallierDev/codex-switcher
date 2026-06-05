//! 邮箱 OTP 自动登录主流程（纯协议）。
//!
//! 流程（与 HAR 对齐）：
//! 1. GET https://auth.openai.com/oauth/authorize?... → 302 链 → 落到 /log-in，
//!    cookie jar 拿到 `oai-did`/`__cf_bm`/`__Secure-next-auth-state` 等
//! 2. POST sentinel.openai.com/backend-api/sentinel/req → 拿 server token
//! 3. POST /api/accounts/authorize/continue {"username":{"kind":"email","value":...}}
//!    + header openai-sentinel-token
//! 4. POST /api/accounts/passwordless/send-otp + sentinel header → 触发邮件
//! 5. 轮询临时邮箱 → 6 位 OTP
//! 6. POST /api/accounts/email-otp/validate {"code":"..."} + sentinel header → 拿 continue_url + workspace_id
//! 7. 如响应携带 workspace_id 且需要选择：POST /api/accounts/workspace/select
//! 8. 跟 continue_url 重定向链 → localhost:1455/auth/callback?code=... 拿 code
//! 9. 复用 oauth::exchange_code 拿 access_token / refresh_token / id_token

use crate::mailbox::{MailboxProvider, UsmailMyId};
use crate::oauth;
use crate::sentinel::{fetch_sentinel_token, make_sentinel_header};

use reqwest::redirect::Policy;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36";
const REDIRECT_HOST: &str = "localhost";
const REDIRECT_PORT: u16 = 1455;
const FLOW: &str = "authorize_continue";

#[derive(Debug, Clone)]
pub struct LoginInput {
    pub email: String,
    pub otp_timeout_secs: u64,
}

/// run_login 的输出：token 与浏览器 OAuth 一致，调用方走 finalize 同一条路。
pub struct LoginOutput {
    pub email: String,
    pub token: oauth::TokenResponse,
}

/// 入口：跑完整 OTP 登录流程，返回与浏览器 OAuth 一致的 TokenResponse。
/// `mailbox` 决定从哪取 OTP（usmail.my.id / sorryios.net 等）。传 None 时默认 usmail。
pub async fn run_login(
    input: LoginInput,
    mailbox: Option<MailboxProvider>,
) -> Result<LoginOutput, String> {
    log_step("0. 初始化 PKCE + Cookie Jar");
    let pkce = oauth::generate_pkce();
    let state = generate_state();
    let redirect_uri = format!("http://{REDIRECT_HOST}:{REDIRECT_PORT}/auth/callback");

    let jar = Arc::new(reqwest::cookie::Jar::default());
    // 自定义 redirect 策略：遇到指向我们 redirect_uri 的 host 时停下，把 code 暴露在 final url
    let policy = Policy::custom(|attempt| {
        let url = attempt.url();
        let h = url.host_str().unwrap_or("");
        if h == REDIRECT_HOST || h == "localhost" {
            attempt.stop()
        } else if attempt.previous().len() > 15 {
            attempt.error("too many redirects")
        } else {
            attempt.follow()
        }
    });
    let client = reqwest::Client::builder()
        .cookie_provider(jar.clone())
        .redirect(policy)
        .user_agent(UA)
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("构建 HTTP client 失败: {e}"))?;

    log_step("1. GET /oauth/authorize 拿初始 cookies + 跟到 /log-in");
    let auth_url = build_auth_url(&pkce.code_challenge, &state, &redirect_uri);
    let resp = client
        .get(&auth_url)
        .header(
            "accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8",
        )
        .header("accept-language", "en-US,en;q=0.9")
        .header(
            "sec-ch-ua",
            r#""Chromium";v="142", "Google Chrome";v="142", "Not?A_Brand";v="99""#,
        )
        .header("sec-ch-ua-mobile", "?0")
        .header("sec-ch-ua-platform", "\"macOS\"")
        .header("sec-fetch-dest", "document")
        .header("sec-fetch-mode", "navigate")
        .header("sec-fetch-site", "none")
        .header("sec-fetch-user", "?1")
        .header("upgrade-insecure-requests", "1")
        .send()
        .await
        .map_err(|e| format!("/oauth/authorize 请求失败: {e}"))?;
    println!("  → 最终 URL: {}", resp.url());
    println!("  → 状态码: {}", resp.status());
    let _ = resp.text().await;

    let device_id = read_oai_did(&jar).unwrap_or_else(|| {
        let id = uuid::Uuid::new_v4().to_string();
        // 兜底：手动塞一个 oai-did 进 jar，免得后续请求带不到
        let cookie_str = format!("oai-did={id}; Domain=.openai.com; Path=/");
        let url: reqwest::Url = "https://auth.openai.com/".parse().unwrap();
        jar.add_cookie_str(&cookie_str, &url);
        let cookie_str2 = format!("oai-did={id}; Domain=.chatgpt.com; Path=/");
        let url2: reqwest::Url = "https://chatgpt.com/".parse().unwrap();
        jar.add_cookie_str(&cookie_str2, &url2);
        id
    });
    println!("  → device_id (oai-did): {}", device_id);

    log_step("2. POST sentinel/req 拿 server token");
    let server_token = fetch_sentinel_token(&client, UA, &device_id, FLOW).await?;
    println!(
        "  → server_token (前 24 字符): {}…",
        &server_token.chars().take(24).collect::<String>()
    );
    let sentinel_header = make_sentinel_header(&server_token, &device_id, FLOW);

    log_step("3. POST /api/accounts/authorize/continue（提交邮箱）");
    let continue_body = json!({
        "username": {"kind": "email", "value": input.email}
    })
    .to_string();
    let resp = client
        .post("https://auth.openai.com/api/accounts/authorize/continue")
        .header("origin", "https://auth.openai.com")
        .header("referer", "https://auth.openai.com/log-in")
        .header("accept", "application/json")
        .header("content-type", "application/json")
        .header("openai-sentinel-token", &sentinel_header)
        .body(continue_body)
        .send()
        .await
        .map_err(|e| format!("authorize/continue 失败: {e}"))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("authorize/continue 读响应失败: {e}"))?;
    println!("  → status: {status}");
    println!("  → body: {}", short(&body));
    if !status.is_success() {
        return Err(format!(
            "authorize/continue 非 200: {status} body={}",
            short(&body)
        ));
    }
    // 检查 page.type，应该是 email_otp_verification
    if let Ok(v) = serde_json::from_str::<Value>(&body) {
        if let Some(pt) = v.pointer("/page/type").and_then(|x| x.as_str()) {
            println!("  → page.type = {pt}");
        }
    }

    log_step("4. POST /api/accounts/passwordless/send-otp 触发邮件");
    let resp = client
        .post("https://auth.openai.com/api/accounts/passwordless/send-otp")
        .header("origin", "https://auth.openai.com")
        .header("referer", "https://auth.openai.com/email-verification")
        .header("accept", "application/json")
        .header("content-type", "application/json")
        .body("")
        .send()
        .await
        .map_err(|e| format!("send-otp 失败: {e}"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    println!("  → status: {status}");
    println!("  → body: {}", short(&body));
    if !status.is_success() {
        return Err(format!("send-otp 非 200: {status} body={}", short(&body)));
    }

    log_step(&format!(
        "5. 轮询 usmail.my.id 等 6 位验证码（最多 {}s）",
        input.otp_timeout_secs
    ));
    let mb = mailbox
        .unwrap_or_else(|| MailboxProvider::Usmail(UsmailMyId::new(client.clone()).since_now()));
    let deadline = Instant::now() + Duration::from_secs(input.otp_timeout_secs);
    let hit = mb.fetch_otp(&input.email, deadline).await?;
    println!("  → 命中: from={} subject={}", hit.from, hit.subject);
    println!("  → code: {}", hit.code);

    log_step("6. POST /api/accounts/email-otp/validate 提交验证码");
    let resp = client
        .post("https://auth.openai.com/api/accounts/email-otp/validate")
        .header("origin", "https://auth.openai.com")
        .header("referer", "https://auth.openai.com/email-verification")
        .header("accept", "application/json")
        .header("content-type", "application/json")
        .body(format!(r#"{{"code":"{}"}}"#, hit.code))
        .send()
        .await
        .map_err(|e| format!("email-otp/validate 失败: {e}"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    println!("  → status: {status}");
    println!("  → body: {}", short(&body));
    if !status.is_success() {
        return Err(format!(
            "email-otp/validate 非 200: {status} body={}",
            short(&body)
        ));
    }
    let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let workspace_id = pluck_str(&v, &["workspace_id", "workspaceId", "default_workspace_id"]);
    let mut continue_url = pluck_str(
        &v,
        &[
            "continue_url",
            "continueUrl",
            "next_url",
            "nextUrl",
            "redirect_url",
        ],
    );
    println!("  → workspace_id: {:?}", workspace_id);
    println!("  → continue_url(otp): {:?}", continue_url);

    // 如果 OTP validate 没给 workspace_id，从 oai-client-auth-session cookie 里挖
    let mut effective_wsid = workspace_id.clone();
    if effective_wsid.is_none() {
        log_step("6.5 从 oai-client-auth-session cookie 里挖 workspace_id");
        if let Some(ws) = extract_workspace_from_session_cookie(&jar) {
            effective_wsid = Some(ws);
        }
        println!("  → workspace_id from cookie: {:?}", effective_wsid);
    }
    // 兜底：再走一次 session_dump
    if effective_wsid.is_none() {
        log_step("6.6 GET /api/accounts/client_auth_session_dump 兜底");
        let resp = client
            .get("https://auth.openai.com/api/accounts/client_auth_session_dump")
            .header("accept", "application/json")
            .header(
                "referer",
                "https://auth.openai.com/sign-in-with-chatgpt/codex/consent",
            )
            .send()
            .await
            .map_err(|e| format!("session_dump 失败: {e}"))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        println!("  → status: {status}");
        println!("  → body(完整): {}", body);
        if let Ok(v) = serde_json::from_str::<Value>(&body) {
            // 尝试多种结构
            for ptr in [
                "/workspace_id",
                "/default_workspace_id",
                "/client_auth_session/workspace_id",
                "/client_auth_session/default_workspace_id",
            ] {
                if let Some(s) = v.pointer(ptr).and_then(|x| x.as_str()) {
                    effective_wsid = Some(s.to_string());
                    break;
                }
            }
            if effective_wsid.is_none() {
                for path in ["workspaces", "client_auth_session/workspaces"] {
                    let target = if path.contains('/') {
                        v.pointer(&format!("/{path}"))
                    } else {
                        v.get(path)
                    };
                    if let Some(arr) = target.and_then(|x| x.as_array()) {
                        if let Some(first) = arr.first() {
                            if let Some(id) = first.get("id").and_then(|x| x.as_str()) {
                                effective_wsid = Some(id.to_string());
                                break;
                            }
                        }
                    }
                }
            }
        }
        println!("  → workspace_id from dump: {:?}", effective_wsid);
    }

    if let Some(wsid) = effective_wsid.clone() {
        log_step("7. POST /api/accounts/workspace/select");
        let body = format!(r#"{{"workspace_id":"{}"}}"#, wsid);
        let resp = client
            .post("https://auth.openai.com/api/accounts/workspace/select")
            .header("origin", "https://auth.openai.com")
            .header(
                "referer",
                "https://auth.openai.com/sign-in-with-chatgpt/codex/consent",
            )
            .header("accept", "application/json")
            .header("content-type", "application/json")
            .header("openai-sentinel-token", &sentinel_header)
            .body(body)
            .send()
            .await
            .map_err(|e| format!("workspace/select 失败: {e}"))?;
        let status = resp.status();
        // 当前 client 设置了对 localhost 停下；这里允许的话 reqwest 会 follow 到非 localhost 的 redirect。
        // 我们手动检查 Location header 提取 continue_url。
        let loc = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let body = resp.text().await.unwrap_or_default();
        println!("  → status: {status}");
        println!("  → location: {:?}", loc);
        println!("  → body: {}", short(&body));
        if let Some(l) = loc {
            continue_url = Some(l);
        } else if let Ok(jv) = serde_json::from_str::<Value>(&body) {
            if let Some(cu) = pluck_str(&jv, &["continue_url", "continueUrl"]) {
                continue_url = Some(cu);
            }
        }
    }

    let continue_url = continue_url.ok_or_else(|| {
        "无法获取 continue_url，OTP 校验/workspace 选择都没给出，无法继续".to_string()
    })?;
    println!("  → 最终 continue_url: {}", short(&continue_url));

    log_step("8. 跟 continue_url 直到 redirect 到 localhost callback");
    let final_url = follow_until_callback(&client, &continue_url, &redirect_uri).await?;
    let code = extract_code_from_url(&final_url)?;
    println!("  → 拿到 OAuth code: {}…", &code[..code.len().min(16)]);

    log_step("9. POST /oauth/token 兑换 access/refresh/id_token");
    let token = oauth::exchange_code(&code, &redirect_uri, &pkce.code_verifier).await?;
    let email = token
        .id_token
        .as_deref()
        .and_then(oauth::parse_user_info)
        .map(|u| u.email)
        .unwrap_or(input.email);
    Ok(LoginOutput { email, token })
}

fn build_auth_url(code_challenge: &str, state: &str, redirect_uri: &str) -> String {
    // 与 oauth_server.rs 保持一致：raw 拼接，不 urlencode
    let qs = format!(
        "response_type=code&client_id={}&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&id_token_add_organizations=true&codex_cli_simplified_flow=true&state={}&originator=codex_cli_rs",
        oauth::CLIENT_ID,
        redirect_uri,
        "openid profile email offline_access",
        code_challenge,
        state
    );
    format!("{}?{}", oauth::AUTH_URL, qs)
}

fn generate_state() -> String {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64URL, Engine as _};
    use rand::{rng, RngCore};
    let mut bytes = [0u8; 32];
    rng().fill_bytes(&mut bytes);
    B64URL.encode(bytes)
}

/// 从 oai-client-auth-session cookie 里挖 workspace_id。
/// 该 cookie 是 JWT-like 三段，每段都是 base64url 编码 JSON，里面会有 workspaces[0].id。
fn extract_workspace_from_session_cookie(jar: &reqwest::cookie::Jar) -> Option<String> {
    use base64::{
        engine::general_purpose::STANDARD as B64STD,
        engine::general_purpose::URL_SAFE_NO_PAD as B64URL, Engine as _,
    };
    use reqwest::cookie::CookieStore;

    let raw = {
        let mut found: Option<String> = None;
        for url in [
            "https://auth.openai.com/",
            "https://chatgpt.com/",
            "https://openai.com/",
        ] {
            let u: reqwest::Url = url.parse().ok()?;
            if let Some(hv) = jar.cookies(&u) {
                if let Ok(s) = hv.to_str() {
                    for part in s.split(';') {
                        let part = part.trim();
                        if let Some(rest) = part.strip_prefix("oai-client-auth-session=") {
                            found = Some(rest.to_string());
                            break;
                        }
                    }
                }
            }
            if found.is_some() {
                break;
            }
        }
        found?
    };
    println!(
        "  → cookie raw 头 60: {}…",
        raw.chars().take(60).collect::<String>()
    );
    for seg in raw.split('.') {
        if seg.is_empty() {
            continue;
        }
        let bytes = B64URL.decode(seg).or_else(|_| B64STD.decode(seg)).ok();
        let bytes = match bytes {
            Some(b) => b,
            None => continue,
        };
        let s = match String::from_utf8(bytes) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Ok(v) = serde_json::from_str::<Value>(&s) {
            for ptr in ["/workspace_id", "/default_workspace_id"] {
                if let Some(id) = v.pointer(ptr).and_then(|x| x.as_str()) {
                    if !id.is_empty() {
                        return Some(id.to_string());
                    }
                }
            }
            if let Some(arr) = v.get("workspaces").and_then(|x| x.as_array()) {
                if let Some(first) = arr.first() {
                    if let Some(id) = first.get("id").and_then(|x| x.as_str()) {
                        return Some(id.to_string());
                    }
                }
            }
            if let Some(workspace) = v.get("workspace") {
                if let Some(id) = workspace.get("id").and_then(|x| x.as_str()) {
                    return Some(id.to_string());
                }
            }
        }
    }
    None
}

fn read_oai_did(jar: &reqwest::cookie::Jar) -> Option<String> {
    use reqwest::cookie::CookieStore;
    for url in [
        "https://auth.openai.com/",
        "https://chatgpt.com/",
        "https://openai.com/",
    ] {
        let u: reqwest::Url = url.parse().ok()?;
        if let Some(hv) = jar.cookies(&u) {
            let s = hv.to_str().ok()?.to_string();
            for part in s.split(';') {
                let part = part.trim();
                if let Some(rest) = part.strip_prefix("oai-did=") {
                    if !rest.is_empty() {
                        return Some(rest.to_string());
                    }
                }
            }
        }
    }
    None
}

async fn follow_until_callback(
    client: &reqwest::Client,
    start: &str,
    expected_redirect_prefix: &str,
) -> Result<String, String> {
    let mut url = start.to_string();
    for hop in 0..15 {
        if url.starts_with(expected_redirect_prefix) {
            return Ok(url);
        }
        let resp = client
            .get(&url)
            .header(
                "accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .send()
            .await
            .map_err(|e| format!("follow {url} 失败: {e}"))?;
        let final_u = resp.url().to_string();
        let status = resp.status();
        // 3xx 带 Location：reqwest 的自定义 redirect 策略在 host=localhost 时 stop，会把 3xx 原样返回
        if status.is_redirection() {
            if let Some(loc) = resp
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
            {
                let next = if loc.starts_with("http") {
                    loc
                } else {
                    // 相对路径：基于当前 final_u 拼
                    reqwest::Url::parse(&final_u)
                        .and_then(|u| u.join(&loc))
                        .map(|u| u.to_string())
                        .unwrap_or(loc)
                };
                println!("  → hop {hop}: {status} → {}", short(&next));
                if next.starts_with(expected_redirect_prefix)
                    || next.starts_with("http://localhost:1455")
                {
                    return Ok(next);
                }
                url = next;
                continue;
            }
        }
        if final_u.starts_with(expected_redirect_prefix)
            || final_u.starts_with("http://localhost:1455")
        {
            return Ok(final_u);
        }
        // 200 + JSON { continue_url: ... }
        let body = resp.text().await.unwrap_or_default();
        if let Ok(v) = serde_json::from_str::<Value>(&body) {
            if let Some(next) = pluck_str(&v, &["continue_url", "next_url", "redirect_url"]) {
                println!("  → hop {hop}: {status} JSON continue_url={}", short(&next));
                url = next;
                continue;
            }
        }
        return Err(format!(
            "follow_until_callback 卡住: hop={hop} final_url={} status={status} body={}",
            short(&final_u),
            short(&body)
        ));
    }
    Err("follow_until_callback 超过 15 跳".into())
}

fn extract_code_from_url(url: &str) -> Result<String, String> {
    let u = reqwest::Url::parse(url).map_err(|e| format!("URL 解析失败: {e}"))?;
    for (k, v) in u.query_pairs() {
        if k == "code" {
            return Ok(v.into_owned());
        }
    }
    Err(format!("URL 里没有 code 参数: {url}"))
}

fn pluck_str(v: &Value, keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Some(s) = v.get(k).and_then(|x| x.as_str()) {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
        // 再翻一层 data/result/payload
        for nk in ["data", "result", "next", "payload"] {
            if let Some(s) = v.get(nk).and_then(|x| x.get(k)).and_then(|x| x.as_str()) {
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            }
        }
    }
    None
}

fn short(s: &str) -> String {
    let s = s.replace('\n', " ").replace("  ", " ");
    if s.len() <= 600 {
        s
    } else {
        format!("{}…[{} bytes total]", &s[..600], s.len())
    }
}

fn log_step(s: &str) {
    println!("\n=== {s} ===");
}
