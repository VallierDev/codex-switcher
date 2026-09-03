use crate::relay_translate::{self, TranslatorState};
use serde_json::{json, Map, Value};

pub fn responses_to_antigravity(
    codex_body: &[u8],
    model: &str,
    project_id: &str,
) -> Result<(Vec<u8>, TranslatorState), String> {
    let (chat_body, state) = relay_translate::translate_request(codex_body, model)
        .map_err(|e| format!("Responses normalization failed: {e}"))?;
    let chat: Value = serde_json::from_slice(&chat_body).map_err(|e| e.to_string())?;
    let mut request = chat_to_gemini_request(&chat)?;
    // The shared chat normalizer intentionally doesn't own provider-specific
    // reasoning settings. Read them from the original Responses request here.
    let original: Value = serde_json::from_slice(codex_body).map_err(|e| e.to_string())?;
    apply_thinking_config(&mut request, &original, model)?;
    let envelope = json!({
        "model": model,
        "userAgent": "antigravity",
        "requestType": "agent",
        "project": project_id,
        "requestId": format!("agent-{}", uuid::Uuid::new_v4()),
        "request": request,
    });
    serde_json::to_vec(&envelope)
        .map(|bytes| (bytes, state))
        .map_err(|e| e.to_string())
}

fn apply_thinking_config(request: &mut Value, original: &Value, model: &str) -> Result<(), String> {
    let effort = original
        .pointer("/reasoning/effort")
        .or_else(|| original.get("reasoning_effort"));
    let Some(effort) = effort.filter(|value| !value.is_null()) else {
        return Ok(()); // Preserve the upstream default for existing callers.
    };
    let effort = effort
        .as_str()
        .ok_or_else(|| "reasoning.effort must be a string".to_string())?;
    let metadata = super::models::model_for_id(model)
        .ok_or_else(|| format!("Unknown Antigravity model: {model}"))?;
    if !metadata.thinking_levels.contains(&effort) {
        return Err(format!(
            "{model} supports reasoning effort: {}",
            metadata.thinking_levels.join(", ")
        ));
    }
    // GPT OSS currently has only its upstream medium preset; do not invent controls.
    if model.starts_with("gpt-oss-") {
        return Ok(());
    }
    let include_thoughts = original
        .pointer("/reasoning/summary")
        .and_then(Value::as_str)
        != Some("none");
    let thinking = if model.starts_with("gemini-3") || model == "gemini-pro-agent" {
        json!({"thinkingLevel": effort.to_ascii_uppercase(), "includeThoughts": include_thoughts})
    } else {
        // Antigravity's native Google envelope translates this budget to Claude's
        // extended-thinking budget. Claude requires >=1024 and maxOutputTokens > budget.
        let budget = match effort {
            "low" => 1024,
            "medium" => 8192,
            "high" => 24576,
            _ => unreachable!(),
        };
        json!({"thinkingBudget": budget, "includeThoughts": include_thoughts})
    };
    let max_output = match original.get("max_output_tokens").filter(|v| !v.is_null()) {
        Some(value) => value
            .as_u64()
            .filter(|v| *v > 0)
            .ok_or_else(|| "max_output_tokens must be a positive integer".to_string())?,
        None => metadata.max_completion_tokens,
    }
    .min(metadata.max_completion_tokens);
    if model.starts_with("claude-")
        && max_output <= thinking["thinkingBudget"].as_u64().unwrap_or(0)
    {
        return Err("max_output_tokens must exceed the selected Claude thinking budget; lower effort or raise the output limit".to_string());
    }
    request["generationConfig"] =
        json!({"thinkingConfig": thinking, "maxOutputTokens": max_output});
    Ok(())
}

