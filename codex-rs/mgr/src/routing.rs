use anyhow::Context;
use axum::http::HeaderMap;
use base64::Engine;
use sha2::Digest;

use std::collections::HashMap;

use crate::usage;

const STICKY_KEY_PREFIX: &str = "gw:sticky:";

#[derive(Debug, Clone)]
pub(crate) struct RouteInfo {
    pub(crate) account_pool_id: String,
    pub(crate) candidates: Vec<String>,
    pub(crate) conversation_id: Option<String>,
}

pub(crate) struct RouteAccountArgs<'a> {
    pub(crate) account_pool_id: &'a str,
    pub(crate) labels: &'a [String],
    pub(crate) policy_key: Option<&'a str>,
    pub(crate) sticky_ttl_seconds: i64,
    pub(crate) conversation_id: Option<String>,
    pub(crate) non_sticky_key: &'a str,
    pub(crate) usage_scores: &'a HashMap<String, usage::Score>,
}

pub(crate) async fn route_account(
    conn: &mut redis::aio::ConnectionManager,
    args: RouteAccountArgs<'_>,
) -> anyhow::Result<RouteInfo> {
    let RouteAccountArgs {
        account_pool_id,
        labels,
        policy_key,
        sticky_ttl_seconds,
        conversation_id,
        non_sticky_key,
        usage_scores,
    } = args;

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
                    let list = select_candidates(
                        account_pool_id,
                        policy_key,
                        conversation_id,
                        labels,
                        usage_scores,
                    )?;
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
                    let list = select_candidates(
                        account_pool_id,
                        policy_key,
                        conversation_id,
                        labels,
                        usage_scores,
                    )?;
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
        None => select_candidates(
            account_pool_id,
            policy_key,
            non_sticky_key,
            labels,
            usage_scores,
        )?,
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
    usage_scores: &HashMap<String, usage::Score>,
) -> anyhow::Result<Vec<String>> {
    // If usage scores are empty, fall back to consistent hashing ring for distribution
    if usage_scores.is_empty() {
        return select_candidates_ring(account_pool_id, policy_key, key, labels);
    }

    let mut candidates: Vec<String> = labels.to_vec();

    // Sort candidates:
    // 1. Availability: Both limits > 0%
    // 2. Weekly Remaining: Descending
    // 3. 5h Remaining: Descending
    // 4. Stable tie-break: Label string
    candidates.sort_by(|a, b| {
        let score_a = usage_scores.get(a);
        let score_b = usage_scores.get(b);

        // Define a helper to extract sort keys
        // (is_available, weekly, five)
        let keys = |score: Option<&usage::Score>| {
            if let Some(s) = score {
                let available = s.weekly_remaining > 0.0 && s.five_remaining > 0.0;
                // If not available, treat remaining as -1.0 for sorting purposes to push to bottom?
                // Actually tuple comparison works well.
                // We want available=true > available=false
                // then weekly desc
                // then five desc
                (available, s.weekly_remaining, s.five_remaining)
            } else {
                // No score (unknown) -> Assume available, but low priority?
                // Or treat as "fresh" (high priority)?
                // Strategy: Treat unknown as available=true, max usage?
                // User said "if usage is 0, skip". Unknown != 0.
                // Let's treat unknown as "available (true), 100.0, 100.0" to discover/probe it.
                (true, 100.0, 100.0)
            }
        };

        let k_a = keys(score_a);
        let k_b = keys(score_b);

        if k_a.0 != k_b.0 {
            return k_a.0.cmp(&k_b.0).reverse();
        }

        // 2. Weekly (desc)
        // f64 doesn't impl Ord. Use partial_cmp.
        if (k_a.1 - k_b.1).abs() > f64::EPSILON {
            return k_b
                .1
                .partial_cmp(&k_a.1)
                .unwrap_or(std::cmp::Ordering::Equal);
        }

        // 3. 5h (desc)
        if (k_a.2 - k_b.2).abs() > f64::EPSILON {
            return k_b
                .2
                .partial_cmp(&k_a.2)
                .unwrap_or(std::cmp::Ordering::Equal);
        }

        // 4. Tie-break (Label Ascending for stability)
        a.cmp(b)
    });

    Ok(candidates)
}

fn select_candidates_ring(
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

    let mut ring = Vec::with_capacity(len);
    for i in 0..len {
        ring.push(labels[(idx_usize + i) % len].clone());
    }
    Ok(ring)
}

