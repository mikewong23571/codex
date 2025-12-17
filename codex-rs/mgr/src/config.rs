use anyhow::Context;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use toml::Value;

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

pub(crate) fn load_value_for_update(state_root: &Path) -> anyhow::Result<Value> {
    let path = config_path(state_root);
    match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("parsing config file {path:?}")),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Ok(Value::Table(toml::Table::new()))
        }
        Err(err) => Err(err).with_context(|| format!("reading config file {path:?}")),
    }
}

pub(crate) fn load_value_optional(state_root: &Path) -> anyhow::Result<Value> {
    let path = config_path(state_root);
    match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("parsing config file {path:?}")),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Ok(Value::Table(toml::Table::new()))
        }
        Err(err) => Err(err).with_context(|| format!("reading config file {path:?}")),
    }
}

pub(crate) fn write_value(state_root: &Path, root: &Value) -> anyhow::Result<()> {
    let path = config_path(state_root);
    let Some(parent) = path.parent() else {
        anyhow::bail!("invalid config path {path:?}");
    };
    std::fs::create_dir_all(parent).with_context(|| format!("creating parent dir {parent:?}"))?;

    let tmp = path.with_file_name("config.toml.tmp");
    let mut out = toml::to_string_pretty(root).context("rendering config.toml")?;
    if !out.ends_with('\n') {
        out.push('\n');
    }
    std::fs::write(&tmp, out.as_bytes()).with_context(|| format!("writing temp {tmp:?}"))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("replacing config {path:?}"))?;
    Ok(())
}

pub(crate) fn ensure_gateway_defaults(root: &mut Value) -> anyhow::Result<()> {
    let table = root.as_table_mut().context("config root is not a table")?;
    let gateway_value = table
        .entry("gateway")
        .or_insert_with(|| Value::Table(toml::Table::new()));
    let gateway = gateway_value
        .as_table_mut()
        .context("[gateway] is not a table")?;

    gateway
        .entry("listen")
        .or_insert_with(|| Value::String(DEFAULT_LISTEN.to_string()));
    gateway
        .entry("upstream_base_url")
        .or_insert_with(|| Value::String(DEFAULT_UPSTREAM_BASE_URL.to_string()));
    gateway
        .entry("redis_url")
        .or_insert_with(|| Value::String(DEFAULT_REDIS_URL.to_string()));
    gateway
        .entry("sticky_ttl_seconds")
        .or_insert_with(|| Value::Integer(DEFAULT_STICKY_TTL_SECONDS));
    gateway
        .entry("token_safety_window_seconds")
        .or_insert_with(|| Value::Integer(DEFAULT_TOKEN_SAFETY_WINDOW_SECONDS));

    Ok(())
}

pub(crate) fn set_pool(
    root: &mut Value,
    pool_id: &str,
    labels: &[String],
    policy_key: Option<&str>,
) -> anyhow::Result<()> {
    let table = root.as_table_mut().context("config root is not a table")?;
    let pools_value = table
        .entry("pools")
        .or_insert_with(|| Value::Table(toml::Table::new()));
    let pools = pools_value
        .as_table_mut()
        .context("[pools] is not a table")?;

    let existing_policy_key = pools
        .get(pool_id)
        .and_then(Value::as_table)
        .and_then(|t| t.get("policy_key"))
        .and_then(Value::as_str)
        .map(str::to_string);

    let policy_key = match policy_key {
        Some(value) if !value.trim().is_empty() => Some(value.to_string()),
        Some(_) => None,
        None => existing_policy_key,
    };

    let mut pool = toml::Table::new();
    pool.insert(
        "labels".to_string(),
        Value::Array(labels.iter().cloned().map(Value::String).collect()),
    );
    if let Some(policy_key) = policy_key {
        pool.insert("policy_key".to_string(), Value::String(policy_key));
    }
    pools.insert(pool_id.to_string(), Value::Table(pool));
    Ok(())
}

pub(crate) fn remove_pool(root: &mut Value, pool_id: &str) -> anyhow::Result<bool> {
    let Some(table) = root.as_table_mut() else {
        return Ok(false);
    };
    let Some(pools_value) = table.get_mut("pools") else {
        return Ok(false);
    };
    let Some(pools) = pools_value.as_table_mut() else {
        return Ok(false);
    };
    Ok(pools.remove(pool_id).is_some())
}

pub(crate) fn extract_pools(root: &Value) -> anyhow::Result<BTreeMap<String, PoolConfig>> {
    let Some(table) = root.as_table() else {
        return Ok(BTreeMap::new());
    };
    let Some(pools_value) = table.get("pools") else {
        return Ok(BTreeMap::new());
    };
    let Some(pools) = pools_value.as_table() else {
        return Ok(BTreeMap::new());
    };

    let mut out = BTreeMap::new();
    for (pool_id, value) in pools {
        let pool = value
            .as_table()
            .with_context(|| format!("[pools.{pool_id}] is not a table"))?;
        let labels = pool
            .get("labels")
            .and_then(Value::as_array)
            .with_context(|| format!("[pools.{pool_id}].labels is missing or not an array"))?
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .with_context(|| format!("[pools.{pool_id}].labels must contain only strings"))
            })
            .collect::<anyhow::Result<Vec<String>>>()?;
        let policy_key = pool
            .get("policy_key")
            .and_then(Value::as_str)
            .map(str::to_string);
        out.insert(pool_id.to_string(), PoolConfig { labels, policy_key });
    }

    Ok(out)
}
