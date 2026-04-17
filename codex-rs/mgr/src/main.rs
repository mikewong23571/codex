#[tokio::main]
async fn main() -> anyhow::Result<()> {
    codex_utils_rustls_provider::ensure_rustls_crypto_provider();
    codex_mgr::app::run().await
}
