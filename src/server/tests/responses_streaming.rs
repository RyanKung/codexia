use super::*;

#[tokio::test]
async fn responses_returns_response_object() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    store
        .save(&Credentials {
            provider: crate::config::Provider::Codex,
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: now_unix() + 600,
            account_id: "acc_old".into(),
        })
        .unwrap();
    let http = Client::new();
    let state = AppState::new(
        TokenManager::new(
            store,
            CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
        ),
        CodexClient::new(http, spawn_codex_server(false).await),
        Some("secret".into()),
        ModelList::from_ids(["gpt-5.5"]),
    );
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static("secret"));

    let response = handlers::responses(
        axum::extract::State(state.clone()),
        headers.clone(),
        Json(
            serde_json::from_value(json!({
                "model": "gpt-5.5",
                "input": "draw a cat",
                "tools": [{"type":"image_generation","size":"1024x1024"}],
                "tool_choice": {"type":"image_generation"}
            }))
            .unwrap(),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice::<Value>(&body).unwrap();
    assert_eq!(value["object"], "response");
    assert_eq!(value["status"], "completed");
    assert_eq!(value["output"][0]["type"], "message");
    assert_eq!(value["text"]["format"]["type"], "text");
    assert_eq!(value["truncation"], "disabled");
    assert!(value["top_p"].is_null());
    assert!(value["reasoning"]["effort"].is_null());
}

#[tokio::test]
async fn responses_stream_image_generation_passthroughs_upstream_events() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    store
        .save(&Credentials {
            provider: crate::config::Provider::Codex,
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: now_unix() + 600,
            account_id: "acc_old".into(),
        })
        .unwrap();
    let http = Client::new();
    let state = AppState::new(
        TokenManager::new(
            store,
            CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
        ),
        CodexClient::new(http, spawn_image_stream_codex_server().await),
        Some("secret".into()),
        ModelList::from_ids(["gpt-5.5"]),
    );
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static("secret"));

    let response = handlers::responses(
        axum::extract::State(state),
        headers,
        Json(
            serde_json::from_value(json!({
                "model": "gpt-5.5",
                "stream": true,
                "input": "draw a cat",
                "tools": [{"type":"image_generation","size":"1024x1024"}],
                "tool_choice": {"type":"image_generation"}
            }))
            .unwrap(),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("event: response.created"));
    assert!(text.contains("event: response.completed"));
    assert!(text.contains("\"type\":\"image_generation_call\""));
    assert!(text.contains("\"result\":\"YWJj\""));
}

#[tokio::test]
async fn responses_stream_emits_text_output_item_lifecycle() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    store
        .save(&Credentials {
            provider: crate::config::Provider::Codex,
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: now_unix() + 600,
            account_id: "acc_old".into(),
        })
        .unwrap();
    let http = Client::new();
    let state = AppState::new(
        TokenManager::new(
            store,
            CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
        ),
        CodexClient::new(http, spawn_cache_usage_stream_codex_server().await),
        Some("secret".into()),
        ModelList::from_ids(["gpt-5.5"]),
    );
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static("secret"));

    let response = handlers::responses(
        axum::extract::State(state),
        headers,
        Json(
            serde_json::from_value(json!({
                "model": "gpt-5.5",
                "stream": true,
                "input": "hello"
            }))
            .unwrap(),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert_stream_events_in_order(
        &text,
        &[
            "event: response.created",
            "event: response.in_progress",
            "event: response.output_item.added",
            "event: response.content_part.added",
            "event: response.output_text.delta",
            "event: response.output_text.done",
            "event: response.content_part.done",
            "event: response.output_item.done",
            "event: response.completed",
        ],
    );
    assert!(text.contains("\"type\":\"message\""));
    assert!(text.contains("\"role\":\"assistant\""));
    assert!(text.contains("\"status\":\"in_progress\""));
    assert!(text.contains("\"status\":\"completed\""));
    assert!(text.contains("\"text\":\"OK\""));
}

#[tokio::test]
async fn responses_stream_tool_call_emits_function_call_lifecycle() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    store
        .save(&Credentials {
            provider: crate::config::Provider::Codex,
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: now_unix() + 600,
            account_id: "acc_old".into(),
        })
        .unwrap();
    let http = Client::new();
    let state = AppState::new(
        TokenManager::new(
            store,
            CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
        ),
        CodexClient::new(http, spawn_codex_server(true).await),
        Some("secret".into()),
        ModelList::from_ids(["gpt-5.5"]),
    );
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static("secret"));

    let response = handlers::responses(
        axum::extract::State(state),
        headers,
        Json(
            serde_json::from_value(json!({
                "model": "gpt-5.5",
                "stream": true,
                "input": "look up x"
            }))
            .unwrap(),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert_stream_events_in_order(
        &text,
        &[
            "event: response.created",
            "event: response.in_progress",
            "event: response.output_item.added",
            "event: response.function_call_arguments.delta",
            "event: response.function_call_arguments.done",
            "event: response.output_item.done",
            "event: response.completed",
        ],
    );
    assert!(text.contains("\"type\":\"function_call\""));
    assert!(text.contains("\"name\":\"lookup\""));
    assert!(text.contains("\"arguments\":\"{\\\"q\\\":\\\"x\\\"}\""));
    let done_frame = sse_frame(&text, "event: response.function_call_arguments.done");
    assert!(done_frame.contains("\"name\":\"lookup\""));
    assert!(!text.contains("\"finish_reason\""));
    assert!(!text.contains("event: response.content_part.added"));
    assert!(!text.contains("\"type\":\"message\""));
}

#[tokio::test]
async fn responses_raw_stream_backfills_null_terminal_output() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    store
        .save(&Credentials {
            provider: crate::config::Provider::Codex,
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: now_unix() + 600,
            account_id: "acc_old".into(),
        })
        .unwrap();
    let http = Client::new();
    let state = AppState::new(
        TokenManager::new(
            store,
            CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
        ),
        CodexClient::new(http, spawn_null_output_stream_codex_server().await),
        Some("secret".into()),
        ModelList::from_ids(["gpt-5.5"]),
    );
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static("secret"));

    let response = handlers::responses(
        axum::extract::State(state),
        headers,
        Json(
            serde_json::from_value(json!({
                "model": "gpt-5.5",
                "stream": true,
                "input": "hello",
                "tools": [{"type":"image_generation","size":"1024x1024"}],
                "tool_choice": {"type":"image_generation"}
            }))
            .unwrap(),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("event: response.completed"));
    assert!(!text.contains("\"output\":null"));
    assert!(text.contains("\"output\":["));
    assert!(text.contains("\"type\":\"message\""));
    assert!(text.contains("\"id\":\"msg_1\""));
    assert!(text.contains("\"text\":\"OK\""));
}

