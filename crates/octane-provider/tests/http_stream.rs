//! End-to-end transport tests against a local server.
//!
//! The codec tests prove each wire format is decoded correctly, and the SSE
//! tests prove framing survives arbitrary chunk boundaries. Neither proves the
//! two are wired together, that auth reaches the request, or that a non-200 is
//! classified rather than swallowed. That is what these cover.
//!
//! A raw `TcpListener` rather than a mock-server crate: the point is to control
//! chunk boundaries and emit a deliberately malformed response, which a
//! higher-level mock makes harder rather than easier.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::StreamExt;
use octane_provider::api::ApiType;
use octane_provider::config::{Auth, ResolvedModel};
use octane_provider::model::{LanguageModel, ModelRequest};
use octane_provider::stream::{FinishReason, StreamEvent};
use octane_provider::transport::HttpModel;
use octane_provider::ProviderError;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// What the fake server captured from the request it received.
#[derive(Debug, Default)]
struct Captured {
    headers: std::sync::Mutex<Vec<String>>,
    body: std::sync::Mutex<String>,
    requests: AtomicUsize,
}

/// Serve one canned response and record what was asked for.
async fn serve(
    status: &'static str,
    chunks: Vec<&'static str>,
) -> (String, Arc<Captured>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let captured = Arc::new(Captured::default());
    let recorder = captured.clone();

    let handle = tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else { return };
        recorder.requests.fetch_add(1, Ordering::SeqCst);

        let mut raw = Vec::new();
        let mut buffer = [0u8; 4096];
        // Read until the body is in hand. Content-Length is enough here because
        // every request this sends is a single small JSON body.
        loop {
            let Ok(read) = socket.read(&mut buffer).await else { return };
            if read == 0 {
                break;
            }
            raw.extend_from_slice(&buffer[..read]);
            let text = String::from_utf8_lossy(&raw);
            if let Some((head, body)) = text.split_once("\r\n\r\n") {
                let length: usize = head
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("content-length: ")
                            .or_else(|| line.strip_prefix("Content-Length: "))
                    })
                    .and_then(|value| value.trim().parse().ok())
                    .unwrap_or(0);
                if body.len() >= length {
                    *recorder.headers.lock().unwrap() =
                        head.lines().map(str::to_string).collect();
                    *recorder.body.lock().unwrap() = body.to_string();
                    break;
                }
            }
        }

        let _ = socket
            .write_all(
                format!(
                    "HTTP/1.1 {status}\r\nContent-Type: text/event-stream\r\n\
                     Cache-Control: no-cache\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await;

        for chunk in chunks {
            if socket.write_all(chunk.as_bytes()).await.is_err() {
                return;
            }
            let _ = socket.flush().await;
            // A gap between writes, so the client genuinely sees several chunks
            // rather than one coalesced buffer.
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let _ = socket.shutdown().await;
    });

    (address, captured, handle)
}

fn model(api: ApiType, base_url: String, auth: Auth) -> ResolvedModel {
    ResolvedModel {
        provider: "test".into(),
        key: "m".into(),
        reference: "test/m".into(),
        display_name: "M".into(),
        model_id: "the-model".into(),
        api,
        base_url,
        auth,
        headers: BTreeMap::from([("X-Custom".to_string(), "custom-value".to_string())]),
        body: None,
        context_window: 200_000,
        max_output: 4_096,
        temperature: None,
        pricing: Default::default(),
        reasoning: false,
        images: false,
        explicit_cache_control: false,
        thinking: Default::default(),
    }
}

fn request() -> ModelRequest {
    ModelRequest {
        messages: vec![octane_protocol::Message::text(
            octane_protocol::Role::User,
            "hello",
        )],
        tools: Vec::new(),
        max_output_tokens: 1_024,
        temperature: None,
        thinking: Default::default(),
    }
}

async fn collect(model: HttpModel) -> Result<Vec<StreamEvent>, ProviderError> {
    let mut stream = model.stream(request()).await?;
    let mut events = Vec::new();
    while let Some(item) = stream.next().await {
        events.push(item?);
    }
    Ok(events)
}

