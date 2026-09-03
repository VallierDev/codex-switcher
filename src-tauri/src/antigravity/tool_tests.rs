use super::translate::*;
use crate::relay_translate;
use serde_json::{json, Value};

const MODEL: &str = "gemini-3.7-flash-high";
fn request() -> Value {
    json!({"model":MODEL,"input":"test","stream":true,"tools":[
        {"type":"namespace","name":"functions","tools":[
            {"type":"custom","name":"exec","description":"Run code","format":{"type":"text"}},
            {"type":"function","name":"probe","parameters":{"type":"object","properties":{}}}
        ]}
    ]})
}
fn setup(req: &Value) -> (Value, relay_translate::TranslatorState) {
    let (bytes, state) =
        responses_to_antigravity(&serde_json::to_vec(req).unwrap(), MODEL, "p").unwrap();
    (serde_json::from_slice(&bytes).unwrap(), state)
}
fn events(bytes: &[u8]) -> Vec<Value> {
    std::str::from_utf8(bytes)
        .unwrap()
        .lines()
        .filter_map(|line| {
            line.strip_prefix("data: ")
                .and_then(|data| serde_json::from_str(data).ok())
        })
        .collect()
}
fn feed(state: &mut relay_translate::TranslatorState, value: Value) -> Vec<u8> {
    let chunk =
        antigravity_sse_event_to_chat_chunk(&serde_json::to_vec(&value).unwrap(), MODEL).unwrap();
    relay_translate::handle_chunk(state, &chunk).concat()
}

#[test]
fn strips_codex_schema_annotations_but_not_parameter_names() {
    let req = json!({"input":"test","tools":[{"type":"function","name":"probe","parameters":{
        "type":"object","properties":{"encrypted":{"type":"string","encrypted":true}},"required":["encrypted"]
    }}]});
    let (wire, _) = setup(&req);
    let schema = &wire["request"]["tools"][0]["functionDeclarations"][0]["parameters"];
    assert!(schema["properties"].get("encrypted").is_some());
    assert!(schema["properties"]["encrypted"].get("encrypted").is_none());
}

