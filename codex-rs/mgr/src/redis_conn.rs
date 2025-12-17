use anyhow::Context;

pub(crate) async fn connect(url: &str) -> anyhow::Result<redis::aio::ConnectionManager> {
    let client =
        redis::Client::open(url).with_context(|| format!("opening redis client {url:?}"))?;
    redis::aio::ConnectionManager::new(client)
        .await
        .with_context(|| format!("connecting to redis {url:?}"))
}
