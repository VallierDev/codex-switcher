//! Codex `/v1/responses` ⇄ OpenAI `/chat/completions` translator.
//!
//! Used when a Relay account is flagged with `relay_protocol = "chat_completions"`,
//! e.g. GLM Coding Plan. Codex CLI talks the new `/v1/responses` wire format,
//! but GLM (and many "OpenAI-compatible" relays) only speak `/chat/completions`.
//! This module translates requests outbound and SSE / sync responses inbound,
//! all in-process — no separate daemon.
//!
//! Ported 1:1 from `cornellsh/codex-proxy` (MIT) — see
//! `reference/codex-proxy/{normalizer,zai_provider,zai_stream}.py`.
//!
//! Extension beyond the Python reference: GLM emits `delta.reasoning_content`
//! chunks for thinking. We surface those as `response.reasoning_summary_text.delta`
//! so codex CLI shows reasoning in real time.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

#[derive(Debug)]
pub enum TranslateError {
    InvalidJson(String),
    Serialize(String),
}

impl std::fmt::Display for TranslateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidJson(e) => write!(f, "invalid JSON in request body: {}", e),
            Self::Serialize(e) => write!(f, "serialization failed: {}", e),
        }
    }
}

impl std::error::Error for TranslateError {}

/// State threaded between the request translation and SSE / sync response
/// translation. Owned by the per-request task on proxy.rs.
#[derive(Debug, Clone)]
pub struct TranslatorState {
    pub response_id: String,
    pub model: String,
    pub created_at: u64,
    pub stream_requested: bool,
    pub request_metadata: Value,

    // Streaming state (mutated by handle_chunk):
    seq_num: u64,
    full_content: String,
    full_reasoning_content: String,
    message_idx: Option<usize>,
    item_id: Option<String>,
    next_idx: usize,
    tool_calls: BTreeMap<usize, ToolCallInProgress>,
    reasoning_idx: Option<usize>,
    reasoning_item_id: Option<String>,
    finalized: bool,
}

#[derive(Debug, Clone)]
struct ToolCallInProgress {
    output_index: usize,
    call_id: String,
    name: String,
    arguments: String,
}

