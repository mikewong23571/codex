use anyhow::Context;
use axum::Router;
use axum::routing::get;
use std::path::Path;
use tokio::net::TcpListener;

use crate::config;

pub(crate) async fn run(state_root: &Path) -> anyhow::Result<()> {
    let cfg = config::load(state_root)?;
    let listener = TcpListener::bind(&cfg.gateway.listen)
        .await
        .with_context(|| format!("binding to {}", cfg.gateway.listen))?;
    let addr = listener.local_addr().context("getting bound address")?;

    eprintln!("codex-mgr gateway listening on http://{addr}");

    let router = Router::new().route("/healthz", get(|| async { "ok\n" }));

    axum::serve(listener, router).await?;
    Ok(())
}
