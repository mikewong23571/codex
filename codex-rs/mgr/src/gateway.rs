use anyhow::Context;
use base64::Engine;
use rand::TryRngCore;
use serde::Serialize;
use std::path::Path;

use crate::config;
use crate::gateway_sessions;
use crate::redis_conn;
use crate::time::now_ms;

const DEFAULT_SESSION_TTL_SECONDS: i64 = 86_400;

#[derive(Debug, Clone, Serialize)]
struct GatewaySessionRow {
    token: String,
    pool_id: String,
    policy_key: Option<String>,
    expires_at_ms: i64,
    expires_in_seconds: i64,
    note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct GatewayIssueOut {
    token: String,
    pool_id: String,
    policy_key: Option<String>,
    expires_at_ms: i64,
    ttl_seconds: i64,
    note: Option<String>,
}

pub(crate) async fn issue(
    state_root: &Path,
    pool_id: String,
    ttl_seconds: Option<i64>,
    note: Option<String>,
    json: bool,
) -> anyhow::Result<()> {
    let cfg = config::load(state_root)?;

    let policy_key = if pool_id == "default" {
        None
    } else {
        let pool = cfg
            .pools
            .get(&pool_id)
            .with_context(|| format!("pool {pool_id:?} does not exist"))?;
        if pool.labels.is_empty() {
            anyhow::bail!("pool {pool_id:?} has no labels configured");
        }
        pool.policy_key.clone()
    };

    let ttl_seconds = ttl_seconds.unwrap_or(DEFAULT_SESSION_TTL_SECONDS);
    if ttl_seconds <= 0 {
        anyhow::bail!("--ttl-seconds must be > 0");
    }

    let token = generate_gateway_token()?;
    let now_ms = now_ms();
    let expires_at_ms = now_ms.saturating_add(ttl_seconds.saturating_mul(1000));

    let session = gateway_sessions::GatewaySession {
        account_pool_id: pool_id.clone(),
        policy_key: policy_key.clone(),
        issued_at_ms: now_ms,
        expires_at_ms,
        note: note.clone(),
    };

    let mut conn = redis_conn::connect(&cfg.gateway.redis_url).await?;
    gateway_sessions::put(&mut conn, &token, &session, ttl_seconds).await?;

    if json {
        let out = GatewayIssueOut {
            token,
            pool_id,
            policy_key: policy_key.clone(),
            expires_at_ms,
            ttl_seconds,
            note,
        };
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        println!("{token}");
    }

    Ok(())
}

pub(crate) async fn list(state_root: &Path, json: bool) -> anyhow::Result<()> {
    let cfg = config::load(state_root)?;
    let mut conn = redis_conn::connect(&cfg.gateway.redis_url).await?;
    let sessions = gateway_sessions::list(&mut conn).await?;

    let now_ms = now_ms();
    let mut rows: Vec<GatewaySessionRow> = sessions
        .into_iter()
        .map(|(token, session)| {
            let expires_in_seconds = (session.expires_at_ms - now_ms) / 1000;
            GatewaySessionRow {
                token,
                pool_id: session.account_pool_id,
                policy_key: session.policy_key,
                expires_at_ms: session.expires_at_ms,
                expires_in_seconds,
                note: session.note,
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        a.expires_at_ms
            .cmp(&b.expires_at_ms)
            .then_with(|| a.token.cmp(&b.token))
    });

    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    if rows.is_empty() {
        println!("no gateway sessions");
        return Ok(());
    }

    let mut token_w = "token".len();
    let mut pool_w = "pool".len();
    let mut policy_w = "policy_key".len();
    for row in &rows {
        token_w = token_w.max(row.token.len());
        pool_w = pool_w.max(row.pool_id.len());
        policy_w = policy_w.max(row.policy_key.as_deref().unwrap_or("-").len());
    }

    println!(
        "{:<token_w$} {:<pool_w$} {:>10} {:<policy_w$} note",
        "token",
        "pool",
        "expires_in",
        "policy_key",
        token_w = token_w,
        pool_w = pool_w,
        policy_w = policy_w
    );
    for row in rows {
        let expires = if row.expires_in_seconds <= 0 {
            "expired".to_string()
        } else {
            format!("{}s", row.expires_in_seconds)
        };
        let policy = row.policy_key.as_deref().unwrap_or("-");
        let note = row.note.as_deref().unwrap_or("-");
        println!(
            "{:<token_w$} {:<pool_w$} {:>10} {:<policy_w$} {note}",
            row.token,
            row.pool_id,
            expires,
            policy,
            token_w = token_w,
            pool_w = pool_w,
            policy_w = policy_w
        );
    }

    Ok(())
}

pub(crate) async fn revoke(state_root: &Path, token: String) -> anyhow::Result<()> {
    let cfg = config::load(state_root)?;
    let mut conn = redis_conn::connect(&cfg.gateway.redis_url).await?;
    let removed = gateway_sessions::del(&mut conn, &token).await?;
    if !removed {
        anyhow::bail!("gateway session not found for token {token:?}");
    }
    Ok(())
}

fn generate_gateway_token() -> anyhow::Result<String> {
    let mut bytes = [0u8; 32];
    let mut rng = rand::rngs::OsRng;
    rng.try_fill_bytes(&mut bytes)
        .context("generating secure random bytes")?;
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    Ok(format!("gw_{encoded}"))
}
