//! Independently selectable native Responses relays. No mutation of store.current.
use crate::account::{Account, AccountStore};
use serde_json::{json, Value};
use std::collections::BTreeSet;

pub const PREFIX: &str = "relay-model:";
pub const CURRENT_PREFIX: &str = "relay-current:";

pub fn is_relay_model_slug(slug: &str) -> bool {
    slug.starts_with(PREFIX) || slug.starts_with(CURRENT_PREFIX)
}

pub fn account_models(account: &Account) -> BTreeSet<String> {
    if !account.is_relay() || account.relay_protocol_or_default() != "responses" {
        return BTreeSet::new();
    }
    account
        .relay_model_map
        .iter()
        .flat_map(|m| m.values())
        .chain(account.relay_model_fallback.iter())
        .map(|m| m.trim().to_owned())
        .filter(|m| !m.is_empty())
        .collect()
}

#[derive(Clone, Debug)]
pub struct Model {
    pub kimi_coding: bool,
    pub slug: String,
    pub account_id: String,
    pub account_name: String,
    pub upstream: String,
}

pub fn candidates(store: &AccountStore) -> Vec<Model> {
    let mut result = Vec::new();
    for account in store.accounts.values().filter(|a| eligible(a)) {
        let ids: BTreeSet<_> = account
            .relay_model_map
            .iter()
            .flat_map(|map| map.values())
            .chain(account.relay_model_fallback.iter())
            .map(|id| id.trim())
            .filter(|id| !id.is_empty())
            .collect();
        for id in ids {
            result.push(Model {
                kimi_coding: account
                    .relay_base_url
                    .as_deref()
                    .is_some_and(crate::kimi_quota::is_official_coding_url)
                    || account.relay_usage_preset.as_deref() == Some("kimi_coding"),
                slug: format!("{PREFIX}{}:{id}", account.id),
                account_id: account.id.clone(),
                account_name: account.name.clone(),
                upstream: id.to_owned(),
            });
        }
    }
    result.sort_by(|a, b| {
        a.account_name
            .cmp(&b.account_name)
            .then(a.upstream.cmp(&b.upstream))
            .then(a.slug.cmp(&b.slug))
    });
    result
}

fn choose<'a>(store: &AccountStore, items: &'a [Model], upstream: &str) -> Option<&'a Model> {
    if let Some(id) = store.settings.current_relay_accounts.get(upstream) {
        return items
            .iter()
            .find(|m| m.upstream == upstream && &m.account_id == id);
    }
    items
        .iter()
        .filter(|m| m.upstream == upstream)
        .min_by(|a, b| {
            store.accounts[&a.account_id]
                .created_at
                .cmp(&store.accounts[&b.account_id].created_at)
                .then(a.account_id.cmp(&b.account_id))
        })
}

pub fn models(store: &AccountStore) -> Vec<Model> {
    let items = candidates(store);
    let ids: BTreeSet<_> = items.iter().map(|m| m.upstream.as_str()).collect();
    ids.into_iter()
        .filter_map(|upstream| choose(store, &items, upstream))
        .map(|m| {
            let mut m = m.clone();
            m.slug = format!("{CURRENT_PREFIX}{}", m.upstream);
            m
        })
        .collect()
}

pub fn ensure_currents(store: &mut AccountStore) -> bool {
    let before = store.settings.current_relay_accounts.clone();
    store.settings.current_relay_accounts.retain(|model, id| {
        store
            .accounts
            .get(id)
            .is_some_and(|a| account_models(a).contains(model))
    });
    for model in models(store) {
        store
            .settings
            .current_relay_accounts
            .entry(model.upstream)
            .or_insert(model.account_id);
    }
    before != store.settings.current_relay_accounts
}

pub fn select_current(
    store: &mut AccountStore,
    account_id: &str,
    upstream: Option<&str>,
) -> Result<(), String> {
    let account = store
        .accounts
        .get(account_id)
        .filter(|a| eligible(a))
        .ok_or("中转账号不可用")?;
    let ids = account_models(account);
    if ids.is_empty() {
        return Err("请先配置此中转的模型 ID".into());
    }
    let selected = if let Some(model) = upstream {
        if !ids.contains(model) {
            return Err("该账号未配置这个模型".into());
        }
        vec![model.to_owned()]
    } else {
        ids.into_iter().collect()
    };
    ensure_currents(store);
    for model in selected {
        store
            .settings
            .current_relay_accounts
            .insert(model, account_id.to_owned());
    }
    Ok(())
}

