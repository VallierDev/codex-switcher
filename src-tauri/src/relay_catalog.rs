//! Independently selectable native Responses relays. No mutation of store.current.
use crate::account::{Account, AccountStore};
use serde_json::{json, Value};
use std::collections::BTreeSet;

pub const PREFIX: &str = "relay-model:";
pub const CURRENT_PREFIX: &str = "relay-current:";

/// Models exposed by the local official `agy` bridge in gemini-switcher.
/// They are treated as Relay models only when the account base URL is the
/// loopback 28100 endpoint, so unrelated generic relays are unaffected.
pub const LOCAL_AGY_MODELS: &[&str] = &[
    "gemini-3.8-flash-high",
    "gemini-3.8-flash-medium",
    "gemini-3.8-flash-low",
    "gemini-3.7-flash-high",
    "gemini-3.7-flash-medium",
    "gemini-3.7-flash-low",
    "gemini-3.6-flash-high",
    "gemini-3.6-flash-medium",
    "gemini-3.6-flash-low",
    "gemini-3.1-pro-high",
    "gemini-3.1-pro-low",
    "claude-sonnet-4-6",
    "claude-opus-4-6-thinking",
    "gpt-oss-120b-medium",
];

fn is_local_agy_account(account: &Account) -> bool {
    let Some(base) = account.relay_base_url.as_deref() else {
        return false;
    };
    let Ok(url) = reqwest::Url::parse(base) else {
        return false;
    };
    matches!(url.scheme(), "http" | "https")
        && matches!(url.host_str(), Some("127.0.0.1" | "localhost"))
        && url.port_or_known_default() == Some(28100)
        && url.path().trim_end_matches('/') == "/v1"
}

pub fn is_relay_model_slug(slug: &str) -> bool {
    slug.starts_with(PREFIX) || slug.starts_with(CURRENT_PREFIX)
}

pub fn account_models(account: &Account) -> BTreeSet<String> {
    if !account.is_relay() || account.relay_protocol_or_default() != "responses" {
        return BTreeSet::new();
    }
    let mut models: BTreeSet<String> = account
        .relay_model_map
        .iter()
        .flat_map(|m| m.values())
        .chain(account.relay_model_fallback.iter())
        .map(|m| m.trim().to_owned())
        .filter(|m| !m.is_empty())
        .collect();
    if is_local_agy_account(account) {
        models.extend(LOCAL_AGY_MODELS.iter().map(|model| (*model).to_owned()));
    }
    models
}

#[derive(Clone, Debug)]
pub struct Model {
    pub kimi_coding: bool,
    pub local_agy: bool,
    pub variants: Vec<String>,
    pub slug: String,
    pub account_id: String,
    pub account_name: String,
    pub upstream: String,
}

