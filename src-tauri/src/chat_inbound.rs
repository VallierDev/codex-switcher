//! OpenAI `chat/completions` 入站 → Codex `responses` 出站 的翻译层。
//!
//! 用途：让任意 OpenAI 兼容客户端（如 glance）通过 codex-switcher 用上 ChatGPT
//! 账号的 codex 模型（尤其 `gpt-5.3-codex-spark` 这种订阅内、编码用不到的闲置额度）。
//!
//! 方向与 `relay_translate`（responses→chat，给 GLM 上游用）相反：这里是
//! **chat→responses**。glance 是非流式（一次性 JSON），所以入站端缓冲整条
//! responses SSE 后组装成单条 chat/completions JSON 返回，无需 SSE 透传。
//!
//! 关键：**不塞 reasoning 字段**——实测多轮工具回放只要不带 reasoning item，就
//! 不会撞 `encrypted_content` 400（见 [[mimo_reasoning_content_strict]] 同族问题）。

use serde_json::{json, Value};

/// 有图时强制改用的视觉模型。实测：`gpt-5.3-codex-spark` 不支持图片输入（400
/// "does not support image inputs"）；`gpt-5.4-mini` / `gpt-5.4` / `gpt-5.5` 同端点
/// 都支持视觉。选 mini —— 视觉是偶尔用，mini 对主 5h/周额度消耗最小（文本仍走
/// Spark 的独立免费桶）。glance image_describe 发 glm-4.5v+图 → 这里自动改成它。
pub const VISION_MODEL: &str = "gpt-5.4-mini";

/// 解析一条 message 的 content（字符串 或 OpenAI 数组形态），返回
/// (拼接文本, 图片 data-url 列表)。
fn content_parts(content: &Value) -> (String, Vec<String>) {
    match content {
        Value::String(s) => (s.clone(), Vec::new()),
        Value::Array(parts) => {
            let mut text = String::new();
            let mut images = Vec::new();
            for p in parts {
                match p.get("type").and_then(|t| t.as_str()) {
                    Some("text") | Some("input_text") => {
                        if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
                            text.push_str(t);
                        }
                    }
                    Some("image_url") => {
                        // OpenAI: {type:image_url, image_url:{url:"data:..."}}
                        if let Some(u) = p.pointer("/image_url/url").and_then(|v| v.as_str()) {
                            images.push(u.to_string());
                        } else if let Some(u) = p.get("image_url").and_then(|v| v.as_str()) {
                            images.push(u.to_string());
                        }
                    }
                    Some("input_image") => {
                        if let Some(u) = p.get("image_url").and_then(|v| v.as_str()) {
                            images.push(u.to_string());
                        }
                    }
                    _ => {}
                }
            }
            (text, images)
        }
        _ => (String::new(), Vec::new()),
    }
}