#[tokio::test]
async fn responses_raw_stream_completes_incomplete_result_with_output() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    store
        .save(&Credentials {
            provider: crate::config::Provider::Codex,
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: now_unix() + 600,
            account_id: "acc_old".into(),
        })
        .unwrap();
    let http = Client::new();
    let state = AppState::new(
        TokenManager::new(
            store,
            CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
        ),
        CodexClient::new(http, spawn_incomplete_result_stream_codex_server().await),
        Some("secret".into()),
        ModelList::from_ids(["gpt-5.5"]),
    );
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static("secret"));

    let response = handlers::responses(
        axum::extract::State(state),
        headers,
        Json(
            serde_json::from_value(json!({
                "model": "gpt-5.5",
                "stream": true,
                "input": "hello",
                "tools": [{"type":"image_generation","size":"1024x1024"}],
                "tool_choice": {"type":"image_generation"}
            }))
            .unwrap(),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("event: response.completed"));
    assert!(!text.contains("event: response.incomplete"));
    assert!(!text.contains("\"type\":\"response.incomplete\""));
    assert!(text.contains("\"status\":\"completed\""));
    assert!(!text.contains("\"incomplete_result\""));
    assert!(text.contains("\"text\":\"OK\""));
}

#[tokio::test]
async fn responses_chat_stream_completes_incomplete_result_without_finish_reason() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    store
        .save(&Credentials {
            provider: crate::config::Provider::Codex,
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: now_unix() + 600,
            account_id: "acc_old".into(),
        })
        .unwrap();
    let http = Client::new();
    let state = AppState::new(
        TokenManager::new(
            store,
            CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
        ),
        CodexClient::new(http, spawn_incomplete_result_stream_codex_server().await),
        Some("secret".into()),
        ModelList::from_ids(["gpt-5.5"]),
    );
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static("secret"));

    let response = handlers::responses(
        axum::extract::State(state),
        headers,
        Json(
            serde_json::from_value(json!({
                "model": "gpt-5.5",
                "stream": true,
                "input": "hello"
            }))
            .unwrap(),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("event: response.completed"));
    assert!(!text.contains("\"finish_reason\""));
    assert!(!text.contains("\"incomplete_result\""));
    assert!(text.contains("\"text\":\"OK\""));
}

