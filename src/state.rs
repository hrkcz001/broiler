//! Local state: the one persisted fact reconciliation needs.
//!
//! When two loaded profiles request identical values, current reads cannot tell them apart. The only
//! honest tie-break is "which of these did this installation last apply successfully, and was its
//! content the same then". That is what `state.json` holds — a profile id and the fingerprint of the
//! profile content at the moment it was verified — and nothing else.
//!
//! It is history, never a claim: a stored entry may only decide between candidates that current reads
//! already proved, and it is ignored the moment the profile file is edited. The file is small, is
//! written atomically, and a corrupt file is treated as "no history" rather than as a reason to stop.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// State schema version understood by this build.
pub const STATE_SCHEMA_VERSION: u32 = 1;

/// One monitor's applied history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppliedRecord {
    /// The profile id that was applied and verified.
    pub profile_id: String,
    /// The content fingerprint of that profile at the moment it was applied.
    pub fingerprint: String,
    /// When it was verified.
    pub at: String,
}

/// The whole local state file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub schema_version: u32,
    #[serde(default)]
    pub applied: BTreeMap<String, AppliedRecord>,
}

impl Default for State {
    fn default() -> State {
        State {
            schema_version: STATE_SCHEMA_VERSION,
            applied: BTreeMap::new(),
        }
    }
}

/// Load the state file. A missing file is empty state; an unusable one is empty state with `false`
/// so the caller can say so in the log instead of silently pretending the history never existed.
pub fn load(path: &Path) -> (State, bool) {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return (State::default(), true),
        Err(_) => return (State::default(), false),
    };
    match serde_json::from_str::<State>(&text) {
        Ok(state) if state.schema_version == STATE_SCHEMA_VERSION => (state, true),
        // A newer or older schema is not history this build can use; it is not overwritten here.
        _ => (State::default(), false),
    }
}

/// Write the state file atomically: a temporary file in the same directory, then a rename.
pub fn save(path: &Path, state: &State) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(state)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::Other, error.to_string()))?;
    let temporary = temporary_path(path);
    {
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(text.as_bytes())?;
        file.sync_all().ok();
    }
    match std::fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            Err(error)
        }
    }
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    name.push(".tmp");
    path.with_file_name(name)
}

impl State {
    /// Record a verified application. The timestamp and the fingerprint describe the profile as it was
    /// when the hardware confirmed it.
    pub fn record_applied(&mut self, monitor: &str, profile_id: &str, fingerprint: &str, at: &str) {
        self.applied.insert(
            monitor.to_string(),
            AppliedRecord {
                profile_id: profile_id.to_string(),
                fingerprint: fingerprint.to_string(),
                at: at.to_string(),
            },
        );
    }

    /// Forget a monitor's history, for a write whose outcome left the hardware unverified.
    pub fn forget(&mut self, monitor: &str) {
        self.applied.remove(monitor);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let mut directory = std::env::temp_dir();
        directory.push(format!("broiler-state-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("scratch");
        directory
    }

    #[test]
    fn a_missing_file_is_empty_state_not_an_error() {
        let directory = scratch("missing");
        let (state, usable) = load(&directory.join("state.json"));
        assert!(usable);
        assert!(state.applied.is_empty());
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn what_is_written_survives_a_round_trip() {
        let directory = scratch("round");
        let path = directory.join("state.json");
        let mut state = State::default();
        state.record_applied("mon-a", "night", "abc123", "2026-10-25T09:00:00");
        save(&path, &state).expect("save");

        let (loaded, usable) = load(&path);
        assert!(usable);
        assert_eq!(loaded, state);
        assert!(!directory.join("state.json.tmp").exists());
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_corrupt_or_unknown_file_is_not_used_and_not_overwritten_by_reading() {
        let directory = scratch("corrupt");
        let path = directory.join("state.json");
        std::fs::write(&path, "{not json").expect("write");
        let (state, usable) = load(&path);
        assert!(!usable);
        assert!(state.applied.is_empty());
        // Reading never writes: the file is exactly as it was.
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "{not json");

        std::fs::write(&path, "{\"schema_version\": 99, \"applied\": {}}").expect("write");
        let (_, usable) = load(&path);
        assert!(!usable);
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn forgetting_a_monitor_removes_only_its_history() {
        let mut state = State::default();
        state.record_applied("mon-a", "day", "aaa", "t");
        state.record_applied("mon-b", "night", "bbb", "t");
        state.forget("mon-a");
        assert!(!state.applied.contains_key("mon-a"));
        assert!(state.applied.contains_key("mon-b"));
    }
}