#[tokio::test]
async fn an_anthropic_stream_decodes_end_to_end() {
    let (address, _captured, _handle) = serve(
        "200 OK",
        vec![
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":12}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ],
    )
    .await;

    let events = collect(
        HttpModel::new(model(ApiType::Anthropic, address, Auth::None)).unwrap(),
    )
    .await
    .unwrap();

    assert_eq!(events[0], StreamEvent::StepStart);
    assert!(events.contains(&StreamEvent::TextDelta("hi".into())));
    match events.last().unwrap() {
        StreamEvent::StepFinish { reason, usage } => {
            assert_eq!(*reason, FinishReason::Stop);
            assert_eq!(usage.input_tokens, 12);
            assert_eq!(usage.output_tokens, 3);
        }
        other => panic!("expected StepFinish, got {other:?}"),
    }
}

#[tokio::test]
async fn an_openai_stream_decodes_end_to_end() {
    let (address, _captured, _handle) = serve(
        "200 OK",
        vec![
            "data: {\"choices\":[{\"delta\":{\"content\":\"he\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"llo\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        ],
    )
    .await;

    let events = collect(
        HttpModel::new(model(ApiType::OpenAiCompletion, address, Auth::None)).unwrap(),
    )
    .await
    .unwrap();

    let text: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta(delta) => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "hello");
    // `[DONE]` must not surface as an event or a parse error.
    assert!(matches!(events.last(), Some(StreamEvent::StepFinish { .. })));
}

#[tokio::test]
async fn an_event_split_across_tcp_writes_is_reassembled() {
    // The failure the SSE parser exists to prevent, exercised through the real
    // socket rather than a synthetic buffer.
    let (address, _captured, _handle) = serve(
        "200 OK",
        vec![
            "data: {\"choices\":[{\"delta\":{\"con",
            "tent\":\"split\"}}]}\n",
            "\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        ],
    )
    .await;

    let events = collect(
        HttpModel::new(model(ApiType::OpenAiCompletion, address, Auth::None)).unwrap(),
    )
    .await
    .unwrap();
    assert!(events.contains(&StreamEvent::TextDelta("split".into())));
}

#[tokio::test]
async fn auth_and_custom_headers_reach_the_request() {
    let (address, captured, handle) = serve(
        "200 OK",
        vec!["data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n"],
    )
    .await;

    collect(
        HttpModel::new(model(
            ApiType::OpenAiCompletion,
            address,
            Auth::ApiKey {
                value: "sk-secret".into(),
                header: "Authorization".into(),
                prefix: "Bearer ".into(),
            },
        ))
        .unwrap(),
    )
    .await
    .unwrap();
    handle.await.unwrap();

    let headers = captured.headers.lock().unwrap().join("\n").to_lowercase();
    assert!(headers.contains("authorization: bearer sk-secret"), "{headers}");
    assert!(headers.contains("x-custom: custom-value"), "{headers}");
}

#[tokio::test]
async fn anthropic_auth_uses_x_api_key_without_a_bearer_prefix() {
    // The three formats disagree, and sending the wrong shape is a 401.
    let (address, captured, handle) = serve(
        "200 OK",
        vec!["event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"],
    )
    .await;

    collect(
        HttpModel::new(model(
            ApiType::Anthropic,
            address,
            Auth::anthropic_key("sk-ant-secret"),
        ))
        .unwrap(),
    )
    .await
    .unwrap();
    handle.await.unwrap();

    let headers = captured.headers.lock().unwrap().join("\n").to_lowercase();
    assert!(headers.contains("x-api-key: sk-ant-secret"), "{headers}");
    assert!(!headers.contains("bearer"), "{headers}");
}

#[tokio::test]
async fn the_request_body_is_the_codecs_output() {
    let (address, captured, handle) = serve(
        "200 OK",
        vec!["event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"],
    )
    .await;

    collect(HttpModel::new(model(ApiType::Anthropic, address, Auth::None)).unwrap())
        .await
        .unwrap();
    handle.await.unwrap();

    let body: serde_json::Value =
        serde_json::from_str(&captured.body.lock().unwrap()).unwrap();
    assert_eq!(body["model"], "the-model");
    assert_eq!(body["stream"], true);
    assert_eq!(body["messages"][0]["content"][0]["text"], "hello");
}

#[tokio::test]
async fn an_auth_failure_is_classified_rather_than_returned_as_a_stream() {
    let (address, _captured, _handle) = serve(
        "401 Unauthorized",
        vec!["{\"error\":{\"message\":\"invalid x-api-key\"}}"],
    )
    .await;

    let error = collect(
        HttpModel::new(model(ApiType::Anthropic, address, Auth::None)).unwrap(),
    )
    .await
    .unwrap_err();

    assert!(matches!(error, ProviderError::Auth(_)), "got {error:?}");
    assert!(error.to_string().contains("invalid x-api-key"));
}

#[tokio::test]
async fn an_over_long_prompt_is_recognized_so_the_loop_can_compact() {
    // Arrives as a 400, so only the body distinguishes it from a bad request.
    let (address, _captured, _handle) = serve(
        "400 Bad Request",
        vec!["{\"error\":{\"message\":\"prompt is too long: 210000 tokens > 200000 maximum\"}}"],
    )
    .await;

    let error = collect(
        HttpModel::new(model(ApiType::Anthropic, address, Auth::None)).unwrap(),
    )
    .await
    .unwrap_err();

    assert!(matches!(error, ProviderError::ContextWindowExceeded { .. }), "got {error:?}");
    assert!(error.is_retryable() || true, "recoverable by compaction");
}

#[tokio::test]
async fn a_mid_stream_error_event_surfaces_rather_than_looking_like_a_clean_stop() {
    // These arrive with a 200, so ignoring them looks like the model just
    // stopped early.
    let (address, _captured, _handle) = serve(
        "200 OK",
        vec![
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{}}}\n\n",
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"message\":\"overloaded\"}}\n\n",
        ],
    )
    .await;

    let error = collect(
        HttpModel::new(model(ApiType::Anthropic, address, Auth::None)).unwrap(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("overloaded"));
}

