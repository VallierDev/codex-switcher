//! End-to-end-ish integration test for the GLM translator.
//!
//! Strategy: instead of booting the whole Tauri proxy (heavy, requires
//! AppHandle), boot a tiny TCP server that pretends to be GLM's
//! `/chat/completions` endpoint and emits a hand-written SSE stream, then run
//! the translator pipeline against it the same way `proxy.rs` does. This is
//! the closest we can get without hitting GLM for real.

use std::io::Write;
use std::net::TcpListener;
use std::time::Duration;

use codex_switcher_lib::relay_translate::{self, ChatSseBuffer, ChatSseEvent};

fn spawn_mock_chat_completions_server(sse_body: &'static str) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.set_nonblocking(false).ok();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (mut stream, _) = match listener.accept() {
            Ok(p) => p,
            Err(_) => return,
        };
        // Read until end of headers (\r\n\r\n) — minimal HTTP/1.1 parsing.
        let mut buf = [0u8; 4096];
        let _ = std::io::Read::read(&mut stream, &mut buf);
        let header = "HTTP/1.1 200 OK\r\n\
                     Content-Type: text/event-stream; charset=utf-8\r\n\
                     Cache-Control: no-cache\r\n\
                     Connection: close\r\n\r\n";
        let _ = stream.write_all(header.as_bytes());
        let _ = stream.write_all(sse_body.as_bytes());
        let _ = stream.flush();
    });
    port
}

#[test]
fn translator_request_to_chat_completions_then_back_to_responses_sse() {
    // Mock GLM SSE: 3 content chunks then [DONE]
    let sse_body = "\
data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"delta\":{\"content\":\"hi \"}}]}\n\n\
data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"delta\":{\"content\":\"there\"}}]}\n\n\
data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"delta\":{\"content\":\"!\"}}]}\n\n\
data: [DONE]\n\n";
    let port = spawn_mock_chat_completions_server(sse_body);

    // Step 1: codex CLI body → translator → chat body
    let codex_body = serde_json::json!({
        "model": "gpt-5",
        "instructions": "be terse",
        "input": "hi",
        "stream": true,
    });
    let bytes = serde_json::to_vec(&codex_body).unwrap();
    let (chat_body, mut state) = relay_translate::translate_request(&bytes, "glm-5.1").unwrap();
    assert!(state.stream_requested);
    assert_eq!(state.model, "glm-5.1");

    // Step 2: send chat body to mock /chat/completions endpoint with reqwest
    let url = format!("http://127.0.0.1:{}/chat/completions", port);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let collected: Vec<u8> = rt.block_on(async {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let resp = client
            .post(&url)
            .header("Authorization", "Bearer test")
            .header("Content-Type", "application/json")
            .body(chat_body)
            .send()
            .await
            .expect("mock chat completions returned");
        assert_eq!(resp.status(), 200);
        // Step 3: collect upstream SSE bytes
        resp.bytes().await.unwrap().to_vec()
    });

    // Step 4: feed bytes through ChatSseBuffer + translator → Responses SSE bytes
    let mut buf = ChatSseBuffer::new();
    buf.push(&collected);
    let evts = buf.drain_events();

    let mut all_out: Vec<u8> = Vec::new();
    all_out.extend(relay_translate::emit_created(&state));
    let mut saw_done = false;
    for e in evts {
        match e {
            ChatSseEvent::Done => {
                saw_done = true;
                break;
            }
            ChatSseEvent::Data(payload) => {
                for tev in relay_translate::handle_chunk(&mut state, &payload) {
                    all_out.extend(tev);
                }
            }
        }
    }
    all_out.extend(relay_translate::emit_completed(&mut state));
    assert!(saw_done, "mock stream emitted [DONE]");

    let blob = String::from_utf8_lossy(&all_out);
    assert!(blob.contains("event: response.created"));
    assert!(blob.contains("event: response.output_item.added"));
    let n_deltas = blob.matches("event: response.output_text.delta").count();
    assert_eq!(n_deltas, 3, "one delta per upstream content chunk");
    assert!(blob.contains("event: response.output_item.done"));
    assert!(blob.contains("event: response.completed"));
    // Final accumulated text appears in the close
    assert!(
        blob.contains("hi there!"),
        "the completed event must carry the joined content"
    );
}

#[test]
fn translator_tolerates_chained_request() {
    let codex_body = serde_json::json!({
        "model": "gpt-5",
        "input": "hi",
        "previous_response_id": "resp_old",
    });
    let bytes = serde_json::to_vec(&codex_body).unwrap();
    let (translated, _state) = relay_translate::translate_request(&bytes, "glm-5.1").unwrap();
    let translated: serde_json::Value = serde_json::from_slice(&translated).unwrap();
    assert!(translated.get("previous_response_id").is_none());
}

#[test]
fn synthetic_models_endpoint_response() {
    let body = relay_translate::synthetic_models_response("glm-5.1");
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["object"], "list");
    let arr = v["data"].as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"], "glm-5.1");
}
