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

pub(crate) async fn add_member(
    state_root: &Path,
    accounts_root: &Path,
    pool_id: String,
    label: String,
) -> anyhow::Result<()> {
    validate_pool_id(&pool_id)?;
    validate_label(&label)?;
    ensure_auth_present(accounts_root, &label)?;

    let mut root = config::load_value_for_update(state_root)?;
    // We need to fetch existing pool definition.
    // config module doesn't expose get_pool easily for update, it exposes set_pool and extract_pools.
    // We can extract, find, modify, then set.

    // Actually, config::load_value_for_update returns a toml::Value (Table).
    // We can navigate it.

    let pools_table = root
        .as_table_mut()
        .and_then(|t| t.get_mut("pools"))
        .and_then(|v| v.as_table_mut());

    let pools_table = match pools_table {
        Some(t) => t,
        None => anyhow::bail!("pool {pool_id:?} not found (no pools section)"),
    };

    let pool_entry = pools_table.get_mut(&pool_id);
    let pool_entry = match pool_entry {
        Some(e) => e,
        None => anyhow::bail!("pool {pool_id:?} does not exist"),
    };

    // pool_entry should be a Table with "labels" Array.
    let labels_array = pool_entry
        .get_mut("labels")
        .and_then(|v| v.as_array_mut())
        .context("invalid pool config: labels is not an array")?;

    let label_val = toml::Value::String(label.clone());
    if !labels_array.contains(&label_val) {
        labels_array.push(label_val);
        // Sort for consistency?
        labels_array.sort_by(|a, b| {
            let s_a = a.as_str().unwrap_or("");
            let s_b = b.as_str().unwrap_or("");
            s_a.cmp(s_b)
        });
        config::write_value(state_root, &root)?;
        println!("Added {label:?} to pool {pool_id:?}");
    } else {
        println!("{label:?} is already in pool {pool_id:?}");
    }

    Ok(())
}

pub(crate) async fn remove_member(
    state_root: &Path,
    pool_id: String,
    label: String,
) -> anyhow::Result<()> {
    validate_pool_id(&pool_id)?;
    // No need to validate label format strictly, just remove it if matches string.

    let mut root = config::load_value_for_update(state_root)?;
    let pools_table = root
        .as_table_mut()
        .and_then(|t| t.get_mut("pools"))
        .and_then(|v| v.as_table_mut());

    let pools_table = match pools_table {
        Some(t) => t,
        None => anyhow::bail!("pool {pool_id:?} not found"),
    };

    let pool_entry = pools_table.get_mut(&pool_id);
    let pool_entry = match pool_entry {
        Some(e) => e,
        None => anyhow::bail!("pool {pool_id:?} does not exist"),
    };

    let labels_array = pool_entry
        .get_mut("labels")
        .and_then(|v| v.as_array_mut())
        .context("invalid pool config: labels is not an array")?;

    let label_val = toml::Value::String(label.clone());
    if let Some(pos) = labels_array.iter().position(|x| x == &label_val) {
        if labels_array.len() <= 1 {
            anyhow::bail!("cannot remove last member {label:?} from pool {pool_id:?}");
        }
        labels_array.remove(pos);
        config::write_value(state_root, &root)?;
        println!("Removed {label:?} from pool {pool_id:?}");
    } else {
        anyhow::bail!("member {label:?} not found in pool {pool_id:?}");
    }

    Ok(())
}

pub(crate) async fn validate(
    state_root: &Path,
    accounts_root: &Path,
    target_pool_id: Option<String>,
) -> anyhow::Result<()> {
    let root = config::load_value_optional(state_root)?;
    let pools = config::extract_pools(&root)?;

    if pools.is_empty() {
        println!("No pools configured to validate.");
        return Ok(());
    }

    let mut all_ok = true;

    for (pool_id, pool) in pools {
        if let Some(target) = &target_pool_id
            && &pool_id != target
        {
            continue;
        }

        print!("Validating pool {pool_id:?}... ");
        let mut pool_errors = Vec::new();

        for label in pool.labels {
            match ensure_auth_present(accounts_root, &label) {
                Ok(_) => {}
                Err(e) => {
                    pool_errors.push(format!("member {label:?}: {e}"));
                }
            }
        }

        if pool_errors.is_empty() {
            println!("OK");
        } else {
            all_ok = false;
            println!("FAIL");
            for err in pool_errors {
                println!("  - {err}");
            }
        }
    }

    if !all_ok {
        anyhow::bail!("validation failed");
    }
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
