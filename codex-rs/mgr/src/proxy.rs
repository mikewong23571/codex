use axum::body::Body;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::header;
use axum::http::header::HeaderValue;
use axum::http::request::Parts;
use axum::response::Response;
use bytes::Bytes;
use futures::Stream;
use serde::Serialize;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::time::Instant;

use crate::header_policy;
use crate::observability::GatewayMetrics;

pub(crate) const MAX_REQUEST_BODY_BYTES: usize = 250 * 1024 * 1024;

#[derive(Debug, Clone)]
pub(crate) struct GatewayError {
    status: StatusCode,
    detail: String,
}

#[derive(Serialize)]
struct ErrorResponseBody<'a> {
    detail: &'a str,
}

impl GatewayError {
    pub(crate) fn bad_gateway(detail: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            detail: detail.into(),
        }
    }

    pub(crate) fn status(&self) -> StatusCode {
        self.status
    }

    pub(crate) fn detail(&self) -> &str {
        &self.detail
    }

    pub(crate) fn into_response(self) -> Response {
        json_error_response(self.status, self.detail)
    }
}

pub(crate) fn json_error_response(status: StatusCode, detail: impl Into<String>) -> Response {
    let detail = detail.into();
    let body = serde_json::to_vec(&ErrorResponseBody { detail: &detail }).map_or_else(
        |err| {
            tracing::error!(error = %err, %status, "failed to serialize gateway error response");
            Body::from(detail.clone())
        },
        Body::from,
    );

    let mut response = Response::new(body);
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
        .headers_mut()
        .insert("X-Accel-Buffering", HeaderValue::from_static("no"));
    response
}

pub(crate) struct ForwardRequest<'a> {
    pub(crate) parts: Parts,
    pub(crate) body_bytes: Bytes,
    pub(crate) authorization: &'a str,
    pub(crate) chatgpt_account_id: Option<&'a str>,
}

pub(crate) async fn forward(
    http: &reqwest::Client,
    upstream_base_url: &str,
    request: ForwardRequest<'_>,
    metrics: Arc<GatewayMetrics>,
    debug: bool,
) -> Result<Response, GatewayError> {
    let ForwardRequest {
        parts,
        body_bytes,
        authorization,
        chatgpt_account_id,
    } = request;

    if debug {
        tracing::info!("--- [DEBUG] Incoming Request Headers ---");
        for (name, value) in &parts.headers {
            tracing::info!("{}: {:?}", name, value);
        }
    }

    let wants_event_stream = request_accepts_event_stream(&parts.headers);

    let path_and_query = parts
        .uri
        .path_and_query()
        .map(axum::http::uri::PathAndQuery::as_str)
        .unwrap_or_else(|| parts.uri.path());
    let base = upstream_base_url.trim().trim_end_matches('/');
    let upstream_url = format!("{base}{path_and_query}");

    let mut headers = header_policy::forward_request_headers(&parts.headers);
    let auth = HeaderValue::from_str(authorization).map_err(|_| {
        GatewayError::bad_gateway("failed to construct upstream authorization header")
    })?;
    headers.insert(header::AUTHORIZATION, auth);
    if let Some(chatgpt_account_id) = chatgpt_account_id {
        let account_id = HeaderValue::from_str(chatgpt_account_id).map_err(|_| {
            GatewayError::bad_gateway("failed to construct ChatGPT-Account-ID header")
        })?;
        let _ = headers.insert("ChatGPT-Account-ID", account_id);
    }

    if debug {
        tracing::info!("--- [DEBUG] Outgoing Request Headers ---");
        for (name, value) in &headers {
            tracing::info!("{}: {:?}", name, value);
        }
    }

    metrics
        .upstream_requests_total
        .fetch_add(1, Ordering::Relaxed);
    let upstream_start = Instant::now();
    let response = match http
        .request(parts.method, upstream_url)
        .headers(headers)
        .body(body_bytes)
        .send()
        .await
    {
        Ok(response) => response,
        Err(err) => {
            tracing::warn!(error = %err, "upstream request failed");
            metrics
                .upstream_errors_total
                .fetch_add(1, Ordering::Relaxed);
            return Err(GatewayError::bad_gateway(format!(
                "failed to send upstream request: {err}"
            )));
        }
    };

    let status = response.status();
    record_upstream_status(&metrics, status);
    record_upstream_latency_ms(&metrics, upstream_start.elapsed());

    let upstream_headers = response.headers().clone();
    let headers = header_policy::forward_response_headers(&upstream_headers);
    let body = if should_stream_upstream_response(wants_event_stream, status, &upstream_headers) {
        metrics.sse_streams_total.fetch_add(1, Ordering::Relaxed);
        metrics.sse_streams_inflight.fetch_add(1, Ordering::Relaxed);
        let guard = InflightGuard { metrics };
        Body::from_stream(GuardedBytesStream::new(response.bytes_stream(), guard))
    } else {
        let response_body = response.bytes().await.map_err(|err| {
            tracing::warn!(error = %err, "upstream response body read failed");
            GatewayError::bad_gateway(format!("failed to read upstream response body: {err}"))
        })?;
        if status.is_client_error() || status.is_server_error() {
            log_upstream_error_response(status, &upstream_headers, &response_body);
        }
        Body::from(response_body)
    };

    let mut out = Response::new(body);
    *out.status_mut() = status;
    out.headers_mut().extend(headers);

    // Client Experience Optimization:
    // 1. Cache-Control: no-cache -> Prevent browser/proxy caching of partial streams.
    // 2. X-Accel-Buffering: no -> Tell Nginx (if present) to disable buffering.
    out.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    out.headers_mut()
        .insert("X-Accel-Buffering", HeaderValue::from_static("no"));

    Ok(out)
}

