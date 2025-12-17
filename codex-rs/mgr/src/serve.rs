use anyhow::Context;
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::Request;
use axum::http::StatusCode;
use axum::middleware;
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::get;
use std::path::Path;
use std::sync::Arc;
use tokio::net::TcpListener;

use crate::config;
use crate::gateway_sessions;
use crate::proxy;
use crate::redis_conn;

#[derive(Clone)]
struct ServeState {
    redis: redis::aio::ConnectionManager,
    upstream_base_url: String,
    http: reqwest::Client,
}

pub(crate) async fn run(state_root: &Path) -> anyhow::Result<()> {
    let config_path = config::config_path(state_root);
    let cfg = config::load(state_root)?;

    eprintln!("codex-mgr serve");
    eprintln!("config: {}", config_path.display());
    eprintln!("listen: {}", cfg.gateway.listen);
    eprintln!("upstream_base_url: {}", cfg.gateway.upstream_base_url);
    eprintln!("redis_url: {}", redact_url(&cfg.gateway.redis_url));
    eprintln!("sticky_ttl_seconds: {}", cfg.gateway.sticky_ttl_seconds);
    eprintln!(
        "token_safety_window_seconds: {}",
        cfg.gateway.token_safety_window_seconds
    );

    let listener = TcpListener::bind(&cfg.gateway.listen)
        .await
        .with_context(|| format!("binding to {}", cfg.gateway.listen))?;
    let addr = listener.local_addr().context("getting bound address")?;

    eprintln!("codex-mgr gateway listening on http://{addr}");

    let state = Arc::new(ServeState {
        redis: redis_conn::connect(&cfg.gateway.redis_url).await?,
        upstream_base_url: cfg.gateway.upstream_base_url.clone(),
        http: reqwest::Client::new(),
    });

    let router = Router::new()
        .route("/healthz", get(|| async { "ok\n" }))
        .route("/authz", get(|| async { "ok\n" }))
        .fallback(proxy_non_streaming)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_gateway_session,
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
    if request.uri().path() == "/healthz" {
        return Ok(next.run(request).await);
    }

    let token = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_bearer_token)
        .ok_or(StatusCode::UNAUTHORIZED)?;

    let mut conn = state.redis.clone();
    let session = gateway_sessions::get(&mut conn, token)
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
        .ok_or(StatusCode::UNAUTHORIZED)?;
    request.extensions_mut().insert(session);
    Ok(next.run(request).await)
}

async fn proxy_non_streaming(
    State(state): State<Arc<ServeState>>,
    request: Request<Body>,
) -> Result<Response, StatusCode> {
    proxy::forward_non_streaming(&state.http, &state.upstream_base_url, request).await
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
