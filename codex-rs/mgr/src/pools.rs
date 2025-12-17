use anyhow::Context;
use codex_core::auth::AuthDotJson;
use serde::Serialize;
use std::path::Path;

use crate::config;
use crate::label::validate_label;

const POOL_ID_MAX_LEN: i64 = 64;

#[derive(Debug, Clone, Serialize)]
struct PoolRow {
    pool_id: String,
    labels: Vec<String>,
    policy_key: Option<String>,
}

pub(crate) async fn set(
    state_root: &Path,
    accounts_root: &Path,
    pool_id: String,
    mut labels: Vec<String>,
    policy_key: Option<String>,
) -> anyhow::Result<()> {
    validate_pool_id(&pool_id)?;
    if labels.is_empty() {
        anyhow::bail!("--labels must not be empty");
    }
    for label in &labels {
        validate_label(label)?;
        ensure_auth_present(accounts_root, label)?;
    }

    labels.sort();
    labels.dedup();

    let mut root = config::load_value_for_update(state_root)?;
    config::ensure_gateway_defaults(&mut root)?;
    config::set_pool(&mut root, &pool_id, &labels, policy_key.as_deref())?;
    config::write_value(state_root, &root)?;
    Ok(())
}

pub(crate) async fn list(state_root: &Path, json: bool) -> anyhow::Result<()> {
    let root = config::load_value_optional(state_root)?;
    let pools = config::extract_pools(&root)?;

    let mut rows: Vec<PoolRow> = pools
        .into_iter()
        .map(|(pool_id, pool)| PoolRow {
            pool_id,
            labels: pool.labels,
            policy_key: pool.policy_key,
        })
        .collect();
    rows.sort_by(|a, b| a.pool_id.cmp(&b.pool_id));

    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    if rows.is_empty() {
        println!("no pools configured");
        return Ok(());
    }

    let mut pool_w = "pool".len();
    for row in &rows {
        pool_w = pool_w.max(row.pool_id.len());
    }

    println!(
        "{:<pool_w$} {:>7} policy_key",
        "pool",
        "labels",
        pool_w = pool_w
    );
    for row in rows {
        let policy = row.policy_key.as_deref().unwrap_or("-");
        println!(
            "{:<pool_w$} {:>7} {}",
            row.pool_id,
            row.labels.len(),
            policy,
            pool_w = pool_w
        );
    }

    Ok(())
}

pub(crate) async fn del(state_root: &Path, pool_id: String) -> anyhow::Result<()> {
    validate_pool_id(&pool_id)?;
    let mut root = config::load_value_for_update(state_root)?;
    let removed = config::remove_pool(&mut root, &pool_id)?;
    if !removed {
        anyhow::bail!("pool {pool_id:?} does not exist");
    }
    config::write_value(state_root, &root)?;
    Ok(())
}

fn validate_pool_id(pool_id: &str) -> anyhow::Result<()> {
    if pool_id.is_empty() {
        anyhow::bail!("pool_id must not be empty");
    }
    let len = i64::try_from(pool_id.len()).unwrap_or(i64::MAX);
    if len > POOL_ID_MAX_LEN {
        anyhow::bail!("pool_id is too long (max {POOL_ID_MAX_LEN})");
    }
    if pool_id == "." || pool_id == ".." {
        anyhow::bail!("pool_id {pool_id:?} is not allowed");
    }
    if pool_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !pool_id.starts_with('.')
    {
        return Ok(());
    }
    anyhow::bail!(
        "invalid pool_id {pool_id:?}; use only ASCII letters/numbers plus '-', '_' or '.', and do not start with '.'"
    );
}

fn ensure_auth_present(accounts_root: &Path, label: &str) -> anyhow::Result<()> {
    let auth_path = accounts_root.join(label).join("auth.json");
    let text = std::fs::read_to_string(&auth_path)
        .with_context(|| format!("reading {auth_path:?} for pool member {label:?}"))?;
    let parsed: AuthDotJson = serde_json::from_str(&text)
        .with_context(|| format!("parsing {auth_path:?} for pool member {label:?}"))?;
    let refresh_ok = parsed
        .tokens
        .as_ref()
        .is_some_and(|t| !t.refresh_token.trim().is_empty());
    if !refresh_ok {
        anyhow::bail!("auth.json missing refresh_token for pool member {label:?}");
    }
    Ok(())
}
