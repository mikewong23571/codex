use anyhow::Context;
use axum::Router;
use axum::body::Body;
use axum::extract::Extension;
use axum::extract::State;
use axum::http::Request;
use axum::http::StatusCode;
use axum::http::header;
use axum::http::header::HeaderValue;
use axum::middleware;
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::get;
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::Ordering;
use std::time::Instant;
use tokio::net::TcpListener;

use crate::account_token_provider;
use crate::config;
use crate::gateway_sessions;
use crate::observability;
use crate::proxy;
use crate::redis_conn;
use crate::routing;

#[derive(Clone)]
struct ServeState {
    redis: redis::aio::ConnectionManager,
    upstream_base_url: String,
    http: reqwest::Client,
    pools: BTreeMap<String, config::PoolConfig>,
    sticky_ttl_seconds: i64,
    accounts_root: PathBuf,
    token_safety_window_seconds: i64,
    metrics: Arc<observability::GatewayMetrics>,
}

pub(crate) async fn run(state_root: &Path, accounts_root: &Path) -> anyhow::Result<()> {
    let config_path = config::config_path(state_root);
    let cfg = config::load(state_root)?;

    tracing::info!(
        event = %"serve_start",
        config = %config_path.display(),
        listen = %cfg.gateway.listen,
        upstream_base_url = %cfg.gateway.upstream_base_url,
        redis_url = %redact_url(&cfg.gateway.redis_url),
        sticky_ttl_seconds = cfg.gateway.sticky_ttl_seconds,
        token_safety_window_seconds = cfg.gateway.token_safety_window_seconds,
    );
    warn_if_upstream_base_url_is_suspicious(&cfg.gateway.upstream_base_url);

    let listener = TcpListener::bind(&cfg.gateway.listen)
        .await
        .with_context(|| format!("binding to {}", cfg.gateway.listen))?;
    let addr = listener.local_addr().context("getting bound address")?;

    tracing::info!(event = %"serve_listening", addr = %addr);

    let gateway_metrics = Arc::new(observability::GatewayMetrics::default());
    let state = Arc::new(ServeState {
        redis: redis_conn::connect(&cfg.gateway.redis_url).await?,
        upstream_base_url: cfg.gateway.upstream_base_url.clone(),
        http: reqwest::Client::new(),
        pools: cfg.pools.clone(),
        sticky_ttl_seconds: cfg.gateway.sticky_ttl_seconds,
        accounts_root: accounts_root.to_path_buf(),
        token_safety_window_seconds: cfg.gateway.token_safety_window_seconds,
        metrics: gateway_metrics,
    });

    let router = Router::new()
        .route("/healthz", get(|| async { "ok\n" }))
        .route("/readyz", get(readyz_handler))
        .route("/metrics", get(metrics_handler))
        .route("/authz", get(authz))
        .fallback(proxy_non_streaming)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            ensure_routing,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_gateway_session,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            with_request_context,
        ))
        .with_state(state);

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn require_gateway_session(
    State(state): State<Arc<ServeState>>,
    mut request: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    if is_public_path(request.uri().path()) {
        return Ok(next.run(request).await);
    }

    let token = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_bearer_token)
        .ok_or_else(|| {
            tracing::warn!("missing bearer token");
            StatusCode::UNAUTHORIZED
        })?;

    let mut conn = state.redis.clone();
    let session = gateway_sessions::get(&mut conn, token)
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "redis error in session lookup");
            state
                .metrics
                .redis_errors_total
                .fetch_add(1, Ordering::Relaxed);
            StatusCode::SERVICE_UNAVAILABLE
        })?
        .ok_or_else(|| {
            tracing::warn!("gateway session not found");
            StatusCode::UNAUTHORIZED
        })?;
    if let Some(trace_data) = request.extensions().get::<Arc<RequestTraceData>>() {
        let _ = trace_data.pool_id.set(session.account_pool_id.clone());
    }
    request.extensions_mut().insert(session);
    Ok(next.run(request).await)
}

