use anyhow::Context;
use clap::Args;
use clap::Parser;
use clap::Subcommand;
use std::ffi::OsString;
use std::path::PathBuf;

use crate::accounts;
use crate::gateway;
use crate::observability;
use crate::pools;
use crate::run_cmd;
use crate::serve;

const DEFAULT_STATE_DIRNAME: &str = ".codex-mgr";

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

#[derive(Subcommand, Debug)]
enum Commands {
    Login(LoginArgs),
    Accounts(AccountsArgs),
    Pools(PoolsArgs),
    Gateway(GatewayArgs),
    Run(RunArgs),
    Serve,
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
struct PoolsArgs {
    #[command(subcommand)]
    command: PoolsCommands,
}

#[derive(Subcommand, Debug)]
enum PoolsCommands {
    Set(PoolsSetArgs),
    List(PoolsListArgs),
    Del(PoolsDelArgs),
}

#[derive(Args, Debug)]
struct PoolsSetArgs {
    pool_id: String,

    /// Comma-separated account labels (e.g. --labels a,b,c).
    #[arg(long, value_delimiter = ',', num_args = 1..)]
    labels: Vec<String>,

    /// Optional selection policy key for this pool.
    #[arg(long)]
    policy_key: Option<String>,
}

#[derive(Args, Debug)]
struct PoolsListArgs {
    /// Output JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct PoolsDelArgs {
    pool_id: String,
}

#[derive(Args, Debug)]
struct GatewayArgs {
    #[command(subcommand)]
    command: GatewayCommands,
}

#[derive(Subcommand, Debug)]
enum GatewayCommands {
    Issue(GatewayIssueArgs),
    List(GatewayListArgs),
    Revoke(GatewayRevokeArgs),
}

#[derive(Args, Debug)]
struct GatewayIssueArgs {
    /// Pool id (configured via `codex-mgr pools set`).
    #[arg(long)]
    pool: String,

    /// TTL for this gateway token session (default: 86400).
    #[arg(long)]
    ttl_seconds: Option<i64>,

    /// Optional human note to store alongside the session.
    #[arg(long)]
    note: Option<String>,

    /// Output JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct GatewayListArgs {
    /// Output JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct GatewayRevokeArgs {
    token: String,
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

pub async fn run() -> anyhow::Result<()> {
    observability::init_tracing();
    let cli = Cli::parse();

    let home = dirs::home_dir().context("failed to resolve home directory")?;
    let state_root = cli
        .state_root
        .clone()
        .unwrap_or_else(|| home.join(DEFAULT_STATE_DIRNAME));

    let shared_root = cli
        .shared_root
        .clone()
        .unwrap_or_else(|| state_root.join("shared"));

    let accounts_root = cli
        .accounts_root
        .clone()
        .unwrap_or_else(|| state_root.join("accounts"));

    if cli.shared_root.is_none() {
        let legacy_shared = home.join(".codex-shared");
        if legacy_shared.exists() && !shared_root.exists() {
            tracing::warn!(
                "Legacy shared directory found at {:?}, but new location {:?} does not exist. Please move it: `mv {:?} {:?}`",
                legacy_shared,
                shared_root,
                legacy_shared,
                shared_root
            );
        }
    }

    if cli.accounts_root.is_none() {
        let legacy_accounts = home.join(".codex-accounts");
        if legacy_accounts.exists() && !accounts_root.exists() {
            tracing::warn!(
                "Legacy accounts directory found at {:?}, but new location {:?} does not exist. Please move it: `mv {:?} {:?}`",
                legacy_accounts,
                accounts_root,
                legacy_accounts,
                accounts_root
            );
        }
    }

    std::fs::create_dir_all(&shared_root).context("creating shared_root")?;
    std::fs::create_dir_all(&accounts_root).context("creating accounts_root")?;
    std::fs::create_dir_all(&state_root).context("creating state_root")?;

    match cli.command {
        Commands::Login(args) => {
            accounts::login(
                cli.codex_path.as_ref(),
                &shared_root,
                &accounts_root,
                &state_root,
                args.label,
            )
            .await
        }
        Commands::Accounts(args) => match args.command {
            AccountsCommands::List(list) => {
                accounts::list(&accounts_root, &state_root, list.json).await
            }
            AccountsCommands::Del(del) => {
                accounts::del(&accounts_root, &state_root, del.label).await
            }
        },
        Commands::Pools(args) => match args.command {
            PoolsCommands::Set(set) => {
                pools::set(
                    &state_root,
                    &accounts_root,
                    set.pool_id,
                    set.labels,
                    set.policy_key,
                )
                .await
            }
            PoolsCommands::List(list) => pools::list(&state_root, list.json).await,
            PoolsCommands::Del(del) => pools::del(&state_root, del.pool_id).await,
        },
        Commands::Gateway(args) => match args.command {
            GatewayCommands::Issue(issue) => {
                gateway::issue(
                    &state_root,
                    issue.pool,
                    issue.ttl_seconds,
                    issue.note,
                    issue.json,
                )
                .await
            }
            GatewayCommands::List(list) => gateway::list(&state_root, list.json).await,
            GatewayCommands::Revoke(revoke) => gateway::revoke(&state_root, revoke.token).await,
        },
        Commands::Run(args) => {
            run_cmd::run(
                cli.codex_path.as_ref(),
                &shared_root,
                &accounts_root,
                &state_root,
                run_cmd::RunOptions {
                    auto: args.auto,
                    label: args.label,
                    refresh: args.refresh,
                    no_cache: args.no_cache,
                    upstream_args: args.args,
                },
            )
            .await
        }
        Commands::Serve => serve::run(&state_root, &accounts_root).await,
    }
}
