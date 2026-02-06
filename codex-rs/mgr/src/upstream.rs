use anyhow::Context;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;

pub(crate) fn resolve_codex_binary(codex_path: Option<&PathBuf>) -> PathBuf {
    codex_path
        .cloned()
        .unwrap_or_else(|| PathBuf::from("codex"))
}

pub(crate) fn exec_upstream(
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

pub(crate) fn is_help_or_version(args: &[OsString]) -> bool {
    args.iter()
        .any(|a| a == "--help" || a == "-h" || a == "--version" || a == "-V")
}

pub(crate) fn is_login_command(args: &[OsString]) -> bool {
    args.first().is_some_and(|a| a == "login")
}

pub(crate) fn is_logout_command(args: &[OsString]) -> bool {
    args.first().is_some_and(|a| a == "logout")
}