fn eligible(a: &Account) -> bool {
    a.is_relay()
        && a.relay_protocol_or_default() == "responses"
        && !a.is_banned
        && !a.is_logged_out
        && !a.is_token_invalid
        && a.relay_base_url
            .as_deref()
            .is_some_and(|url| !url.is_empty())
}

pub fn resolve(store: &AccountStore, slug: &str) -> Option<Model> {
    let items = candidates(store);
    let upstream = if let Some(model) = slug.strip_prefix(CURRENT_PREFIX) {
        model
    } else {
        items.iter().find(|m| m.slug == slug)?.upstream.as_str()
    };
    let mut model = choose(store, &items, upstream)?.clone();
    model.slug = slug.to_owned();
    Some(model)
}

pub fn catalog_entry(model: &Model, template: Option<&Value>) -> Value {
    let mut entry = template
        .cloned()
        .filter(Value::is_object)
        .unwrap_or(json!({}));
    let kimi = model.upstream == "kimi-k3";
    let coding_k3 = model.kimi_coding && matches!(model.upstream.as_str(), "k3" | "k3-256k");
    let deepseek = model.upstream.starts_with("deepseek-v4-");
    let display = if kimi {
        "Kimi K3".to_string()
    } else if coding_k3 {
        if model.upstream == "k3-256k" {
            "Kimi K3 256K (Coding)".into()
        } else {
            "Kimi K3 (Coding)".into()
        }
    } else if deepseek {
        model.upstream.replace("deepseek-v4-", "DeepSeek V4 ")
    } else {
        model.upstream.clone()
    };
    let metadata = json!({
        "slug":model.slug, "display_name":format!("{display} · {}",model.account_name),
        "description":format!("{} via {} (Responses API)",model.upstream,model.account_name),
        "base_instructions":"", "model_messages":{"instructions_template":"","instructions_variables":{}},
        "visibility":"list", "supported_in_api":true, "priority":50,
        "upgrade":null,"availability_nux":null,"deprecation":null,"retirement_at":null,
        "prefer_websockets":false,"supports_websockets":false,"use_responses_lite":false,
        "tool_mode":null,"shell_type":"shell_command","apply_patch_tool_type":"freeform",
        "multi_agent_version":"v2","supports_parallel_tool_calls":true,
        "supported_reasoning_levels":if kimi || deepseek || coding_k3 {
            json!([{"effort":"low","description":"Low"},{"effort":"high","description":"High"},{"effort":"max","description":"Max"}])
        } else { json!([]) },
        "default_reasoning_level":if kimi {"max"} else {"high"},
        "default_reasoning_summary":"none","support_verbosity":false,
        "additional_speed_tiers":[],"service_tiers":[],
        "input_modalities":if kimi || model.kimi_coding || model.upstream.ends_with("-vision-exp") {json!(["text","image"])} else {json!(["text"])},
        "supports_image_detail_original":false,
        "context_window":if kimi || deepseek {1_048_576} else if model.kimi_coding {262_144} else {128_000},
        "max_context_window":if kimi || deepseek {1_048_576} else if model.kimi_coding {262_144} else {128_000},
        "effective_context_window_percent":95
    });
    entry
        .as_object_mut()
        .unwrap()
        .extend(metadata.as_object().unwrap().clone());
    entry
}

pub fn responses_url(base: &str) -> Result<String, String> {
    let mut url = reqwest::Url::parse(base.trim()).map_err(|_| "Invalid relay API URL")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("Relay API URL must be HTTP(S), without credentials, query or fragment".into());
    }
    let path = url.path().trim_end_matches('/');
    let path = if path.ends_with("/responses") {
        path.to_owned()
    } else if path.is_empty() {
        "/responses".into()
    } else {
        format!("{path}/responses")
    };
    url.set_path(&path);
    Ok(url.to_string())
}

/// Preserve the native protocol and history. Only the catalog slug is internal.
pub fn request_body(raw: &[u8], model: &Model) -> Result<Vec<u8>, String> {
    let mut value: Value = serde_json::from_slice(raw).map_err(|e| e.to_string())?;
    value["model"] = json!(model.upstream);
    if model.upstream == "kimi-k3" {
        // Kimi's documented compatibility exception. No identity/prompt injection.
        if let Some(tools) = value.get_mut("tools").and_then(Value::as_array_mut) {
            for tool in tools {
                if tool
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|t| t.starts_with("web_search"))
                {
                    if let Some(object) = tool.as_object_mut() {
                        object.remove("search_context_size");
                    }
                }
            }
        }
    }
    serde_json::to_vec(&value).map_err(|e| e.to_string())
}

