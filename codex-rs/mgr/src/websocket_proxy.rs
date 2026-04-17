use anyhow::Context;
use axum::body::Body;
use axum::extract::ws::CloseFrame as AxumCloseFrame;
use axum::extract::ws::Message as AxumMessage;
use axum::extract::ws::WebSocket;
use axum::extract::ws::WebSocketUpgrade;
use axum::http::HeaderMap;
use axum::http::Request;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::http::header;
use axum::http::header::HeaderValue;
use axum::response::Response;
use futures::SinkExt;
use futures::StreamExt;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio_tungstenite::connect_async_with_config;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message as TungsteniteMessage;
use tokio_tungstenite::tungstenite::Utf8Bytes;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::CloseFrame as TungsteniteCloseFrame;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use crate::account_token_provider;
use crate::observability::GatewayMetrics;
use crate::routing;
use crate::serve::RequestTraceData;
use crate::serve::ServeState;
use crate::ws_header_policy;

pub(crate) async fn forward(
    state: Arc<ServeState>,
    route_info: routing::RouteInfo,
    websocket: WebSocketUpgrade,
    request: Request<Body>,
) -> Result<Response, StatusCode> {
    let request_headers = request.headers().clone();
    let request_uri = request.uri().clone();
    let trace_data = request.extensions().get::<Arc<RequestTraceData>>().cloned();
    let mut conn = state.redis.clone();

    for (idx, account_id) in route_info.candidates.iter().enumerate() {
        let is_last = idx + 1 == route_info.candidates.len();
        let auth_result = account_token_provider::get(
            &mut conn,
            &state.accounts_root,
            account_id,
            state.token_safety_window_seconds,
        )
        .await;

        let auth = match auth_result {
            Ok(auth) => auth,
            Err(err) => {
                if err.downcast_ref::<redis::RedisError>().is_some() {
                    tracing::error!(error = %err, "redis error in websocket token provider");
                } else {
                    tracing::warn!(error = %err, %account_id, "websocket token provider error");
                    state
                        .metrics
                        .token_errors_total
                        .fetch_add(1, Ordering::Relaxed);
                }
                if is_last {
                    return Err(StatusCode::BAD_GATEWAY);
                }
                continue;
            }
        };

        match connect_upstream(
            &state.upstream_base_url,
            &request_uri,
            &request_headers,
            &auth.authorization,
            auth.chatgpt_account_id.as_deref(),
        )
        .await
        {
            Ok(upstream) => {
                if let Some(trace_data) = trace_data.as_ref() {
                    let _ = trace_data.account_id.set(account_id.clone());
                }

                let metrics = Arc::clone(&state.metrics);
                return Ok(websocket.on_upgrade(move |downstream| async move {
                    metrics
                        .websocket_connections_total
                        .fetch_add(1, Ordering::Relaxed);
                    metrics
                        .websocket_connections_inflight
                        .fetch_add(1, Ordering::Relaxed);
                    let _guard = WebsocketInflightGuard {
                        metrics: Arc::clone(&metrics),
                    };
                    relay(downstream, upstream, metrics).await;
                }));
            }
            Err(err) => {
                state
                    .metrics
                    .websocket_connect_failures_total
                    .fetch_add(1, Ordering::Relaxed);
                if err.is_handshake_failure() {
                    state
                        .metrics
                        .websocket_upstream_handshake_failures_total
                        .fetch_add(1, Ordering::Relaxed);
                }
                tracing::warn!(
                    %account_id,
                    error = %err,
                    "websocket upstream connect failed"
                );
                if is_last {
                    return Err(err.status_code());
                }
            }
        }
    }

    Err(StatusCode::BAD_GATEWAY)
}

pub(crate) fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    let has_upgrade_header = headers
        .get(header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
    if !has_upgrade_header {
        return false;
    }

    headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .any(|token| token.eq_ignore_ascii_case("upgrade"))
}

