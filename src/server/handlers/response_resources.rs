use crate::{
    Error,
    codex::client::{ResponseResourceCapabilities, ResponseResourceCapability},
    openai::response::{response_deleted, response_input_item_list},
    server::{AppState, UpstreamState, auth::authorize},
};
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};

fn select_response_resource_upstream(
    state: &AppState,
    operation: &str,
    capability: fn(ResponseResourceCapabilities) -> ResponseResourceCapability,
) -> crate::Result<Option<UpstreamState>> {
    let mut candidates = state
        .upstreams
        .iter()
        .filter(|upstream| {
            capability(upstream.client.response_resource_capabilities())
                == ResponseResourceCapability::UpstreamSupported
        })
        .cloned()
        .collect::<Vec<_>>();
    match candidates.len() {
        0 => Ok(None),
        1 => Ok(candidates.pop()),
        _ => Err(Error::config(format!(
            "Responses resource {operation} is ambiguous across configured upstreams"
        ))),
    }
}

fn response_resource_not_found(response_id: &str) -> Response {
    Error::upstream_with_status(
        StatusCode::NOT_FOUND,
        format!("response `{response_id}` was not found"),
    )
    .into_response()
}

fn upstream_for_provider(
    state: &AppState,
    provider: crate::config::Provider,
) -> Option<UpstreamState> {
    state
        .upstreams
        .iter()
        .find(|upstream| upstream.provider == provider)
        .cloned()
}

/// Retrieves a stored Responses API object.
///
/// Existing local response ids are served from rotom's in-memory compatibility
/// store. Unknown ids are forwarded only when a single configured upstream
/// explicitly supports response retrieval.
pub async fn get_response(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(response_id): Path<String>,
) -> Response {
    if let Err(error) = authorize(&headers, state.api_key.as_deref()) {
        return error.into_response();
    }

    if let Some(stored) = state.responses.get(&response_id).await {
        return Json(stored.response).into_response();
    }

    let upstream = match select_response_resource_upstream(&state, "retrieve", |caps| caps.retrieve)
    {
        Ok(Some(upstream)) => upstream,
        Ok(None) => return response_resource_not_found(&response_id),
        Err(error) => return error.into_response(),
    };
    let credentials = match upstream.token_manager.credentials().await {
        Ok(credentials) => credentials,
        Err(error) => return error.into_response(),
    };
    match upstream
        .client
        .retrieve_response(&response_id, &credentials)
        .await
    {
        Ok(value) => Json(value).into_response(),
        Err(error) => error.into_response(),
    }
}

/// Deletes a stored Responses API object.
///
/// Local compatibility responses are removed from rotom's in-memory store.
/// Unknown ids are forwarded only when a single configured upstream explicitly
/// supports response deletion.
pub async fn delete_response(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(response_id): Path<String>,
) -> Response {
    if let Err(error) = authorize(&headers, state.api_key.as_deref()) {
        return error.into_response();
    }

    if let Some(stored) = state.responses.get(&response_id).await {
        if stored.upstream_resource {
            let Some(upstream) = upstream_for_provider(&state, stored.provider) else {
                return Error::config(format!(
                    "provider {} is not configured for response `{response_id}`",
                    stored.provider
                ))
                .into_response();
            };
            if upstream.client.response_resource_capabilities().delete
                != ResponseResourceCapability::UpstreamSupported
            {
                return Error::upstream_with_status(
                    StatusCode::NOT_IMPLEMENTED,
                    format!(
                        "{} upstream does not support Responses resource DELETE",
                        stored.provider.display_name()
                    ),
                )
                .into_response();
            }
            let credentials = match upstream.token_manager.credentials().await {
                Ok(credentials) => credentials,
                Err(error) => return error.into_response(),
            };
            let deleted = match upstream
                .client
                .delete_response(&response_id, &credentials)
                .await
            {
                Ok(value) => value,
                Err(error) => return error.into_response(),
            };
            state.responses.remove(&response_id).await;
            return Json(deleted).into_response();
        }
    }

    if let Some(stored) = state.responses.remove(&response_id).await {
        tracing::debug!(
            provider = %stored.provider,
            upstream_resource = stored.upstream_resource,
            response_id = %response_id,
            "deleted_local_response_resource"
        );
        return Json(response_deleted(response_id)).into_response();
    }

    let upstream = match select_response_resource_upstream(&state, "delete", |caps| caps.delete) {
        Ok(Some(upstream)) => upstream,
        Ok(None) => return response_resource_not_found(&response_id),
        Err(error) => return error.into_response(),
    };
    let credentials = match upstream.token_manager.credentials().await {
        Ok(credentials) => credentials,
        Err(error) => return error.into_response(),
    };
    match upstream
        .client
        .delete_response(&response_id, &credentials)
        .await
    {
        Ok(value) => Json(value).into_response(),
        Err(error) => error.into_response(),
    }
}

/// Lists locally stored input items for a Responses API object.
pub async fn list_response_input_items(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(response_id): Path<String>,
) -> Response {
    if let Err(error) = authorize(&headers, state.api_key.as_deref()) {
        return error.into_response();
    }

    match state.responses.get(&response_id).await {
        Some(stored) => {
            let response = response_input_item_list(stored.input_items, false);
            Json(response).into_response()
        }
        None => response_resource_not_found(&response_id),
    }
}

/// Cancels a Responses API object when the upstream supports cancellation.
pub async fn cancel_response(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(response_id): Path<String>,
) -> Response {
    if let Err(error) = authorize(&headers, state.api_key.as_deref()) {
        return error.into_response();
    }

    if state.responses.get(&response_id).await.is_some() {
        return Error::config(format!(
            "response `{response_id}` cannot be canceled by rotom after it is locally stored"
        ))
        .into_response();
    }

    match select_response_resource_upstream(&state, "cancel", |caps| caps.cancel) {
        Ok(Some(_)) => Error::upstream_with_status(
            StatusCode::NOT_IMPLEMENTED,
            "Responses resource cancel is not implemented for configured upstreams",
        )
        .into_response(),
        Ok(None) => response_resource_not_found(&response_id),
        Err(error) => error.into_response(),
    }
}