pub async fn forward_native(
    client: &reqwest::Client,
    base: &str,
    key: &str,
    raw: &[u8],
    model: &Model,
) -> Result<reqwest::Response, String> {
    client
        .post(responses_url(base)?)
        .bearer_auth(key)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("accept-encoding", "identity")
        .header("user-agent", "codex-switcher-relay/1.0")
        .body(request_body(raw, model)?)
        .send()
        .await
        .map_err(|e| format!("Relay connection failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn store() -> AccountStore {
        let mut store = AccountStore::default();
        store.current = Some("existing-chatgpt".into());
        store.add_relay_account(
            "Kimi".into(),
            "https://my-relay.example/custom/v1".into(),
            "test-key".into(),
            None,
            None,
            None,
            None,
            None,
            Some("kimi-k3".into()),
            None,
            None,
        );
        store
    }
    #[test]
    fn isolated_catalog_and_account_route() {
        let mut store = store();
        let model = models(&store).pop().unwrap();
        assert_eq!(store.current.as_deref(), Some("existing-chatgpt"));
        assert!(resolve(&store, "gpt-5.5").is_none());
        assert!(resolve(&store, "kimi-k3").is_none());
        assert_eq!(
            resolve(&store, &model.slug).unwrap().account_id,
            model.account_id
        );
        store
            .accounts
            .get_mut(&model.account_id)
            .unwrap()
            .is_token_invalid = true;
        assert!(resolve(&store, &model.slug).is_none());
    }

    #[test]
    fn repeated_providers_keep_distinct_routes_without_overwriting() {
        let mut store = store();
        let first = models(&store).len();
        for (name, model) in [
            ("Kimi other", "kimi-k3"),
            ("DeepSeek official", "deepseek-v4-pro"),
            ("DeepSeek other", "deepseek-v4-pro"),
        ] {
            store.add_relay_account(
                name.into(),
                "https://another.example/v1".into(),
                "different-fixture-key".into(),
                None,
                None,
                None,
                None,
                None,
                Some(model.into()),
                None,
                None,
            );
        }
        let catalog = candidates(&store);
        assert_eq!(catalog.len(), first + 3);
        assert_eq!(models(&store).len(), 2); // one visible choice per model
        let unique: std::collections::HashSet<_> = catalog.iter().map(|m| &m.slug).collect();
        assert_eq!(unique.len(), catalog.len());
        assert_eq!(
            catalog.iter().filter(|m| m.upstream == "kimi-k3").count(),
            2
        );
        assert_eq!(
            catalog
                .iter()
                .filter(|m| m.upstream == "deepseek-v4-pro")
                .count(),
            2
        );
        assert_eq!(store.current.as_deref(), Some("existing-chatgpt"));
    }

    #[test]
    fn current_accounts_are_per_model_persistent_and_legacy_compatible() {
        let mut store = store();
        store.settings.current_antigravity_account_id = Some("existing-google".into());
        let a = candidates(&store).pop().unwrap();
        let b = store.add_relay_account(
            "B".into(),
            "https://b.example/v1".into(),
            "key-b".into(),
            None,
            None,
            None,
            None,
            Some(std::collections::HashMap::from([(
                "deepseek-v4-pro".into(),
                "deepseek-v4-pro".into(),
            )])),
            Some("kimi-k3".into()),
            None,
            None,
        );
        let c = store.add_relay_account(
            "C".into(),
            "https://c.example/v1".into(),
            "key-c".into(),
            None,
            None,
            None,
            None,
            None,
            Some("deepseek-v4-pro".into()),
            None,
            None,
        );
        select_current(&mut store, &c.id, Some("deepseek-v4-pro")).unwrap();
        select_current(&mut store, &b.id, Some("kimi-k3")).unwrap();
        assert_eq!(
            resolve(&store, "relay-current:kimi-k3").unwrap().account_id,
            b.id
        );
        assert_eq!(resolve(&store, &a.slug).unwrap().account_id, b.id);
        assert_eq!(
            resolve(&store, "relay-current:deepseek-v4-pro")
                .unwrap()
                .account_id,
            c.id
        );
        assert_eq!(store.current.as_deref(), Some("existing-chatgpt"));
        assert_eq!(
            store.settings.current_antigravity_account_id.as_deref(),
            Some("existing-google")
        );
        let reloaded: AccountStore =
            serde_json::from_str(&serde_json::to_string(&store).unwrap()).unwrap();
        assert_eq!(
            reloaded.settings.current_relay_accounts,
            store.settings.current_relay_accounts
        );
        store.accounts.get_mut(&b.id).unwrap().is_token_invalid = true;
        assert!(resolve(&store, "relay-current:kimi-k3").is_none()); // no paid-source fallback
        store.delete_account(&b.id).unwrap();
        assert_eq!(
            resolve(&store, "relay-current:kimi-k3").unwrap().account_id,
            a.account_id
        );
        assert!(select_current(&mut store, &c.id, Some("kimi-k3")).is_err());
    }

    #[test]
    fn first_native_relay_does_not_become_codex_identity() {
        let mut store = AccountStore::default();
        let a = store.add_relay_account(
            "Kimi".into(),
            "https://api.kimi.com/coding/v1".into(),
            "test-key".into(),
            None,
            None,
            None,
            None,
            None,
            Some("k3".into()),
            None,
            None,
        );
        assert!(store.current.is_none());
        assert_eq!(store.settings.current_relay_accounts.get("k3"), Some(&a.id));
    }
    #[test]
    fn provider_metadata_does_not_inherit_gpt_identity_or_retirement() {
        let model = models(&store()).pop().unwrap();
        let entry = catalog_entry(
            &model,
            Some(
                &json!({"base_instructions":"You are GPT", "upgrade":{"retirement_at":1},"service_tiers":["priority"]}),
            ),
        );
        assert_eq!(entry["base_instructions"], "");
        assert!(entry["upgrade"].is_null());
        assert_eq!(entry["context_window"], 1048576);
        assert_eq!(entry["supported_reasoning_levels"][2]["effort"], "max");
        assert_eq!(entry["use_responses_lite"], false);
    }
    #[test]
    fn configurable_url_keeps_custom_host_and_prefix() {
        for (base, expected) in [
            (
                "https://api.deepseek.com",
                "https://api.deepseek.com/responses",
            ),
            (
                "https://other.example/custom/v1/",
                "https://other.example/custom/v1/responses",
            ),
            (
                "http://127.0.0.1:18090/v1/responses",
                "http://127.0.0.1:18090/v1/responses",
            ),
        ] {
            assert_eq!(responses_url(base).unwrap(), expected);
        }
        assert!(responses_url("https://user:secret@host/v1").is_err());
    }
    #[test]
    fn native_custom_tool_history_is_not_translated() {
        let model = models(&store()).pop().unwrap();
        let body = json!({"model":model.slug,"input":[{"type":"custom_tool_call","name":"exec","input":"text(1)"}],"tools":[{"type":"web_search","search_context_size":"low"}],"reasoning":{"effort":"max"}});
        let out: Value = serde_json::from_slice(
            &request_body(&serde_json::to_vec(&body).unwrap(), &model).unwrap(),
        )
        .unwrap();
        assert_eq!(out["model"], "kimi-k3");
        assert_eq!(out["input"], body["input"]);
        assert!(out.get("instructions").is_none());
        assert!(out["tools"][0].get("search_context_size").is_none());
    }

    #[tokio::test]
    async fn mock_custom_endpoint_gets_only_its_key_and_native_model() {
        use bytes::Bytes;
        use http_body_util::{BodyExt, Full};
        use hyper::{body::Incoming, service::service_fn, Request, Response};
        use hyper_util::rt::TokioIo;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/custom/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(stream),service_fn(|req: Request<Incoming>| async move {
                assert_eq!(req.uri().path(),"/custom/v1/responses");
                assert_eq!(req.headers()["authorization"],"Bearer isolated-test-key");
                assert!(!req.headers().contains_key("chatgpt-account-id"));
                assert!(!req.headers().contains_key("cookie"));
                let raw = req.into_body().collect().await.unwrap().to_bytes();
                let value: Value = serde_json::from_slice(&raw).unwrap();
                assert_eq!(value["model"],"kimi-k3");
                assert_eq!(value["input"][0]["input"],"text(1)");
                Ok::<_,std::convert::Infallible>(Response::builder().header("content-type","text/event-stream").body(Full::new(Bytes::from_static(b"data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n"))).unwrap())
            })).await.unwrap();
        });
        let model = models(&store()).pop().unwrap();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let body = json!({"model":model.slug,"input":[{"type":"custom_tool_call","input":"text(1)"}],"stream":true});
        let response = forward_native(
            &client,
            &base,
            "isolated-test-key",
            &serde_json::to_vec(&body).unwrap(),
            &model,
        )
        .await
        .unwrap();
        assert!(response.status().is_success());
        assert!(response
            .text()
            .await
            .unwrap()
            .contains("response.completed"));
        drop(client);
        tokio::time::timeout(std::time::Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
    }
}