pub fn candidates(store: &AccountStore) -> Vec<Model> {
    let mut result = Vec::new();
    for account in store.accounts.values().filter(|a| eligible(a)) {
        // account_models 已含 local AGY 的 LOCAL_AGY_MODELS 扩展；
        // 这里不要再自己拼 map+fallback，否则扩展模型进不了目录。
        for id in account_models(account) {
            result.push(Model {
                kimi_coding: account
                    .relay_base_url
                    .as_deref()
                    .is_some_and(crate::kimi_quota::is_official_coding_url)
                    || account.relay_usage_preset.as_deref() == Some("kimi_coding"),
                local_agy: is_local_agy_account(account),
                variants: Vec::new(),
                slug: format!("{PREFIX}{}:{id}", account.id),
                account_id: account.id.clone(),
                account_name: account.name.clone(),
                upstream: id,
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
            .find(|m| normalized_upstream(m) == upstream && &m.account_id == id);
    }
    items
        .iter()
        .filter(|m| normalized_upstream(m) == upstream)
        .min_by(|a, b| {
            store.accounts[&a.account_id]
                .created_at
                .cmp(&store.accounts[&b.account_id].created_at)
                .then(a.account_id.cmp(&b.account_id))
        })
}

fn normalized_upstream(model: &Model) -> &str {
    if !model.local_agy {
        return model.upstream.as_str();
    }
    model
        .upstream
        .strip_suffix("-high")
        .or_else(|| model.upstream.strip_suffix("-medium"))
        .or_else(|| model.upstream.strip_suffix("-low"))
        .unwrap_or(model.upstream.as_str())
}

pub fn models(store: &AccountStore) -> Vec<Model> {
    let items = candidates(store);
    let ids: BTreeSet<_> = items.iter().map(normalized_upstream).collect();
    let mut result: Vec<Model> = ids
        .into_iter()
        .filter_map(|upstream| choose(store, &items, upstream))
        .map(|m| {
            let mut m = m.clone();
            if m.local_agy {
                let base = normalized_upstream(&m).to_string();
                m.variants = items
                    .iter()
                    .filter(|item| normalized_upstream(item) == base)
                    .map(|item| item.upstream.clone())
                    .collect();
                m.upstream = base;
            }
            m.slug = if m.local_agy {
                format!("{CURRENT_PREFIX}agy:{}", m.upstream)
            } else {
                format!("{CURRENT_PREFIX}{}", m.upstream)
            };
            m
        })
        .collect();
    result.sort_by(|a, b| model_order(&a.upstream, &b.upstream));
    result
}

fn model_order(a: &str, b: &str) -> std::cmp::Ordering {
    let rank = |id: &str| {
        if id.starts_with("gemini-") {
            0
        } else if id.starts_with("claude-") {
            1
        } else if id.starts_with("gpt-") {
            2
        } else {
            3
        }
    };
    rank(a)
        .cmp(&rank(b))
        .then_with(|| {
            let av: Vec<u32> = a
                .split(|c: char| !c.is_ascii_digit())
                .filter_map(|v| v.parse().ok())
                .collect();
            let bv: Vec<u32> = b
                .split(|c: char| !c.is_ascii_digit())
                .filter_map(|v| v.parse().ok())
                .collect();
            bv.cmp(&av)
        })
        .then_with(|| {
            let effort = |id: &str| {
                if id.ends_with("-high") {
                    0
                } else if id.ends_with("-medium") {
                    1
                } else if id.ends_with("-low") {
                    2
                } else {
                    3
                }
            };
            effort(a).cmp(&effort(b)).then_with(|| a.cmp(b))
        })
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
        model.strip_prefix("agy:").unwrap_or(model)
    } else {
        items.iter().find(|m| m.slug == slug)?.upstream.as_str()
    };
    let mut model = choose(store, &items, upstream)?.clone();
    model.slug = slug.to_owned();
    Some(model)
}

pub fn catalog_entry(model: &Model, template: Option<&Value>) -> Value {
    let kimi = model.upstream == "kimi-k3";
    // Kimi has its own published Codex capabilities. Never copy unknown GPT
    // capabilities into it when the upstream OpenAI catalog adds new fields.
    let mut entry = if kimi || model.kimi_coding {
        json!({})
    } else {
        template
            .cloned()
            .filter(Value::is_object)
            .unwrap_or(json!({}))
    };
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
    let display = if model.local_agy {
        format!("AGY · {display}")
    } else {
        display
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
        // Do not inherit GPT hosted discovery: when false Codex exposes MCP
        // functions directly instead of emitting unsupported tool_search specs.
        "supports_search_tool":false,"experimental_supported_tools":[],
        "supported_reasoning_levels":if model.local_agy {
            json!([{"effort":"low","description":"Low"},{"effort":"medium","description":"Medium"},{"effort":"high","description":"High"}])
        } else if kimi || deepseek || coding_k3 {
            json!([{"effort":"low","description":"Low"},{"effort":"high","description":"High"},{"effort":"max","description":"Max"}])
        } else { json!([]) },
        "default_reasoning_level":if kimi {"max"} else {"high"},
        "default_reasoning_summary":"none","support_verbosity":false,
        "additional_speed_tiers":[],"service_tiers":[],
        "input_modalities":if model.local_agy || kimi || model.kimi_coding || model.upstream.ends_with("-vision-exp") {json!(["text","image"])} else {json!(["text"])},
        "supports_image_detail_original":false,
        "context_window":if kimi || deepseek {1_048_576} else if model.kimi_coding {262_144} else {128_000},
        "max_context_window":if kimi || deepseek {1_048_576} else if model.kimi_coding {262_144} else {128_000},
        "effective_context_window_percent":95
    });
    entry
        .as_object_mut()
        .unwrap()
        .extend(metadata.as_object().unwrap().clone());
    if kimi || model.kimi_coding {
        entry["supports_reasoning_summaries"] = json!(true);
        entry["truncation_policy"] = json!({"mode":"bytes","limit":10000});
    }
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
    let requested_effort = value
        .pointer("/reasoning/effort")
        .or_else(|| value.get("reasoning_effort"))
        .and_then(Value::as_str);
    let actual_model = if model.local_agy && !model.variants.is_empty() {
        let suffix = match requested_effort {
            Some("low") => "-low",
            Some("medium") => "-medium",
            _ => "-high",
        };
        model
            .variants
            .iter()
            .find(|id| id.ends_with(suffix))
            .cloned()
            .or_else(|| {
                model
                    .variants
                    .iter()
                    .find(|id| id.ends_with("-high"))
                    .cloned()
            })
            .unwrap_or_else(|| model.variants[0].clone())
    } else {
        model.upstream.clone()
    };
    value["model"] = json!(actual_model);
    // ChatGPT 私有历史 item：compaction_trigger 是空载请求控制标记，
    // compaction/context_compaction 的摘要只有 ChatGPT 后端能解密。
    // 第三方 Responses 端点会严格校验直接 400（Kimi: item type
    // "compaction_trigger" is not supported），relay 只能丢弃。
    if let Some(input) = value.get_mut("input").and_then(Value::as_array_mut) {
        let before = input.len();
        input.retain(|item| {
            !matches!(
                item.get("type").and_then(Value::as_str),
                Some(
                    "compaction_trigger"
                        | "compaction"
                        | "compaction_summary"
                        | "context_compaction"
                )
            )
        });
        let dropped = before - input.len();
        if dropped > 0 {
            println!(
                "[relay_catalog] {}: 丢弃 {} 个 ChatGPT 私有 compaction item",
                model.upstream, dropped
            );
        }
    }
    if model.upstream == "kimi-k3" || model.kimi_coding {
        if value.pointer("/tool_choice/type").and_then(Value::as_str) == Some("tool_search") {
            return Err(
                "Kimi does not support hosted tool_search; use declared function tools".into(),
            );
        }
        // Older Desktop caches may still include hosted discovery. Filter only
        // declarations, never tool call/output history or a function named tool_search.
        prepare_kimi_tools(value.get_mut("tools"));
        if let Some(input) = value.get_mut("input").and_then(Value::as_array_mut) {
            for item in input {
                if item.get("type").and_then(Value::as_str) == Some("additional_tools") {
                    prepare_kimi_tools(item.get_mut("tools"));
                }
            }
        }
    }
    serde_json::to_vec(&value).map_err(|e| e.to_string())
}

fn prepare_kimi_tools(value: Option<&mut Value>) {
    let Some(tools) = value.and_then(Value::as_array_mut) else {
        return;
    };
    tools.retain(|tool| tool.get("type").and_then(Value::as_str) != Some("tool_search"));
    for tool in tools {
        if tool.get("type").and_then(Value::as_str) == Some("namespace") {
            prepare_kimi_tools(tool.get_mut("tools"));
        } else if tool.get("type").and_then(Value::as_str) == Some("function") {
            if let Some(parameters) = tool.get_mut("parameters") {
                explicit_kimi_enum_types(parameters);
            }
            if let Some(parameters) = tool.pointer_mut("/function/parameters") {
                explicit_kimi_enum_types(parameters);
            }
        } else if tool
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|t| t.starts_with("web_search"))
        {
            // Kimi's documented compatibility exception. No prompt injection.
            tool.as_object_mut().unwrap().remove("search_context_size");
        }
    }
}