fn chat_to_gemini_request(chat: &Value) -> Result<Value, String> {
    let messages = chat
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "translated chat payload has no messages".to_string())?;
    let mut system_parts = Vec::new();
    let mut contents = Vec::new();
    let mut call_names = std::collections::HashMap::<String, String>::new();

    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        if role == "system" || role == "developer" {
            append_content_parts(message.get("content"), &mut system_parts);
            continue;
        }

        if role == "tool" {
            let call_id = message
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let name = call_names
                .get(call_id)
                .cloned()
                .unwrap_or_else(|| "tool".to_string());
            let output = content_as_text(message.get("content"));
            contents.push(json!({
                "role": "user",
                "parts": [{"functionResponse": {"name": name, "id": call_id, "response": {"output": output}}}],
            }));
            continue;
        }

        let mut parts = Vec::new();
        let thought_signature = message.get("thought_signature").cloned();
        append_content_parts(message.get("content"), &mut parts);
        if let Some(reasoning) = message.get("reasoning_content").and_then(Value::as_str) {
            if !reasoning.is_empty() {
                let mut thought = json!({"text": reasoning, "thought": true});
                if message
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .is_none()
                {
                    if let (Some(signature), Some(object)) =
                        (thought_signature.clone(), thought.as_object_mut())
                    {
                        object.insert("thoughtSignature".to_string(), signature);
                    }
                }
                parts.push(thought);
            }
        }
        if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
            for (index, call) in tool_calls.iter().enumerate() {
                let id = call.get("id").and_then(Value::as_str).unwrap_or_default();
                let function = call.get("function").unwrap_or(&Value::Null);
                let name = function
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("tool");
                let arguments = function
                    .get("arguments")
                    .and_then(Value::as_str)
                    .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                    .unwrap_or_else(|| json!({}));
                if !id.is_empty() {
                    call_names.insert(id.to_string(), name.to_string());
                }
                let mut part = json!({"functionCall": {"name": name, "args": arguments, "id": id}});
                if index == 0 {
                    if let (Some(signature), Some(object)) =
                        (thought_signature.clone(), part.as_object_mut())
                    {
                        object.insert("thoughtSignature".to_string(), signature);
                    }
                }
                parts.push(part);
            }
        }
        if !parts.is_empty() {
            contents.push(json!({
                "role": if role == "assistant" { "model" } else { "user" },
                "parts": parts,
            }));
        }
    }

    let mut request = Map::new();
    request.insert("contents".to_string(), Value::Array(contents));
    if !system_parts.is_empty() {
        request.insert(
            "systemInstruction".to_string(),
            json!({"role": "user", "parts": system_parts}),
        );
    }
    if let Some(tools) = chat.get("tools").and_then(Value::as_array) {
        let declarations: Vec<Value> = tools
            .iter()
            .filter_map(|tool| {
                let function = if tool.get("type").and_then(Value::as_str) == Some("function") {
                    tool.get("function")
                } else {
                    Some(tool)
                }?;
                let name = function.get("name")?.as_str()?;
                let parameters = sanitize_schema_value(
                    function
                        .get("parameters")
                        .cloned()
                        .unwrap_or_else(|| json!({"type":"object","properties":{}})),
                );
                Some(json!({
                    "name": name,
                    "description": function.get("description").cloned().unwrap_or(Value::String(String::new())),
                    "parameters": parameters,
                }))
            })
            .collect();
        if !declarations.is_empty() {
            request.insert(
                "tools".to_string(),
                json!([{"functionDeclarations": declarations}]),
            );
        }
    }
    Ok(Value::Object(request))
}

