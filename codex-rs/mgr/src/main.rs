#[tokio::main]
async fn main() -> anyhow::Result<()> {
    codex_mgr::app::run().await
}
