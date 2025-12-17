use anyhow::Context;
use axum::Router;
use axum::routing::get;
use std::path::Path;
use tokio::net::TcpListener;

use crate::config;

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

    let router = Router::new().route("/healthz", get(|| async { "ok\n" }));

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
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