fn sha256_bytes(input: &[u8]) -> [u8; 32] {
    let mut hasher = sha2::Sha256::new();
    hasher.update(input);
    hasher.finalize().into()
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn score(present: bool, weekly_remaining: f64, five_remaining: f64) -> crate::usage::Score {
        crate::usage::Score {
            weekly_present: present,
            weekly_remaining,
            five_present: present,
            five_remaining,
        }
    }

    #[test]
    fn test_select_candidates_usage() {
        let labels = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let mut usage_scores = HashMap::new();

        // Case 1: All have usage
        usage_scores.insert("a".to_string(), score(true, 10.0, 10.0));
        usage_scores.insert("b".to_string(), score(true, 20.0, 20.0));
        usage_scores.insert("c".to_string(), score(true, 30.0, 30.0));

        // Sorting: c (30/30) > b (20/20) > a (10/10)
        let candidates = select_candidates("pool", None, "key", &labels, &usage_scores).unwrap();
        assert_eq!(candidates[0], "c");
        assert_eq!(candidates[1], "b");
        assert_eq!(candidates[2], "a");

        // Case 2: 'a' has 0 usage
        usage_scores.insert("a".to_string(), score(true, 0.0, 0.0));
        let candidates = select_candidates("pool", None, "key", &labels, &usage_scores).unwrap();
        // c > b > a (dead)
        assert_eq!(candidates[0], "c");
        assert_eq!(candidates[1], "b");
        assert_eq!(candidates[2], "a");
        assert!(usage_scores[&candidates[2]].five_remaining == 0.0);

        // Case 3: All empty
        usage_scores.insert("b".to_string(), score(true, 0.0, 0.0));
        usage_scores.insert("c".to_string(), score(true, 0.0, 0.0));
        let candidates = select_candidates("pool", None, "key", &labels, &usage_scores).unwrap();
        // Sort by label "a", "b", "c" since usage is equal (dead)
        // Tie-break is label ASC.
        assert_eq!(candidates[0], "a");
        assert_eq!(candidates[1], "b");
        assert_eq!(candidates[2], "c");
    }

    #[test]
    fn test_select_candidates_sorting() {
        let labels = vec![
            "tiger".to_string(),   // 99% weekly, 99% 5h
            "fee".to_string(),     // 84% weekly, 52% 5h
            "fly".to_string(),     // 67% weekly, 100% 5h (low weekly)
            "wolf".to_string(),    // 78% weekly, 56% 5h
            "dead".to_string(),    // 0% weekly, 0% 5h
            "unknown".to_string(), // No score
        ];

        let mut usage_scores = HashMap::new();
        usage_scores.insert("tiger".to_string(), score(true, 99.0, 99.0));
        usage_scores.insert("fee".to_string(), score(true, 84.0, 52.0));
        usage_scores.insert("fly".to_string(), score(true, 67.0, 100.0));
        usage_scores.insert("wolf".to_string(), score(true, 78.0, 56.0));
        usage_scores.insert("dead".to_string(), score(true, 0.0, 0.0));
        // "unknown" is not inserted

        let candidates = select_candidates("pool", None, "key", &labels, &usage_scores).unwrap();

        // Expected order:
        // 1. Available > Unavailable.
        // 2. Weekly DESC.
        // 3. 5h DESC.

        // "unknown" is treated as available (100, 100). So it should be first.
        assert_eq!(candidates[0], "unknown");

        // Among known available:
        // Tiger (99) > Fee (84) > Wolf (78) > Fly (67)
        assert_eq!(candidates[1], "tiger");
        assert_eq!(candidates[2], "fee");
        assert_eq!(candidates[3], "wolf");
        assert_eq!(candidates[4], "fly");

        // "dead" is unavailable (0 usage), so it should be last.
        assert_eq!(candidates[5], "dead");
    }

    #[test]
    fn test_tie_breaking() {
        let labels = vec!["b".to_string(), "a".to_string()];
        let mut usage_scores = HashMap::new();
        usage_scores.insert("a".to_string(), score(true, 50.0, 50.0));
        usage_scores.insert("b".to_string(), score(true, 50.0, 50.0));

        let candidates = select_candidates("pool", None, "key", &labels, &usage_scores).unwrap();
        // "a" < "b", so "a" comes first.
        assert_eq!(candidates[0], "a");
        assert_eq!(candidates[1], "b");
    }
}
