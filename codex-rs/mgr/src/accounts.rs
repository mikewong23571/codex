use anyhow::Context;
use codex_core::auth::AuthDotJson;
use serde::Serialize;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use crate::label::validate_label;
use crate::layout::ensure_shared_layout;
use crate::state::load_state;
use crate::state::save_state;
use crate::time::now_ms;
use crate::upstream;
use crate::usage;

#[derive(Debug, Clone, Serialize)]
struct AccountsListRow {
    label: String,
    email: Option<String>,
    workspace_id: Option<String>,
    five_hour_remaining_percent: Option<f64>,
    weekly_remaining_percent: Option<f64>,
    snapshot_age_seconds: Option<i64>,
    status: String,
}

pub(crate) async fn login(
    codex_path: Option<&PathBuf>,
    shared_root: &Path,
    accounts_root: &Path,
    state_root: &Path,
    label: String,
) -> anyhow::Result<()> {
    validate_label(&label)?;
    let account_home = accounts_root.join(&label);
    if account_home.exists() {
        anyhow::bail!("label {label} already exists");
    }
    std::fs::create_dir_all(&account_home).context("create account home")?;
    ensure_shared_layout(&account_home, shared_root).context("ensure shared layout")?;

    let codex = upstream::resolve_codex_binary(codex_path);
    let status = Command::new(codex)
        .arg("login")
        .env("CODEX_HOME", &account_home)
        .status()
        .context("spawning upstream codex login")?;
    if !status.success() {
        anyhow::bail!("upstream codex login failed for label {label}");
    }

    let auth_path = account_home.join("auth.json");
    let auth_contents = std::fs::read_to_string(&auth_path)
        .with_context(|| format!("reading {auth_path:?} after login"))?;
    let parsed: AuthDotJson = serde_json::from_str(&auth_contents)
        .with_context(|| format!("parsing {auth_path:?} after login"))?;
    let refresh_ok = parsed
        .tokens
        .as_ref()
        .is_some_and(|t| !t.refresh_token.trim().is_empty());
    if !refresh_ok {
        anyhow::bail!("login completed but auth.json is missing refresh_token for label {label}");
    }

    let mut state = load_state(state_root).unwrap_or_default();
    if !state.labels.iter().any(|l| l == &label) {
        state.labels.push(label);
        state.labels.sort();
        save_state(state_root, &state).context("save state")?;
    }

    Ok(())
}

pub(crate) async fn list(
    accounts_root: &Path,
    state_root: &Path,
    json: bool,
) -> anyhow::Result<()> {
    let now_ms = now_ms();
    let state = load_state(state_root).unwrap_or_default();

    let mut rows = Vec::new();
    for label in list_labels(accounts_root)? {
        let account_home = accounts_root.join(&label);
        let auth_path = account_home.join("auth.json");

        let (email, workspace_id, auth_present) = match read_auth_dot_json(&auth_path) {
            Ok(Some(auth)) => {
                let info = auth
                    .tokens
                    .as_ref()
                    .map(|t| (&t.id_token.email, &t.id_token.chatgpt_account_id));
                let (email, workspace_id) = match info {
                    Some((email, workspace_id)) => (email.clone(), workspace_id.clone()),
                    None => (None, None),
                };
                (email, workspace_id, true)
            }
            Ok(None) => (None, None, false),
            Err(_) => (None, None, true),
        };

        let cached = state.usage_cache.get(&label);
        let snapshot_age_seconds = cached.map(|c| (now_ms - c.captured_at_ms) / 1000);

        let five_hour_remaining_percent =
            cached.and_then(|c| c.snapshot.five_hour.as_ref().map(|w| w.remaining_percent));
        let weekly_remaining_percent =
            cached.and_then(|c| c.snapshot.weekly.as_ref().map(|w| w.remaining_percent));

        let status = if !auth_present {
            "auth_missing".to_string()
        } else if cached.is_none() {
            "usage_unknown".to_string()
        } else if snapshot_age_seconds.is_some_and(|age| age > usage::USAGE_CACHE_TTL_SECONDS) {
            "stale".to_string()
        } else {
            "ok".to_string()
        };

        rows.push(AccountsListRow {
            label,
            email,
            workspace_id,
            five_hour_remaining_percent,
            weekly_remaining_percent,
            snapshot_age_seconds,
            status,
        });
    }

    if json {
        let out = serde_json::to_string_pretty(&rows)?;
        println!("{out}");
        return Ok(());
    }

    let mut label_w = "label".len();
    let mut email_w = "email".len();
    for row in &rows {
        label_w = label_w.max(row.label.len());
        email_w = email_w.max(row.email.as_deref().unwrap_or("unknown").len());
    }

    println!(
        "{:<12} {:<label_w$} {:<email_w$} {:>8} {:>8} {:>6}",
        "status",
        "label",
        "email",
        "weekly",
        "5h",
        "age",
        label_w = label_w,
        email_w = email_w
    );

    for row in rows {
        let email = row.email.as_deref().unwrap_or("unknown");
        let weekly = row
            .weekly_remaining_percent
            .map(|p| format!("{p:.0}%"))
            .unwrap_or_else(|| "unknown".to_string());
        let five = row
            .five_hour_remaining_percent
            .map(|p| format!("{p:.0}%"))
            .unwrap_or_else(|| "unknown".to_string());
        let age = row
            .snapshot_age_seconds
            .map(|s| s.to_string())
            .unwrap_or_else(|| "-".to_string());

        println!(
            "{:<12} {:<label_w$} {:<email_w$} {:>8} {:>8} {:>6}",
            row.status,
            row.label,
            email,
            weekly,
            five,
            age,
            label_w = label_w,
            email_w = email_w
        );
    }

    Ok(())
}

pub(crate) async fn del(
    accounts_root: &Path,
    state_root: &Path,
    label: String,
) -> anyhow::Result<()> {
    validate_label(&label)?;
    let account_home = accounts_root.join(&label);
    if !account_home.exists() {
        anyhow::bail!("label {label} does not exist");
    }

    let auth_path = account_home.join("auth.json");
    let _ = std::fs::remove_file(&auth_path);

    if let Ok(mut state) = load_state(state_root) {
        state.labels.retain(|l| l != &label);
        state.usage_cache.remove(&label);
        let _ = save_state(state_root, &state);
    }

    Ok(())
}

pub(crate) fn list_labels(accounts_root: &Path) -> anyhow::Result<Vec<String>> {
    let mut labels = Vec::new();
    for entry in std::fs::read_dir(accounts_root).context("read accounts_root")? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with('.') {
                labels.push(name);
            }
        }
    }
    labels.sort();
    Ok(labels)
}

fn read_auth_dot_json(path: &Path) -> anyhow::Result<Option<AuthDotJson>> {
    let contents = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    Ok(Some(serde_json::from_str(&contents)?))
}
