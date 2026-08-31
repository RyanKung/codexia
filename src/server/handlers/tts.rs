//! Native xAI text-to-speech HTTP and WebSocket proxy handlers.

use super::trace_request;
use crate::{
    Error, Result,
    codex::client::{GrokTtsWebSocket, GrokTtsWebSocketConnect, resolve_grok_tts_websocket_url},
    config::{Credentials, Provider},
    server::{AppState, UpstreamState, auth::authorize},
};
use axum::{
    Json,
    body::Body,
    extract::{
        RawQuery, State, WebSocketUpgrade,
        ws::{CloseFrame as DownstreamCloseFrame, Message as DownstreamMessage, WebSocket},
    },
    http::{HeaderMap, HeaderName, StatusCode},
    response::{IntoResponse, Response},
};
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use serde_json::Value;
use std::{fmt::Display, future::Future, time::Duration};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::{
    Message as UpstreamMessage,
    protocol::{CloseFrame as UpstreamCloseFrame, frame::coding::CloseCode},
};

const GROK_TTS_WEBSOCKET_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const GROK_TTS_WEBSOCKET_CLOSE_GRACE: Duration = Duration::from_secs(1);

enum RelayMessage<Message> {
    Forward(Message),
    Ignore,
}

/// Proxies the native xAI text-to-speech request shape to the logged-in Grok upstream.
pub async fn tts(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Response {
    trace_request("tts", &request);
    if let Err(error) = authorize(&headers, state.api_key.as_deref()) {
        return error.into_response();
    }

    let (upstream, credentials) = match grok_upstream_and_credentials(&state).await {
        Ok(result) => result,
        Err(error) => return error.into_response(),
    };
    match upstream
        .client
        .synthesize_grok_speech(&request, &credentials)
        .await
    {
        Ok(response) => proxy_upstream_response(response),
        Err(error) => error.into_response(),
    }
}

/// Proxies the native xAI bidirectional text-to-speech WebSocket protocol.
pub async fn tts_websocket(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
    websocket: WebSocketUpgrade,
) -> Response {
    if let Err(error) = authorize(&headers, state.api_key.as_deref()) {
        return error.into_response();
    }

    let (upstream, credentials) = match grok_upstream_and_credentials(&state).await {
        Ok(result) => result,
        Err(error) => return error.into_response(),
    };
    let websocket_url =
        match resolve_grok_tts_websocket_url(upstream.client.base_url(), raw_query.as_deref()) {
            Ok(url) => url,
            Err(error) => return error.into_response(),
        };
    let connection = timeout(
        GROK_TTS_WEBSOCKET_CONNECT_TIMEOUT,
        upstream
            .client
            .connect_grok_tts_websocket(&websocket_url, &credentials),
    )
    .await;
    let upstream_socket = match connection {
        Ok(Ok(GrokTtsWebSocketConnect::Connected(socket))) => socket,
        Ok(Ok(GrokTtsWebSocketConnect::Rejected(response))) => {
            return proxy_upstream_response(response);
        }
        Ok(Err(error)) => return Error::upstream(error.to_string()).into_response(),
        Err(_elapsed) => {
            return Error::upstream_with_status(
                StatusCode::GATEWAY_TIMEOUT,
                "Grok TTS WebSocket handshake timed out",
            )
            .into_response();
        }
    };

    websocket
        .on_upgrade(move |socket| proxy_grok_tts_websocket(socket, upstream_socket))
        .into_response()
}

/// Proxies the native xAI built-in text-to-speech voice list.
pub async fn tts_voices(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(error) = authorize(&headers, state.api_key.as_deref()) {
        return error.into_response();
    }

    let (upstream, credentials) = match grok_upstream_and_credentials(&state).await {
        Ok(result) => result,
        Err(error) => return error.into_response(),
    };
    match upstream.client.list_grok_tts_voices(&credentials).await {
        Ok(response) => proxy_upstream_response(response),
        Err(error) => error.into_response(),
    }
}

async fn grok_upstream_and_credentials(state: &AppState) -> Result<(&UpstreamState, Credentials)> {
    let upstream = state.upstream_for_provider(Provider::Grok).ok_or_else(|| {
        Error::config("not logged in for provider grok; run `rotom login --provider grok` first")
    })?;
    let credentials = upstream.token_manager.credentials().await?;
    Ok((upstream, credentials))
}

async fn proxy_grok_tts_websocket(downstream: WebSocket, upstream: GrokTtsWebSocket) {
    let (downstream_sender, downstream_receiver) = downstream.split();
    let (upstream_sender, upstream_receiver) = upstream.split();
    let downstream_to_upstream = relay_websocket_messages(
        downstream_receiver,
        upstream_sender,
        downstream_to_upstream_message,
        downstream_message_is_close,
        || UpstreamMessage::Close(None),
        "downstream-to-upstream",
    );
    let upstream_to_downstream = relay_websocket_messages(
        upstream_receiver,
        downstream_sender,
        upstream_to_downstream_message,
        upstream_message_is_close,
        || DownstreamMessage::Close(None),
        "upstream-to-downstream",
    );
    drive_bidirectional_relays(
        downstream_to_upstream,
        upstream_to_downstream,
        GROK_TTS_WEBSOCKET_CLOSE_GRACE,
    )
    .await;
}

async fn relay_websocket_messages<Source, Destination, Input, Output, SourceError, SinkError>(
    mut source: Source,
    mut destination: Destination,
    convert: fn(Input) -> RelayMessage<Output>,
    is_close: fn(&Input) -> bool,
    close_message: fn() -> Output,
    direction: &'static str,
) where
    Source: Stream<Item = std::result::Result<Input, SourceError>> + Unpin,
    Destination: Sink<Output, Error = SinkError> + Unpin,
    SourceError: Display,
    SinkError: Display,
{
    loop {
        let message = match source.next().await {
            Some(Ok(message)) => message,
            Some(Err(error)) => {
                tracing::debug!(%direction, error = %error, "Grok TTS WebSocket source closed with an error");
                let _ = destination.send(close_message()).await;
                return;
            }
            None => {
                let _ = destination.send(close_message()).await;
                return;
            }
        };
        let closes_connection = is_close(&message);
        if let RelayMessage::Forward(message) = convert(message)
            && let Err(error) = destination.send(message).await
        {
            tracing::debug!(%direction, error = %error, "Grok TTS WebSocket destination closed with an error");
            return;
        }
        if closes_connection {
            return;
        }
    }
}

async fn drive_bidirectional_relays<DownstreamToUpstream, UpstreamToDownstream>(
    downstream_to_upstream: DownstreamToUpstream,
    upstream_to_downstream: UpstreamToDownstream,
    close_grace: Duration,
) where
    DownstreamToUpstream: Future<Output = ()>,
    UpstreamToDownstream: Future<Output = ()>,
{
    tokio::pin!(downstream_to_upstream, upstream_to_downstream);
    tokio::select! {
        () = downstream_to_upstream.as_mut() => {
            let _ = timeout(close_grace, upstream_to_downstream.as_mut()).await;
        }
        () = upstream_to_downstream.as_mut() => {
            let _ = timeout(close_grace, downstream_to_upstream.as_mut()).await;
        }
    }
}

const fn downstream_message_is_close(message: &DownstreamMessage) -> bool {
    matches!(message, DownstreamMessage::Close(_))
}

const fn upstream_message_is_close(message: &UpstreamMessage) -> bool {
    matches!(message, UpstreamMessage::Close(_))
}

fn downstream_to_upstream_message(message: DownstreamMessage) -> RelayMessage<UpstreamMessage> {
    match message {
        DownstreamMessage::Text(text) => {
            RelayMessage::Forward(UpstreamMessage::Text(text.to_string().into()))
        }
        DownstreamMessage::Binary(bytes) => RelayMessage::Forward(UpstreamMessage::Binary(bytes)),
        DownstreamMessage::Ping(bytes) => RelayMessage::Forward(UpstreamMessage::Ping(bytes)),
        DownstreamMessage::Pong(bytes) => RelayMessage::Forward(UpstreamMessage::Pong(bytes)),
        DownstreamMessage::Close(frame) => {
            RelayMessage::Forward(UpstreamMessage::Close(frame.map(|frame| {
                UpstreamCloseFrame {
                    code: CloseCode::from(frame.code),
                    reason: frame.reason.to_string().into(),
                }
            })))
        }
    }
}

fn upstream_to_downstream_message(message: UpstreamMessage) -> RelayMessage<DownstreamMessage> {
    match message {
        UpstreamMessage::Text(text) => {
            RelayMessage::Forward(DownstreamMessage::Text(text.to_string().into()))
        }
        UpstreamMessage::Binary(bytes) => RelayMessage::Forward(DownstreamMessage::Binary(bytes)),
        UpstreamMessage::Ping(bytes) => RelayMessage::Forward(DownstreamMessage::Ping(bytes)),
        UpstreamMessage::Pong(bytes) => RelayMessage::Forward(DownstreamMessage::Pong(bytes)),
        UpstreamMessage::Close(frame) => {
            RelayMessage::Forward(DownstreamMessage::Close(frame.map(|frame| {
                DownstreamCloseFrame {
                    code: u16::from(frame.code),
                    reason: frame.reason.to_string().into(),
                }
            })))
        }
        UpstreamMessage::Frame(_) => RelayMessage::Ignore,
    }
}

fn proxy_upstream_response(upstream: reqwest::Response) -> Response {
    let status = upstream.status();
    let headers = upstream.headers().clone();
    let mut response = Response::new(Body::from_stream(upstream.bytes_stream()));
    *response.status_mut() = status;
    for (name, value) in &headers {
        if !is_hop_by_hop_header(name) {
            response.headers_mut().append(name.clone(), value.clone());
        }
    }
    response
}

fn is_hop_by_hop_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;
    use std::{
        convert::Infallible,
        marker::PhantomData,
        pin::Pin,
        sync::{Arc, Mutex},
        task::{Context, Poll},
    };

    struct PendingSink<Message> {
        marker: PhantomData<Message>,
    }

    impl<Message> PendingSink<Message> {
        fn new() -> Self {
            Self {
                marker: PhantomData,
            }
        }
    }

    impl<Message> Sink<Message> for PendingSink<Message> {
        type Error = Infallible;

        fn poll_ready(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<std::result::Result<(), Self::Error>> {
            Poll::Pending
        }

        fn start_send(
            self: Pin<&mut Self>,
            _item: Message,
        ) -> std::result::Result<(), Self::Error> {
            Ok(())
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<std::result::Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<std::result::Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    struct RecordingSink<Message> {
        messages: Arc<Mutex<Vec<Message>>>,
    }

    impl<Message> RecordingSink<Message> {
        fn new(messages: Arc<Mutex<Vec<Message>>>) -> Self {
            Self { messages }
        }
    }

    impl<Message> Sink<Message> for RecordingSink<Message> {
        type Error = Infallible;

        fn poll_ready(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<std::result::Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(self: Pin<&mut Self>, item: Message) -> std::result::Result<(), Self::Error> {
            self.messages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(item);
            Ok(())
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<std::result::Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<std::result::Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn client_events_progress_while_downstream_audio_sink_is_blocked() {
        let forwarded = Arc::new(Mutex::new(Vec::new()));
        let downstream_messages = stream::iter([
            Ok::<_, std::io::Error>(DownstreamMessage::Text(r#"{"type":"text.clear"}"#.into())),
            Ok(DownstreamMessage::Close(None)),
        ]);
        let upstream_messages = stream::iter([Ok::<_, std::io::Error>(UpstreamMessage::Text(
            r#"{"type":"audio.delta","delta":"SUQz"}"#.into(),
        ))]);
        let downstream_to_upstream = relay_websocket_messages(
            downstream_messages,
            RecordingSink::new(forwarded.clone()),
            downstream_to_upstream_message,
            downstream_message_is_close,
            || UpstreamMessage::Close(None),
            "test-downstream-to-upstream",
        );
        let upstream_to_downstream = relay_websocket_messages(
            upstream_messages,
            PendingSink::new(),
            upstream_to_downstream_message,
            upstream_message_is_close,
            || DownstreamMessage::Close(None),
            "test-upstream-to-downstream",
        );

        drive_bidirectional_relays(
            downstream_to_upstream,
            upstream_to_downstream,
            Duration::ZERO,
        )
        .await;

        let messages = forwarded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(matches!(
            messages.first(),
            Some(UpstreamMessage::Text(text)) if text.contains("text.clear")
        ));
        drop(messages);
    }
}