fn should_stream_upstream_response(
    wants_event_stream: bool,
    status: reqwest::StatusCode,
    headers: &HeaderMap,
) -> bool {
    wants_event_stream && status.is_success() && response_is_event_stream(headers)
}

fn request_accepts_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/event-stream"))
}

fn response_is_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/event-stream"))
}

fn log_upstream_error_response(status: reqwest::StatusCode, headers: &HeaderMap, body: &Bytes) {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-");
    let upstream_request_id = headers
        .get("x-oai-request-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-");
    let body_preview = String::from_utf8_lossy(&body[..body.len().min(1024)]).replace('\n', "\\n");
    tracing::warn!(
        %status,
        %content_type,
        %upstream_request_id,
        response_body_bytes = body.len(),
        %body_preview,
        "upstream returned error response"
    );
}

fn record_upstream_status(metrics: &GatewayMetrics, status: reqwest::StatusCode) {
    if status.is_success() {
        metrics
            .upstream_responses_2xx_total
            .fetch_add(1, Ordering::Relaxed);
        return;
    }
    if status.is_redirection() {
        metrics
            .upstream_responses_3xx_total
            .fetch_add(1, Ordering::Relaxed);
        return;
    }
    if status.is_client_error() {
        metrics
            .upstream_responses_4xx_total
            .fetch_add(1, Ordering::Relaxed);
        return;
    }
    metrics
        .upstream_responses_5xx_total
        .fetch_add(1, Ordering::Relaxed);
}

fn record_upstream_latency_ms(metrics: &GatewayMetrics, elapsed: std::time::Duration) {
    let Ok(ms) = i64::try_from(elapsed.as_millis()) else {
        return;
    };
    metrics
        .upstream_latency_ms_sum
        .fetch_add(ms, Ordering::Relaxed);
    metrics
        .upstream_latency_ms_count
        .fetch_add(1, Ordering::Relaxed);
}

struct InflightGuard {
    metrics: Arc<GatewayMetrics>,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.metrics
            .sse_streams_inflight
            .fetch_sub(1, Ordering::Relaxed);
    }
}

struct GuardedBytesStream {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>,
    _guard: InflightGuard,
}

impl GuardedBytesStream {
    fn new(
        inner: impl Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
        guard: InflightGuard,
    ) -> Self {
        Self {
            inner: Box::pin(inner),
            _guard: guard,
        }
    }
}

impl Stream for GuardedBytesStream {
    type Item = Result<Bytes, reqwest::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        this.inner.as_mut().poll_next(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::json_error_response;
    use super::should_stream_upstream_response;
    use axum::body;
    use axum::http::HeaderMap;
    use axum::http::StatusCode;
    use axum::http::header;
    use axum::http::header::HeaderValue;
    use bytes::Bytes;
    use pretty_assertions::assert_eq;

    #[test]
    fn json_error_response_contains_detail_body() {
        let response = json_error_response(StatusCode::BAD_REQUEST, "bad request");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE),
            Some(&HeaderValue::from_static("application/json"))
        );

        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let body = runtime.block_on(body::to_bytes(response.into_body(), usize::MAX));
        let body = body.expect("body bytes");
        assert_eq!(body, Bytes::from_static(br#"{"detail":"bad request"}"#));
    }

    #[test]
    fn streams_successful_event_stream_responses() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream; charset=utf-8"),
        );

        assert!(should_stream_upstream_response(
            true,
            reqwest::StatusCode::OK,
            &headers
        ));
    }

    #[test]
    fn does_not_stream_error_json_responses() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );

        assert!(!should_stream_upstream_response(
            true,
            reqwest::StatusCode::BAD_REQUEST,
            &headers
        ));
    }
}
