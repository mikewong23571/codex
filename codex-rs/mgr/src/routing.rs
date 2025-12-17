use anyhow::Context;
use axum::http::HeaderMap;
use base64::Engine;
use sha2::Digest;

const STICKY_KEY_PREFIX: &str = "gw:sticky:";

#[derive(Debug, Clone)]
pub(crate) struct RouteInfo {
    pub(crate) account_pool_id: String,
    pub(crate) account_id: String,
    pub(crate) conversation_id: Option<String>,
}

pub(crate) async fn route_account(
    conn: &mut redis::aio::ConnectionManager,
    account_pool_id: &str,
    labels: &[String],
    policy_key: Option<&str>,
    sticky_ttl_seconds: i64,
    conversation_id: Option<String>,
    non_sticky_key: &str,
) -> anyhow::Result<RouteInfo> {
    if labels.is_empty() {
        anyhow::bail!("pool {account_pool_id:?} has no labels configured");
    }
    if sticky_ttl_seconds <= 0 {
        anyhow::bail!("sticky_ttl_seconds must be > 0");
    }

    let account_id = match conversation_id.as_deref() {
        Some(conversation_id) => {
            let sticky_key = sticky_key(account_pool_id, conversation_id);
            let existing: Option<String> =
                redis::cmd("GET").arg(&sticky_key).query_async(conn).await?;
            match existing {
                Some(existing) if labels.iter().any(|l| l == &existing) => existing,
                Some(_) => {
                    let selected =
                        select_account_id(account_pool_id, policy_key, conversation_id, labels)?;
                    let _: () = redis::cmd("SET")
                        .arg(&sticky_key)
                        .arg(&selected)
                        .arg("EX")
                        .arg(sticky_ttl_seconds)
                        .query_async(conn)
                        .await?;
                    selected
                }
                None => {
                    let selected =
                        select_account_id(account_pool_id, policy_key, conversation_id, labels)?;

                    let set: Option<String> = redis::cmd("SET")
                        .arg(&sticky_key)
                        .arg(&selected)
                        .arg("NX")
                        .arg("EX")
                        .arg(sticky_ttl_seconds)
                        .query_async(conn)
                        .await?;

                    if set.is_some() {
                        selected
                    } else {
                        let current: Option<String> =
                            redis::cmd("GET").arg(&sticky_key).query_async(conn).await?;
                        current.unwrap_or(selected)
                    }
                }
            }
        }
        None => select_account_id(account_pool_id, policy_key, non_sticky_key, labels)?,
    };

    Ok(RouteInfo {
        account_pool_id: account_pool_id.to_string(),
        account_id,
        conversation_id,
    })
}

pub(crate) fn extract_conversation_id(headers: &HeaderMap) -> Option<String> {
    read_header(headers, "conversation_id").or_else(|| read_header(headers, "session_id"))
}

fn read_header(headers: &HeaderMap, name: &'static str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

fn sticky_key(account_pool_id: &str, conversation_id: &str) -> String {
    let digest = sha256_bytes(conversation_id.as_bytes());
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    format!("{STICKY_KEY_PREFIX}{account_pool_id}:{encoded}")
}

fn select_account_id(
    account_pool_id: &str,
    policy_key: Option<&str>,
    key: &str,
    labels: &[String],
) -> anyhow::Result<String> {
    let mut hasher = sha2::Sha256::new();
    hasher.update(account_pool_id.as_bytes());
    hasher.update([0]);
    if let Some(policy_key) = policy_key {
        hasher.update(policy_key.as_bytes());
    }
    hasher.update([0]);
    hasher.update(key.as_bytes());
    let digest = hasher.finalize();

    let len_i64 = i64::try_from(labels.len()).unwrap_or(i64::MAX);
    if len_i64 <= 0 {
        anyhow::bail!("labels must not be empty");
    }

    let prefix = <[u8; 8]>::try_from(&digest[..8]).context("hash output too short")?;
    let value = i64::from_be_bytes(prefix);
    let value = value.checked_abs().unwrap_or(i64::MAX);
    let idx_i64 = value.rem_euclid(len_i64);
    let idx_usize = usize::try_from(idx_i64).context("index does not fit in usize")?;
    Ok(labels[idx_usize].clone())
}

fn sha256_bytes(input: &[u8]) -> [u8; 32] {
    let mut hasher = sha2::Sha256::new();
    hasher.update(input);
    hasher.finalize().into()
}
