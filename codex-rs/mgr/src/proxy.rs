use axum::body;
use axum::body::Body;
use axum::http::HeaderMap;
use axum::http::Request;
use axum::http::StatusCode;
use axum::http::header;
use axum::http::header::HeaderValue;
use axum::response::Response;
use bytes::Bytes;
use futures::Stream;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::time::Instant;

use crate::header_policy;
use crate::observability::GatewayMetrics;

const MAX_BODY_BYTES: i64 = 10 * 1024 * 1024;

pub(crate) async fn forward(
    http: &reqwest::Client,
    upstream_base_url: &str,
    request: Request<Body>,
    authorization: &str,
    chatgpt_account_id: Option<&str>,
    metrics: Arc<GatewayMetrics>,
    debug: bool,
) -> Result<Response, StatusCode> {
    let (parts, body) = request.into_parts();

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

    let limit = match usize::try_from(MAX_BODY_BYTES) {
        Ok(value) => value,
        Err(_) => usize::MAX,
    };
    let body_bytes = body::to_bytes(body, limit)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;

    let mut headers = header_policy::forward_request_headers(&parts.headers);
    let auth =
        HeaderValue::from_str(authorization).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    headers.insert(header::AUTHORIZATION, auth);
    if let Some(chatgpt_account_id) = chatgpt_account_id {
        let account_id = HeaderValue::from_str(chatgpt_account_id)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
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
            return Err(StatusCode::BAD_GATEWAY);
        }
    };

    let status = response.status();
    record_upstream_status(&metrics, status);
    record_upstream_latency_ms(&metrics, upstream_start.elapsed());

    let headers = header_policy::forward_response_headers(response.headers());
    let body = if wants_event_stream {
        metrics.sse_streams_total.fetch_add(1, Ordering::Relaxed);
        metrics.sse_streams_inflight.fetch_add(1, Ordering::Relaxed);
        let guard = InflightGuard { metrics };
        Body::from_stream(GuardedBytesStream::new(response.bytes_stream(), guard))
    } else {
        let response_body = response.bytes().await.map_err(|err| {
            tracing::warn!(error = %err, "upstream response body read failed");
            StatusCode::BAD_GATEWAY
        })?;
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

fn request_accepts_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/event-stream"))
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