/// Kimi rejects enum-only schemas although they are valid JSON Schema. Add
/// only a type already implied by all enum values (or const); never guess a
/// type for an unconstrained schema, collapse unions, or walk example data.
fn explicit_kimi_enum_types(schema: &mut Value) {
    fn kind(value: &Value) -> &'static str {
        match value {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        }
    }
    let Some(node) = schema.as_object_mut() else {
        return;
    };
    if !node.contains_key("type") {
        let implied = node
            .get("enum")
            .and_then(Value::as_array)
            .and_then(|values| {
                let first = kind(values.first()?);
                values.iter().all(|v| kind(v) == first).then_some(first)
            })
            .or_else(|| node.get("const").map(kind));
        if let Some(implied) = implied {
            node.insert("type".into(), json!(implied));
        }
    }
    for key in [
        "properties",
        "patternProperties",
        "$defs",
        "definitions",
        "dependentSchemas",
        "dependencies",
    ] {
        if let Some(map) = node.get_mut(key).and_then(Value::as_object_mut) {
            for child in map.values_mut() {
                explicit_kimi_enum_types(child);
            }
        }
    }
    for key in ["anyOf", "oneOf", "allOf", "prefixItems"] {
        if let Some(branches) = node.get_mut(key).and_then(Value::as_array_mut) {
            for child in branches {
                explicit_kimi_enum_types(child);
            }
        }
    }
    for key in [
        "items",
        "additionalItems",
        "additionalProperties",
        "contains",
        "not",
        "if",
        "then",
        "else",
        "propertyNames",
        "unevaluatedProperties",
        "unevaluatedItems",
    ] {
        if let Some(child) = node.get_mut(key) {
            if let Some(items) = child.as_array_mut() {
                for item in items {
                    explicit_kimi_enum_types(item);
                }
            } else {
                explicit_kimi_enum_types(child);
            }
        }
    }
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
    fn nested_icon_parameters() -> Value {
        json!({"type":"object","properties":{
            "value":{"type":"string"},
            "details":{"type":"object","properties":{
                "icon":{"anyOf":[{"type":"object","properties":{
                    "color":{"enum":["blue","green","orange","pink","purple","red","yellow"]},
                    "name":{"enum":["aime","appy","bloop"]}
                },"required":["name"]},{"type":"null"}]}
            }}
        },"required":["value"]})
    }

    #[test]
    fn kimi_nested_schema_types_preserve_values_and_other_providers() {
        let mut model = models(&store()).pop().unwrap();
        model.upstream = "k3".into();
        model.kimi_coding = true;
        let schema = nested_icon_parameters();
        let tool = json!({"type":"function","name":"probe","parameters":schema});
        let payload = json!({"model":model.slug,"tools":[tool,{"type":"namespace","name":"functions","tools":[tool]}],
            "input":[{"type":"additional_tools","tools":[tool]}, {"type":"function_call_output","call_id":"old","output":schema}]});
        let result: Value = serde_json::from_slice(
            &request_body(&serde_json::to_vec(&payload).unwrap(), &model).unwrap(),
        )
        .unwrap();
        let mut expected = schema.clone();
        expected["properties"]["details"]["properties"]["icon"]["anyOf"][0]["properties"]
            ["color"]["type"] = json!("string");
        expected["properties"]["details"]["properties"]["icon"]["anyOf"][0]["properties"]["name"]
            ["type"] = json!("string");
        for pointer in [
            "/tools/0/parameters",
            "/tools/1/tools/0/parameters",
            "/input/0/tools/0/parameters",
        ] {
            assert_eq!(result.pointer(pointer).unwrap(), &expected);
        }
        assert_eq!(result["input"][1], payload["input"][1]);
        assert_eq!(
            request_body(&serde_json::to_vec(&result).unwrap(), &model).unwrap(),
            serde_json::to_vec(&result).unwrap()
        );
        model.kimi_coding = false;
        model.upstream = "deepseek-v4-pro".into();
        let result: Value = serde_json::from_slice(
            &request_body(&serde_json::to_vec(&payload).unwrap(), &model).unwrap(),
        )
        .unwrap();
        assert_eq!(result["tools"], payload["tools"]);
        assert_eq!(result["input"], payload["input"]);
    }

    #[test]
    fn relay_strips_chatgpt_only_compaction_items() {
        let mut model = models(&store()).pop().unwrap();
        model.upstream = "gemini-3.8-flash-high".into();
        model.kimi_coding = false;
        let message =
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]});
        let tool_out = json!({"type":"function_call_output","call_id":"c1","output":"ok"});
        let payload = json!({"model":model.slug,"input":[
            message,
            {"type":"compaction_trigger"},
            {"type":"compaction","encrypted_content":"dGVzdA=="},
            {"type":"context_compaction","encrypted_content":"dGVzdA=="},
            tool_out,
        ]});
        let result: Value = serde_json::from_slice(
            &request_body(&serde_json::to_vec(&payload).unwrap(), &model).unwrap(),
        )
        .unwrap();
        assert_eq!(
            result["input"],
            json!([payload["input"][0], payload["input"][4]])
        );
        assert_eq!(result["model"], "gemini-3.8-flash-high");
    }

    #[test]
    fn kimi_schema_walks_only_schema_keywords() {
        let data = json!({"enum":["data"],"properties":{"x":{"enum":["also data"]}}});
        let mut schema = json!({"type":"object","default":data,"examples":[data],"const":data,
            "properties":{
                "enum":{"enum":["literal parameter name"]},"const":{"const":true},
                "numbers":{"enum":[1,2.5]},"explicit":{"type":"integer","enum":[1,2]},
                "empty":{},"mixed":{"enum":[1,"one"]},"empty_enum":{"enum":[]},
                "array":{"type":"array","items":{"enum":["a","b"]}},
                "union":{"oneOf":[{"const":"a"},{"enum":[null]}]},
                "free":{"additionalProperties":true}
            },"$defs":{"choice":{"enum":[false,true]}},"additionalProperties":{"enum":["extra"]}});
        explicit_kimi_enum_types(&mut schema);
        assert_eq!(schema["default"], data);
        assert_eq!(schema["const"], data);
        assert_eq!(schema["examples"], json!([data]));
        for (key, typ) in [
            ("enum", "string"),
            ("const", "boolean"),
            ("numbers", "number"),
            ("explicit", "integer"),
        ] {
            assert_eq!(schema["properties"][key]["type"], typ);
        }
        assert_eq!(schema["properties"]["empty"], json!({}));
        assert_eq!(schema["properties"]["mixed"], json!({"enum":[1,"one"]}));
        assert_eq!(schema["properties"]["empty_enum"], json!({"enum":[]}));
        assert_eq!(schema["properties"]["free"]["additionalProperties"], true);
        assert_eq!(schema["properties"]["array"]["items"]["type"], "string");
        assert_eq!(schema["properties"]["union"]["oneOf"][0]["type"], "string");
        assert_eq!(schema["properties"]["union"]["oneOf"][1]["type"], "null");
        assert_eq!(schema["$defs"]["choice"]["type"], "boolean");
        assert_eq!(schema["additionalProperties"]["type"], "string");
    }

    #[tokio::test]
    #[ignore = "explicit live Kimi tool roundtrip; consumes a small amount of quota"]
    async fn live_kimi_tool_roundtrip() {
        let path =
            std::env::var("KIMI_QUOTA_ACCOUNT_FILE").expect("explicit account file required");
        let id = std::env::var("KIMI_QUOTA_ACCOUNT_ID").expect("explicit account id required");
        let store: AccountStore =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let account = store.accounts.get(&id).unwrap();
        let model = candidates(&store)
            .into_iter()
            .find(|m| m.account_id == id)
            .unwrap();
        assert!(model.kimi_coding);
        let key = AccountStore::extract_access_token(&account.auth_json).unwrap();
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(90))
            .build()
            .unwrap();
        let mut request = json!({"model":model.slug,"stream":false,"reasoning":{"effort":"low"},
            "input":[{"role":"user","content":"Call the probe function once with value OK. After receiving its output, reply with exactly that output. Do not answer before calling it."}],
            "tools":[{"type":"tool_search"},{"type":"function","name":"probe","description":"Returns the test marker","parameters":{"type":"object","properties":{"value":{"type":"string"}},"required":["value"],"additionalProperties":false}}]});
        request["tools"][1]["parameters"] = nested_icon_parameters();
        if std::env::var("KIMI_SCHEMA_REPRO").as_deref() == Ok("1") {
            let mut original = request.clone();
            original["model"] = json!(model.upstream);
            original["tools"].as_array_mut().unwrap().remove(0);
            let response = client
                .post(responses_url(account.relay_base_url.as_deref().unwrap()).unwrap())
                .bearer_auth(&key)
                .header("user-agent", "codex-switcher-relay/1.0")
                .json(&original)
                .send()
                .await
                .unwrap();
            let status = response.status();
            let body: Value = response.json().await.unwrap();
            assert_eq!(status.as_u16(), 400);
            let message = body
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("");
            assert!(
                message.contains("type is not defined"),
                "unexpected validation error"
            );
            println!("Reproduced original missing-type validation: {message}");
        }
        let started = std::time::Instant::now();
        for turn in 0..2 {
            let response = if let Ok(url) = std::env::var("KIMI_PROBE_PROXY_URL") {
                let url = reqwest::Url::parse(&url).unwrap();
                assert_eq!(url.host_str(), Some("127.0.0.1"));
                // Exercise the installed Switcher; never forward the stored key.
                client.post(url).json(&request).send().await.unwrap()
            } else {
                forward_native(
                    &client,
                    account.relay_base_url.as_deref().unwrap(),
                    &key,
                    &serde_json::to_vec(&request).unwrap(),
                    &model,
                )
                .await
                .unwrap()
            };
            let status = response.status();
            let body: Value = response.json().await.unwrap();
            assert!(
                status.is_success(),
                "HTTP {status}, error type={:?} code={:?}",
                body.pointer("/error/type"),
                body.pointer("/error/code")
            );
            let output = body["output"].as_array().expect("Responses output array");
            if turn == 0 {
                let call = output
                    .iter()
                    .find(|i| i["type"] == "function_call")
                    .expect("actual function call required");
                assert_eq!(call["name"], "probe");
                let input = request["input"].as_array_mut().unwrap();
                input.extend(output.iter().cloned());
                input.push(json!({"type":"function_call_output","call_id":call["call_id"],"output":"TOOL_ROUNDTRIP_OK"}));
            } else {
                assert!(
                    output
                        .iter()
                        .any(|i| i["content"]
                            .as_array()
                            .is_some_and(|parts| parts.iter().any(|p| p["text"]
                                .as_str()
                                .is_some_and(|s| s.contains("TOOL_ROUNDTRIP_OK"))))),
                    "final marker missing"
                );
            }
        }
        println!(
            "Kimi native function -> output -> final passed in {}ms",
            started.elapsed().as_millis()
        );
    }

    #[test]
    fn kimi_rejects_inherited_hosted_search_but_keeps_real_tools_and_history() {
        let mut model = models(&store()).pop().unwrap();
        model.upstream = "k3".into();
        model.kimi_coding = true;
        let entry = catalog_entry(
            &model,
            Some(
                &json!({"supports_search_tool":true,"experimental_supported_tools":["tool_search"],"future_gpt_only_capability":true}),
            ),
        );
        assert_eq!(entry["supports_search_tool"], false);
        assert_eq!(entry["experimental_supported_tools"], json!([]));
        assert!(entry.get("future_gpt_only_capability").is_none());
        assert_eq!(entry["supports_reasoning_summaries"], true);
        assert_eq!(
            entry["truncation_policy"],
            json!({"mode":"bytes","limit":10000})
        );
        let history =
            json!({"type":"function_call_output","call_id":"existing","output":"preserve me"});
        let body = json!({"model":model.slug,"tools":[
            {"type":"tool_search"},
            {"type":"function","name":"tool_search","parameters":{"type":"object"}},
            {"type":"namespace","name":"functions","tools":[{"type":"tool_search"},{"type":"custom","name":"exec"}]},
            {"type":"web_search"}
        ],"input":[{"type":"additional_tools","tools":[{"type":"tool_search"},{"type":"function","name":"probe"}]},history]});
        let out: Value = serde_json::from_slice(
            &request_body(&serde_json::to_vec(&body).unwrap(), &model).unwrap(),
        )
        .unwrap();
        assert_eq!(out["tools"].as_array().unwrap().len(), 3);
        assert_eq!(out["tools"][0]["name"], "tool_search");
        assert_eq!(out["tools"][1]["tools"][0]["name"], "exec");
        assert_eq!(out["tools"][2]["type"], "web_search");
        assert_eq!(out["input"][0]["tools"].as_array().unwrap().len(), 1);
        assert_eq!(out["input"][1], history);
        assert!(out.get("instructions").is_none());
        let mut forced = body.clone();
        forced["tool_choice"] = json!({"type":"tool_search"});
        assert!(request_body(&serde_json::to_vec(&forced).unwrap(), &model).is_err());
        model.kimi_coding = false;
        model.upstream = "deepseek-v4-pro".into();
        let out: Value = serde_json::from_slice(
            &request_body(&serde_json::to_vec(&body).unwrap(), &model).unwrap(),
        )
        .unwrap();
        assert_eq!(out["tools"], body["tools"]);
        assert_eq!(out["input"], body["input"]);
    }
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
