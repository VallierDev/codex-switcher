//! Lossless tool identity mapping for the native Google adapter. No prompt injection.
use crate::relay_translate::ToolWireSpec;
use base64::Engine;
use serde_json::{json, Value};
use std::collections::HashMap;

const SIGNATURE_PREFIX: &str = "switcher-google-v1:";

pub fn signature_carrier(signature: &Value, reasoning: &str, call_ids: Vec<String>) -> String {
    let payload = json!({"signature":signature,"reasoning":reasoning,"call_ids":call_ids});
    format!(
        "{SIGNATURE_PREFIX}{}",
        base64::engine::general_purpose::STANDARD.encode(payload.to_string())
    )
}

fn restore_signatures(items: &mut [Value]) -> Result<(), String> {
    let mut signatures = HashMap::new();
    for item in items.iter_mut() {
        if item.get("type").and_then(Value::as_str) != Some("reasoning") {
            continue;
        }
        let Some(encoded) = item
            .get("encrypted_content")
            .and_then(Value::as_str)
            .and_then(|s| s.strip_prefix(SIGNATURE_PREFIX))
        else {
            continue;
        };
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| "Invalid Google signature carrier")?;
        let data: Value =
            serde_json::from_slice(&bytes).map_err(|_| "Invalid Google signature carrier")?;
        for id in data
            .get("call_ids")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            signatures.insert(id.to_owned(), data["signature"].clone());
        }
        item["thought_signature"] = data["signature"].clone();
        item["encrypted_content"] = data["reasoning"].clone();
    }
    for item in items {
        if matches!(
            item.get("type").and_then(Value::as_str),
            Some("function_call" | "custom_tool_call")
        ) {
            if let Some(signature) = item
                .get("call_id")
                .and_then(Value::as_str)
                .and_then(|id| signatures.get(id))
            {
                item["thought_signature"] = signature.clone();
            }
        }
    }
    Ok(())
}

pub fn prepare_request(body: &mut Value) -> Result<HashMap<String, ToolWireSpec>, String> {
    let mut specs = HashMap::new();
    let mut sources = body
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // Responses Lite / code-mode declares tools inside input, not at the root.
    // Later additional_tools definitions override root declarations of the same identity.
    if let Some(items) = body.get("input").and_then(Value::as_array) {
        for item in items {
            if item.get("type").and_then(Value::as_str) == Some("additional_tools") {
                if let Some(tools) = item.get("tools").and_then(Value::as_array) {
                    sources.extend(tools.iter().cloned());
                }
            }
        }
    }
    if !sources.is_empty() {
        let mut flat = Vec::new();
        flatten(&sources, None, &mut flat, &mut specs)?;
        body["tools"] = Value::Array(flat);
    }
    if let Some(choice) = body.get("tool_choice").filter(|v| v.is_object()).cloned() {
        if choice.get("type").and_then(Value::as_str) == Some("allowed_tools") {
            let mut allowed = Vec::new();
            for tool in choice
                .get("tools")
                .and_then(Value::as_array)
                .ok_or("allowed_tools requires tools")?
            {
                allowed.push(resolve_name(tool, &specs)?);
            }
            body["tool_choice"] = json!({"type":"allowed_tools", "mode":choice.get("mode").cloned().unwrap_or(json!("auto")), "names":allowed});
        } else {
            let name = resolve_name(&choice, &specs)?;
            body["tool_choice"] = json!({"type":"function","function":{"name":name}});
        }
    }
    if let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) {
        items.retain(|item| item.get("type").and_then(Value::as_str) != Some("additional_tools"));
        restore_signatures(items)?;
        for item in items {
            let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
            if !matches!(kind, "custom_tool_call" | "function_call") {
                continue;
            }
            if kind == "custom_tool_call" {
                let input = item
                    .get("input")
                    .and_then(Value::as_str)
                    .ok_or("custom_tool_call.input must be a string")?
                    .to_owned();
                item["arguments"] = Value::String(json!({"input":input}).to_string());
            }
            let name = item.get("name").and_then(Value::as_str).unwrap_or("");
            let namespace = item.get("namespace").and_then(Value::as_str);
            let matches: Vec<_> = specs
                .iter()
                .filter(|(_, spec)| {
                    let qualified = spec
                        .namespace
                        .as_ref()
                        .map(|ns| format!("{ns}.{}", spec.name));
                    (spec.name == name
                        && namespace.is_none_or(|ns| spec.namespace.as_deref() == Some(ns)))
                        || qualified.as_deref() == Some(name)
                })
                .map(|(wire, _)| wire.clone())
                .collect();
            if matches.len() == 1 {
                item["name"] = json!(matches[0]);
            }
        }
    }
    Ok(specs)
}

