use anyhow::Context;
use clap::Args;
use clap::Parser;
use clap::Subcommand;
use codex_backend_client::Client as BackendClient;
use codex_core::CodexAuth;
use codex_core::auth::AuthCredentialsStoreMode;
use codex_core::auth::AuthDotJson;
use codex_protocol::protocol::RateLimitSnapshot;
use codex_protocol::protocol::RateLimitWindow;
use futures::StreamExt;
use futures::stream;
use serde::Deserialize;
use serde::Serialize;
use std::ffi::OsString;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

#[cfg(unix)]
use std::os::unix::fs as unix_fs;

const DEFAULT_SHARED_DIRNAME: &str = ".codex-shared";
const DEFAULT_ACCOUNTS_DIRNAME: &str = ".codex-accounts";
const DEFAULT_STATE_DIRNAME: &str = ".codex-mgr";

const DEFAULT_CHATGPT_BASE_URL: &str = "https://chatgpt.com/backend-api/";
const USAGE_CACHE_TTL_SECONDS: i64 = 900;
const USAGE_CACHE_TTL_MS: i64 = 900_000;
const USAGE_FETCH_CONCURRENCY: i64 = 5;
const LABEL_MAX_LEN: usize = 64;

#[derive(Parser, Debug)]
#[command(name = "codex-mgr")]
#[command(about = "Multi-account launcher/manager for Codex (ChatGPT login).")]
struct Cli {
    /// Path to the upstream `codex` binary. If unset, `codex` is resolved via PATH.
    #[arg(long, global = true)]
    codex_path: Option<PathBuf>,

    /// Root directory for shared non-auth Codex state.
    #[arg(long, global = true)]
    shared_root: Option<PathBuf>,

    /// Root directory for per-account auth homes.
    #[arg(long, global = true)]
    accounts_root: Option<PathBuf>,

    /// Root directory for codex-mgr state (cache, metadata).
    #[arg(long, global = true)]
    state_root: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Clone)]
struct GlobalOpts {
    codex_path: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Login(LoginArgs),
    Accounts(AccountsArgs),
    Run(RunArgs),
}

#[derive(Args, Debug)]
struct LoginArgs {
    /// Local label for this account (unique).
    #[arg(long)]
    label: String,
}

#[derive(Args, Debug)]
struct AccountsArgs {
    #[command(subcommand)]
    command: AccountsCommands,
}

#[derive(Subcommand, Debug)]
enum AccountsCommands {
    List(AccountsListArgs),
    Del(AccountsDelArgs),
}

#[derive(Args, Debug)]
struct AccountsListArgs {
    /// Output JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct AccountsDelArgs {
    label: String,
}

#[derive(Args, Debug)]
struct RunArgs {
    /// Select an account automatically based on usage.
    #[arg(long, conflicts_with = "label")]
    auto: bool,

    /// Use a specific account label.
    #[arg(long)]
    label: Option<String>,

    /// Force a token refresh before fetching usage.
    #[arg(long)]
    refresh: bool,

    /// Ignore cached usage snapshots.
    #[arg(long)]
    no_cache: bool,

