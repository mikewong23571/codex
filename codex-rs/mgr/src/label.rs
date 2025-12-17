const LABEL_MAX_LEN: i64 = 64;

pub(crate) fn validate_label(label: &str) -> anyhow::Result<()> {
    if label.is_empty() {
        anyhow::bail!("label must not be empty");
    }

    let len = i64::try_from(label.len()).unwrap_or(i64::MAX);
    if len > LABEL_MAX_LEN {
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