fn resolve_name(tool: &Value, specs: &HashMap<String, ToolWireSpec>) -> Result<String, String> {
    let name = tool
        .get("name")
        .or_else(|| tool.pointer("/function/name"))
        .and_then(Value::as_str)
        .ok_or("tool_choice requires a tool name")?;
    let namespace = tool.get("namespace").and_then(Value::as_str);
    let matches: Vec<_> = specs
        .iter()
        .filter(|(_, spec)| {
            spec.name == name && namespace.is_none_or(|ns| spec.namespace.as_deref() == Some(ns))
        })
        .collect();
    if matches.len() != 1 {
        return Err("tool_choice references an unknown or ambiguous tool".into());
    }
    Ok(matches[0].0.clone())
}

fn flatten(
    tools: &[Value],
    namespace: Option<&str>,
    flat: &mut Vec<Value>,
    specs: &mut HashMap<String, ToolWireSpec>,
) -> Result<(), String> {
    for tool in tools {
        let kind = tool.get("type").and_then(Value::as_str).unwrap_or("");
        if kind == "namespace" {
            let name = tool
                .get("name")
                .and_then(Value::as_str)
                .ok_or("tool namespace has no name")?;
            let scope = namespace
                .map(|ns| format!("{ns}.{name}"))
                .unwrap_or_else(|| name.to_string());
            if let Some(children) = tool.get("tools").and_then(Value::as_array) {
                flatten(children, Some(&scope), flat, specs)?;
            }
            continue;
        }
        if !matches!(kind, "custom" | "function") {
            if kind == "local_shell" {
                specs.insert(
                    "shell".into(),
                    ToolWireSpec {
                        name: "shell".into(),
                        namespace: None,
                        custom: false,
                        local_shell: true,
                    },
                );
            }
            flat.push(tool.clone());
            continue;
        }
        let name = tool
            .get("name")
            .or_else(|| tool.pointer("/function/name"))
            .and_then(Value::as_str)
            .ok_or("tool has no name")?;
        let namespace = namespace.or_else(|| tool.get("namespace").and_then(Value::as_str));
        let qualified = namespace
            .map(|ns| format!("{ns}__{name}"))
            .unwrap_or_else(|| name.to_string());
        let mut wire: String = qualified
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let existing = specs
            .iter()
            .find(|(_, spec)| spec.name == name && spec.namespace.as_deref() == namespace)
            .map(|(wire, _)| wire.clone());
        if let Some(existing) = existing {
            wire = existing;
            flat.retain(|tool| {
                tool.get("name")
                    .or_else(|| tool.pointer("/function/name"))
                    .and_then(Value::as_str)
                    != Some(wire.as_str())
            });
        } else if wire.len() > 64 || wire.is_empty() || specs.contains_key(&wire) {
            wire = format!("tool_{}", specs.len());
            while specs.contains_key(&wire) {
                wire.push('_');
            }
        }
        specs.insert(
            wire.clone(),
            ToolWireSpec {
                name: name.to_string(),
                namespace: namespace.map(str::to_owned),
                custom: kind == "custom",
                local_shell: false,
            },
        );
        let mut mapped = tool.clone();
        if mapped.get("function").is_some() {
            mapped["function"]["name"] = json!(wire);
        } else {
            mapped["name"] = json!(wire);
        }
        flat.push(mapped);
    }
    Ok(())
}
