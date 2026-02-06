use anyhow::Context;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;

use crate::label::validate_label;
use crate::layout::ensure_shared_config;
use crate::layout::ensure_shared_layout;
use crate::upstream;
use crate::usage;

pub(crate) struct RunOptions {
    pub(crate) auto: bool,
    pub(crate) label: Option<String>,
    pub(crate) refresh: bool,
    pub(crate) no_cache: bool,
    pub(crate) upstream_args: Vec<OsString>,
}

pub(crate) async fn run(
    codex_path: Option<&PathBuf>,
    shared_root: &Path,
    accounts_root: &Path,
    state_root: &Path,
    args: RunOptions,
) -> anyhow::Result<()> {
    let codex = upstream::resolve_codex_binary(codex_path);

    if upstream::is_help_or_version(&args.upstream_args) {
        upstream::exec_upstream(codex, None, args.upstream_args)?;
        return Ok(());
    }

    if upstream::is_login_command(&args.upstream_args) {
        anyhow::bail!("upstream `codex login` is disabled; use `codex-mgr login --label ...`");
    }

    ensure_shared_config(shared_root).context("ensure shared config")?;

    let pinned = args.label.is_some();
    let label = if args.auto || !pinned {
        usage::select_best_label(
            shared_root,
            accounts_root,
            state_root,
            args.refresh,
            args.no_cache,
        )
        .await?
    } else {
        let label = args
            .label
            .context("label is required unless --auto is used")?;
        validate_label(&label)?;
        label
    };

    let account_home = accounts_root.join(&label);
    ensure_shared_layout(&account_home, shared_root).context("ensure shared layout")?;

    if upstream::is_logout_command(&args.upstream_args) && !pinned {
        anyhow::bail!(
            "upstream `codex logout` is disabled for auto selection; use `codex-mgr accounts del {label}` or `codex-mgr run --label {label} -- logout`"
        );
    }

    upstream::exec_upstream(codex, Some(account_home), args.upstream_args)?;
    Ok(())
}