    /// Arguments passed through to the upstream `codex` binary after `--`.
    #[arg(trailing_var_arg = true)]
    args: Vec<OsString>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ManagerState {
    labels: Vec<String>,
    usage_cache: std::collections::BTreeMap<String, CachedUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedUsage {
    captured_at_ms: i64,
    snapshot: UsageSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UsageSnapshot {
    five_hour: Option<WindowSnapshot>,
    weekly: Option<WindowSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WindowSnapshot {
    used_percent: f64,
    remaining_percent: f64,
    window_minutes: Option<i64>,
    resets_at: Option<i64>,
}

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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let global_opts = GlobalOpts {
        codex_path: cli.codex_path.clone(),
    };

    let home = dirs::home_dir().context("failed to resolve home directory")?;
    let shared_root = cli
        .shared_root
        .clone()
        .unwrap_or_else(|| home.join(DEFAULT_SHARED_DIRNAME));
    let accounts_root = cli
        .accounts_root
        .clone()
        .unwrap_or_else(|| home.join(DEFAULT_ACCOUNTS_DIRNAME));
    let state_root = cli
        .state_root
        .clone()
        .unwrap_or_else(|| home.join(DEFAULT_STATE_DIRNAME));

    std::fs::create_dir_all(&shared_root).context("creating shared_root")?;
    std::fs::create_dir_all(&accounts_root).context("creating accounts_root")?;
    std::fs::create_dir_all(&state_root).context("creating state_root")?;

    match cli.command {
        Commands::Login(args) => {
            cmd_login(
                &global_opts,
                &shared_root,
                &accounts_root,
                &state_root,
                args,
            )
            .await
        }
        Commands::Accounts(args) => {
            cmd_accounts(
                &global_opts,
                &shared_root,
                &accounts_root,
                &state_root,
                args,
            )
            .await
        }
        Commands::Run(args) => {
            cmd_run(
                &global_opts,
                &shared_root,
                &accounts_root,
                &state_root,
                args,
            )
            .await
        }
    }
}

async fn cmd_login(
    global_opts: &GlobalOpts,
    shared_root: &Path,
    accounts_root: &Path,
    state_root: &Path,
    args: LoginArgs,
) -> anyhow::Result<()> {
    let label = args.label;
    validate_label(&label)?;
    let account_home = accounts_root.join(&label);
    if account_home.exists() {
        anyhow::bail!("label {label} already exists");
    }
    std::fs::create_dir_all(&account_home).context("create account home")?;
    ensure_shared_layout(&account_home, shared_root).context("ensure shared layout")?;

    let codex = resolve_codex_binary(global_opts);
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

async fn cmd_accounts(
    global_opts: &GlobalOpts,
    shared_root: &Path,
    accounts_root: &Path,
    state_root: &Path,
    args: AccountsArgs,
) -> anyhow::Result<()> {
    match args.command {
        AccountsCommands::List(list) => {
            cmd_accounts_list(global_opts, shared_root, accounts_root, state_root, list).await
        }
        AccountsCommands::Del(del) => {
            cmd_accounts_del(shared_root, accounts_root, state_root, del).await
        }
    }
}

async fn cmd_accounts_list(
    _global_opts: &GlobalOpts,
    _shared_root: &Path,
    accounts_root: &Path,
    state_root: &Path,
    args: AccountsListArgs,
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
        } else if snapshot_age_seconds.is_some_and(|age| age > USAGE_CACHE_TTL_SECONDS) {
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

    if args.json {
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

async fn cmd_accounts_del(
    _shared_root: &Path,
    accounts_root: &Path,
    state_root: &Path,
    args: AccountsDelArgs,
) -> anyhow::Result<()> {
    let label = args.label;
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

async fn cmd_run(
    global_opts: &GlobalOpts,
    shared_root: &Path,
    accounts_root: &Path,
    state_root: &Path,
    args: RunArgs,
) -> anyhow::Result<()> {
    let codex = resolve_codex_binary(global_opts);
    let upstream_args = args.args;

    if is_help_or_version(&upstream_args) {
        exec_upstream(codex, None, upstream_args)?;
        return Ok(());
    }

    if is_login_command(&upstream_args) {
        anyhow::bail!("upstream `codex login` is disabled; use `codex-mgr login --label ...`");
    }

    ensure_shared_config(shared_root).context("ensure shared config")?;

    let pinned_label = args.label.clone();
    let label = if args.auto || pinned_label.is_none() {
        select_best_label(
            shared_root,
            accounts_root,
            state_root,
            args.refresh,
            args.no_cache,
        )
        .await?
    } else {
        let label = pinned_label
            .clone()
            .context("label is required unless --auto is used")?;
        validate_label(&label)?;
        label
    };

    let account_home = accounts_root.join(&label);
    ensure_shared_layout(&account_home, shared_root).context("ensure shared layout")?;

    if is_logout_command(&upstream_args) && pinned_label.is_none() {
        anyhow::bail!(
            "upstream `codex logout` is disabled for auto selection; use `codex-mgr accounts del {label}` or `codex-mgr run --label {label} -- logout`"
        );
    }

    exec_upstream(codex, Some(account_home), upstream_args)?;
    Ok(())
}

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

async fn select_best_label(
    shared_root: &Path,
    accounts_root: &Path,
    state_root: &Path,
    refresh: bool,
    no_cache: bool,
) -> anyhow::Result<String> {
    let labels = list_labels(accounts_root)?;
    if labels.is_empty() {
        anyhow::bail!("no accounts found; run `codex-mgr login --label ...` first");
    }

    // We keep base_url simple and deterministic for v1.
    let chatgpt_base_url = load_chatgpt_base_url(shared_root)
        .await
        .unwrap_or_else(|_| DEFAULT_CHATGPT_BASE_URL.to_string());

    let mut state = load_state(state_root).unwrap_or_default();
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

    save_state(state_root, &state).ok();

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

async fn load_chatgpt_base_url(shared_root: &Path) -> anyhow::Result<String> {
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

fn list_labels(accounts_root: &Path) -> anyhow::Result<Vec<String>> {
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

fn ensure_shared_layout(account_home: &Path, shared_root: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        let entries: [(&str, bool); 10] = [
            ("config.toml", false),
            ("managed_config.toml", false),
            ("history.jsonl", false),
            ("prompts", true),
            ("log", true),
            ("sessions", true),
            ("archived_sessions", true),
            ("models_cache.json", false),
            (".credentials.json", false),
            ("version.json", false),
        ];

        for (name, is_dir) in entries {
            let link_path = account_home.join(name);
            let target = shared_root.join(name);

            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating parent dir {parent:?}"))?;
            }

            let metadata = match std::fs::symlink_metadata(&link_path) {
                Ok(meta) => Some(meta),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
                Err(err) => return Err(err).with_context(|| format!("stat {link_path:?}")),
            };

            if let Some(meta) = metadata {
                // Some upstream writes are done via write-to-temp + rename, which can replace a
                // symlink with a regular file. To keep the account home clean, we repair such
                // paths by moving/copying the materialized data back into `shared_root` and then
                // restoring the symlink.
                if !meta.file_type().is_symlink() {
                    if is_dir {
                        // For directories, only repair if the shared target does not exist or is
                        // empty. Otherwise, fail fast to avoid unreviewed merges.
                        if target.exists() {
                            let target_empty = std::fs::read_dir(&target)
                                .with_context(|| format!("read_dir {target:?}"))?
                                .next()
                                .is_none();
                            if !target_empty {
                                anyhow::bail!(
                                    "expected {link_path:?} to be a symlink to {target:?}, but it exists as a directory and the target is non-empty"
                                );
                            }
                            std::fs::remove_dir_all(&target)
                                .with_context(|| format!("remove empty {target:?}"))?;
                        }

                        std::fs::rename(&link_path, &target)
                            .with_context(|| format!("move {link_path:?} -> {target:?}"))?;
                        unix_fs::symlink(&target, &link_path).with_context(|| {
                            format!("creating symlink {link_path:?} -> {target:?}")
                        })?;
                        continue;
                    }

                    // For files, overwrite the shared target with the local contents (last
                    // writer wins) and restore the symlink.
                    if let Some(parent) = target.parent() {
                        std::fs::create_dir_all(parent)
                            .with_context(|| format!("creating parent dir {parent:?}"))?;
                    }
                    std::fs::copy(&link_path, &target)
                        .with_context(|| format!("copy {link_path:?} -> {target:?}"))?;
                    std::fs::remove_file(&link_path)
                        .with_context(|| format!("remove {link_path:?}"))?;
                    unix_fs::symlink(&target, &link_path)
                        .with_context(|| format!("creating symlink {link_path:?} -> {target:?}"))?;
                    continue;
                }

                let actual_target = std::fs::read_link(&link_path)
                    .with_context(|| format!("readlink {link_path:?}"))?;
                if actual_target != target {
                    anyhow::bail!(
                        "expected symlink {link_path:?} -> {target:?}, but found {actual_target:?}"
                    );
                }
                continue;
            }

            if is_dir {
                std::fs::create_dir_all(&target)
                    .with_context(|| format!("creating shared dir {target:?}"))?;
            }
            unix_fs::symlink(&target, &link_path)
                .with_context(|| format!("creating symlink {link_path:?} -> {target:?}"))?;
        }

        Ok(())
    }

    #[cfg(not(unix))]
    {
        let _ = (account_home, shared_root);
        anyhow::bail!("unsupported platform (v1 supports unix only)");
    }
}

fn ensure_shared_config(shared_root: &Path) -> anyhow::Result<()> {
    let path = shared_root.join("config.toml");
    let cwd = std::env::current_dir().context("resolving current directory")?;

    let file_name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "config.toml".to_string());
    let pid = std::process::id();

    for attempt in 0..10_i64 {
        let existing = std::fs::read_to_string(&path);
        let (old_text, existed) = match existing {
            Ok(s) => (Some(s), true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => (None, false),
            Err(err) => return Err(err).with_context(|| format!("reading shared config {path:?}")),
        };

        let mut root: toml::Value = match old_text.as_deref() {
            Some(contents) => toml::from_str(contents)
                .with_context(|| format!("parsing shared config {path:?}"))?,
            None => toml::Value::Table(toml::map::Map::new()),
        };

        let table = root
            .as_table_mut()
            .context("shared config root is not a table")?;
        let projects_entry = table
            .entry("projects")
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
        let projects = projects_entry
            .as_table_mut()
            .context("shared config projects is not a table")?;

        let key = cwd.to_string_lossy().to_string();
        if projects.contains_key(&key) {
            return Ok(());
        }

        let mut t = toml::map::Map::new();
        t.insert(
            "trust_level".to_string(),
            toml::Value::String("trusted".to_string()),
        );
        projects.insert(key, toml::Value::Table(t));

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating shared config parent {parent:?}"))?;
        }

        let tmp = path.with_file_name(format!("{file_name}.tmp.{pid}.{attempt}"));
        let out = toml::to_string_pretty(&root).context("rendering shared config")?;
        std::fs::write(&tmp, out.as_bytes()).with_context(|| format!("writing temp {tmp:?}"))?;

        if existed {
            let current = std::fs::read_to_string(&path);
            match current {
                Ok(cur) if old_text.as_ref().is_some_and(|old| old == &cur) => {
                    std::fs::rename(&tmp, &path)
                        .with_context(|| format!("replacing shared config {path:?}"))?;
                    return Ok(());
                }
                Ok(_) => {
                    let _ = std::fs::remove_file(&tmp);
                    continue;
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    let _ = std::fs::remove_file(&tmp);
                    continue;
                }
                Err(err) => {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(err).with_context(|| format!("re-reading shared config {path:?}"));
                }
            }
        } else if path.exists() {
            let _ = std::fs::remove_file(&tmp);
            continue;
        } else {
            std::fs::rename(&tmp, &path)
                .with_context(|| format!("creating shared config {path:?}"))?;
            return Ok(());
        }
    }

    anyhow::bail!("failed to update shared config due to concurrent modifications");
}

fn load_state(state_root: &Path) -> anyhow::Result<ManagerState> {
    let path = state_root.join("state.json");
    let contents = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ManagerState::default());
        }
        Err(err) => return Err(err.into()),
    };
    Ok(serde_json::from_str(&contents)?)
}

fn save_state(state_root: &Path, state: &ManagerState) -> anyhow::Result<()> {
    let path = state_root.join("state.json");
    let tmp = state_root.join("state.json.tmp");
    let mut f = File::create(&tmp)?;
    let out = serde_json::to_vec_pretty(state)?;
    f.write_all(&out)?;
    f.write_all(b"\n")?;
    f.sync_all()?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

fn resolve_codex_binary(global_opts: &GlobalOpts) -> PathBuf {
    global_opts
        .codex_path
        .clone()
        .unwrap_or_else(|| PathBuf::from("codex"))
}

fn exec_upstream(
    codex: PathBuf,
    codex_home: Option<PathBuf>,
    args: Vec<OsString>,
) -> anyhow::Result<()> {
    let mut cmd = Command::new(codex);
    if let Some(home) = codex_home {
        cmd.env("CODEX_HOME", home);
    }
    let status = cmd.args(args).status().context("running upstream codex")?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("upstream codex exited with {status}")
    }
}

fn is_help_or_version(args: &[OsString]) -> bool {
    args.iter()
        .any(|a| a == "--help" || a == "-h" || a == "--version" || a == "-V")
}

fn is_login_command(args: &[OsString]) -> bool {
    args.first().is_some_and(|a| a == "login")
}

fn is_logout_command(args: &[OsString]) -> bool {
    args.first().is_some_and(|a| a == "logout")
}

fn now_ms() -> i64 {
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn validate_label(label: &str) -> anyhow::Result<()> {
    if label.is_empty() {
        anyhow::bail!("label must not be empty");
    }
    if label.len() > LABEL_MAX_LEN {
        anyhow::bail!("label is too long (max {LABEL_MAX_LEN})");
    }

    if label == "." || label == ".." {
        anyhow::bail!("label {label:?} is not allowed");
    }

    if label
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !label.starts_with('.')
    {
        return Ok(());
    }

    anyhow::bail!(
        "invalid label {label:?}; use only ASCII letters/numbers plus '-', '_' or '.', and do not start with '.'"
    );
}
