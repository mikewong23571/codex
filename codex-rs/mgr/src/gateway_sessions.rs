use anyhow::Context;
use serde::Deserialize;
use serde::Serialize;

const SESSION_KEY_PREFIX: &str = "gw:session:";
const SESSION_KEY_PATTERN: &str = "gw:session:*";
const SESSION_SCAN_COUNT: i64 = 1000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct GatewaySession {
    pub(crate) account_pool_id: String,
    pub(crate) policy_key: Option<String>,
    pub(crate) issued_at_ms: i64,
    pub(crate) expires_at_ms: i64,
    pub(crate) note: Option<String>,
}

pub(crate) fn key_for_token(token: &str) -> String {
    format!("{SESSION_KEY_PREFIX}{token}")
}

pub(crate) fn token_from_key(key: &str) -> Option<&str> {
    key.strip_prefix(SESSION_KEY_PREFIX)
}

pub(crate) async fn get(
    conn: &mut redis::aio::ConnectionManager,
    token: &str,
) -> anyhow::Result<Option<GatewaySession>> {
    let key = key_for_token(token);
    let value: Option<String> = redis::cmd("GET").arg(&key).query_async(conn).await?;
    match value {
        Some(value) => serde_json::from_str(&value)
            .with_context(|| format!("parsing redis session value for {key:?}"))
            .map(Some),
        None => Ok(None),
    }
}

pub(crate) async fn put(
    conn: &mut redis::aio::ConnectionManager,
    token: &str,
    session: &GatewaySession,
    ttl_seconds: i64,
) -> anyhow::Result<()> {
    if ttl_seconds <= 0 {
        anyhow::bail!("ttl_seconds must be > 0");
    }
    let key = key_for_token(token);
    let value = serde_json::to_string(session).context("serializing GatewaySession")?;
    let _: () = redis::cmd("SET")
        .arg(&key)
        .arg(value)
        .arg("EX")
        .arg(ttl_seconds)
        .query_async(conn)
        .await?;
    Ok(())
}

pub(crate) async fn del(
    conn: &mut redis::aio::ConnectionManager,
    token: &str,
) -> anyhow::Result<bool> {
    let key = key_for_token(token);
    let deleted: i64 = redis::cmd("DEL").arg(&key).query_async(conn).await?;
    Ok(deleted > 0)
}

pub(crate) async fn list(
    conn: &mut redis::aio::ConnectionManager,
) -> anyhow::Result<Vec<(String, GatewaySession)>> {
    let mut cursor = "0".to_string();
    let mut keys = Vec::new();
    loop {
        let (next_cursor, mut batch): (String, Vec<String>) = redis::cmd("SCAN")
            .arg(&cursor)
            .arg("MATCH")
            .arg(SESSION_KEY_PATTERN)
            .arg("COUNT")
            .arg(SESSION_SCAN_COUNT)
            .query_async(conn)
            .await?;
        keys.append(&mut batch);
        cursor = next_cursor;
        if cursor == "0" {
            break;
        }
    }

    let mut out = Vec::new();
    for key in keys {
        let Some(token) = token_from_key(&key) else {
            continue;
        };
        let value: Option<String> = redis::cmd("GET").arg(&key).query_async(conn).await?;
        let Some(value) = value else {
            continue;
        };
        let session: GatewaySession = serde_json::from_str(&value)
            .with_context(|| format!("parsing redis session value for {key:?}"))?;
        out.push((token.to_string(), session));
    }

    out.sort_by(|(a_token, a), (b_token, b)| {
        a.expires_at_ms
            .cmp(&b.expires_at_ms)
            .then_with(|| a_token.cmp(b_token))
    });
    Ok(out)
}