#[test]
fn responses_lite_additional_tools_are_real_declarations_not_messages() {
    let mut req = request();
    let tools = req.as_object_mut().unwrap().remove("tools").unwrap();
    req["input"] =
        json!([{"type":"additional_tools","tools":tools},{"role":"user","content":"test"}]);
    let (wire, mut state) = setup(&req);
    assert_eq!(
        wire["request"]["tools"][0]["functionDeclarations"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(wire["request"]["contents"].as_array().unwrap().len(), 1);
    feed(
        &mut state,
        json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"functions__exec","args":{"input":"text(1)"}}}]}}]}),
    );
    assert_eq!(
        events(&finish_codex_stream(&mut state, Some("STOP")))
            .last()
            .unwrap()["response"]["output"][0]["type"],
        "custom_tool_call"
    );
    // Duplicate root / additional declarations must not manufacture another tool.
    req["tools"] = tools.clone();
    let (wire, _) = setup(&req);
    assert_eq!(
        wire["request"]["tools"][0]["functionDeclarations"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn custom_tool_wire_identity_and_lossless_roundtrip() {
    let (wire, mut state) = setup(&request());
    let declaration = &wire["request"]["tools"][0]["functionDeclarations"][0];
    assert_eq!(declaration["name"], "functions__exec");
    assert_eq!(declaration["parameters"]["required"], json!(["input"]));
    let code = "text(\"你好\\\"world\");\nawait tools.probe({});";
    let mut stream = relay_translate::emit_google_created(&mut state);
    stream.extend(feed(&mut state, json!({"response":{"candidates":[{"content":{"parts":[
        {"thoughtSignature":"test-signature","functionCall":{"id":"call_a","name":"functions__exec","args":{"input":code}}}
    ]}}]}})));
    stream.extend(finish_codex_stream(&mut state, Some("STOP")));
    let ev = events(&stream);
    assert!(ev
        .windows(2)
        .all(|pair| pair[0]["sequence_number"].as_u64() < pair[1]["sequence_number"].as_u64()));
    assert!(!ev.iter().any(|e| e["type"]
        .as_str()
        .unwrap()
        .contains("function_call_arguments")));
    let done = ev
        .iter()
        .find(|e| e["type"] == "response.custom_tool_call_input.done")
        .unwrap();
    assert_eq!(done["input"], code);
    let output = ev.last().unwrap()["response"]["output"].as_array().unwrap();
    let call = output
        .iter()
        .find(|v| v["type"] == "custom_tool_call")
        .unwrap();
    assert_eq!(call["name"], "exec");
    assert_eq!(call["namespace"], "functions");
    assert_eq!(call["input"], code);
    assert!(call.get("arguments").is_none());
    // Simulate Codex serialization dropping provider-specific extension fields.
    let mut history = vec![json!({"role":"user","content":"test"})];
    for item in output {
        let mut item = item.clone();
        item.as_object_mut().unwrap().remove("thought_signature");
        history.push(item);
    }
    history.push(json!({"type":"custom_tool_call_output","call_id":"call_a","output":"TOOL_OK"}));
    let mut next = request();
    next["input"] = json!(history);
    let (wire, _) = setup(&next);
    let parts: Vec<&Value> = wire["request"]["contents"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|m| m["parts"].as_array().unwrap())
        .collect();
    let call = parts
        .iter()
        .find(|p| p.get("functionCall").is_some())
        .unwrap();
    assert_eq!(call["functionCall"]["args"]["input"], code);
    assert_eq!(call["thoughtSignature"], "test-signature");
    let result = parts
        .iter()
        .find(|p| p.get("functionResponse").is_some())
        .unwrap();
    assert_eq!(result["functionResponse"]["name"], "functions__exec");
    assert_eq!(result["functionResponse"]["id"], "call_a");
    assert_eq!(result["functionResponse"]["response"]["output"], "TOOL_OK");
}

#[test]
fn separate_sse_frames_do_not_merge_tools_at_index_zero() {
    let (_, mut state) = setup(&request());
    for (id, name, args) in [
        ("a", "functions__exec", json!({"input":"one"})),
        ("b", "functions__probe", json!({})),
    ] {
        feed(
            &mut state,
            json!({"candidates":[{"content":{"parts":[{"functionCall":{"id":id,"name":name,"args":args}}]}}]}),
        );
    }
    let ev = events(&finish_codex_stream(&mut state, Some("STOP")));
    let output = ev.last().unwrap()["response"]["output"].as_array().unwrap();
    assert_eq!(output.len(), 2);
    assert_eq!(output[0]["type"], "custom_tool_call");
    assert_eq!(output[1]["type"], "function_call");
    assert_eq!(output[1]["namespace"], "functions");
    assert!(ev
        .iter()
        .any(|e| e["type"] == "response.function_call_arguments.done"));
}

#[test]
fn fragment_custom_input_and_reject_invalid_payload() {
    let (_, mut state) = setup(&request());
    for (i, args) in ["{\"input\":\"你", "好\\n\"}"].into_iter().enumerate() {
        let mut tc = json!({"index":0,"function":{"arguments":args}});
        if i == 0 {
            tc["id"] = json!("a");
            tc["function"]["name"] = json!("functions__exec");
        }
        let chunk = format!(
            "data: {}\n\n",
            json!({"choices":[{"delta":{"tool_calls":[tc]}}]})
        );
        let ev = events(&relay_translate::handle_chunk(&mut state, chunk.as_bytes()).concat());
        assert!(!ev
            .iter()
            .any(|e| e["type"] == "response.function_call_arguments.delta"));
    }
    let ev = events(&finish_codex_stream(&mut state, Some("STOP")));
    assert_eq!(
        ev.last().unwrap()["response"]["output"][0]["input"],
        "你好\n"
    );
    let (_, mut state) = setup(&request());
    feed(
        &mut state,
        json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"functions__exec","args":{}}}]}}]}),
    );
    let ev = events(&finish_codex_stream(&mut state, Some("STOP")));
    assert_eq!(ev.last().unwrap()["type"], "response.failed");
}

