use super::*;

#[tokio::test]
async fn response_resource_handlers_preserve_local_compatibility() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    store
        .save(&Credentials {
            provider: Provider::Codex,
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: now_unix() + 600,
            account_id: "acc".into(),
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

    let created = handlers::responses(
        axum::extract::State(state.clone()),
        headers.clone(),
        Json(
            serde_json::from_value(json!({
                "model": "gpt-5.5",
                "input": "hello"
            }))
            .unwrap(),
        ),
    )
    .await;
    let body = to_bytes(created.into_body(), usize::MAX).await.unwrap();
    let created_value = serde_json::from_slice::<Value>(&body).unwrap();
    let response_id = created_value["id"].as_str().unwrap().to_owned();

    let retrieved = handlers::get_response(
        axum::extract::State(state.clone()),
        headers.clone(),
        axum::extract::Path(response_id.clone()),
    )
    .await;
    assert_eq!(retrieved.status(), StatusCode::OK);
    let body = to_bytes(retrieved.into_body(), usize::MAX).await.unwrap();
    let retrieved_value = serde_json::from_slice::<Value>(&body).unwrap();
    assert_eq!(retrieved_value["id"], response_id);

    let input_items = handlers::list_response_input_items(
        axum::extract::State(state.clone()),
        headers.clone(),
        axum::extract::Path(response_id.clone()),
    )
    .await;
    assert_eq!(input_items.status(), StatusCode::OK);
    let body = to_bytes(input_items.into_body(), usize::MAX).await.unwrap();
    let input_items_value = serde_json::from_slice::<Value>(&body).unwrap();
    assert_eq!(input_items_value["object"], "list");
    assert!(input_items_value["data"].as_array().unwrap().len() >= 2);

    let canceled = handlers::cancel_response(
        axum::extract::State(state.clone()),
        headers.clone(),
        axum::extract::Path(response_id.clone()),
    )
    .await;
    assert_eq!(canceled.status(), StatusCode::BAD_REQUEST);

    let deleted = handlers::delete_response(
        axum::extract::State(state.clone()),
        headers.clone(),
        axum::extract::Path(response_id.clone()),
    )
    .await;
    assert_eq!(deleted.status(), StatusCode::OK);
    let body = to_bytes(deleted.into_body(), usize::MAX).await.unwrap();
    let deleted_value = serde_json::from_slice::<Value>(&body).unwrap();
    assert_eq!(deleted_value["id"], response_id);
    assert_eq!(deleted_value["object"], "response");
    assert_eq!(deleted_value["deleted"], true);

    let missing = handlers::get_response(
        axum::extract::State(state),
        headers,
        axum::extract::Path(response_id),
    )
    .await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn grok_resource_handlers_forward_unknown_ids_to_supported_upstream() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    store
        .save(&Credentials {
            provider: Provider::Grok,
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: now_unix() + 600,
            account_id: String::new(),
        })
        .unwrap();

    let captured = Arc::new(Mutex::new(Vec::new()));
    let base_url = spawn_grok_response_resource_server(captured.clone()).await;
    let http = Client::new();
    let state = AppState::new_multi_with_model_fallback(
        vec![UpstreamState {
            provider: Provider::Grok,
            token_manager: TokenManager::new_for_provider(store, Provider::Grok, http.clone()),
            client: CodexClient::new_for_provider_base_url(http, Provider::Grok, base_url),
        }],
        None,
        ModelList::from_ids(["grok-4.3"]),
        None,
    );

    let retrieved = handlers::get_response(
        axum::extract::State(state.clone()),
        HeaderMap::new(),
        axum::extract::Path("resp_upstream".to_owned()),
    )
    .await;
    assert_eq!(retrieved.status(), StatusCode::OK);
    let body = to_bytes(retrieved.into_body(), usize::MAX).await.unwrap();
    let retrieved_value = serde_json::from_slice::<Value>(&body).unwrap();
    assert_eq!(retrieved_value["id"], "resp_upstream");
    assert_eq!(
        retrieved_value["output"][0]["content"][0]["text"],
        "upstream"
    );

    let deleted = handlers::delete_response(
        axum::extract::State(state),
        HeaderMap::new(),
        axum::extract::Path("resp_upstream".to_owned()),
    )
    .await;
    assert_eq!(deleted.status(), StatusCode::OK);
    let body = to_bytes(deleted.into_body(), usize::MAX).await.unwrap();
    let deleted_value = serde_json::from_slice::<Value>(&body).unwrap();
    assert_eq!(deleted_value["object"], "response");

    assert_eq!(
        captured.lock().await.as_slice(),
        ["GET resp_upstream", "DELETE resp_upstream"]
    );
}

#[tokio::test]
async fn codex_unknown_response_resources_do_not_hit_upstream() {
    let dir = TempDir::new().unwrap();
    let store = AuthStore::new(dir.path().join("auth.json"));
    store
        .save(&Credentials {
            provider: Provider::Codex,
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: now_unix() + 600,
            account_id: "acc".into(),
        })
        .unwrap();

    let http = Client::new();
    let state = AppState::new(
        TokenManager::new(
            store,
            CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
        ),
        CodexClient::new(http, spawn_codex_server(false).await),
        None,
        ModelList::from_ids(["gpt-5.5"]),
    );

    let missing = handlers::get_response(
        axum::extract::State(state),
        HeaderMap::new(),
        axum::extract::Path("resp_missing".to_owned()),
    )
    .await;

    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}
