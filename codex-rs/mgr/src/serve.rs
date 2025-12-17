use anyhow::Context;
use axum::Router;
use axum::body::Body;
use axum::extract::Extension;
use axum::extract::State;
use axum::http::Request;
use axum::http::StatusCode;
use axum::middleware;
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::get;
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;

use crate::account_token_provider;
use crate::config;
use crate::gateway_sessions;
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
}

pub(crate) async fn run(state_root: &Path, accounts_root: &Path) -> anyhow::Result<()> {
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
        pools: cfg.pools.clone(),
        sticky_ttl_seconds: cfg.gateway.sticky_ttl_seconds,
        accounts_root: accounts_root.to_path_buf(),
        token_safety_window_seconds: cfg.gateway.token_safety_window_seconds,
    });

    let router = Router::new()
        .route("/healthz", get(|| async { "ok\n" }))
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
    .map_err(map_auth_error)?;

    proxy::forward(
        &state.http,
        &state.upstream_base_url,
        request,
        &auth.authorization,
        auth.chatgpt_account_id.as_deref(),
    )
    .await
}

async fn ensure_routing(
    State(state): State<Arc<ServeState>>,
    mut request: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    if request.uri().path() == "/healthz" {
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
    .map_err(map_routing_error)?;

    request.extensions_mut().insert(route_info);
    Ok(next.run(request).await)
}

fn map_routing_error(err: anyhow::Error) -> StatusCode {
    if err.downcast_ref::<redis::RedisError>().is_some() {
        return StatusCode::SERVICE_UNAVAILABLE;
    }
    StatusCode::INTERNAL_SERVER_ERROR
}

fn map_auth_error(err: anyhow::Error) -> StatusCode {
    if err.downcast_ref::<redis::RedisError>().is_some() {
        return StatusCode::SERVICE_UNAVAILABLE;
    }
    StatusCode::BAD_GATEWAY
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
