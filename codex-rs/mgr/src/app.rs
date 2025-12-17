use anyhow::Context;
use clap::Args;
use clap::Parser;
use clap::Subcommand;
use std::ffi::OsString;
use std::path::PathBuf;

use crate::accounts;
use crate::run_cmd;
use crate::serve;

const DEFAULT_SHARED_DIRNAME: &str = ".codex-shared";
const DEFAULT_ACCOUNTS_DIRNAME: &str = ".codex-accounts";
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
    let cli = Cli::parse();

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
        Commands::Serve => serve::run(&state_root).await,
    }
}
