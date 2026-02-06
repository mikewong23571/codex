use anyhow::Context;
use base64::Engine;
use codex_core::AuthManager;
use codex_core::auth::AuthCredentialsStoreMode;
use rand::TryRngCore;
use serde::Deserialize;
use serde::Serialize;
use std::path::Path;
use std::time::Duration;

use crate::time::now_ms;

const TOKEN_CACHE_KEY_PREFIX: &str = "gw:acct_token:";
const TOKEN_REFRESH_LOCK_KEY_PREFIX: &str = "gw:lock:acct_token_refresh:";

const REFRESH_LOCK_TTL_MS: i64 = 15_000;
const LOCK_WAIT_POLL_MS: i64 = 200;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AuthMaterial {
    pub(crate) authorization: String,
    pub(crate) chatgpt_account_id: Option<String>,
    pub(crate) expires_at_ms: i64,
}

pub(crate) async fn get(
    conn: &mut redis::aio::ConnectionManager,
    accounts_root: &Path,
    account_id: &str,
    token_safety_window_seconds: i64,
) -> anyhow::Result<AuthMaterial> {
    let start_ms = now_ms();
    if token_safety_window_seconds < 0 {
        anyhow::bail!("token_safety_window_seconds must be >= 0");
    }
    let safety_ms = token_safety_window_seconds.saturating_mul(1000);

    if let Some(material) = get_cached(conn, account_id).await?
        && material.expires_at_ms.saturating_sub(start_ms) > safety_ms
    {
        return Ok(material);
    }

    let lock_key = format!("{TOKEN_REFRESH_LOCK_KEY_PREFIX}{account_id}");
    let lock_value = random_value()?;
    let acquired: Option<String> = redis::cmd("SET")
        .arg(&lock_key)
        .arg(&lock_value)
        .arg("NX")
        .arg("PX")
        .arg(REFRESH_LOCK_TTL_MS)
        .query_async(conn)
        .await?;

    if acquired.is_some() {
        let material =
            load_from_auth(accounts_root, account_id, token_safety_window_seconds).await?;
        put_cached(conn, account_id, &material, token_safety_window_seconds).await?;
        return Ok(material);
    }

    let deadline_ms = start_ms.saturating_add(REFRESH_LOCK_TTL_MS);
    loop {
        tokio::time::sleep(Duration::from_millis(
            u64::try_from(LOCK_WAIT_POLL_MS).unwrap_or(0),
        ))
        .await;

        if let Some(material) = get_cached(conn, account_id).await?
            && material.expires_at_ms.saturating_sub(now_ms()) > safety_ms
        {
            return Ok(material);
        }

        if now_ms() >= deadline_ms {
            break;
        }
    }

    let material = load_from_auth(accounts_root, account_id, token_safety_window_seconds).await?;
    put_cached(conn, account_id, &material, token_safety_window_seconds).await?;
    Ok(material)
}

async fn get_cached(
    conn: &mut redis::aio::ConnectionManager,
    account_id: &str,
) -> anyhow::Result<Option<AuthMaterial>> {
    let key = format!("{TOKEN_CACHE_KEY_PREFIX}{account_id}");
    let value: Option<String> = redis::cmd("GET").arg(&key).query_async(conn).await?;
    let Some(value) = value else {
        return Ok(None);
    };
    let parsed: AuthMaterial = serde_json::from_str(&value)
        .with_context(|| format!("parsing redis token cache {key:?}"))?;
    Ok(Some(parsed))
}

async fn put_cached(
    conn: &mut redis::aio::ConnectionManager,
    account_id: &str,
    material: &AuthMaterial,
    token_safety_window_seconds: i64,
) -> anyhow::Result<()> {
    let key = format!("{TOKEN_CACHE_KEY_PREFIX}{account_id}");
    let now_ms = now_ms();
    let ttl_seconds =
        (material.expires_at_ms.saturating_sub(now_ms) / 1000) - token_safety_window_seconds;
    if ttl_seconds <= 0 {
        anyhow::bail!(
            "refusing to cache expired/near-expiry access token for account {account_id:?}"
        );
    }
    let value = serde_json::to_string(material).context("serializing AuthMaterial")?;
    let _: () = redis::cmd("SET")
        .arg(&key)
        .arg(value)
        .arg("EX")
        .arg(ttl_seconds)
        .query_async(conn)
        .await?;
    Ok(())
}

async fn load_from_auth(
    accounts_root: &Path,
    account_id: &str,
    token_safety_window_seconds: i64,
) -> anyhow::Result<AuthMaterial> {
    let account_home = accounts_root.join(account_id);
    let auth_manager = AuthManager::new(
        account_home.to_path_buf(),
        false,
        AuthCredentialsStoreMode::File,
    );
    let Some(mut auth) = auth_manager.auth().await else {
        anyhow::bail!("missing auth for account {account_id:?}");
    };

    let mut token_data = auth
        .get_token_data()
        .with_context(|| format!("reading token data for account {account_id:?}"))?;
    let mut expires_at_ms = jwt_exp_ms(&token_data.access_token)
        .with_context(|| format!("parsing access token exp for account {account_id:?}"))?;

    let safety_ms = token_safety_window_seconds.saturating_mul(1000);
    let now_ms = now_ms();
    if expires_at_ms.saturating_sub(now_ms) <= safety_ms {
        auth_manager
            .refresh_token()
            .await
            .with_context(|| format!("refreshing access token for account {account_id:?}"))?;
        let Some(refreshed_auth) = auth_manager.auth().await else {
            anyhow::bail!("missing auth for account {account_id:?}");
        };
        auth = refreshed_auth;
        token_data = auth.get_token_data().with_context(|| {
            format!("reading token data after refresh for account {account_id:?}")
        })?;
        expires_at_ms = jwt_exp_ms(&token_data.access_token).with_context(|| {
            format!("parsing access token exp after refresh for {account_id:?}")
        })?;
    }

    Ok(AuthMaterial {
        authorization: format!("Bearer {}", token_data.access_token),
        chatgpt_account_id: token_data.id_token.chatgpt_account_id,
        expires_at_ms,
    })
}

fn jwt_exp_ms(jwt: &str) -> anyhow::Result<i64> {
    #[derive(Deserialize)]
    struct Claims {
        exp: i64,
    }

    let mut parts = jwt.split('.');
    let _header_b64 = parts.next().context("missing jwt header")?;
    let payload_b64 = parts.next().context("missing jwt payload")?;
    let _sig_b64 = parts.next().context("missing jwt signature")?;

    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .context("decoding jwt payload")?;
    let claims: Claims = serde_json::from_slice(&payload).context("parsing jwt payload json")?;
    Ok(claims.exp.saturating_mul(1000))
}

fn random_value() -> anyhow::Result<String> {
    let mut bytes = [0u8; 16];
    let mut rng = rand::rngs::OsRng;
    rng.try_fill_bytes(&mut bytes)
        .context("generating random bytes")?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}
