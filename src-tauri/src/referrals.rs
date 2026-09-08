//! ChatGPT Desktop referrals (2026-09 client contract).
//! This API is separate from banked wham/rate-limit-reset-credits.
use serde_json::{json, Value};
use std::time::Duration;

fn context(program: &str) -> Result<Value, String> {
    match program {
        "codex_referral_consumer" | "codex_referral_workspace" => {
            Ok(json!({"program_id": program, "entrypoint": "persistent"}))
        }
        _ => Err("不支持的邀请活动类型".into()),
    }
}

fn request(
    token: &str,
    account_id: Option<&str>,
    method: reqwest::Method,
    path: &str,
) -> reqwest::RequestBuilder {
    let mut req = crate::usage::usage_client()
        .request(
            method,
            format!("https://chatgpt.com/backend-api/referrals/invite{path}"),
        )
        .bearer_auth(token)
        .header("User-Agent", crate::codex_ua::codex_user_agent())
        .header("originator", crate::codex_ua::CODEX_ORIGINATOR)
        .header("Accept", "application/json")
        .timeout(Duration::from_secs(30));
    if let Some(id) = account_id {
        req = req.header("ChatGPT-Account-Id", id);
    }
    req
}

async fn response(req: reqwest::RequestBuilder, sending: bool) -> Result<Value, String> {
    let uncertain = if sending {
        "；发送结果未确认，请先查询邀请记录，勿直接重发"
    } else {
        ""
    };
    let resp = req
        .send()
        .await
        .map_err(|e| format!("邀请接口网络错误：{e}{uncertain}"))?;
    let status = resp.status();
    let data = resp.json::<Value>().await.map_err(|_| {
        format!(
            "邀请接口返回 HTTP {} 非 JSON 响应，可能需要官方桌面版登录会话；无法确认活动资格{}",
            status.as_u16(),
            uncertain
        )
    })?;
    if !status.is_success() {
        let detail = data.get("detail").unwrap_or(&data);
        let message = detail
            .as_str()
            .or_else(|| detail.get("message").and_then(Value::as_str))
            .unwrap_or("请求被上游拒绝");
        let failed = detail
            .get("failed_emails")
            .or_else(|| data.get("failed_emails"));
        return Err(format!(
            "HTTP {}：{}{}{}",
            status.as_u16(),
            message,
            failed.map(|v| format!("；邮箱：{v}")).unwrap_or_default(),
            uncertain
        ));
    }
    if !data.is_object() {
        return Err(format!("邀请接口响应格式无法识别{uncertain}"));
    }
    Ok(data)
}

pub async fn eligibility(token: &str, aid: Option<&str>, program: &str) -> Result<Value, String> {
    let ctx = context(program)?;
    response(
        request(token, aid, reqwest::Method::GET, "/eligibility").query(&[
            ("program_id", ctx["program_id"].as_str().unwrap()),
            ("entrypoint", "persistent"),
        ]),
        false,
    )
    .await
}

pub async fn tracking(
    token: &str,
    aid: Option<&str>,
    program: &str,
    cursor: Option<&str>,
) -> Result<Value, String> {
    context(program)?;
    let mut req = request(token, aid, reqwest::Method::GET, "/tracking").query(&[
        ("program_id", program),
        ("period", "past_90_days"),
        ("limit", "100"),
    ]);
    if let Some(cursor) = cursor {
        req = req.query(&[("cursor", cursor)]);
    }
    let data = response(req, false).await?;
    if !data["items"].is_array() {
        return Err("邀请记录响应缺少 items，无法确认记录".into());
    }
    Ok(data)
}

fn send_body(
    program: &str,
    emails: Vec<String>,
    offer: &Value,
    expected: &Value,
) -> Result<Value, String> {
    let mut body = context(program)?;
    if offer["should_show"] != true {
        return Err("当前账号没有可发送的邀请活动，请刷新资格".into());
    }
    // Detect an offer change between review and submission, including grant amounts.
    for key in ["offer_id", "grants", "requires_explicit_confirmation"] {
        if offer[key] != expected[key] {
            return Err("邀请奖励或活动条件已变化，请刷新资格后确认".into());
        }
    }
    let mut seen = std::collections::HashSet::new();
    let emails: Vec<String> = emails
        .into_iter()
        .map(|e| e.trim().to_string())
        .filter(|e| !e.is_empty() && seen.insert(e.to_lowercase()))
        .collect();
    let mut cap = offer["remaining_send_capacity"]
        .as_u64()
        .unwrap_or(0)
        .min(5);
    // Match the official client: reward capacity limits offers with grants or legacy offer_id.
    let grants = offer["grants"]
        .as_array()
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    let has_offer = offer["offer_id"].as_str().is_some_and(|v| v != "none");
    if grants || has_offer {
        cap = cap.min(offer["remaining_reward_capacity"].as_u64().unwrap_or(0));
    }
    if emails.is_empty() || emails.len() as u64 > cap {
        return Err(format!("本次最多可邀请 {cap} 个邮箱"));
    }
    let email_re = regex::Regex::new(r"^[^\s@]+@[^\s@]+\.[^\s@]+$").unwrap();
    if emails.iter().any(|e| !email_re.is_match(e)) {
        return Err("邮箱格式不正确".into());
    }
    body["emails"] = json!(emails);
    Ok(body)
}

pub async fn send(
    token: &str,
    aid: Option<&str>,
    program: &str,
    emails: Vec<String>,
    expected: Value,
) -> Result<Value, String> {
    let offer = eligibility(token, aid, program).await?;
    let body = send_body(program, emails, &offer, &expected)?;
    // Never retry a POST or fall back to the old referral_key campaign.
    let data = response(
        request(token, aid, reqwest::Method::POST, "").json(&body),
        true,
    )
    .await?;
    if !data["invites"].is_array() {
        return Err("发送结果缺少 invites，请先查询邀请记录，勿直接重发".into());
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn offer() -> Value {
        json!({"should_show":true,"offer_id":"credits_1000","remaining_send_capacity":5,"remaining_reward_capacity":2,"grants":[{"recipient":"referrer","grant_type":"personal_credits","amount":1000}]})
    }
    #[test]
    fn new_payload_deduplicates_and_never_uses_old_key() {
        let o = offer();
        let b = send_body(
            "codex_referral_consumer",
            vec![" A@example.com ".into(), "a@example.com".into()],
            &o,
            &o,
        )
        .unwrap();
        assert_eq!(
            b,
            json!({"program_id":"codex_referral_consumer","entrypoint":"persistent","emails":["A@example.com"]})
        );
    }
    #[test]
    fn rejects_capacity_ineligibility_and_changed_rewards() {
        let o = offer();
        let emails = vec!["a@example.com".into()];
        let mut n = o.clone();
        n["grants"][0]["amount"] = json!(500);
        assert!(send_body("codex_referral_consumer", emails.clone(), &n, &o).is_err());
        n = o.clone();
        n["should_show"] = json!(false);
        assert!(send_body("codex_referral_consumer", emails.clone(), &n, &o).is_err());
        n = o.clone();
        n["remaining_send_capacity"] = json!(0);
        assert!(send_body("codex_referral_consumer", emails, &n, &o).is_err());
    }
}