async fn proxy_non_streaming(
    State(state): State<Arc<ServeState>>,
    Extension(route_info): Extension<routing::RouteInfo>,
    request: Request<Body>,
) -> Result<Response, StatusCode> {
    let mut conn = state.redis.clone();
    let auth = account_token_provider::get(
        &mut conn,
        &state.accounts_root,
        &route_info.account_id,
        state.token_safety_window_seconds,
    )
    .await
    .map_err(|err| {
        if err.downcast_ref::<redis::RedisError>().is_some() {
            tracing::error!(error = %err, "redis error in account token provider");
            state
                .metrics
                .redis_errors_total
                .fetch_add(1, Ordering::Relaxed);
            return StatusCode::SERVICE_UNAVAILABLE;
        }

        tracing::warn!(error = %err, account_id = %route_info.account_id, "token provider error");
        state
            .metrics
            .token_errors_total
            .fetch_add(1, Ordering::Relaxed);
        StatusCode::BAD_GATEWAY
    })?;

    proxy::forward(
        &state.http,
        &state.upstream_base_url,
        request,
        &auth.authorization,
        auth.chatgpt_account_id.as_deref(),
        Arc::clone(&state.metrics),
    )
    .await
}

async fn ensure_routing(
    State(state): State<Arc<ServeState>>,
    mut request: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    if is_public_path(request.uri().path()) {
        return Ok(next.run(request).await);
    }

    let session = request
        .extensions()
        .get::<gateway_sessions::GatewaySession>()
        .cloned()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    let pool = state
        .pools
        .get(&session.account_pool_id)
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    let conversation_id = routing::extract_conversation_id(request.headers());
    let path_and_query = request
        .uri()
        .path_and_query()
        .map(axum::http::uri::PathAndQuery::as_str)
        .unwrap_or_else(|| request.uri().path());
    let method = request.method();
    let non_sticky_key = format!("non-sticky:{method} {path_and_query}");

    let mut conn = state.redis.clone();
    let route_info = routing::route_account(
        &mut conn,
        &session.account_pool_id,
        &pool.labels,
        session.policy_key.as_deref(),
        state.sticky_ttl_seconds,
        conversation_id,
        &non_sticky_key,
    )
    .await
    .map_err(|err| {
        if err.downcast_ref::<redis::RedisError>().is_some() {
            tracing::error!(error = %err, "redis error in routing");
            state
                .metrics
                .redis_errors_total
                .fetch_add(1, Ordering::Relaxed);
            return StatusCode::SERVICE_UNAVAILABLE;
        }

        tracing::error!(error = %err, "routing error");
        state
            .metrics
            .routing_errors_total
            .fetch_add(1, Ordering::Relaxed);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    if let Some(trace_data) = request.extensions().get::<Arc<RequestTraceData>>() {
        let _ = trace_data.account_id.set(route_info.account_id.clone());
    }

    request.extensions_mut().insert(route_info);
    Ok(next.run(request).await)
}

async fn authz(Extension(route_info): Extension<routing::RouteInfo>) -> String {
    let conversation_id = route_info.conversation_id.as_deref().unwrap_or("-");
    let pool_id = &route_info.account_pool_id;
    let account_id = &route_info.account_id;
    format!("ok\npool: {pool_id}\naccount: {account_id}\nconversation_id: {conversation_id}\n")
}

fn parse_bearer_token(value: &str) -> Option<&str> {
    let mut parts = value.split_whitespace();
    let scheme = parts.next()?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    parts.next()
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

fn redact_url(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };

    let scheme_end = scheme_end + "://".len();
    let Some(at) = url[scheme_end..].find('@').map(|i| i + scheme_end) else {
        return url.to_string();
    };
    let userinfo = &url[scheme_end..at];
    let rest = &url[at..];

    match userinfo.split_once(':') {
        Some((user, _password)) => format!("{}{}:****{}", &url[..scheme_end], user, rest),
        None => url.to_string(),
    }
}

fn warn_if_upstream_base_url_is_suspicious(upstream_base_url: &str) {
    let base = upstream_base_url.trim_end_matches('/').to_ascii_lowercase();
    if base.ends_with("/backend-api") && !base.ends_with("/backend-api/codex") {
        tracing::warn!(
            upstream_base_url,
            "upstream_base_url may be incorrect for Codex Responses; expected https://chatgpt.com/backend-api/codex so /responses maps to /backend-api/codex/responses"
        );
    }
}

#[derive(Debug)]
struct RequestTraceData {
    request_id: String,
    conversation_hash: Option<String>,
    pool_id: OnceLock<String>,
    account_id: OnceLock<String>,
}

async fn with_request_context(
    State(state): State<Arc<ServeState>>,
    mut request: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let start = Instant::now();
    let public_path = is_public_path(request.uri().path());
    if !public_path {
        state.metrics.requests_total.fetch_add(1, Ordering::Relaxed);
        state
            .metrics
            .requests_inflight
            .fetch_add(1, Ordering::Relaxed);
    }

    let request_id = observability::new_request_id();
    let conversation_id = routing::extract_conversation_id(request.headers());
    let conversation_hash = conversation_id
        .as_deref()
        .map(observability::hash_opaque_id);
    let trace_data = Arc::new(RequestTraceData {
        request_id,
        conversation_hash,
        pool_id: OnceLock::new(),
        account_id: OnceLock::new(),
    });
    request.extensions_mut().insert(Arc::clone(&trace_data));

    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let mut response = next.run(request).await;

    let elapsed = start.elapsed();
    if !public_path {
        state
            .metrics
            .requests_inflight
            .fetch_sub(1, Ordering::Relaxed);
        record_request_duration_ms(&state.metrics, elapsed);
    }

    let status = response.status();
    if !public_path {
        if status == StatusCode::UNAUTHORIZED {
            state
                .metrics
                .requests_unauthorized_total
                .fetch_add(1, Ordering::Relaxed);
        }
        if status.is_server_error() {
            state
                .metrics
                .requests_5xx_total
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    if let Ok(value) = HeaderValue::from_str(&trace_data.request_id) {
        let _ = response
            .headers_mut()
            .insert("x-codex-mgr-request-id", value);
    }

    let duration_ms = duration_ms(elapsed);
    let pool = trace_data.pool_id.get().map(String::as_str).unwrap_or("-");
    let account = trace_data
        .account_id
        .get()
        .map(String::as_str)
        .unwrap_or("-");
    let conversation = trace_data.conversation_hash.as_deref().unwrap_or("-");

    if !public_path {
        tracing::info!(
            event = %"request",
            request_id = %trace_data.request_id,
            method = %method,
            path = %path,
            conversation = %conversation,
            status = i64::from(status.as_u16()),
            duration_ms,
            pool = %pool,
            account = %account,
        );
    }

    Ok(response)
}

fn record_request_duration_ms(
    metrics: &observability::GatewayMetrics,
    elapsed: std::time::Duration,
) {
    let Ok(ms) = i64::try_from(elapsed.as_millis()) else {
        return;
    };
    metrics
        .request_duration_ms_sum
        .fetch_add(ms, Ordering::Relaxed);
    metrics
        .request_duration_ms_count
        .fetch_add(1, Ordering::Relaxed);
}

fn duration_ms(elapsed: std::time::Duration) -> i64 {
    i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
}

fn is_public_path(path: &str) -> bool {
    matches!(path, "/healthz" | "/readyz" | "/metrics")
}

async fn readyz_handler(State(state): State<Arc<ServeState>>) -> Result<String, StatusCode> {
    let mut conn = state.redis.clone();
    let pong: redis::RedisResult<String> = redis::cmd("PING").query_async(&mut conn).await;
    match pong {
        Ok(_) => Ok("ok\n".to_string()),
        Err(err) => {
            state
                .metrics
                .redis_errors_total
                .fetch_add(1, Ordering::Relaxed);
            tracing::error!(error = %err, "redis PING failed");
            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}

async fn metrics_handler(State(state): State<Arc<ServeState>>) -> Response {
    let body = state.metrics.render_prometheus();
    let mut out = Response::new(Body::from(body));
    let _ = out.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4"),
    );
    out
}
