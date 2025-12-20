use anyhow::Context;
use axum::http::HeaderMap;
use base64::Engine;
use sha2::Digest;

const STICKY_KEY_PREFIX: &str = "gw:sticky:";

#[derive(Debug, Clone)]
pub(crate) struct RouteInfo {
    pub(crate) account_pool_id: String,
    pub(crate) candidates: Vec<String>,
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

    let candidates = match conversation_id.as_deref() {
        Some(conversation_id) => {
            let sticky_key = sticky_key(account_pool_id, conversation_id);
            let existing: Option<String> =
                redis::cmd("GET").arg(&sticky_key).query_async(conn).await?;
            match existing {
                Some(existing) if labels.iter().any(|l| l == &existing) => {
                    // Start with sticky, then append others in a deterministic order (relying on select_candidates logic)
                    // but verifying the sticky one is first.
                    // Actually, simpler: take sticky, append all other labels filtered.
                    let mut list = Vec::with_capacity(labels.len());
                    list.push(existing.clone());
                    for label in labels {
                        if label != &existing {
                            list.push(label.clone());
                        }
                    }
                    list
                }
                Some(_) => {
                    // Existing sticky is invalid (removed from pool), re-select
                    let list =
                        select_candidates(account_pool_id, policy_key, conversation_id, labels)?;
                    let selected = &list[0];
                    let _: () = redis::cmd("SET")
                        .arg(&sticky_key)
                        .arg(selected)
                        .arg("EX")
                        .arg(sticky_ttl_seconds)
                        .query_async(conn)
                        .await?;
                    list
                }
                None => {
                    let list =
                        select_candidates(account_pool_id, policy_key, conversation_id, labels)?;
                    let selected = &list[0];

                    let set: Option<String> = redis::cmd("SET")
                        .arg(&sticky_key)
                        .arg(selected)
                        .arg("NX")
                        .arg("EX")
                        .arg(sticky_ttl_seconds)
                        .query_async(conn)
                        .await?;

                    if set.is_some() {
                        list
                    } else {
                        // Race condition: someone else set it. Read it back.
                        let current: Option<String> =
                            redis::cmd("GET").arg(&sticky_key).query_async(conn).await?;
                         match current {
                            Some(c) if labels.contains(&c) => {
                                let mut list = Vec::with_capacity(labels.len());
                                list.push(c.clone());
                                for label in labels {
                                    if label != &c {
                                        list.push(label.clone());
                                    }
                                }
                                list
                            }
                            _ => list, // Fallback to our selection if race result is weird
                        }
                    }
                }
            }
        }
        None => select_candidates(account_pool_id, policy_key, non_sticky_key, labels)?,
    };

    Ok(RouteInfo {
        account_pool_id: account_pool_id.to_string(),
        candidates,
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

fn select_candidates(
    account_pool_id: &str,
    policy_key: Option<&str>,
    key: &str,
    labels: &[String],
) -> anyhow::Result<Vec<String>> {
    let mut hasher = sha2::Sha256::new();
    hasher.update(account_pool_id.as_bytes());
    hasher.update([0]);
    if let Some(policy_key) = policy_key {
        hasher.update(policy_key.as_bytes());
    }
    hasher.update([0]);
    hasher.update(key.as_bytes());
    let digest = hasher.finalize();

    let len = labels.len();
    let len_i64 = i64::try_from(len).unwrap_or(i64::MAX);
    if len_i64 <= 0 {
        anyhow::bail!("labels must not be empty");
    }

    let prefix = <[u8; 8]>::try_from(&digest[..8]).context("hash output too short")?;
    let value = i64::from_be_bytes(prefix);
    let value = value.checked_abs().unwrap_or(i64::MAX);
    let idx_i64 = value.rem_euclid(len_i64);
    let idx_usize = usize::try_from(idx_i64).context("index does not fit in usize")?;
    
    // Rotate labels so the selected one is first
    let mut candidates = Vec::with_capacity(len);
    for i in 0..len {
        candidates.push(labels[(idx_usize + i) % len].clone());
    }

    Ok(candidates)
}

fn sha256_bytes(input: &[u8]) -> [u8; 32] {
    let mut hasher = sha2::Sha256::new();
    hasher.update(input);
    hasher.finalize().into()
}
