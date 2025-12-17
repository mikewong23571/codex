use anyhow::Context;
use codex_backend_client::Client as BackendClient;
use codex_core::CodexAuth;
use codex_core::auth::AuthCredentialsStoreMode;
use codex_protocol::protocol::RateLimitSnapshot;
use codex_protocol::protocol::RateLimitWindow;
use futures::StreamExt;
use futures::stream;
use serde::Deserialize;
use std::path::Path;

use crate::accounts;
use crate::layout::ensure_shared_layout;
use crate::state::CachedUsage;
use crate::state::UsageSnapshot;
use crate::state::WindowSnapshot;
use crate::time::now_ms;

const DEFAULT_CHATGPT_BASE_URL: &str = "https://chatgpt.com/backend-api/";
pub(crate) const USAGE_CACHE_TTL_SECONDS: i64 = 900;
const USAGE_CACHE_TTL_MS: i64 = 900_000;
const USAGE_FETCH_CONCURRENCY: i64 = 5;

#[derive(Clone, Copy, Debug)]
struct Score {
    weekly_present: bool,
    weekly_remaining: f64,
    five_present: bool,
    five_remaining: f64,
}

fn usage_score(snapshot: &UsageSnapshot) -> Option<Score> {
    let weekly = snapshot.weekly.as_ref().map(|w| w.remaining_percent);
    let five = snapshot.five_hour.as_ref().map(|w| w.remaining_percent);
    if weekly.is_none() && five.is_none() {
        return None;
    }
    let clamp = |v: f64| v.clamp(0.0, 100.0);
    Some(Score {
        weekly_present: weekly.is_some(),
        weekly_remaining: weekly.map(clamp).unwrap_or(-1.0),
        five_present: five.is_some(),
        five_remaining: five.map(clamp).unwrap_or(-1.0),
    })
}

pub(crate) async fn select_best_label(
    shared_root: &Path,
    accounts_root: &Path,
    state_root: &Path,
    refresh: bool,
    no_cache: bool,
) -> anyhow::Result<String> {
    let labels = accounts::list_labels(accounts_root)?;
    if labels.is_empty() {
        anyhow::bail!("no accounts found; run `codex-mgr login --label ...` first");
    }

    // We keep base_url simple and deterministic for v1.
    let chatgpt_base_url =
        load_chatgpt_base_url(shared_root).unwrap_or_else(|_| DEFAULT_CHATGPT_BASE_URL.to_string());

    let mut state = crate::state::load_state(state_root).unwrap_or_default();
    let now = now_ms();

    let mut best: Option<(String, Score)> = None;
    let mut to_fetch = Vec::new();

    for label in labels {
        let account_home = accounts_root.join(&label);
        ensure_shared_layout(&account_home, shared_root).context("ensure shared layout")?;

        if !no_cache
            && let Some(cached) = state.usage_cache.get(&label)
            && (now - cached.captured_at_ms) <= USAGE_CACHE_TTL_MS
            && let Some(score) = usage_score(&cached.snapshot)
        {
            best = pick_best(best, label.clone(), score);
            continue;
        }

        to_fetch.push(label);
    }

    let concurrency = usize::try_from(USAGE_FETCH_CONCURRENCY).unwrap_or(1);
    let stream = stream::iter(to_fetch.into_iter().map(|label| {
        let chatgpt_base_url = chatgpt_base_url.clone();
        let accounts_root = accounts_root.to_path_buf();
        async move {
            let account_home = accounts_root.join(&label);
            let auth_res =
                CodexAuth::from_auth_storage(&account_home, AuthCredentialsStoreMode::File);
            let Some(auth) = auth_res.ok().flatten() else {
                return (label, None);
            };

            let auth = if refresh {
                let _ = auth.refresh_token().await;
                auth
            } else {
                auth
            };

            let snapshot = fetch_usage_snapshot(&chatgpt_base_url, &auth).await.ok();
            (label, snapshot)
        }
    }))
    .buffer_unordered(concurrency);

    futures::pin_mut!(stream);
    while let Some((label, snapshot)) = stream.next().await {
        let Some(snapshot) = snapshot else { continue };

        let score = usage_score(&snapshot);
        state.usage_cache.insert(
            label.clone(),
            CachedUsage {
                captured_at_ms: now_ms(),
                snapshot,
            },
        );

        if let Some(score) = score {
            best = pick_best(best, label, score);
        }
    }

    crate::state::save_state(state_root, &state).ok();

    let Some((label, _score)) = best else {
        anyhow::bail!(
            "no usable accounts (usage unavailable); try `codex-mgr run --refresh --auto -- <args>` or re-login"
        );
    };
    Ok(label)
}

fn pick_best(
    current: Option<(String, Score)>,
    label: String,
    score: Score,
) -> Option<(String, Score)> {
    let key = |s: &Score| {
        (
            i32::from(s.weekly_present),
            s.weekly_remaining,
            i32::from(s.five_present),
            s.five_remaining,
        )
    };

    match current {
        Some((best_label, best_score)) => {
            let best_key = key(&best_score);
            let new_key = key(&score);
            if new_key > best_key || (new_key == best_key && label < best_label) {
                Some((label, score))
            } else {
                Some((best_label, best_score))
            }
        }
        None => Some((label, score)),
    }
}

async fn fetch_usage_snapshot(base_url: &str, auth: &CodexAuth) -> anyhow::Result<UsageSnapshot> {
    let client = BackendClient::from_auth(base_url.to_string(), auth).await?;
    let rl = client.get_rate_limits().await?;
    Ok(rate_limits_to_usage_snapshot(&rl))
}

fn rate_limits_to_usage_snapshot(rl: &RateLimitSnapshot) -> UsageSnapshot {
    let mut five_hour = None;
    let mut weekly = None;

    let mut consider = |window: &RateLimitWindow| {
        let used = window.used_percent.clamp(0.0, 100.0);
        let remaining = (100.0 - used).clamp(0.0, 100.0);
        let snapshot = WindowSnapshot {
            used_percent: used,
            remaining_percent: remaining,
            window_minutes: window.window_minutes,
            resets_at: window.resets_at,
        };

        match window.window_minutes {
            Some(minutes) if (minutes - 300).abs() <= 5 => five_hour = Some(snapshot),
            Some(minutes) if (minutes - 10_080).abs() <= 60 => weekly = Some(snapshot),
            Some(minutes) if minutes <= 24 * 60 && five_hour.is_none() => {
                five_hour = Some(snapshot)
            }
            Some(minutes) if minutes <= 7 * 24 * 60 && weekly.is_none() => weekly = Some(snapshot),
            _ => {}
        }
    };

    if let Some(primary) = rl.primary.as_ref() {
        consider(primary);
    }
    if let Some(secondary) = rl.secondary.as_ref() {
        consider(secondary);
    }

    UsageSnapshot { five_hour, weekly }
}

fn load_chatgpt_base_url(shared_root: &Path) -> anyhow::Result<String> {
    let config_path = shared_root.join("config.toml");
    let contents = std::fs::read_to_string(&config_path)
        .with_context(|| format!("reading {config_path:?}"))?;
    #[derive(Deserialize)]
    struct RawConfig {
        chatgpt_base_url: Option<String>,
    }
    let raw: RawConfig = toml::from_str(&contents).context("parsing shared config.toml")?;
    Ok(raw
        .chatgpt_base_url
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_CHATGPT_BASE_URL.to_string()))
}