async fn connect_upstream(
    upstream_base_url: &str,
    request_uri: &Uri,
    request_headers: &HeaderMap,
    authorization: &str,
    chatgpt_account_id: Option<&str>,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    UpstreamConnectError,
> {
    let url = build_upstream_websocket_url(upstream_base_url, request_uri)
        .map_err(UpstreamConnectError::other)?;
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(UpstreamConnectError::other)?;
    let mut headers = ws_header_policy::forward_request_headers(request_headers);
    let auth = HeaderValue::from_str(authorization).map_err(UpstreamConnectError::other)?;
    headers.insert(header::AUTHORIZATION, auth);
    if let Some(chatgpt_account_id) = chatgpt_account_id {
        let account_id =
            HeaderValue::from_str(chatgpt_account_id).map_err(UpstreamConnectError::other)?;
        headers.insert("ChatGPT-Account-ID", account_id);
    }
    request.headers_mut().extend(headers);

    let (stream, _response) = connect_async_with_config(request, Some(websocket_config()), false)
        .await
        .map_err(UpstreamConnectError::from_ws_error)?;
    Ok(stream)
}

fn build_upstream_websocket_url(
    upstream_base_url: &str,
    request_uri: &Uri,
) -> anyhow::Result<reqwest::Url> {
    if request_uri.path() == "/ws" {
        return build_realtime_websocket_url(upstream_base_url, request_uri);
    }

    let path_and_query = request_uri
        .path_and_query()
        .map(axum::http::uri::PathAndQuery::as_str)
        .unwrap_or_else(|| request_uri.path());
    let base = upstream_base_url.trim().trim_end_matches('/');
    let url = reqwest::Url::parse(&format!("{base}{path_and_query}"))
        .with_context(|| format!("parsing websocket upstream url for {path_and_query}"))?;
    switch_to_websocket_scheme(url)
}

fn build_realtime_websocket_url(
    upstream_base_url: &str,
    request_uri: &Uri,
) -> anyhow::Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(upstream_base_url.trim())
        .with_context(|| format!("parsing realtime upstream base url {upstream_base_url:?}"))?;
    url.set_path(request_uri.path());
    url.set_query(request_uri.query());
    switch_to_websocket_scheme(url)
}

fn switch_to_websocket_scheme(mut url: reqwest::Url) -> anyhow::Result<reqwest::Url> {
    let scheme = match url.scheme() {
        "http" => Some("ws"),
        "https" => Some("wss"),
        "ws" | "wss" => None,
        other => anyhow::bail!("unsupported websocket upstream scheme {other:?}"),
    };

    if let Some(scheme) = scheme {
        let _ = url.set_scheme(scheme);
    }

    Ok(url)
}

fn websocket_config() -> WebSocketConfig {
    WebSocketConfig::default()
}

async fn relay(
    downstream: WebSocket,
    upstream: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    metrics: Arc<GatewayMetrics>,
) {
    let (mut downstream_tx, mut downstream_rx) = downstream.split();
    let (mut upstream_tx, mut upstream_rx) = upstream.split();

    loop {
        tokio::select! {
            maybe_message = downstream_rx.next() => {
                match maybe_message {
                    Some(Ok(message)) => {
                        let message = axum_message_to_tungstenite(message);
                        let is_close = matches!(message, Some(TungsteniteMessage::Close(_)));
                        if let Some(message) = message
                            && let Err(err) = upstream_tx.send(message).await
                        {
                            record_relay_error(&metrics, format!("sending websocket frame upstream failed: {err}"));
                            break;
                        }
                        if is_close {
                            break;
                        }
                    }
                    Some(Err(err)) => {
                        record_relay_error(&metrics, format!("reading websocket frame from downstream failed: {err}"));
                        break;
                    }
                    None => {
                        let _ = upstream_tx.close().await;
                        break;
                    }
                }
            }
            maybe_message = upstream_rx.next() => {
                match maybe_message {
                    Some(Ok(message)) => {
                        let message = tungstenite_message_to_axum(message);
                        let is_close = matches!(message, Some(AxumMessage::Close(_)));
                        if let Some(message) = message
                            && let Err(err) = downstream_tx.send(message).await
                        {
                            record_relay_error(&metrics, format!("sending websocket frame downstream failed: {err}"));
                            break;
                        }
                        if is_close {
                            break;
                        }
                    }
                    Some(Err(err)) => {
                        record_relay_error(&metrics, format!("reading websocket frame from upstream failed: {err}"));
                        break;
                    }
                    None => {
                        let _ = downstream_tx.close().await;
                        break;
                    }
                }
            }
        }
    }
}