#[test]
fn clean_eof_with_complete_tool_and_usage_is_supported() {
    let (_, mut state) = setup(&request());
    feed(
        &mut state,
        json!({"response":{"candidates":[{"content":{"parts":[{"functionCall":{"name":"functions__exec","args":{"input":"text(1)"}}}]}}],"usageMetadata":{"totalTokenCount":20}}}),
    );
    assert_eq!(
        events(&finish_codex_stream(&mut state, None))
            .last()
            .unwrap()["type"],
        "response.completed"
    );
}

#[test]
fn sync_custom_tools_use_same_carrier_and_validation_as_streaming() {
    let (_, mut state) = setup(&request());
    let raw = json!({"candidates":[{"finishReason":"STOP","content":{"parts":[{"thoughtSignature":"sig", "functionCall":{"id":"a","name":"functions__exec","args":{"input":"code"}}}]}}],"usageMetadata":{"totalTokenCount":20}});
    let output =
        antigravity_response_to_codex(&serde_json::to_vec(&raw).unwrap(), &mut state, MODEL, false)
            .unwrap();
    let output: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(output["status"], "completed");
    assert!(output["output"]
        .as_array()
        .unwrap()
        .iter()
        .any(|i| i["type"] == "custom_tool_call" && i["input"] == "code"));
    assert!(output["output"]
        .as_array()
        .unwrap()
        .iter()
        .any(|i| i["type"] == "reasoning"
            && i["encrypted_content"]
                .as_str()
                .unwrap()
                .starts_with("switcher-google-v1:")));
}

#[test]
fn abnormal_finishes_never_become_success() {
    for raw in [
        json!({"error":{"status":"INTERNAL"}}),
        json!({"candidates":[{"finishReason":"MALFORMED_FUNCTION_CALL"}]}),
        json!({"promptFeedback":{"blockReason":"SAFETY"}}),
    ] {
        assert!(inspect_stream_event(&serde_json::to_vec(&raw).unwrap()).is_err());
    }
    assert!(inspect_stream_event(b"{broken").is_err());
    for (finish, event) in [
        (None, "response.failed"),
        (Some("STOP"), "response.failed"),
        (Some("MAX_TOKENS"), "response.incomplete"),
    ] {
        let (_, mut state) = setup(&request());
        feed(
            &mut state,
            json!({"candidates":[{"content":{"parts":[{"thought":true,"text":"thinking"}]}}]}),
        );
        assert_eq!(
            events(&finish_codex_stream(&mut state, finish))
                .last()
                .unwrap()["type"],
            event
        );
    }
}

#[test]
fn tool_choice_uses_wire_names_without_prompt_injection() {
    for (choice, mode, names) in [
        (json!("none"), "NONE", Value::Null),
        (json!("required"), "ANY", Value::Null),
        (
            json!({"type":"custom","name":"exec","namespace":"functions"}),
            "ANY",
            json!(["functions__exec"]),
        ),
        (
            json!({"type":"allowed_tools","mode":"required","tools":[{"name":"probe","namespace":"functions"}]}),
            "ANY",
            json!(["functions__probe"]),
        ),
    ] {
        let mut req = request();
        req["tool_choice"] = choice;
        let (wire, _) = setup(&req);
        assert_eq!(
            wire["request"]["toolConfig"]["functionCallingConfig"]["mode"],
            mode
        );
        assert_eq!(
            wire["request"]["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"],
            names
        );
        assert!(wire["request"].get("systemInstruction").is_none());
    }
}

#[test]
fn relay_default_is_not_opted_into_google_custom_events() {
    let (_, mut state) =
        relay_translate::translate_request(&serde_json::to_vec(&request()).unwrap(), "relay")
            .unwrap();
    let bytes = feed(
        &mut state,
        json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"exec","args":{"input":"text(1)"}}}]}}]}),
    );
    assert!(events(&bytes)
        .iter()
        .any(|e| e["type"] == "response.function_call_arguments.delta"));
}
