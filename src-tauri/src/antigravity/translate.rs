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
    let request = chat_to_gemini_request(&chat)?;
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
                "parts": [{"functionResponse": {"name": name, "response": {"output": output}}}],
            }));
            continue;
        }

        let mut parts = Vec::new();
        append_content_parts(message.get("content"), &mut parts);
        if let Some(reasoning) = message.get("reasoning_content").and_then(Value::as_str) {
            if !reasoning.is_empty() {
                parts.push(json!({"text": reasoning, "thought": true}));
            }
        }
        if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
            for call in tool_calls {
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
                parts.push(json!({"functionCall": {"name": name, "args": arguments, "id": id}}));
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
                Some(json!({
                    "name": name,
                    "description": function.get("description").cloned().unwrap_or(Value::String(String::new())),
                    "parameters": function.get("parameters").cloned().unwrap_or_else(|| json!({"type":"object","properties":{}})),
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
    for part in parts {
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
    serde_json::to_vec(&json!({
        "id": format!("chatcmpl-{}", uuid::Uuid::new_v4()),
        "object": "chat.completion",
        "model": model,
        "choices": [{"index": 0, "message": message, "finish_reason": "stop"}],
    }))
    .map_err(|e| e.to_string())
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
}
