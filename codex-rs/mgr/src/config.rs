use anyhow::Context;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

const DEFAULT_LISTEN: &str = "127.0.0.1:8787";
const DEFAULT_UPSTREAM_BASE_URL: &str = "https://chatgpt.com/backend-api/";
const DEFAULT_REDIS_URL: &str = "redis://127.0.0.1:6379";
const DEFAULT_STICKY_TTL_SECONDS: i64 = 7200;
const DEFAULT_TOKEN_SAFETY_WINDOW_SECONDS: i64 = 120;

pub(crate) fn config_path(state_root: &Path) -> PathBuf {
    state_root.join("config.toml")
}

#[derive(Debug, Clone)]
pub(crate) struct ManagerConfig {
    pub(crate) gateway: GatewayConfig,
    pub(crate) pools: BTreeMap<String, PoolConfig>,
}

#[derive(Debug, Clone)]
pub(crate) struct GatewayConfig {
    pub(crate) listen: String,
    pub(crate) upstream_base_url: String,
    pub(crate) redis_url: String,
    pub(crate) sticky_ttl_seconds: i64,
    pub(crate) token_safety_window_seconds: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct PoolConfig {
    pub(crate) labels: Vec<String>,
    pub(crate) policy_key: Option<String>,
}

pub(crate) fn load(state_root: &Path) -> anyhow::Result<ManagerConfig> {
    let path = config_path(state_root);
    let text = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "reading config file {path:?} (create it; see docs/multi_account_gateway.md for an example)"
        )
    })?;

    #[derive(Deserialize)]
    struct RawConfig {
        gateway: Option<RawGatewayConfig>,
        #[serde(default)]
        pools: BTreeMap<String, RawPoolConfig>,
    }

    #[derive(Deserialize)]
    struct RawGatewayConfig {
        listen: Option<String>,
        upstream_base_url: Option<String>,
        redis_url: Option<String>,
        sticky_ttl_seconds: Option<i64>,
        token_safety_window_seconds: Option<i64>,
    }

    #[derive(Deserialize)]
    struct RawPoolConfig {
        labels: Vec<String>,
        policy_key: Option<String>,
    }

    let raw: RawConfig =
        toml::from_str(&text).with_context(|| format!("parsing config file {path:?}"))?;
    let gw = raw
        .gateway
        .context("missing [gateway] config section in config.toml")?;

    let gateway = GatewayConfig {
        listen: gw.listen.unwrap_or_else(|| DEFAULT_LISTEN.to_string()),
        upstream_base_url: gw
            .upstream_base_url
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_UPSTREAM_BASE_URL.to_string()),
        redis_url: gw
            .redis_url
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_REDIS_URL.to_string()),
        sticky_ttl_seconds: gw.sticky_ttl_seconds.unwrap_or(DEFAULT_STICKY_TTL_SECONDS),
        token_safety_window_seconds: gw
            .token_safety_window_seconds
            .unwrap_or(DEFAULT_TOKEN_SAFETY_WINDOW_SECONDS),
    };

    let pools = raw
        .pools
        .into_iter()
        .map(|(k, v)| {
            (
                k,
                PoolConfig {
                    labels: v.labels,
                    policy_key: v.policy_key,
                },
            )
        })
        .collect();

    Ok(ManagerConfig { gateway, pools })
}
