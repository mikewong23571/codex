use anyhow::Context;
use std::ffi::OsString;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
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
    #[cfg(unix)]
    {
        let mut cmd = build_upstream_command(codex, codex_home, args);
        let err = cmd.exec();
        Err(err).context("exec upstream codex")
    }

    #[cfg(not(unix))]
    {
        let mut cmd = build_upstream_command(codex, codex_home, args);
        let status = cmd.status().context("running upstream codex")?;
        if status.success() {
            Ok(())
        } else {
            anyhow::bail!("upstream codex exited with {status}")
        }
    }
}

fn build_upstream_command(
    codex: PathBuf,
    codex_home: Option<PathBuf>,
    args: Vec<OsString>,
) -> Command {
    let mut cmd = Command::new(codex);
    if let Some(home) = codex_home {
        cmd.env("CODEX_HOME", home);
    }
    cmd.args(args);
    cmd
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

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn build_upstream_command_sets_program_args_and_codex_home() {
        let codex = PathBuf::from("/tmp/codex");
        let codex_home = Some(PathBuf::from("/tmp/home"));
        let args = vec![
            OsString::from("run"),
            OsString::from("--model"),
            OsString::from("gpt-5.4"),
        ];

        let cmd = build_upstream_command(codex.clone(), codex_home.clone(), args.clone());

        assert_eq!(cmd.get_program(), codex.as_os_str());
        assert_eq!(
            cmd.get_args().collect::<Vec<_>>(),
            args.iter().collect::<Vec<_>>()
        );
        assert_eq!(
            cmd.get_envs().collect::<Vec<_>>(),
            vec![(
                OsString::from("CODEX_HOME").as_os_str(),
                codex_home.as_ref().map(|path| path.as_path().as_os_str()),
            )]
        );
    }
}
