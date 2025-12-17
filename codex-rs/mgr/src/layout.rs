use anyhow::Context;
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs as unix_fs;

pub(crate) fn ensure_shared_layout(account_home: &Path, shared_root: &Path) -> anyhow::Result<()> {
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

pub(crate) fn ensure_shared_config(shared_root: &Path) -> anyhow::Result<()> {
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