/// 把 OpenAI chat/completions 请求体翻成 Codex responses 请求体。
/// `fallback_model` 在请求未带 model 时使用（默认应传 gpt-5.3-codex-spark）。
/// 若请求含图片，自动把 model 改成 [`VISION_MODEL`]（Spark 无视觉）。
pub fn chat_to_responses(chat: &Value, fallback_model: &str) -> Value {
    let mut model = chat
        .get("model")
        .and_then(|m| m.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(fallback_model)
        .to_string();

    let mut instructions = String::new();
    let mut input: Vec<Value> = Vec::new();
    let mut has_image = false;

    if let Some(msgs) = chat.get("messages").and_then(|m| m.as_array()) {
        for msg in msgs {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
            let content_val = msg.get("content").unwrap_or(&Value::Null);
            let (text, images) = content_parts(content_val);
            match role {
                "system" => {
                    if !instructions.is_empty() {
                        instructions.push_str("\n\n");
                    }
                    instructions.push_str(&text);
                }
                "user" => {
                    let mut content_items: Vec<Value> =
                        vec![json!({"type": "input_text", "text": text})];
                    for url in &images {
                        has_image = true;
                        content_items.push(json!({"type": "input_image", "image_url": url}));
                    }
                    input.push(json!({
                        "type": "message",
                        "role": "user",
                        "content": content_items,
                    }));
                }
                "assistant" => {
                    if !text.is_empty() {
                        input.push(json!({
                            "type": "message",
                            "role": "assistant",
                            "content": [{"type": "output_text", "text": text}],
                        }));
                    }
                    if let Some(tcs) = msg.get("tool_calls").and_then(|t| t.as_array()) {
                        for tc in tcs {
                            let name = tc
                                .pointer("/function/name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            let args = tc
                                .pointer("/function/arguments")
                                .and_then(|v| v.as_str())
                                .unwrap_or("{}");
                            let call_id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                            input.push(json!({
                                "type": "function_call",
                                "name": name,
                                "arguments": args,
                                "call_id": call_id,
                            }));
                        }
                    }
                }
                "tool" => {
                    let call_id = msg
                        .get("tool_call_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    input.push(json!({
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": text,
                    }));
                }
                _ => {}
            }
        }
    }

    // 别名映射：任何含 "spark" 的模型名 → 真 Spark 模型。这样客户端能用 **不含
    // "codex" 的名字**（如 `spark-bridge`）来请求 Spark —— 绕开 hermes 那类「看到模型名
    // 带 codex 就自动路由到原生 codex 账号、无视 base_url」的行为。glance 直接发
    // gpt-5.3-codex-spark 也含 spark，映射到自己，无影响。
    if model.to_lowercase().contains("spark") {
        model = "gpt-5.3-codex-spark".to_string();
    }
    // 有图 → 强制视觉模型（Spark/glm-4.5v 在 codex 端都不支持图片）
    if has_image {
        model = VISION_MODEL.to_string();
    }

    if instructions.trim().is_empty() {
        instructions = "You are a helpful assistant. Use the provided tools when needed.".to_string();
    }

    // chat tools → responses tools（function 平铺，无 function 包裹）
    let mut tools: Vec<Value> = Vec::new();
    if let Some(ts) = chat.get("tools").and_then(|t| t.as_array()) {
        for t in ts {
            if t.get("type").and_then(|v| v.as_str()) == Some("function") {
                if let Some(f) = t.get("function") {
                    tools.push(json!({
                        "type": "function",
                        "name": f.get("name").cloned().unwrap_or(Value::Null),
                        "description": f.get("description").cloned().unwrap_or(Value::String(String::new())),
                        "parameters": f.get("parameters").cloned().unwrap_or(json!({"type":"object","properties":{}})),
                    }));
                }
            }
        }
    }

    json!({
        "model": model,
        "instructions": instructions,
        "input": input,
        "tools": tools,
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        "store": false,
        "stream": true,
        // codex 是推理模型：开 reasoning 它才会规划、该收尾时收尾（不开会无脑探索到上限）。
        "reasoning": { "effort": "medium" },
        "include": ["reasoning.encrypted_content"],
    })
}

/// 把 Codex responses SSE 缓冲流组装成 OpenAI chat/completions JSON。
pub fn responses_sse_to_chat(sse: &str, model: &str) -> Value {
    let mut text = String::new();
    // 兜底：有些响应文本只走 output_text.done / output_item.done(message)，不走 delta。
    let mut done_text = String::new();
    let mut item_text = String::new();
    // function_call 累积：item_id → (name, call_id, arguments)
    let mut fc_args: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut usage = json!({"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0});

    for line in sse.lines() {
        let line = line.trim_start();
        let payload = match line.strip_prefix("data:") {
            Some(p) => p.trim(),
            None => continue,
        };
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        let v: Value = match serde_json::from_str(payload) {
            Ok(v) => v,
            Err(_) => continue,
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("response.output_text.delta") => {
                if let Some(d) = v.get("delta").and_then(|d| d.as_str()) {
                    text.push_str(d);
                }
            }
            // 累积 function_call 参数增量（兜底，万一 done 里 arguments 为空）
            Some("response.function_call_arguments.delta") => {
                if let (Some(item_id), Some(delta)) = (
                    v.get("item_id").and_then(|x| x.as_str()),
                    v.get("delta").and_then(|x| x.as_str()),
                ) {
                    fc_args.entry(item_id.to_string()).or_default().push_str(delta);
                }
            }
            // output_text.done 带整段 text —— 作 delta 的兜底
            Some("response.output_text.done") => {
                if let Some(t) = v.get("text").and_then(|t| t.as_str()) {
                    if !t.is_empty() {
                        done_text.push_str(t);
                    }
                }
            }
            Some("response.output_item.done") => {
                if let Some(item) = v.get("item") {
                    // message item：把 content[] 里的 output_text 抠出来当最终兜底
                    if item.get("type").and_then(|t| t.as_str()) == Some("message") {
                        if let Some(parts) = item.get("content").and_then(|c| c.as_array()) {
                            for p in parts {
                                if let Some(t) = p.get("text").and_then(|x| x.as_str()) {
                                    item_text.push_str(t);
                                }
                            }
                        }
                    }
                    if item.get("type").and_then(|t| t.as_str()) == Some("function_call") {
                        let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
                        let call_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
                        let item_id = item.get("id").and_then(|v| v.as_str()).unwrap_or("");
                        let mut args = item
                            .get("arguments")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        if args.is_empty() {
                            if let Some(acc) = fc_args.get(item_id) {
                                args = acc.clone();
                            }
                        }
                        if args.is_empty() {
                            args = "{}".to_string();
                        }
                        tool_calls.push(json!({
                            "id": call_id,
                            "type": "function",
                            "function": {"name": name, "arguments": args},
                        }));
                    }
                }
            }
            Some("response.completed") => {
                if let Some(u) = v.pointer("/response/usage") {
                    let pull = |k: &str| u.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
                    let inp = pull("input_tokens");
                    let out = pull("output_tokens");
                    let tot = if pull("total_tokens") > 0 {
                        pull("total_tokens")
                    } else {
                        inp + out
                    };
                    usage = json!({
                        "prompt_tokens": inp,
                        "completion_tokens": out,
                        "total_tokens": tot,
                    });
                }
            }
            _ => {}
        }
    }

    // 最终文本：delta 累积优先，其次 output_text.done，再次 message item 的 content
    let final_text = if !text.trim().is_empty() {
        text
    } else if !done_text.trim().is_empty() {
        done_text
    } else {
        item_text
    };

    let finish_reason = if !tool_calls.is_empty() {
        "tool_calls"
    } else {
        "stop"
    };
    let mut message = json!({"role": "assistant"});
    message["content"] = if final_text.is_empty() {
        Value::Null
    } else {
        Value::String(final_text)
    };
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }

    json!({
        "id": "chatcmpl-glance-codex",
        "object": "chat.completion",
        "model": model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason,
        }],
        "usage": usage,
    })
}