fn axum_message_to_tungstenite(message: AxumMessage) -> Option<TungsteniteMessage> {
    match message {
        AxumMessage::Text(text) => Some(TungsteniteMessage::Text(text.to_string().into())),
        AxumMessage::Binary(data) => Some(TungsteniteMessage::Binary(data)),
        AxumMessage::Ping(data) => Some(TungsteniteMessage::Ping(data)),
        AxumMessage::Pong(data) => Some(TungsteniteMessage::Pong(data)),
        AxumMessage::Close(frame) => Some(TungsteniteMessage::Close(frame.map(|frame| {
            TungsteniteCloseFrame {
                code: frame.code.into(),
                reason: Utf8Bytes::from(frame.reason.to_string()),
            }
        }))),
    }
}

fn tungstenite_message_to_axum(message: TungsteniteMessage) -> Option<AxumMessage> {
    match message {
        TungsteniteMessage::Text(text) => Some(AxumMessage::Text(text.to_string().into())),
        TungsteniteMessage::Binary(data) => Some(AxumMessage::Binary(data)),
        TungsteniteMessage::Ping(data) => Some(AxumMessage::Ping(data)),
        TungsteniteMessage::Pong(data) => Some(AxumMessage::Pong(data)),
        TungsteniteMessage::Close(frame) => {
            Some(AxumMessage::Close(frame.map(|frame| AxumCloseFrame {
                code: frame.code.into(),
                reason: frame.reason.to_string().into(),
            })))
        }
        TungsteniteMessage::Frame(_) => None,
    }
}

fn record_relay_error(metrics: &GatewayMetrics, message: String) {
    metrics
        .websocket_relay_errors_total
        .fetch_add(1, Ordering::Relaxed);
    tracing::warn!(error = %message, "websocket relay error");
}

struct WebsocketInflightGuard {
    metrics: Arc<GatewayMetrics>,
}

impl Drop for WebsocketInflightGuard {
    fn drop(&mut self) {
        self.metrics
            .websocket_connections_inflight
            .fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Debug)]
struct UpstreamConnectError {
    status: Option<StatusCode>,
    message: String,
}

impl UpstreamConnectError {
    fn from_ws_error(error: WsError) -> Self {
        match error {
            WsError::Http(response) => Self {
                status: Some(response.status()),
                message: format!(
                    "upstream websocket handshake failed with {}",
                    response.status()
                ),
            },
            other => Self {
                status: None,
                message: other.to_string(),
            },
        }
    }

    fn other(error: impl std::fmt::Display) -> Self {
        Self {
            status: None,
            message: error.to_string(),
        }
    }

    fn is_handshake_failure(&self) -> bool {
        self.status.is_some()
    }

    fn status_code(&self) -> StatusCode {
        self.status.unwrap_or(StatusCode::BAD_GATEWAY)
    }
}

impl std::fmt::Display for UpstreamConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for UpstreamConnectError {}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use pretty_assertions::assert_eq;

    #[test]
    fn websocket_upgrade_detection_requires_connection_and_upgrade_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONNECTION,
            HeaderValue::from_static("keep-alive, Upgrade"),
        );
        headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
        assert!(is_websocket_upgrade(&headers));

        headers.remove(header::UPGRADE);
        assert!(!is_websocket_upgrade(&headers));
    }

    #[test]
    fn responses_websocket_url_uses_upstream_base_path() {
        let uri: Uri = "/responses?foo=bar".parse().expect("uri");
        let url = build_upstream_websocket_url("https://chatgpt.com/backend-api/codex", &uri)
            .expect("url");
        assert_eq!(
            url.as_str(),
            "wss://chatgpt.com/backend-api/codex/responses?foo=bar"
        );
    }

    #[test]
    fn realtime_websocket_url_uses_site_root_ws_path() {
        let uri: Uri = "/ws?model=gpt-realtime".parse().expect("uri");
        let url = build_upstream_websocket_url("https://chatgpt.com/backend-api/codex", &uri)
            .expect("url");
        assert_eq!(url.as_str(), "wss://chatgpt.com/ws?model=gpt-realtime");
    }
}