#[tokio::test]
async fn responses_non_streaming_still_streams_upstream() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    store
        .save(&Credentials {
            provider: crate::config::Provider::Codex,
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: now_unix() + 600,
            account_id: "acc_old".into(),
        })
        .unwrap();
    let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
    let http = Client::new();
    let state = AppState::new(
        TokenManager::new(
            store,
            CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
        ),
        CodexClient::new(
            http,
            spawn_stream_required_codex_server(captured.clone()).await,
        ),
        Some("secret".into()),
        ModelList::from_ids(["gpt-5.5"]),
    );
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static("secret"));

    let response = handlers::responses(
        axum::extract::State(state),
        headers,
        Json(
            serde_json::from_value(json!({
                "model": "gpt-5.5",
                "stream": false,
                "input": "hello"
            }))
            .unwrap(),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice::<Value>(&body).unwrap();
    assert_eq!(value["object"], "response");
    assert_eq!(value["status"], "completed");
    assert_eq!(value["output"][0]["type"], "message");
    let (captured_len, captured_stream) = {
        let captured = captured.lock().await;
        (captured.len(), captured[0]["stream"].clone())
    };
    assert_eq!(captured_len, 1);
    assert_eq!(captured_stream, true);
}

#[tokio::test]
async fn messages_stream_image_generation_returns_anthropic_image_block_events() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    store
        .save(&Credentials {
            provider: crate::config::Provider::Codex,
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: now_unix() + 600,
            account_id: "acc_old".into(),
        })
        .unwrap();
    let http = Client::new();
    let state = AppState::new(
        TokenManager::new(
            store,
            CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
        ),
        CodexClient::new(http, spawn_image_stream_codex_server().await),
        Some("secret".into()),
        ModelList::from_ids(["gpt-5.5"]),
    );
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static("secret"));

    let response = handlers::messages(
        axum::extract::State(state),
        headers,
        Json(
            serde_json::from_value(json!({
                "model": "gpt-5.5",
                "stream": true,
                "max_tokens": 256,
                "messages": [{"role": "user", "content": "draw a cat"}],
                "tools": [{"name":"image_generation"}]
            }))
            .unwrap(),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("event: message_start"));
    assert!(text.contains("event: content_block_start"));
    assert!(text.contains("\"type\":\"image\""));
    assert!(text.contains("\"media_type\":\"image/png\""));
    assert!(text.contains("\"data\":\"YWJj\""));
    assert!(text.contains("event: message_stop"));
}

#[tokio::test]
async fn messages_stream_tool_use_returns_anthropic_tool_events() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    store
        .save(&Credentials {
            provider: crate::config::Provider::Codex,
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: now_unix() + 600,
            account_id: "acc_old".into(),
        })
        .unwrap();
    let http = Client::new();
    let state = AppState::new(
        TokenManager::new(
            store,
            CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
        ),
        CodexClient::new(http, spawn_tool_stream_codex_server().await),
        Some("secret".into()),
        ModelList::from_ids(["gpt-5.5"]),
    );
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static("secret"));

    let response = handlers::messages(
        axum::extract::State(state),
        headers,
        Json(
            serde_json::from_value(json!({
                "model": "gpt-5.5",
                "stream": true,
                "max_tokens": 256,
                "messages": [{"role": "user", "content": "look up x"}]
            }))
            .unwrap(),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("event: content_block_start"));
    assert!(text.contains("\"type\":\"tool_use\""));
    assert!(text.contains("\"type\":\"input_json_delta\""));
    assert!(text.contains("\"partial_json\":\"{\\\"q\\\":\\\"x\\\"}\""));
    assert!(text.contains("\"stop_reason\":\"tool_use\""));
    assert!(text.contains("event: message_stop"));
}

#[tokio::test]
async fn messages_stream_reasoning_returns_thinking_events() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    store
        .save(&Credentials {
            provider: crate::config::Provider::Codex,
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: now_unix() + 600,
            account_id: "acc_old".into(),
        })
        .unwrap();
    let http = Client::new();
    let state = AppState::new(
        TokenManager::new(
            store,
            CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
        ),
        CodexClient::new(http, spawn_reasoning_stream_codex_server().await),
        Some("secret".into()),
        ModelList::from_ids(["gpt-5.5"]),
    );
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static("secret"));
    headers.insert(
        "anthropic-beta",
        HeaderValue::from_static("extended-thinking-2025-05-14"),
    );

    let response = handlers::messages(
        axum::extract::State(state),
        headers,
        Json(
            serde_json::from_value(json!({
                "model": "gpt-5.5",
                "stream": true,
                "max_tokens": 256,
                "thinking": {"type": "enabled", "budget_tokens": 4096},
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .unwrap(),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("event: ping"));
    assert!(text.contains("\"type\":\"thinking\""));
    assert!(text.contains("\"type\":\"thinking_delta\""));
    assert_eq!(text.matches("\"thinking\":\"step\"").count(), 1);
    assert!(text.contains("\"type\":\"signature_delta\""));
    assert!(text.contains("event: message_stop"));
}

#[tokio::test]
async fn messages_stream_preserve_cache_usage_fields() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    store
        .save(&Credentials {
            provider: crate::config::Provider::Codex,
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: now_unix() + 600,
            account_id: "acc_old".into(),
        })
        .unwrap();
    let http = Client::new();
    let state = AppState::new(
        TokenManager::new(
            store,
            CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
        ),
        CodexClient::new(http, spawn_cache_usage_stream_codex_server().await),
        Some("secret".into()),
        ModelList::from_ids(["gpt-5.5"]),
    );
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static("secret"));

    let response = handlers::messages(
        axum::extract::State(state),
        headers,
        Json(
            serde_json::from_value(json!({
                "model": "gpt-5.5",
                "stream": true,
                "max_tokens": 256,
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .unwrap(),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("\"cache_creation_input_tokens\":100"));
    assert!(text.contains("\"cache_read_input_tokens\":200"));
    assert!(text.contains("\"web_search_requests\":1"));
}