impl TranslatorState {
    fn new(model: String, stream_requested: bool, request_metadata: Value) -> Self {
        let now = unix_secs();
        Self {
            response_id: format!("resp_{}", now),
            model,
            created_at: now,
            stream_requested,
            request_metadata,
            seq_num: 0,
            full_content: String::new(),
            full_reasoning_content: String::new(),
            message_idx: None,
            item_id: None,
            next_idx: 0,
            tool_calls: BTreeMap::new(),
            reasoning_idx: None,
            reasoning_item_id: None,
            finalized: false,
        }
    }
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

// ────────────────────────────────────────────────────────────────
// Request translation: codex /v1/responses → /chat/completions
// ────────────────────────────────────────────────────────────────

/// Translate a codex CLI `/v1/responses` request body into an OpenAI Chat
/// Completions body. The returned `TranslatorState` is consumed when the
/// upstream response comes back.
///
/// `model_after_rewrite` is the model the *upstream* sees — already passed
/// through `relay_model_map` / `relay_model_fallback` by `proxy.rs`.
pub fn translate_request(
    codex_body: &[u8],
    model_after_rewrite: &str,
) -> Result<(Vec<u8>, TranslatorState), TranslateError> {
    let mut data: Value = serde_json::from_slice(codex_body)
        .map_err(|e| TranslateError::InvalidJson(e.to_string()))?;

    // `previous_response_id` 在 chat_completions stateless 上游(MiMo/GLM/DeepSeek)
    // 下没有意义；我们靠 `emit_completed` 里 response.id="" 让 codex 自动回退
    // 到"每轮发完整 input"，所以即便 codex 带了这个字段也只需丢弃，不需要再 chain。
    // 见 emit_completed 内的详细说明。

    // Extract metadata we need to surface in the synthesized streaming
    // `response.created` event later.
    let stream_requested = data.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let mut request_metadata = Map::new();
    for k in [
        "temperature",
        "top_p",
        "tool_choice",
        "tools",
        "store",
        "metadata",
    ] {
        if let Some(v) = data.get(k).cloned() {
            request_metadata.insert(k.to_string(), v);
        }
    }

    // Step 1: normalize → messages array (port of normalizer.py)
    normalize_request(&mut data);

    // Step 2: prepare /chat/completions payload (port of zai_provider._prepare_payload)
    let mut payload = prepare_payload(&data, model_after_rewrite);

    // Step 3: transform payload (developer→system, strip strict, web_search shape)
    transform_payload(&mut payload, model_after_rewrite);

    let bytes =
        serde_json::to_vec(&payload).map_err(|e| TranslateError::Serialize(e.to_string()))?;

    let model_for_state = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(model_after_rewrite)
        .to_string();
    let state = TranslatorState::new(
        model_for_state,
        stream_requested,
        Value::Object(request_metadata),
    );
    Ok((bytes, state))
}

// ──── Normalizer (port of normalizer.py) ────

fn normalize_request(data: &mut Value) {
    let mut messages: Vec<Value> = Vec::new();

    // 1. instructions → system message
    if let Some(inst) = data.get("instructions") {
        let mut content = String::new();
        match inst {
            Value::String(s) => content.push_str(s),
            Value::Array(arr) => {
                for block in arr {
                    match block {
                        Value::String(s) => content.push_str(s),
                        Value::Object(obj) => {
                            if let Some(Value::String(t)) = obj.get("text") {
                                content.push_str(t);
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        if !content.is_empty() {
            messages.push(json!({"role": "system", "content": content}));
        }
    }

    // 2. input → messages
    if let Some(input) = data.get("input").cloned() {
        let items: Vec<Value> = match input {
            Value::String(s) => vec![Value::String(s)],
            Value::Array(arr) => arr,
            _ => Vec::new(),
        };
        for item in items {
            match item {
                Value::String(s) => {
                    messages.push(json!({"role": "user", "content": s}));
                }
                Value::Object(_) => {
                    process_input_item(&item, &mut messages);
                }
                _ => {}
            }
        }
    }

    let obj = match data.as_object_mut() {
        Some(o) => o,
        None => return,
    };
    obj.insert("messages".to_string(), Value::Array(messages));
    obj.entry("previous_response_id".to_string())
        .or_insert(Value::Null);
    obj.entry("store".to_string()).or_insert(Value::Bool(false));
    obj.entry("metadata".to_string())
        .or_insert(Value::Object(Map::new()));

    if let Some(tools) = obj.get("tools").cloned() {
        if let Some(arr) = tools.as_array() {
            obj.insert("tools".to_string(), Value::Array(normalize_tools(arr)));
        }
    }
}

fn process_input_item(item: &Value, messages: &mut Vec<Value>) {
    let item_type = item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("message");

    match item_type {
        "message" | "agentMessage" => process_message_item(item, messages),
        "reasoning" => process_reasoning_item(item, messages),
        "function_call" | "commandExecution" | "local_shell_call" | "fileChange"
        | "custom_tool_call" | "web_search_call" => process_tool_call(item, messages),
        "function_call_output"
        | "commandExecutionOutput"
        | "fileChangeOutput"
        | "custom_tool_call_output" => process_tool_output(item, messages),
        _ => {}
    }
}

fn ensure_last_assistant(messages: &mut Vec<Value>) -> &mut Value {
    let need_push = !matches!(messages.last(), Some(m) if m.get("role").and_then(Value::as_str) == Some("assistant"));
    if need_push {
        messages.push(json!({"role": "assistant", "content": Value::Null}));
    }
    messages.last_mut().expect("just pushed or matched")
}

fn process_message_item(item: &Value, messages: &mut Vec<Value>) {
    let mut role = item
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("user")
        .to_string();
    if role == "developer" {
        role = "system".to_string();
    }
    let content_raw = item.get("content");
    let mut content = String::new();
    let mut reasoning_content = item
        .get("reasoning_content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if let Some(c) = content_raw {
        match c {
            Value::String(s) => content.push_str(s),
            Value::Array(arr) => {
                for part in arr {
                    match part {
                        Value::String(s) => content.push_str(s),
                        Value::Object(obj) => {
                            let ptype = obj.get("type").and_then(Value::as_str);
                            match ptype {
                                Some("input_text") | Some("text") | Some("output_text") => {
                                    if let Some(Value::String(t)) = obj.get("text") {
                                        content.push_str(t);
                                    }
                                }
                                Some("reasoning_text") => {
                                    if let Some(Value::String(t)) = obj.get("text") {
                                        reasoning_content.push_str(t);
                                    }
                                }
                                _ => {}
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    if role == "assistant" || role == "model" {
        let amsg = ensure_last_assistant(messages);
        let amsg_obj = amsg.as_object_mut().expect("assistant is object");
        if !content.is_empty() {
            let prev = amsg_obj
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            amsg_obj.insert(
                "content".to_string(),
                Value::String(format!("{}{}", prev, content)),
            );
        }
        if !reasoning_content.is_empty() {
            let prev = amsg_obj
                .get("reasoning_content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            amsg_obj.insert(
                "reasoning_content".to_string(),
                Value::String(format!("{}{}", prev, reasoning_content)),
            );
        }
        if let Some(sig) = item.get("thought_signature").cloned() {
            amsg_obj.insert("thought_signature".to_string(), sig);
        }
    } else {
        messages.push(json!({"role": role, "content": content}));
    }
}

fn process_reasoning_item(item: &Value, messages: &mut Vec<Value>) {
    let mut content = String::new();
    if let Some(arr) = item.get("content").and_then(Value::as_array) {
        for cp in arr {
            match cp {
                Value::String(s) => content.push_str(s),
                Value::Object(obj) => {
                    if let Some(Value::String(t)) = obj.get("text") {
                        content.push_str(t);
                    }
                }
                _ => {}
            }
        }
    }
    if let Some(arr) = item.get("summary").and_then(Value::as_array) {
        for cp in arr {
            match cp {
                Value::String(s) => content.push_str(s),
                Value::Object(obj) => {
                    if let Some(Value::String(t)) = obj.get("text") {
                        content.push_str(t);
                    }
                }
                _ => {}
            }
        }
    }
    if content.is_empty() {
        if let Some(s) = item.get("summary_text").and_then(Value::as_str) {
            content.push_str(s);
        }
    }
    if content.is_empty() {
        if let Some(s) = item.get("reasoning_content").and_then(Value::as_str) {
            content.push_str(s);
        }
    }
    if content.is_empty() {
        return;
    }
    let amsg = ensure_last_assistant(messages);
    let amsg_obj = amsg.as_object_mut().expect("assistant is object");
    let prev = amsg_obj
        .get("reasoning_content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    amsg_obj.insert(
        "reasoning_content".to_string(),
        Value::String(format!("{}{}", prev, content)),
    );
    if let Some(sig) = item.get("thought_signature").cloned() {
        amsg_obj.insert("thought_signature".to_string(), sig);
    }
}

fn process_tool_call(item: &Value, messages: &mut Vec<Value>) {
    let messages_len = messages.len();
    let call_id = item
        .get("call_id")
        .and_then(Value::as_str)
        .or_else(|| item.get("id").and_then(Value::as_str))
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("call_{}", messages_len));
    let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
    let mut name = item
        .get("name")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .unwrap_or_default();
    if name.is_empty() {
        name = match item_type {
            "commandExecution" => "run_shell_command".into(),
            "local_shell_call" => "local_shell_command".into(),
            "fileChange" => "write_file".into(),
            "web_search_call" => "web_search".into(),
            _ => String::new(),
        };
    }

    // Resolve arguments. Mirror Python ordering (which falls through several keys).
    let mut args_value: Value = item
        .get("arguments")
        .cloned()
        .or_else(|| item.get("input").cloned())
        .unwrap_or(Value::Object(Map::new()));
    let args_is_empty = is_empty_value(&args_value);
    if args_is_empty && item_type == "web_search_call" {
        if let Some(action) = item.get("action").cloned() {
            args_value = action;
        }
    }
    let args_is_empty = is_empty_value(&args_value);
    if args_is_empty {
        match item_type {
            "commandExecution" => {
                let cmd = item
                    .get("command")
                    .cloned()
                    .unwrap_or(Value::String("".into()));
                let cwd = item
                    .get("cwd")
                    .cloned()
                    .unwrap_or(Value::String(".".into()));
                args_value = json!({"command": cmd, "dir_path": cwd});
            }
            "local_shell_call" => {
                let action = item
                    .get("action")
                    .cloned()
                    .unwrap_or(Value::Object(Map::new()));
                let exec_data = action
                    .get("exec")
                    .cloned()
                    .unwrap_or(Value::Object(Map::new()));
                let command = exec_data
                    .get("command")
                    .cloned()
                    .unwrap_or(Value::Array(vec![]));
                let working_directory = exec_data.get("working_directory").cloned();
                args_value = json!({
                    "command": command,
                    "working_directory": working_directory,
                });
            }
            "fileChange" => {
                let path = item
                    .get("changes")
                    .and_then(Value::as_array)
                    .and_then(|arr| arr.first())
                    .and_then(|c| c.get("path"))
                    .cloned()
                    .unwrap_or(Value::String("unknown".into()));
                args_value = json!({"file_path": path});
            }
            _ => {}
        }
    }

    let args_string = if args_value.is_string() {
        args_value.as_str().unwrap_or("").to_string()
    } else {
        serde_json::to_string(&args_value).unwrap_or_else(|_| "{}".into())
    };

    if name.is_empty() {
        return;
    }

    let amsg = ensure_last_assistant(messages);
    let amsg_obj = amsg.as_object_mut().expect("assistant is object");
    let tool_calls = amsg_obj
        .entry("tool_calls".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    if let Some(arr) = tool_calls.as_array_mut() {
        arr.push(json!({
            "id": call_id,
            "type": "function",
            "function": {
                "name": name,
                "arguments": args_string,
            }
        }));
    }
    if let Some(sig) = item.get("thought_signature").cloned() {
        amsg_obj.insert("thought_signature".to_string(), sig);
    }
    if let Some(thought) = item.get("thought").and_then(Value::as_str) {
        let prev = amsg_obj
            .get("reasoning_content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        amsg_obj.insert(
            "reasoning_content".to_string(),
            Value::String(format!("{}{}", prev, thought)),
        );
    }
}

fn process_tool_output(item: &Value, messages: &mut Vec<Value>) {
    let call_id = item
        .get("call_id")
        .and_then(Value::as_str)
        .or_else(|| item.get("id").and_then(Value::as_str))
        .map(|s| s.to_string())
        .unwrap_or_default();
    let output_raw = item
        .get("output")
        .or_else(|| item.get("content"))
        .or_else(|| item.get("stdout"))
        .cloned()
        .unwrap_or(Value::String("".into()));
    let mut content = String::new();
    match &output_raw {
        Value::String(s) => content.push_str(s),
        Value::Object(obj) => {
            if let Some(Value::String(s)) = obj.get("content") {
                content.push_str(s);
            }
            if content.is_empty() {
                if let Some(Value::Bool(false)) = obj.get("success") {
                    content.push_str("Error: Tool execution failed");
                }
            }
        }
        Value::Array(arr) => {
            for part in arr {
                match part {
                    Value::String(s) => content.push_str(s),
                    Value::Object(obj) => {
                        let ptype = obj.get("type").and_then(Value::as_str);
                        if matches!(ptype, Some("input_text") | Some("text")) {
                            if let Some(Value::String(t)) = obj.get("text") {
                                content.push_str(t);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    if content.is_empty() {
        if let Some(stderr) = item.get("stderr").and_then(Value::as_str) {
            content = format!("Error: {}", stderr);
        }
    }
    messages.push(json!({
        "role": "tool",
        "tool_call_id": call_id,
        "content": content,
    }));
}

fn is_empty_value(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Object(m) => m.is_empty(),
        Value::Array(a) => a.is_empty(),
        _ => false,
    }
}

fn normalize_tools(tools: &[Value]) -> Vec<Value> {
    let mut out = Vec::with_capacity(tools.len());
    for t in tools {
        let ttype = t.get("type").and_then(Value::as_str);
        let has_function = t.get("function").is_some();
        if ttype == Some("function") && !has_function {
            // Wrap top-level function tools into {type:function, function:{...}}.
            out.push(json!({
                "type": "function",
                "function": {
                    "name": t.get("name").cloned().unwrap_or(Value::Null),
                    "description": t.get("description").cloned().unwrap_or(Value::Null),
                    "parameters": t.get("parameters").cloned().unwrap_or(Value::Null),
                    "strict": t.get("strict").and_then(Value::as_bool).unwrap_or(false),
                }
            }));
        } else {
            out.push(t.clone());
        }
    }
    out
}

// ──── Payload preparation (port of zai_provider) ────

fn prepare_payload(data: &Value, model: &str) -> Value {
    let mut payload = Map::new();
    payload.insert("model".to_string(), Value::String(model.to_string()));
    payload.insert(
        "messages".to_string(),
        data.get("messages")
            .cloned()
            .unwrap_or(Value::Array(vec![])),
    );
    payload.insert(
        "stream".to_string(),
        Value::Bool(data.get("stream").and_then(Value::as_bool).unwrap_or(false)),
    );
    for k in ["tools", "tool_choice", "temperature", "top_p", "max_tokens"] {
        if let Some(v) = data.get(k).cloned() {
            payload.insert(k.to_string(), v);
        }
    }
    Value::Object(payload)
}

fn transform_payload(payload: &mut Value, model_after_rewrite: &str) {
    let obj = match payload.as_object_mut() {
        Some(o) => o,
        None => return,
    };
    let is_mimo = is_mimo_model(model_after_rewrite);
    let is_glm = is_glm_model(model_after_rewrite);

    // Fix roles: developer → system
    if let Some(messages) = obj.get_mut("messages").and_then(Value::as_array_mut) {
        for m in messages {
            if let Some(mo) = m.as_object_mut() {
                if mo.get("role").and_then(Value::as_str) == Some("developer") {
                    mo.insert("role".to_string(), Value::String("system".into()));
                }
                if is_mimo && mo.get("role").and_then(Value::as_str) == Some("assistant") {
                    mo.entry("reasoning_content".to_string())
                        .or_insert_with(|| Value::String(String::new()));
                }
            }
        }
    }

    // MiMo-only: drop any assistant message with tool_calls but empty
    // reasoning_content (and the tool result messages that follow it).
    // MiMo's thinking-mode strict check rejects a multi-turn history that
    // contains assistant turns missing reasoning_content with the cryptic
    // 400 "unexpected character: line 1 column 1 (char 0)" error. Codex CLI
    // 0.130+ keeps reasoning in `encrypted_content` and doesn't surface
    // plaintext reasoning back into `/v1/responses`, so most historical
    // assistant turns arrive here with empty reasoning_content.
    //
    // Short-term workaround: silently drop those incomplete turns. We lose
    // tool-call history but the conversation can continue. The long-term
    // fix is to round-trip MiMo's reasoning through codex's encrypted_content
    // (see memory: mimo_reasoning_content_strict).
    if is_mimo {
        if let Some(messages) = obj.get_mut("messages").and_then(Value::as_array_mut) {
            let original = std::mem::take(messages);
            let mut kept: Vec<Value> = Vec::with_capacity(original.len());
            let mut iter = original.into_iter().peekable();
            let mut dropped_assistant = 0u32;
            let mut dropped_tool = 0u32;
            while let Some(msg) = iter.next() {
                let drop_this = msg
                    .as_object()
                    .map(|mo| {
                        let is_assistant =
                            mo.get("role").and_then(Value::as_str) == Some("assistant");
                        let has_tool_calls = mo
                            .get("tool_calls")
                            .and_then(Value::as_array)
                            .map(|a| !a.is_empty())
                            .unwrap_or(false);
                        let rc_empty = mo
                            .get("reasoning_content")
                            .and_then(Value::as_str)
                            .map(|s| s.is_empty())
                            .unwrap_or(true);
                        is_assistant && has_tool_calls && rc_empty
                    })
                    .unwrap_or(false);
                if !drop_this {
                    kept.push(msg);
                    continue;
                }
                dropped_assistant += 1;
                while let Some(next) = iter.peek() {
                    if next.get("role").and_then(Value::as_str) == Some("tool") {
                        iter.next();
                        dropped_tool += 1;
                    } else {
                        break;
                    }
                }
            }
            if dropped_assistant > 0 || dropped_tool > 0 {
                eprintln!(
                    "[relay_translate] MiMo: dropped {} empty-reasoning assistant + {} tool follow-ups from history",
                    dropped_assistant, dropped_tool
                );
            }
            *messages = kept;
        }
    }

    // Transform tools: drop strict on function; reshape web_search.
    if let Some(tools) = obj.get_mut("tools") {
        if let Some(arr) = tools.as_array_mut() {
            let mut transformed: Vec<Value> = Vec::with_capacity(arr.len());
            for tool in arr.drain(..) {
                let ttype = tool.get("type").and_then(Value::as_str).map(String::from);
                match ttype.as_deref() {
                    Some("function") => {
                        let mut t = tool;
                        if let Some(o) = t.as_object_mut() {
                            o.remove("strict");
                        }
                        transformed.push(t);
                    }
                    Some("web_search") => {
                        // `web_search` is a codex CLI native tool, but every upstream
                        // exposes its own (incompatible) builtin shape — there is no
                        // generic OpenAI-Chat equivalent. Only GLM's `search_pro_jina`
                        // builtin is wired up here; MiMo's needs a separately-billed
                        // plugin (see docs/integration/mimo). Drop it for everyone
                        // else — codex CLI falls back to shell/other tools.
                        if is_glm {
                            transformed.push(json!({
                                "type": "web_search",
                                "web_search": {
                                    "enable": true,
                                    "search_engine": "search_pro_jina",
                                }
                            }));
                        } else {
                            eprintln!(
                                "[relay_translate] dropping web_search tool (no native shape for model={})",
                                model_after_rewrite
                            );
                        }
                    }
                    other => {
                        // codex CLI sends private tool types (local_shell, apply_patch,
                        // custom_tool, etc.) that GLM rejects with "type is illegal".
                        // Match the Python reference (zai_provider.py) and silently
                        // drop them — codex CLI also exposes a regular `function`-typed
                        // `shell` tool that GLM can call, then we remap the call_id back
                        // to local_shell_call shape in the response stream.
                        eprintln!(
                            "[relay_translate] drop non-GLM tool type={:?} name={:?}",
                            other,
                            tool.get("name").or_else(|| tool.pointer("/function/name")),
                        );
                    }
                }
            }
            *arr = transformed;
        }
    }

    // Debug: dump translated body to /tmp for inspection
    if let Ok(debug_bytes) = serde_json::to_vec(&obj) {
        let _ = std::fs::write("/tmp/codex-relay-translate-debug.json", &debug_bytes);
    }
}

fn is_mimo_model(model: &str) -> bool {
    model.to_ascii_lowercase().starts_with("mimo-")
}

fn is_glm_model(model: &str) -> bool {
    model.to_ascii_lowercase().starts_with("glm-")
}

// ────────────────────────────────────────────────────────────────
// Streaming response translation (port of zai_stream.py)
// ────────────────────────────────────────────────────────────────

/// Emit the synthetic `response.created` event. Call once before forwarding
/// any upstream chunks downstream.
pub fn emit_created(state: &TranslatorState) -> Vec<u8> {
    let response_obj = build_response_skeleton(state, "in_progress", Value::Array(vec![]));
    let mut s = state.clone();
    encode_event(
        &mut s,
        "response.created",
        json!({"response": response_obj}),
    )
}

fn build_response_skeleton(state: &TranslatorState, status: &str, output: Value) -> Value {
    let md = state
        .request_metadata
        .as_object()
        .cloned()
        .unwrap_or_default();
    let temperature = md.get("temperature").cloned().unwrap_or(json!(1.0));
    let top_p = md.get("top_p").cloned().unwrap_or(json!(1.0));
    let tool_choice = md.get("tool_choice").cloned().unwrap_or(json!("auto"));
    let tools = md.get("tools").cloned().unwrap_or(json!([]));
    let store = md.get("store").cloned().unwrap_or(json!(true));
    let metadata = md.get("metadata").cloned().unwrap_or(json!({}));
    json!({
        "id": state.response_id,
        "object": "response",
        "created_at": state.created_at,
        "model": state.model,
        "status": status,
        "temperature": temperature,
        "top_p": top_p,
        "tool_choice": tool_choice,
        "tools": tools,
        "parallel_tool_calls": true,
        "store": store,
        "metadata": metadata,
        "output": output,
    })
}

/// Encode one Codex-Responses-shape SSE event from `(evt_type, data_obj)`.
/// Side-effect: bumps `seq_num`.
fn encode_event(state: &mut TranslatorState, evt_type: &str, data: Value) -> Vec<u8> {
    state.seq_num = state.seq_num.saturating_add(1);
    let mut envelope = Map::new();
    envelope.insert(
        "id".to_string(),
        Value::String(format!("evt_{}_{}", unix_ms(), state.seq_num)),
    );
    envelope.insert("object".to_string(), Value::String("response.event".into()));
    envelope.insert("type".to_string(), Value::String(evt_type.to_string()));
    envelope.insert("created_at".to_string(), json!(unix_secs()));
    envelope.insert("sequence_number".to_string(), json!(state.seq_num));
    if let Some(extra) = data.as_object() {
        for (k, v) in extra {
            envelope.insert(k.clone(), v.clone());
        }
    }
    let body = serde_json::to_vec(&Value::Object(envelope)).unwrap_or_default();
    let mut out: Vec<u8> = Vec::with_capacity(body.len() + 64);
    out.extend_from_slice(b"event: ");
    out.extend_from_slice(evt_type.as_bytes());
    out.extend_from_slice(b"\ndata: ");
    out.extend_from_slice(&body);
    out.extend_from_slice(b"\n\n");
    out
}

/// Process one upstream `/chat/completions` SSE payload (the bytes after
/// `data: ` and before the blank line, not including the prefix).
///
/// Returns 0..N fully-encoded `event: …\ndata: …\n\n` byte chunks ready to
/// forward downstream.
pub fn handle_chunk(state: &mut TranslatorState, chunk: &[u8]) -> Vec<Vec<u8>> {
    if state.finalized {
        return Vec::new();
    }
    let mut out: Vec<Vec<u8>> = Vec::new();
    let trimmed = trim_ascii(chunk);
    if trimmed.is_empty() {
        return out;
    }
    // Tolerate a stray `data: ` prefix in case caller didn't strip it.
    let payload = trimmed.strip_prefix(b"data: ").unwrap_or(trimmed);
    if payload == b"[DONE]" {
        return out;
    }
    let data: Value = match serde_json::from_slice(payload) {
        Ok(v) => v,
        Err(_) => return out,
    };

    let choices = match data.get("choices").and_then(Value::as_array) {
        Some(c) if !c.is_empty() => c,
        _ => return out,
    };
    let choice = &choices[0];
    let delta = match choice.get("delta") {
        Some(d) if d.is_object() => d.clone(),
        _ => return out,
    };

    // 1) Tool calls (incremental)
    if let Some(tcs) = delta.get("tool_calls").and_then(Value::as_array) {
        for tc_delta in tcs {
            let idx = tc_delta.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;

            if !state.tool_calls.contains_key(&idx) {
                let output_idx = state.next_idx;
                state.next_idx += 1;
                let call_id = tc_delta
                    .get("id")
                    .and_then(Value::as_str)
                    .map(String::from)
                    .unwrap_or_else(|| format!("call_{}_{}", unix_ms(), output_idx));
                state.tool_calls.insert(
                    idx,
                    ToolCallInProgress {
                        output_index: output_idx,
                        call_id: call_id.clone(),
                        name: String::new(),
                        arguments: String::new(),
                    },
                );
                let item_view = json!({
                    "id": call_id,
                    "type": "function_call",
                    "status": "in_progress",
                    "name": "",
                    "arguments": "",
                    "call_id": call_id,
                });
                out.push(encode_event(
                    state,
                    "response.output_item.added",
                    json!({
                        "response_id": state.response_id,
                        "output_index": output_idx,
                        "item": item_view,
                    }),
                ));
            }

            let tc = state.tool_calls.get_mut(&idx).expect("just inserted");
            if let Some(fn_delta) = tc_delta.get("function") {
                if let Some(name_part) = fn_delta.get("name").and_then(Value::as_str) {
                    tc.name.push_str(name_part);
                }
                if let Some(args_part) = fn_delta.get("arguments") {
                    let appended = match args_part {
                        Value::String(s) => s.clone(),
                        other => serde_json::to_string(other).unwrap_or_default(),
                    };
                    if !appended.is_empty() {
                        let output_idx = tc.output_index;
                        let call_id = tc.call_id.clone();
                        tc.arguments.push_str(&appended);
                        // Emit incremental delta — codex CLI uses this to stream
                        // tool-call argument JSON live.
                        out.push(encode_event(
                            state,
                            "response.function_call_arguments.delta",
                            json!({
                                "response_id": state.response_id,
                                "item_id": call_id,
                                "output_index": output_idx,
                                "delta": appended,
                            }),
                        ));
                    }
                }
            }
        }
    }

    // 2) Reasoning content (GLM extension; not in Python reference)
    if let Some(rc) = delta.get("reasoning_content").and_then(Value::as_str) {
        if !rc.is_empty() {
            state.full_reasoning_content.push_str(rc);
            if state.reasoning_idx.is_none() {
                let idx = state.next_idx;
                state.next_idx += 1;
                let item_id = format!("rs_{}_{}", unix_ms(), idx);
                state.reasoning_idx = Some(idx);
                state.reasoning_item_id = Some(item_id.clone());
                let item_view = json!({
                    "id": item_id,
                    "type": "reasoning",
                    "summary": [{"type": "summary_text", "text": ""}],
                });
                out.push(encode_event(
                    state,
                    "response.output_item.added",
                    json!({
                        "response_id": state.response_id,
                        "output_index": idx,
                        "item": item_view,
                    }),
                ));
            }
            let idx = state.reasoning_idx.expect("just set");
            let item_id = state
                .reasoning_item_id
                .clone()
                .unwrap_or_else(|| format!("rs_{}", unix_ms()));
            out.push(encode_event(
                state,
                "response.reasoning_summary_text.delta",
                json!({
                    "response_id": state.response_id,
                    "item_id": item_id,
                    "output_index": idx,
                    "summary_index": 0,
                    "delta": rc,
                }),
            ));
        }
    }

    // 3) Content (plain assistant text)
    if let Some(content) = delta.get("content").and_then(Value::as_str) {
        if !content.is_empty() {
            state.full_content.push_str(content);
            if state.message_idx.is_none() {
                let idx = state.next_idx;
                state.next_idx += 1;
                let item_id = format!("msg_{}_{}", unix_ms(), idx);
                state.message_idx = Some(idx);
                state.item_id = Some(item_id.clone());
                let message_view = json!({
                    "id": item_id,
                    "type": "message",
                    "role": "assistant",
                    "status": "in_progress",
                    "content": [{"type": "output_text", "text": ""}],
                });
                out.push(encode_event(
                    state,
                    "response.output_item.added",
                    json!({
                        "response_id": state.response_id,
                        "output_index": idx,
                        "item": message_view,
                    }),
                ));
            }
            let idx = state.message_idx.expect("just set");
            let item_id = state
                .item_id
                .clone()
                .unwrap_or_else(|| format!("msg_{}", unix_ms()));
            out.push(encode_event(
                state,
                "response.output_text.delta",
                json!({
                    "response_id": state.response_id,
                    "item_id": item_id,
                    "output_index": idx,
                    "content_index": 0,
                    "delta": content,
                }),
            ));
        }
    }

    out
}

fn trim_ascii(b: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = b.len();
    while start < end
        && (b[start] == b' ' || b[start] == b'\t' || b[start] == b'\r' || b[start] == b'\n')
    {
        start += 1;
    }
    while end > start
        && (b[end - 1] == b' ' || b[end - 1] == b'\t' || b[end - 1] == b'\r' || b[end - 1] == b'\n')
    {
        end -= 1;
    }
    &b[start..end]
}

/// Emit terminator events: `response.output_item.done` per opened item, then
/// `response.completed`.
pub fn emit_completed(state: &mut TranslatorState) -> Vec<u8> {
    if state.finalized {
        return Vec::new();
    }
    state.finalized = true;
    let mut out: Vec<u8> = Vec::new();
    let final_output = collect_final_output(state);

    // Per Python, items are closed in order of their output_index.
    let mut closing: Vec<(usize, Value)> = Vec::new();
    if let Some(idx) = state.message_idx {
        let item_id = state
            .item_id
            .clone()
            .unwrap_or_else(|| format!("msg_{}", unix_ms()));
        closing.push((
            idx,
            json!({
                "id": item_id,
                "type": "message",
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": state.full_content.clone()}],
            }),
        ));
    }
    if let Some(idx) = state.reasoning_idx {
        let item_id = state
            .reasoning_item_id
            .clone()
            .unwrap_or_else(|| format!("rs_{}", unix_ms()));
        closing.push((
            idx,
            json!({
                "id": item_id,
                "type": "reasoning",
                "summary": [{"type": "summary_text", "text": state.full_reasoning_content.clone()}],
            }),
        ));
    }
    for tc in state.tool_calls.values() {
        let mut item = json!({
            "id": tc.call_id,
            "type": "function_call",
            "status": "completed",
            "name": tc.name,
            "arguments": tc.arguments,
            "call_id": tc.call_id,
        });
        // Codex-side coercion: shell tools map onto local_shell_call.
        if matches!(
            tc.name.as_str(),
            "shell" | "container.exec" | "shell_command"
        ) {
            if let Some(o) = item.as_object_mut() {
                o.insert("type".to_string(), Value::String("local_shell_call".into()));
                if let Ok(args_json) = serde_json::from_str::<Value>(&tc.arguments) {
                    let command = args_json
                        .get("command")
                        .cloned()
                        .unwrap_or(Value::Array(vec![]));
                    o.insert(
                        "action".to_string(),
                        json!({"type": "exec", "command": command}),
                    );
                }
            }
        }
        closing.push((tc.output_index, item));
    }
    closing.sort_by_key(|(idx, _)| *idx);

    for (idx, item) in closing {
        out.extend(encode_event(
            state,
            "response.output_item.done",
            json!({
                "response_id": state.response_id,
                "output_index": idx,
                "item": item,
            }),
        ));
    }

    let mut response_obj = build_response_skeleton(state, "completed", final_output);
    if let Some(o) = response_obj.as_object_mut() {
        o.insert("completed_at".to_string(), json!(unix_secs()));
        o.insert(
            "usage".to_string(),
            json!({
                "input_tokens": 0,
                "output_tokens": 0,
                "total_tokens": 0,
            }),
        );
        // 故意把 response.id 设成空串：codex 0.130 WS 客户端
        // (codex-rs/core/src/client.rs:1053) 看到 last_response.response_id.is_empty()
        // 会放弃 `previous_response_id + 增量 input` 优化，下一轮自动回退发
        // 完整 input。chat_completions 上游(MiMo/GLM/DeepSeek)本来就 stateless，
        // 没有 prev-response 状态可以 chain，于是这个降级是免费的胜利。
        o.insert("id".to_string(), Value::String(String::new()));
    }
    out.extend(encode_event(
        state,
        "response.completed",
        json!({"response": response_obj}),
    ));
    out
}

fn collect_final_output(state: &TranslatorState) -> Value {
    let mut closing: Vec<(usize, Value)> = Vec::new();
    if let Some(idx) = state.message_idx {
        let item_id = state
            .item_id
            .clone()
            .unwrap_or_else(|| format!("msg_{}", unix_ms()));
        closing.push((
            idx,
            json!({
                "id": item_id,
                "type": "message",
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": state.full_content.clone()}],
            }),
        ));
    }
    if let Some(idx) = state.reasoning_idx {
        let item_id = state
            .reasoning_item_id
            .clone()
            .unwrap_or_else(|| format!("rs_{}", unix_ms()));
        closing.push((
            idx,
            json!({
                "id": item_id,
                "type": "reasoning",
                "summary": [{"type": "summary_text", "text": state.full_reasoning_content.clone()}],
            }),
        ));
    }
    for tc in state.tool_calls.values() {
        let mut item = json!({
            "id": tc.call_id,
            "type": "function_call",
            "status": "completed",
            "name": tc.name,
            "arguments": tc.arguments,
            "call_id": tc.call_id,
        });
        if matches!(
            tc.name.as_str(),
            "shell" | "container.exec" | "shell_command"
        ) {
            if let Some(o) = item.as_object_mut() {
                o.insert("type".to_string(), Value::String("local_shell_call".into()));
                if let Ok(args_json) = serde_json::from_str::<Value>(&tc.arguments) {
                    let command = args_json
                        .get("command")
                        .cloned()
                        .unwrap_or(Value::Array(vec![]));
                    o.insert(
                        "action".to_string(),
                        json!({"type": "exec", "command": command}),
                    );
                }
            }
        }
        closing.push((tc.output_index, item));
    }
    closing.sort_by_key(|(idx, _)| *idx);
    Value::Array(closing.into_iter().map(|(_, v)| v).collect())
}

// ────────────────────────────────────────────────────────────────
// Sync (non-stream) response translation
// ────────────────────────────────────────────────────────────────

/// Translate a non-stream `/chat/completions` response body into a
/// Responses-shape JSON. Mirrors `_write_mapped_response` in the Python ref.
pub fn translate_sync_response(
    state: &TranslatorState,
    chat_body: &[u8],
) -> Result<Vec<u8>, TranslateError> {
    let z_data: Value = serde_json::from_slice(chat_body)
        .map_err(|e| TranslateError::InvalidJson(e.to_string()))?;
    let choice = z_data
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(Value::Object(Map::new()));
    let message = choice
        .get("message")
        .cloned()
        .unwrap_or(Value::Object(Map::new()));
    let usage = z_data
        .get("usage")
        .cloned()
        .unwrap_or(Value::Object(Map::new()));

    let mut output_items: Vec<Value> = Vec::new();
    if let Some(tcs) = message.get("tool_calls").and_then(Value::as_array) {
        for tc in tcs {
            let id = tc.get("id").cloned().unwrap_or(Value::Null);
            let fn_obj = tc
                .get("function")
                .cloned()
                .unwrap_or(Value::Object(Map::new()));
            let name = fn_obj
                .get("name")
                .cloned()
                .unwrap_or(Value::String("".into()));
            let raw_args = fn_obj.get("arguments").cloned().unwrap_or(Value::Null);
            let args_string = match raw_args {
                Value::String(s) => s,
                other => serde_json::to_string(&other).unwrap_or_default(),
            };
            let mut item = json!({
                "id": id.clone(),
                "type": "function_call",
                "status": "completed",
                "name": name,
                "arguments": args_string.clone(),
                "call_id": id,
            });
            let name_str = item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if matches!(
                name_str.as_str(),
                "shell" | "container.exec" | "shell_command"
            ) {
                if let Some(o) = item.as_object_mut() {
                    o.insert("type".to_string(), Value::String("local_shell_call".into()));
                    if let Ok(args_val) = serde_json::from_str::<Value>(&args_string) {
                        let command = args_val
                            .get("command")
                            .cloned()
                            .unwrap_or(Value::Array(vec![]));
                        o.insert(
                            "action".to_string(),
                            json!({"type": "exec", "command": command}),
                        );
                    }
                }
            }
            output_items.push(item);
        }
    }
    if let Some(content) = message.get("content").and_then(Value::as_str) {
        if !content.is_empty() {
            output_items.push(json!({
                "id": format!("msg_{}", unix_ms()),
                "type": "message",
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "text", "text": content}],
            }));
        }
    }
    if let Some(reasoning_content) = message.get("reasoning_content").and_then(Value::as_str) {
        if !reasoning_content.is_empty() {
            output_items.push(json!({
                "id": format!("rs_{}", unix_ms()),
                "type": "reasoning",
                "summary": [{"type": "summary_text", "text": reasoning_content}],
            }));
        }
    }

    let mut prompt_tokens = 0u64;
    let mut completion_tokens = 0u64;
    let mut total_tokens = 0u64;
    if let Some(o) = usage.as_object() {
        prompt_tokens = o.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0);
        completion_tokens = o
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        total_tokens = o.get("total_tokens").and_then(Value::as_u64).unwrap_or(0);
    }

    let resp_obj = json!({
        "id": format!("zai_{}", z_data.get("id").and_then(Value::as_str).unwrap_or("")),
        "object": "response",
        "created": z_data.get("created").cloned().unwrap_or(json!(state.created_at)),
        "model": z_data.get("model").cloned().unwrap_or(Value::String(state.model.clone())),
        "status": "completed",
        "usage": {
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": total_tokens,
        },
        "output": output_items,
    });
    serde_json::to_vec(&resp_obj).map_err(|e| TranslateError::Serialize(e.to_string()))
}

// ────────────────────────────────────────────────────────────────
// SSE-line splitter (chat/completions style)
// ────────────────────────────────────────────────────────────────

/// Buffer for accumulating partial SSE chunks. /chat/completions emits
/// `data: {…}\n\n`-framed events; we slice them out one at a time and yield
/// the JSON payload bytes (without the `data: ` prefix) to `handle_chunk`.
#[derive(Default, Debug)]
pub struct ChatSseBuffer {
    buf: Vec<u8>,
}

#[derive(Debug)]
pub enum ChatSseEvent {
    /// JSON payload from a `data: {…}` line.
    Data(Vec<u8>),
    /// `[DONE]` sentinel.
    Done,
}

impl ChatSseBuffer {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Yield events available right now. Returns empty Vec when the buffer
    /// has no complete event yet.
    pub fn drain_events(&mut self) -> Vec<ChatSseEvent> {
        let mut out = Vec::new();
        loop {
            // Look for the next \n\n (or \r\n\r\n) frame end.
            let mut end_idx = None;
            let mut frame_skip = 2;
            for i in 0..self.buf.len().saturating_sub(1) {
                if self.buf[i] == b'\n' && self.buf[i + 1] == b'\n' {
                    end_idx = Some(i);
                    frame_skip = 2;
                    break;
                }
                if i + 3 < self.buf.len()
                    && self.buf[i] == b'\r'
                    && self.buf[i + 1] == b'\n'
                    && self.buf[i + 2] == b'\r'
                    && self.buf[i + 3] == b'\n'
                {
                    end_idx = Some(i);
                    frame_skip = 4;
                    break;
                }
            }
            let Some(end) = end_idx else { break };
            let frame = self.buf[..end].to_vec();
            self.buf.drain(..end + frame_skip);
            for line in frame.split(|b| *b == b'\n') {
                let line = strip_cr(line);
                if line.is_empty() {
                    continue;
                }
                if let Some(rest) = line.strip_prefix(b"data: ") {
                    if rest == b"[DONE]" {
                        out.push(ChatSseEvent::Done);
                    } else {
                        out.push(ChatSseEvent::Data(rest.to_vec()));
                    }
                } else if let Some(rest) = line.strip_prefix(b"data:") {
                    // tolerate "data:foo" without space
                    if rest == b"[DONE]" {
                        out.push(ChatSseEvent::Done);
                    } else {
                        out.push(ChatSseEvent::Data(rest.to_vec()));
                    }
                }
                // Other line types (id:, event:, retry:, comments) are ignored —
                // /chat/completions only uses `data:`.
            }
        }
        out
    }
}

fn strip_cr(b: &[u8]) -> &[u8] {
    if let Some(last) = b.last() {
        if *last == b'\r' {
            return &b[..b.len() - 1];
        }
    }
    b
}

// ────────────────────────────────────────────────────────────────
// Models endpoint: synthetic minimal /v1/models response
// ────────────────────────────────────────────────────────────────

/// Small `{data:[{id, object:"model"}], object:"list"}` body to short-circuit
/// `GET /v1/models` when the upstream is `/chat/completions`-only.
pub fn synthetic_models_response(default_model: &str) -> Vec<u8> {
    let body = json!({
        "object": "list",
        "data": [
            {"id": default_model, "object": "model"},
        ],
    });
    serde_json::to_vec(&body).unwrap_or_else(|_| b"{\"object\":\"list\",\"data\":[]}".to_vec())
}

// ────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(b: &[u8]) -> Value {
        serde_json::from_slice(b).expect("translator output must be JSON")
    }

    #[test]
    fn simple_text_request_translation() {
        let codex = json!({
            "model": "gpt-5",
            "instructions": "sys",
            "input": "hi",
            "stream": false,
        });
        let (body, _state) =
            translate_request(&serde_json::to_vec(&codex).unwrap(), "glm-5.1").unwrap();
        let v = parse(&body);
        assert_eq!(v["model"], "glm-5.1");
        assert_eq!(v["stream"], false);
        let messages = v["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "sys");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "hi");
    }

    #[test]
    fn tool_call_round_trip() {
        let codex = json!({
            "model": "gpt-5",
            "input": [
                {
                    "type": "function_call",
                    "call_id": "c1",
                    "name": "lookup",
                    "arguments": {"q": "hello"},
                },
                {
                    "type": "function_call_output",
                    "call_id": "c1",
                    "output": "world",
                },
            ],
        });
        let (body, _state) =
            translate_request(&serde_json::to_vec(&codex).unwrap(), "glm-5.1").unwrap();
        let v = parse(&body);
        let messages = v["messages"].as_array().unwrap();
        // Expect: assistant{tool_calls:[lookup]}, tool{tool_call_id:c1, content:world}
        assert_eq!(messages[0]["role"], "assistant");
        let tool_calls = messages[0]["tool_calls"].as_array().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0]["id"], "c1");
        assert_eq!(tool_calls[0]["type"], "function");
        assert_eq!(tool_calls[0]["function"]["name"], "lookup");
        // arguments must be a JSON-encoded string (not nested object)
        assert!(tool_calls[0]["function"]["arguments"].is_string());
        let args = tool_calls[0]["function"]["arguments"].as_str().unwrap();
        assert!(args.contains("\"q\""));
        assert_eq!(messages[1]["role"], "tool");
        assert_eq!(messages[1]["tool_call_id"], "c1");
        assert_eq!(messages[1]["content"], "world");
    }

    #[test]
    fn consecutive_function_calls_coalesce() {
        let codex = json!({
            "model": "gpt-5",
            "input": [
                {
                    "type": "function_call",
                    "call_id": "c1",
                    "name": "a",
                    "arguments": {"x": 1},
                },
                {
                    "type": "function_call",
                    "call_id": "c2",
                    "name": "b",
                    "arguments": {"y": 2},
                },
            ],
        });
        let (body, _state) =
            translate_request(&serde_json::to_vec(&codex).unwrap(), "glm-5.1").unwrap();
        let v = parse(&body);
        let messages = v["messages"].as_array().unwrap();
        assert_eq!(
            messages.len(),
            1,
            "two consecutive calls collapse into one assistant msg"
        );
        let tool_calls = messages[0]["tool_calls"].as_array().unwrap();
        assert_eq!(tool_calls.len(), 2);
        assert_eq!(tool_calls[0]["function"]["name"], "a");
        assert_eq!(tool_calls[1]["function"]["name"], "b");
    }

    #[test]
    fn developer_role_remapped_to_system() {
        let codex = json!({
            "model": "gpt-5",
            "input": [
                {
                    "type": "message",
                    "role": "developer",
                    "content": "be terse",
                },
            ],
        });
        let (body, _state) =
            translate_request(&serde_json::to_vec(&codex).unwrap(), "glm-5.1").unwrap();
        let v = parse(&body);
        let messages = v["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "be terse");
    }

    #[test]
    fn tools_function_strict_stripped() {
        // Already-wrapped tool (the typical shape codex sends): strict at the
        // top level should be removed by `_transform_payload`.
        let codex = json!({
            "model": "gpt-5",
            "input": "hi",
            "tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "foo",
                        "description": "...",
                        "parameters": {"type": "object"},
                    },
                    "strict": true,
                },
            ],
        });
        let (body, _state) =
            translate_request(&serde_json::to_vec(&codex).unwrap(), "glm-5.1").unwrap();
        let v = parse(&body);
        let tools = v["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert!(tools[0]["function"].is_object());
        assert!(
            tools[0].get("strict").is_none(),
            "top-level strict must be removed after _transform_payload"
        );
    }

    #[test]
    fn previous_response_id_now_tolerated() {
        // codex 0.130 多轮会话总是会带 previous_response_id；之前硬拒，
        // 让 chat_completions 上游（MiMo/GLM/DeepSeek）多轮直接断。
        // 现在改成软忽略 —— 翻译照常完成（消息历史本来就在 input 里）。
        let codex = json!({
            "model": "gpt-5",
            "input": "hi",
            "previous_response_id": "resp_abc",
        });
        let (body, _state) =
            translate_request(&serde_json::to_vec(&codex).unwrap(), "glm-5.1").unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        // body 里不该再带 previous_response_id（chat_completions 协议里不存在）
        assert!(v.get("previous_response_id").map_or(true, |p| p.is_null()),
            "previous_response_id 应当被忽略，不出现在翻译后 chat 请求里");
    }

    #[test]
    fn sse_text_only_stream() {
        let codex = json!({"model": "gpt-5", "input": "hi", "stream": true});
        let (_body, mut state) =
            translate_request(&serde_json::to_vec(&codex).unwrap(), "glm-5.1").unwrap();

        let created = emit_created(&state);
        assert!(contains_event(&created, b"response.created"));

        let mut events: Vec<Vec<u8>> = vec![created];
        for letter in ["a", "b", "c"] {
            let chunk = format!(r#"{{"choices":[{{"delta":{{"content":"{}"}}}}]}}"#, letter);
            events.extend(handle_chunk(&mut state, chunk.as_bytes()));
        }
        let completed = emit_completed(&mut state);
        events.push(completed);

        let blob: Vec<u8> = events.into_iter().flatten().collect();
        assert!(contains_event(&blob, b"response.created"));
        assert!(contains_event(&blob, b"response.output_item.added"));
        let n_deltas = count_event(&blob, b"response.output_text.delta");
        assert_eq!(n_deltas, 3, "one delta per chunk");
        assert!(contains_event(&blob, b"response.output_item.done"));
        assert!(contains_event(&blob, b"response.completed"));
    }

    #[test]
    fn sse_tool_call_stream() {
        let codex = json!({"model": "gpt-5", "input": "hi", "stream": true});
        let (_body, mut state) =
            translate_request(&serde_json::to_vec(&codex).unwrap(), "glm-5.1").unwrap();
        let created = emit_created(&state);

        // One delta containing a full tool-call (id + name + arguments)
        let chunk = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_x",
                        "function": {
                            "name": "shell",
                            "arguments": "{\"cmd\":\"ls\"}"
                        }
                    }]
                }
            }]
        });
        let events1 = handle_chunk(&mut state, &serde_json::to_vec(&chunk).unwrap());
        // finalize chunk (signaling end)
        let finish = json!({
            "choices": [{"delta": {}, "finish_reason": "tool_calls"}]
        });
        let events2 = handle_chunk(&mut state, &serde_json::to_vec(&finish).unwrap());
        let completed = emit_completed(&mut state);

        let mut all: Vec<u8> = created;
        for e in events1 {
            all.extend(e);
        }
        for e in events2 {
            all.extend(e);
        }
        all.extend(completed);
        assert!(contains_event(&all, b"response.output_item.added"));
        assert!(contains_event(
            &all,
            b"response.function_call_arguments.delta"
        ));
        assert!(contains_event(&all, b"response.output_item.done"));
        assert!(contains_event(&all, b"response.completed"));
    }

    #[test]
    fn sse_reasoning_content_stream() {
        let codex = json!({"model": "gpt-5", "input": "hi", "stream": true});
        let (_body, mut state) =
            translate_request(&serde_json::to_vec(&codex).unwrap(), "glm-5.1").unwrap();
        let created = emit_created(&state);
        let chunk = json!({
            "choices": [{"delta": {"reasoning_content": "thinking..."}}]
        });
        let events = handle_chunk(&mut state, &serde_json::to_vec(&chunk).unwrap());
        let completed = emit_completed(&mut state);
        let mut all = created;
        for e in events {
            all.extend(e);
        }
        all.extend(completed);
        assert!(contains_event(&all, b"response.output_item.added"));
        assert!(contains_event(
            &all,
            b"response.reasoning_summary_text.delta"
        ));
        assert!(contains_event(&all, b"thinking..."));
        assert!(contains_event(&all, b"response.completed"));
    }

    #[test]
    fn reasoning_summary_round_trips_to_chat_reasoning_content() {
        let codex = json!({
            "model": "gpt-5",
            "input": [
                {
                    "type": "reasoning",
                    "summary": [{"type": "summary_text", "text": "must pass back"}]
                },
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "answer"}]
                },
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "next"}]
                }
            ]
        });
        let (body, _state) =
            translate_request(&serde_json::to_vec(&codex).unwrap(), "mimo-v2.5-pro").unwrap();
        let v = parse(&body);
        let messages = v["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "assistant");
        assert_eq!(messages[0]["reasoning_content"], "must pass back");
        assert_eq!(messages[0]["content"], "answer");
    }

    #[test]
    fn sync_response_translation() {
        let codex = json!({"model": "gpt-5", "input": "hi"});
        let (_body, state) =
            translate_request(&serde_json::to_vec(&codex).unwrap(), "glm-5.1").unwrap();
        let chat = json!({
            "id": "chatcmpl-xyz",
            "created": 1700000000,
            "model": "glm-5.1",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "hello!",
                },
                "finish_reason": "stop",
            }],
            "usage": {
                "prompt_tokens": 5,
                "completion_tokens": 3,
                "total_tokens": 8,
            },
        });
        let body = translate_sync_response(&state, &serde_json::to_vec(&chat).unwrap()).unwrap();
        let v = parse(&body);
        assert_eq!(v["object"], "response");
        assert_eq!(v["status"], "completed");
        let output = v["output"].as_array().unwrap();
        assert_eq!(output.len(), 1);
        assert_eq!(output[0]["type"], "message");
        assert_eq!(output[0]["content"][0]["text"], "hello!");
        assert_eq!(v["usage"]["total_tokens"], 8);
    }

    #[test]
    fn web_search_tool_reshape() {
        let codex = json!({
            "model": "gpt-5",
            "input": "hi",
            "tools": [
                {"type": "web_search"},
            ],
        });
        let (body, _state) =
            translate_request(&serde_json::to_vec(&codex).unwrap(), "glm-5.1").unwrap();
        let v = parse(&body);
        let tools = v["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "web_search");
        assert_eq!(tools[0]["web_search"]["enable"], true);
        assert_eq!(tools[0]["web_search"]["search_engine"], "search_pro_jina");
        assert!(v.get("webSearchEnabled").is_none());
    }

    #[test]
    fn mimo_web_search_tool_is_dropped() {
        // MiMo Token Plan accounts without the Web Search Plugin reject any request
        // that carries a web_search tool, so we strip it before forwarding upstream.
        let codex = json!({
            "model": "gpt-5",
            "input": "hi",
            "tools": [
                {"type": "web_search"},
                {"type": "function", "function": {"name": "keep_me"}},
            ],
        });
        let (body, _state) =
            translate_request(&serde_json::to_vec(&codex).unwrap(), "mimo-v2.5-pro").unwrap();
        let v = parse(&body);
        let tools = v["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1, "web_search must be stripped for MiMo");
        assert_eq!(tools[0]["function"]["name"], "keep_me");
        assert!(v.get("webSearchEnabled").is_none());
    }

    #[test]
    fn mimo_assistant_history_has_reasoning_content_fallback() {
        let codex = json!({
            "model": "gpt-5",
            "input": [
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "previous"}]
                },
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "next"}]
                }
            ]
        });
        let (body, _state) =
            translate_request(&serde_json::to_vec(&codex).unwrap(), "mimo-v2.5-pro").unwrap();
        let v = parse(&body);
        assert_eq!(v["messages"][0]["role"], "assistant");
        assert_eq!(v["messages"][0]["reasoning_content"], "");
    }

    #[test]
    fn mimo_drops_empty_reasoning_tool_call_history() {
        // MiMo rejects assistant turns with tool_calls but empty reasoning_content
        // (char-0 JSON parse error). The translator should silently prune those
        // turns and their following tool result messages from history.
        let codex = json!({
            "model": "gpt-5",
            "input": [
                // valid assistant turn: has reasoning text → KEEP
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "thinking A"}]},
                {"type": "function_call", "call_id": "c1", "name": "f1", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c1", "output": "out1"},
                // bad assistant turn: tool_calls but no reasoning → DROP (and its tool output)
                {"type": "function_call", "call_id": "c2", "name": "f2", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c2", "output": "out2"},
                // valid user message → KEEP
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "next"}]},
            ]
        });
        let (body, _state) =
            translate_request(&serde_json::to_vec(&codex).unwrap(), "mimo-v2.5-pro").unwrap();
        let v = parse(&body);
        let messages = v["messages"].as_array().unwrap();
        // Expected after pruning:
        //   [0] assistant (kept — reasoning_content non-empty, has c1 tool_call)
        //   [1] tool      (kept — output of c1)
        //   [2] user      (kept — final user msg)
        // The c2 turn was dropped because reasoning_content was empty for it,
        // but it appears the c1 and c2 calls coalesced into one assistant message
        // during normalization. Verify no tool message for c2 leaks through.
        let has_c2_tool = messages.iter().any(|m| {
            m.get("tool_call_id").and_then(Value::as_str) == Some("c2")
        });
        assert!(!has_c2_tool, "tool output for dropped c2 must be pruned");
        // The first assistant message should still carry tool_calls (from c1).
        let first_assistant = messages
            .iter()
            .find(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
            .expect("expected at least one assistant message");
        assert_eq!(
            first_assistant.get("reasoning_content").and_then(Value::as_str),
            Some("thinking A")
        );
    }

    #[test]
    fn sse_buffer_splits_two_events() {
        let mut buf = ChatSseBuffer::new();
        buf.push(b"data: {\"a\":1}\n\ndata: [DONE]\n\n");
        let evts = buf.drain_events();
        assert_eq!(evts.len(), 2);
        match &evts[0] {
            ChatSseEvent::Data(b) => assert_eq!(b, b"{\"a\":1}"),
            _ => panic!("expected Data"),
        }
        assert!(matches!(evts[1], ChatSseEvent::Done));
    }

    fn contains_event(blob: &[u8], evt: &[u8]) -> bool {
        let mut needle: Vec<u8> = Vec::with_capacity(evt.len() + 8);
        needle.extend_from_slice(b"event: ");
        needle.extend_from_slice(evt);
        blob.windows(needle.len()).any(|w| w == needle)
    }

    fn count_event(blob: &[u8], evt: &[u8]) -> usize {
        let mut needle: Vec<u8> = Vec::with_capacity(evt.len() + 8);
        needle.extend_from_slice(b"event: ");
        needle.extend_from_slice(evt);
        let mut count = 0;
        let mut i = 0;
        while i + needle.len() <= blob.len() {
            if &blob[i..i + needle.len()] == needle.as_slice() {
                count += 1;
                i += needle.len();
            } else {
                i += 1;
            }
        }
        count
    }
}
