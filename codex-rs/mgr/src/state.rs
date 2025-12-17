use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct ManagerState {
    pub(crate) labels: Vec<String>,
    pub(crate) usage_cache: BTreeMap<String, CachedUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CachedUsage {
    pub(crate) captured_at_ms: i64,
    pub(crate) snapshot: UsageSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct UsageSnapshot {
    pub(crate) five_hour: Option<WindowSnapshot>,
    pub(crate) weekly: Option<WindowSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WindowSnapshot {
    pub(crate) used_percent: f64,
    pub(crate) remaining_percent: f64,
    pub(crate) window_minutes: Option<i64>,
    pub(crate) resets_at: Option<i64>,
}

pub(crate) fn load_state(state_root: &Path) -> anyhow::Result<ManagerState> {
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

pub(crate) fn save_state(state_root: &Path, state: &ManagerState) -> anyhow::Result<()> {
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