fn append_content_parts(content: Option<&Value>, out: &mut Vec<Value>) {
    match content {
        Some(Value::String(text)) if !text.is_empty() => out.push(json!({"text": text})),
        Some(Value::Array(items)) => {
            for item in items {
                match item.get("type").and_then(Value::as_str) {
                    Some("text") | Some("input_text") | Some("output_text") => {
                        if let Some(text) = item.get("text").and_then(Value::as_str) {
                            out.push(json!({"text": text}));
                        }
                    }
                    Some("image_url") => {
                        if let Some(url) = item
                            .get("image_url")
                            .and_then(|value| value.get("url").or(Some(value)))
                            .and_then(Value::as_str)
                        {
                            out.push(json!({"fileData": {"fileUri": url}}));
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

fn content_as_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(value) => value.to_string(),
        None => String::new(),
    }
}

fn sanitize_schema_value(value: Value) -> Value {
    sanitize_schema_node(value, false)
}

fn sanitize_schema_node(value: Value, property_map: bool) -> Value {
    match value {
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|value| sanitize_schema_node(value, false))
                .collect(),
        ),
        Value::Object(mut object) => {
            // `properties` 下面的 key 是用户工具参数名，不是 Schema 关键字。
            // 例如参数完全可以叫 `format` / `items` / `default`，绝不能删。
            if property_map {
                for value in object.values_mut() {
                    *value = sanitize_schema_node(std::mem::take(value), false);
                }
                return Value::Object(object);
            }
            for union_key in ["anyOf", "oneOf"] {
                if let Some(Value::Array(branches)) = object.remove(union_key) {
                    let nullable = branches
                        .iter()
                        .any(|branch| branch.get("type").and_then(Value::as_str) == Some("null"));
                    if let Some(branch) = branches
                        .into_iter()
                        .find(|branch| branch.get("type").and_then(Value::as_str) != Some("null"))
                    {
                        let mut selected = sanitize_schema_node(branch, false);
                        if nullable {
                            if let Some(selected_object) = selected.as_object_mut() {
                                selected_object.insert("nullable".to_string(), Value::Bool(true));
                            }
                        }
                        for (key, value) in object {
                            if matches!(key.as_str(), "description" | "nullable") {
                                if let Some(selected_object) = selected.as_object_mut() {
                                    selected_object.entry(key).or_insert(value);
                                }
                            }
                        }
                        return selected;
                    }
                }
            }

            if let Some(Value::Array(types)) = object.get("type").cloned() {
                let nullable = types.iter().any(|value| value.as_str() == Some("null"));
                if let Some(kind) = types
                    .into_iter()
                    .find(|value| value.as_str() != Some("null"))
                {
                    object.insert("type".to_string(), kind);
                }
                if nullable {
                    object.insert("nullable".to_string(), Value::Bool(true));
                }
            }

            const UNSUPPORTED: &[&str] = &[
                "$schema",
                "$id",
                "$ref",
                "$defs",
                "definitions",
                "title",
                "format",
                "default",
                "const",
                "examples",
                "example",
                "pattern",
                "patternProperties",
                "additionalProperties",
                "minLength",
                "maxLength",
                "minimum",
                "maximum",
                "exclusiveMinimum",
                "exclusiveMaximum",
                "minItems",
                "maxItems",
                "uniqueItems",
                "allOf",
                "if",
                "then",
                "else",
                "not",
                "dependentSchemas",
                "dependentRequired",
                "unevaluatedProperties",
                "propertyNames",
            ];
            object.retain(|key, _| !UNSUPPORTED.contains(&key.as_str()) && !key.starts_with("x-"));
            for (key, value) in object.iter_mut() {
                *value = sanitize_schema_node(std::mem::take(value), key == "properties");
            }
            if object.get("type").and_then(Value::as_str) == Some("array")
                && !object.contains_key("items")
            {
                object.insert("items".to_string(), json!({"type":"string"}));
            }
            if let (Some(Value::Array(required)), Some(Value::Object(properties))) =
                (object.get("required").cloned(), object.get("properties"))
            {
                let filtered: Vec<Value> = required
                    .into_iter()
                    .filter(|name| {
                        name.as_str()
                            .map(|name| properties.contains_key(name))
                            .unwrap_or(false)
                    })
                    .collect();
                if filtered.is_empty() {
                    object.remove("required");
                } else {
                    object.insert("required".to_string(), Value::Array(filtered));
                }
            }
            Value::Object(object)
        }
        other => other,
    }
}

pub fn antigravity_json_to_chat_response(raw: &[u8], model: &str) -> Result<Vec<u8>, String> {
    let envelope: Value = serde_json::from_slice(raw).map_err(|e| e.to_string())?;
    let response = envelope.get("response").unwrap_or(&envelope);
    let parts = response
        .pointer("/candidates/0/content/parts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    let mut thought_signature = None;
    for part in parts {
        if thought_signature.is_none() {
            thought_signature = part.get("thoughtSignature").cloned();
        }
        if part.get("thought").and_then(Value::as_bool) == Some(true) {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                reasoning.push_str(text);
            }
        } else if let Some(text) = part.get("text").and_then(Value::as_str) {
            content.push_str(text);
        } else if let Some(call) = part.get("functionCall") {
            let name = call.get("name").and_then(Value::as_str).unwrap_or("tool");
            let args = call.get("args").cloned().unwrap_or_else(|| json!({}));
            tool_calls.push(json!({
                "id": call.get("id").and_then(Value::as_str).map(ToOwned::to_owned).unwrap_or_else(|| format!("call_{}", uuid::Uuid::new_v4())),
                "type": "function",
                "function": {"name": name, "arguments": args.to_string()},
            }));
        }
    }
    let mut message = json!({"role": "assistant", "content": if content.is_empty() { Value::Null } else { Value::String(content) }});
    if !reasoning.is_empty() {
        message["reasoning_content"] = Value::String(reasoning);
    }
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }
    if let Some(signature) = thought_signature {
        message["thought_signature"] = signature;
    }
    let usage = antigravity_usage(response)
        .unwrap_or_else(|| json!({"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}));
    serde_json::to_vec(&json!({
        "id": format!("chatcmpl-{}", uuid::Uuid::new_v4()),
        "object": "chat.completion",
        "model": model,
        "choices": [{"index": 0, "message": message, "finish_reason": "stop"}],
        "usage": usage,
    }))
    .map_err(|e| e.to_string())
}

pub fn antigravity_sse_event_to_chat_chunk(raw: &[u8], model: &str) -> Option<Vec<u8>> {
    let envelope: Value = serde_json::from_slice(raw).ok()?;
    let response = envelope.get("response").unwrap_or(&envelope);
    let parts = response
        .pointer("/candidates/0/content/parts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    let mut thought_signature = None;
    for part in parts {
        if thought_signature.is_none() {
            thought_signature = part.get("thoughtSignature").cloned();
        }
        if part.get("thought").and_then(Value::as_bool) == Some(true) {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                reasoning.push_str(text);
            }
        } else if let Some(text) = part.get("text").and_then(Value::as_str) {
            content.push_str(text);
        } else if let Some(call) = part.get("functionCall") {
            let name = call.get("name").and_then(Value::as_str).unwrap_or("tool");
            let args = call.get("args").cloned().unwrap_or_else(|| json!({}));
            tool_calls.push(json!({
                "index": tool_calls.len(),
                "id": call.get("id").and_then(Value::as_str).map(ToOwned::to_owned).unwrap_or_else(|| format!("call_{}", uuid::Uuid::new_v4())),
                "type": "function",
                "function": {"name": name, "arguments": args.to_string()},
            }));
        }
    }
    let mut delta = Map::new();
    if !content.is_empty() {
        delta.insert("content".to_string(), Value::String(content));
    }
    if !reasoning.is_empty() {
        delta.insert("reasoning_content".to_string(), Value::String(reasoning));
    }
    if !tool_calls.is_empty() {
        delta.insert("tool_calls".to_string(), Value::Array(tool_calls));
    }
    if let Some(signature) = thought_signature {
        delta.insert("thought_signature".to_string(), signature);
    }
    let finish_reason = response
        .pointer("/candidates/0/finishReason")
        .filter(|value| !value.is_null())
        .map(|_| Value::String("stop".to_string()))
        .unwrap_or(Value::Null);
    if delta.is_empty() && finish_reason.is_null() {
        return None;
    }
    let mut chunk = json!({
        "id": format!("chatcmpl-{}", uuid::Uuid::new_v4()),
        "object": "chat.completion.chunk",
        "model": model,
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}],
    });
    if let Some(usage) = antigravity_usage(response) {
        chunk["usage"] = usage;
    }
    serde_json::to_vec(&chunk).ok()
}

fn antigravity_usage(response: &Value) -> Option<Value> {
    let usage = response.get("usageMetadata")?;
    let input = usage
        .get("promptTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = usage
        .get("candidatesTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total = usage
        .get("totalTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| input.saturating_add(output));
    Some(json!({
        "prompt_tokens": input,
        "completion_tokens": output,
        "total_tokens": total,
    }))
}

pub fn antigravity_response_to_codex(
    raw: &[u8],
    state: &mut TranslatorState,
    model: &str,
    stream: bool,
) -> Result<Vec<u8>, String> {
    let chat = antigravity_json_to_chat_response(raw, model)?;
    if !stream {
        return relay_translate::translate_sync_response(state, &chat)
            .map_err(|e| format!("Antigravity response translation failed: {e}"));
    }

    let value: Value = serde_json::from_slice(&chat).map_err(|e| e.to_string())?;
    let message = &value["choices"][0]["message"];
    let mut delta = Map::new();
    if let Some(content) = message.get("content").filter(|value| !value.is_null()) {
        delta.insert("content".to_string(), content.clone());
    }
    if let Some(reasoning) = message.get("reasoning_content") {
        delta.insert("reasoning_content".to_string(), reasoning.clone());
    }
    if let Some(tool_calls) = message.get("tool_calls") {
        delta.insert("tool_calls".to_string(), tool_calls.clone());
    }
    if let Some(signature) = message.get("thought_signature") {
        delta.insert("thought_signature".to_string(), signature.clone());
    }
    let chunk = format!(
        "data: {}\n\n",
        json!({"choices":[{"index":0,"delta":delta,"finish_reason":Value::Null}]})
    );
    let mut output = relay_translate::emit_created(state);
    for event in relay_translate::handle_chunk(state, chunk.as_bytes()) {
        output.extend_from_slice(&event);
    }
    output.extend_from_slice(&relay_translate::emit_completed(state));
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effort_reaches_gemini_level_and_claude_budget() {
        for (effort, budget) in [("low", 1024), ("medium", 8192), ("high", 24576)] {
            for model in [
                "gemini-3.7-flash-high",
                "gemini-3.7-flash-low",
                "gemini-pro-agent",
                "claude-opus-4-6-thinking",
                "claude-sonnet-4-6",
                "gemini-2.5-pro",
            ] {
                let input = json!({"model":model,"input":"hello","reasoning":{"effort":effort,"summary":"auto"}});
                let (body, _) =
                    responses_to_antigravity(&serde_json::to_vec(&input).unwrap(), model, "p")
                        .unwrap();
                let value: Value = serde_json::from_slice(&body).unwrap();
                let config = &value["request"]["generationConfig"]["thinkingConfig"];
                if model.starts_with("gemini-3") || model == "gemini-pro-agent" {
                    assert_eq!(config["thinkingLevel"], effort.to_ascii_uppercase());
                    assert!(config.get("thinkingBudget").is_none());
                } else {
                    assert_eq!(config["thinkingBudget"], budget);
                    assert!(config.get("thinkingLevel").is_none());
                }
                assert_eq!(config["includeThoughts"], true);
                assert_eq!(value["model"], model); // No ID remapping or provider fallback.
            }
        }
    }

    #[test]
    fn effort_validation_and_no_effort_compatibility() {
        let mut request = json!({});
        apply_thinking_config(&mut request, &json!({}), "claude-sonnet-4-6").unwrap();
        assert!(request.get("generationConfig").is_none());
        assert!(apply_thinking_config(
            &mut request,
            &json!({"reasoning":{"effort":"xhigh"}}),
            "gemini-3.7-flash-high"
        )
        .is_err());
        assert!(apply_thinking_config(
            &mut request,
            &json!({"reasoning":{"effort":"high"},"max_output_tokens":100}),
            "claude-sonnet-4-6"
        )
        .is_err());
        apply_thinking_config(
            &mut request,
            &json!({"reasoning":{"effort":"low","summary":"none"}}),
            "claude-sonnet-4-6",
        )
        .unwrap();
        assert_eq!(
            request["generationConfig"]["thinkingConfig"]["includeThoughts"],
            false
        );
    }

    #[test]
    fn tool_results_preserve_call_id_for_google_claude_translation() {
        for model in ["claude-sonnet-4-6", "gemini-3.7-flash-high"] {
            let input = json!({"model":model,"input":[
                {"role":"user","content":"Run probe"},
                {"type":"function_call","call_id":"call_probe","name":"probe","arguments":"{}"},
                {"type":"function_call_output","call_id":"call_probe","output":"OK"}
            ]});
            let (body, _) =
                responses_to_antigravity(&serde_json::to_vec(&input).unwrap(), model, "p").unwrap();
            let value: Value = serde_json::from_slice(&body).unwrap();
            let contents = value["request"]["contents"].as_array().unwrap();
            assert_eq!(contents[1]["parts"][0]["functionCall"]["id"], "call_probe");
            assert_eq!(
                contents[2]["parts"][0]["functionResponse"]["id"],
                "call_probe"
            );
            assert_eq!(contents[2]["parts"][0]["functionResponse"]["name"], "probe");
        }
    }

    #[test]
    fn converts_text_request_to_antigravity_envelope() {
        let input = json!({"model":"gemini-3.7-flash-high","instructions":"system","input":"hello","stream":false});
        let (body, _) = responses_to_antigravity(
            &serde_json::to_vec(&input).unwrap(),
            "gemini-3.7-flash-high",
            "p1",
        )
        .unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["project"], "p1");
        assert_eq!(value["request"]["contents"][0]["parts"][0]["text"], "hello");
        assert_eq!(
            value["request"]["systemInstruction"]["parts"][0]["text"],
            "system"
        );
    }

    #[test]
    fn converts_antigravity_text_response_to_chat() {
        let raw = json!({"response":{"candidates":[{"content":{"parts":[{"text":"hello"}]}}]}});
        let chat = antigravity_json_to_chat_response(
            &serde_json::to_vec(&raw).unwrap(),
            "gemini-3.7-flash-high",
        )
        .unwrap();
        let value: Value = serde_json::from_slice(&chat).unwrap();
        assert_eq!(value["choices"][0]["message"]["content"], "hello");
    }

    #[test]
    fn converts_antigravity_text_response_to_codex_sse() {
        let request = json!({"model":"gemini-3.7-flash-high","input":"hello","stream":true});
        let (_, mut state) = responses_to_antigravity(
            &serde_json::to_vec(&request).unwrap(),
            "gemini-3.7-flash-high",
            "p1",
        )
        .unwrap();
        let raw = json!({"response":{"candidates":[{"content":{"parts":[{"text":"world"}]}}]}});
        let out = antigravity_response_to_codex(
            &serde_json::to_vec(&raw).unwrap(),
            &mut state,
            "gemini-3.7-flash-high",
            true,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("response.output_text.delta"));
        assert!(text.contains("response.completed"));
    }

    #[test]
    fn sanitizes_tool_schema_without_touching_property_names() {
        let schema = json!({
            "type": "object",
            "title": "tool",
            "properties": {
                "format": {"type": "string", "format": "uri", "default": "x"},
                "items": {"type": "array"},
                "choice": {"anyOf": [{"type":"string"}, {"type":"null"}]}
            },
            "required": ["format", "missing"]
        });
        let cleaned = sanitize_schema_value(schema);
        assert!(cleaned.get("title").is_none());
        assert_eq!(cleaned["properties"]["format"]["type"], "string");
        assert!(cleaned["properties"]["format"].get("format").is_none());
        assert_eq!(cleaned["properties"]["items"]["items"]["type"], "string");
        assert_eq!(cleaned["properties"]["choice"]["nullable"], true);
        assert_eq!(cleaned["required"], json!(["format"]));
    }

    #[test]
    fn preserves_thought_signature_in_stream_chunk() {
        let raw = json!({"response":{"candidates":[{"content":{"parts":[{
            "thought": true,
            "text": "thinking",
            "thoughtSignature": "sig-native"
        }]}}]}});
        let chunk = antigravity_sse_event_to_chat_chunk(
            &serde_json::to_vec(&raw).unwrap(),
            "gemini-3.7-flash-high",
        )
        .unwrap();
        let value: Value = serde_json::from_slice(&chunk).unwrap();
        assert_eq!(
            value["choices"][0]["delta"]["reasoning_content"],
            "thinking"
        );
        assert_eq!(
            value["choices"][0]["delta"]["thought_signature"],
            "sig-native"
        );
    }
}