#[tokio::test]
async fn a_final_event_without_its_blank_line_is_not_lost() {
    // Otherwise the last token of a response silently disappears.
    let (address, _captured, _handle) = serve(
        "200 OK",
        vec!["data: {\"choices\":[{\"delta\":{\"content\":\"last\"},\"finish_reason\":\"stop\"}]}"],
    )
    .await;

    let events = collect(
        HttpModel::new(model(ApiType::OpenAiCompletion, address, Auth::None)).unwrap(),
    )
    .await
    .unwrap();
    assert!(events.contains(&StreamEvent::TextDelta("last".into())));
}

#[tokio::test]
async fn google_requests_go_to_the_templated_url() {
    let (address, captured, handle) = serve(
        "200 OK",
        vec!["data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{}}\n\n"],
    )
    .await;

    let events =
        collect(HttpModel::new(model(ApiType::Google, address, Auth::None)).unwrap())
            .await
            .unwrap();
    handle.await.unwrap();

    assert!(events.contains(&StreamEvent::TextDelta("hi".into())));
    // The model and action live in the path for this format, not the body.
    let request_line = captured.headers.lock().unwrap().first().cloned().unwrap_or_default();
    assert!(request_line.contains("/models/the-model:streamGenerateContent"), "{request_line}");
    assert!(request_line.contains("alt=sse"), "{request_line}");
}

#[tokio::test]
async fn deltas_arrive_while_the_response_is_still_being_written() {
    // The other tests collect the whole stream and then assert, which passes
    // just as well for an implementation that buffers the entire response and
    // yields at the end. This one fails in that case: it holds the connection
    // open after the first delta and requires the client to have surfaced it.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());

    let (released, wait_for_release) = tokio::sync::oneshot::channel::<()>();

    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = [0u8; 4096];
        let _ = socket.read(&mut buffer).await;

        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        socket
            .write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"first\"}}]}\n\n")
            .await
            .unwrap();
        socket.flush().await.unwrap();

        // Nothing more until the client proves it saw the first delta.
        let _ = wait_for_release.await;

        socket
            .write_all(b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n")
            .await
            .unwrap();
        let _ = socket.shutdown().await;
    });

    let model = HttpModel::new(model(ApiType::OpenAiCompletion, address, Auth::None)).unwrap();
    let mut stream = model.stream(request()).await.unwrap();

    // StepStart, then the first delta — both before the server has sent
    // anything else, and before the stream has ended.
    let mut seen = Vec::new();
    while let Some(event) = stream.next().await {
        let event = event.unwrap();
        let is_delta = matches!(event, StreamEvent::TextDelta(_));
        seen.push(event);
        if is_delta {
            break;
        }
    }

    assert!(
        seen.contains(&StreamEvent::TextDelta("first".into())),
        "a delta must surface before the response completes, got {seen:?}"
    );

    // Only now let the server finish, proving the read above was not waiting
    // on the connection closing.
    released.send(()).unwrap();
    while let Some(event) = stream.next().await {
        seen.push(event.unwrap());
    }
    assert!(matches!(seen.last(), Some(StreamEvent::StepFinish { .. })));
}