/// 从 responses SSE 提取上游错误（非 2xx body 或 SSE error 事件），翻成简短文案。
pub fn extract_upstream_error(raw: &str) -> Option<String> {
    if let Ok(v) = serde_json::from_str::<Value>(raw) {
        if let Some(d) = v.get("detail").and_then(|d| d.as_str()) {
            return Some(d.to_string());
        }
        if let Some(m) = v.pointer("/error/message").and_then(|m| m.as_str()) {
            return Some(m.to_string());
        }
    }
    None
}

/// 把组装好的非流式 chat.completion 转成 Chat Completions SSE（text/event-stream）。
/// 一次性把缓冲好的整条回复拆成几个 chunk 重放，满足 hermes 等只认 SSE delta 的流式客户端。
pub fn chat_completion_to_sse(chat: &Value) -> String {
    let id = chat.get("id").and_then(|v| v.as_str()).unwrap_or("chatcmpl-glance-codex");
    let model = chat.get("model").and_then(|v| v.as_str()).unwrap_or("gpt-5.3-codex-spark");
    let created = chat.get("created").and_then(|v| v.as_u64()).unwrap_or(0);
    let msg = &chat["choices"][0]["message"];
    let finish = chat["choices"][0]
        .get("finish_reason")
        .cloned()
        .unwrap_or(Value::String("stop".into()));

    let chunk = |delta: Value, fin: Value| -> String {
        let v = json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": fin }],
        });
        format!("data: {}\n\n", v)
    };

    let mut out = String::new();
    // 1) 首包：role
    out.push_str(&chunk(json!({ "role": "assistant" }), Value::Null));
    // 2) content（若有）
    if let Some(c) = msg.get("content").and_then(|c| c.as_str()) {
        if !c.is_empty() {
            out.push_str(&chunk(json!({ "content": c }), Value::Null));
        }
    }
    // 3) tool_calls（若有）—— 整条带 index 一次发出
    if let Some(tcs) = msg.get("tool_calls").and_then(|t| t.as_array()) {
        let with_index: Vec<Value> = tcs
            .iter()
            .enumerate()
            .map(|(i, tc)| {
                let mut tc = tc.clone();
                if let Some(obj) = tc.as_object_mut() {
                    obj.insert("index".into(), json!(i));
                }
                tc
            })
            .collect();
        out.push_str(&chunk(json!({ "tool_calls": with_index }), Value::Null));
    }
    // 4) 收尾 + DONE
    out.push_str(&chunk(json!({}), finish));
    out.push_str("data: [DONE]\n\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_system_user_and_tools() {
        let chat = json!({
            "model": "gpt-5.3-codex-spark",
            "messages": [
                {"role": "system", "content": "You are X."},
                {"role": "user", "content": "hi"}
            ],
            "tools": [{"type":"function","function":{"name":"f","description":"d","parameters":{"type":"object"}}}]
        });
        let r = chat_to_responses(&chat, "fallback");
        assert_eq!(r["model"], "gpt-5.3-codex-spark");
        assert_eq!(r["instructions"], "You are X.");
        assert_eq!(r["input"][0]["role"], "user");
        assert_eq!(r["tools"][0]["name"], "f");
        assert_eq!(r["stream"], true);
        assert_eq!(r["reasoning"]["effort"], "medium");
    }

    #[test]
    fn replays_tool_call_and_output() {
        let chat = json!({
            "messages": [
                {"role": "user", "content": "weather?"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"BJ\"}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "25C"}
            ]
        });
        let r = chat_to_responses(&chat, "m");
        let input = r["input"].as_array().unwrap();
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["call_id"], "call_1");
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input[2]["output"], "25C");
    }

    #[test]
    fn image_forces_vision_model_and_input_image() {
        let chat = json!({
            "model": "glm-4.5v",
            "messages": [{"role":"user","content":[
                {"type":"text","text":"图里啥"},
                {"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA"}}
            ]}]
        });
        let r = chat_to_responses(&chat, "gpt-5.3-codex-spark");
        assert_eq!(r["model"], "gpt-5.4-mini"); // 自动切视觉模型
        let content = r["input"][0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "input_text");
        assert_eq!(content[1]["type"], "input_image");
        assert_eq!(content[1]["image_url"], "data:image/png;base64,AAAA");
    }

    #[test]
    fn no_image_keeps_spark() {
        let chat = json!({"model":"gpt-5.3-codex-spark","messages":[{"role":"user","content":"hi"}]});
        let r = chat_to_responses(&chat, "fb");
        assert_eq!(r["model"], "gpt-5.3-codex-spark");
    }

    #[test]
    fn spark_alias_maps_to_real_model() {
        // hermes 用不含 codex 的别名 → 仍映射到真 spark
        let chat = json!({"model":"spark-bridge","messages":[{"role":"user","content":"hi"}]});
        let r = chat_to_responses(&chat, "fb");
        assert_eq!(r["model"], "gpt-5.3-codex-spark");
    }

    #[test]
    fn sse_to_chat_text_via_done_only() {
        // 文本只走 output_text.done（无 delta）→ 仍能解析出 content
        let sse = "data: {\"type\":\"response.output_text.done\",\"text\":\"仅 done 的回复\"}\n\
                   data: {\"type\":\"response.completed\",\"response\":{\"usage\":{}}}\n";
        let c = responses_sse_to_chat(sse, "m");
        assert_eq!(c["choices"][0]["message"]["content"], "仅 done 的回复");
    }

    #[test]
    fn sse_to_chat_text_via_message_item() {
        // 文本只在 output_item.done 的 message content 里
        let sse = "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"item 里的回复\"}]}}\n";
        let c = responses_sse_to_chat(sse, "m");
        assert_eq!(c["choices"][0]["message"]["content"], "item 里的回复");
    }

    #[test]
    fn sse_to_chat_text_and_tool() {
        let sse = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hel\"}\n\
                   data: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\n\
                   data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"name\":\"f\",\"call_id\":\"c1\",\"arguments\":\"{}\"}}\n\
                   data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":2}}}\n";
        let c = responses_sse_to_chat(sse, "m");
        assert_eq!(c["choices"][0]["message"]["content"], "hello");
        assert_eq!(c["choices"][0]["message"]["tool_calls"][0]["function"]["name"], "f");
        assert_eq!(c["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(c["usage"]["total_tokens"], 5);
    }

    #[test]
    fn sse_chunks_carry_content_and_done() {
        let chat = json!({
            "id": "x", "model": "m",
            "choices": [{"index":0,"message":{"role":"assistant","content":"hi there"},"finish_reason":"stop"}]
        });
        let sse = chat_completion_to_sse(&chat);
        assert!(sse.contains("\"role\":\"assistant\""));
        assert!(sse.contains("\"content\":\"hi there\""));
        assert!(sse.contains("\"finish_reason\":\"stop\""));
        assert!(sse.trim_end().ends_with("data: [DONE]"));
    }

    #[test]
    fn sse_chunks_carry_tool_calls_with_index() {
        let chat = json!({
            "id": "x", "model": "m",
            "choices": [{"index":0,"message":{"role":"assistant","content":null,
                "tool_calls":[{"id":"c1","type":"function","function":{"name":"f","arguments":"{}"}}]},
                "finish_reason":"tool_calls"}]
        });
        let sse = chat_completion_to_sse(&chat);
        assert!(sse.contains("\"tool_calls\""));
        assert!(sse.contains("\"index\":0"));
        assert!(sse.contains("\"finish_reason\":\"tool_calls\""));
    }
}
