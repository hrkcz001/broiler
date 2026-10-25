//! The one storage context a process resolves and passes down.
//!
//! Every process that reads or writes user data — the CLI, the resident coordinator and any helper
//! they launch — uses the context resolved here, so no component can independently decide that the
//! data lives somewhere else. Resolution order is fixed:
//!
//! 1. an explicit `--config-dir <path>`: one unified data root;
//! 2. a [`PORTABLE_MARKER`] file beside the running executable: `<executable>/data`, which is what
//!    a Scoop or portable installation uses;
//! 3. the unmarked fallback: roaming AppData for settings and profiles, local AppData for state,
//!    capability evidence and logs.
//!
//! Portable mode is never inferred from the mere existence of a `data` directory, and a root that
//! cannot be created or written is an error: silently falling back to AppData would split the user's
//! configuration between two places without telling anybody.

use std::fmt;
use std::path::{Path, PathBuf};

/// Empty marker file that selects executable-adjacent storage.
pub const PORTABLE_MARKER: &str = "broiler.portable";

/// How the data root was chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageMode {
    /// An explicit `--config-dir` root.
    Explicit,
    /// `broiler.portable` beside the executable; `<executable>/data`.
    Portable,
    /// Unmarked installation: known-folder AppData.
    AppData,
}

impl StorageMode {
    pub fn as_str(self) -> &'static str {
        match self {
            StorageMode::Explicit => "explicit",
            StorageMode::Portable => "portable",
            StorageMode::AppData => "appdata",
        }
    }

    /// True when settings, profiles and local state share one root.
    pub fn is_unified(self) -> bool {
        !matches!(self, StorageMode::AppData)
    }
}

/// Known folders used only by the unmarked fallback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppDataRoots {
    pub roaming: PathBuf,
    pub local: PathBuf,
}

/// Why a storage root could not be resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StorageError {
    /// No explicit root, no marker, and the known folders are unavailable.
    Unavailable,
    /// The current working directory could not be read while resolving a relative explicit root.
    NotAbsolute(String),
    /// The chosen root exists but cannot be used. Never silently replaced with another root.
    Unwritable { path: PathBuf, reason: String },
}

impl fmt::Display for StorageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StorageError::Unavailable => formatter.write_str(
                "no data root is available: the known folders are unavailable and none was given",
            ),
            StorageError::NotAbsolute(reason) => write!(
                formatter,
                "the data root cannot be resolved to an absolute path: {reason}"
            ),
            StorageError::Unwritable { path, reason } => write!(
                formatter,
                "the selected data root {} is not usable: {reason}; broiler does not fall back to \
                 another location",
                path.display()
            ),
        }
    }
}

/// Where this process keeps its data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageContext {
    mode: StorageMode,
    root: PathBuf,
    roaming: PathBuf,
    local: PathBuf,
    marker: Option<PathBuf>,
}

impl StorageContext {
    pub fn mode(&self) -> StorageMode {
        self.mode
    }

    /// The unified data root. In AppData mode this is the roaming directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Directory holding `settings.toml` and `profiles/`.
    pub fn roaming(&self) -> &Path {
        &self.roaming
    }

    /// Directory holding `state.json`, `capabilities.json` and `logs/`.
    pub fn local(&self) -> &Path {
        &self.local
    }

    /// The marker file that selected portable mode, when one did.
    pub fn marker(&self) -> Option<&Path> {
        self.marker.as_deref()
    }

    pub fn settings_file(&self) -> PathBuf {
        self.roaming.join("settings.toml")
    }

    pub fn profiles_dir(&self) -> PathBuf {
        self.roaming.join("profiles")
    }

    pub fn state_file(&self) -> PathBuf {
        self.local.join("state.json")
    }

    pub fn capabilities_file(&self) -> PathBuf {
        self.local.join("capabilities.json")
    }

    pub fn log_dir(&self) -> PathBuf {
        self.local.join("logs")
    }

    pub fn log_file(&self) -> PathBuf {
        self.log_dir().join("broiler.log")
    }

    /// Record of what happened to an older AppData configuration, stored in the selected root.
    pub fn import_record(&self) -> PathBuf {
        self.root.join("migration.json")
    }

    /// An explicit-root context for tests: no known folder is consulted and nothing outside `root`
    /// is touched.
    #[cfg(test)]
    pub fn for_test(root: &Path) -> StorageContext {
        std::fs::create_dir_all(root).expect("test data root");
        StorageContext {
            mode: StorageMode::Explicit,
            root: root.to_path_buf(),
            roaming: root.to_path_buf(),
            local: root.to_path_buf(),
            marker: None,
        }
    }

    /// Machine-readable description shared by `paths --json` and `status --json`.
    pub fn describe(&self) -> serde_json::Value {
        serde_json::json!({
            "mode": self.mode.as_str(),
            "root": self.root.display().to_string(),
            "roaming": self.roaming.display().to_string(),
            "local": self.local.display().to_string(),
            "marker": self.marker.as_ref().map(|path| path.display().to_string()),
            "settings": self.settings_file().display().to_string(),
            "profiles": self.profiles_dir().display().to_string(),
            "state": self.state_file().display().to_string(),
            "capabilities": self.capabilities_file().display().to_string(),
            "logs": self.log_dir().display().to_string(),
        })
    }
}

fn current_executable_dir() -> Result<PathBuf, StorageError> {
    let executable = std::env::current_exe()
        .map_err(|error| StorageError::NotAbsolute(error.to_string()))?;
    executable
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| StorageError::NotAbsolute(String::from("the executable has no parent directory")))
}

/// Known folders for the unmarked fallback, resolved through the Windows API.
pub fn appdata_roots() -> Option<AppDataRoots> {
    Some(AppDataRoots {
        roaming: crate::paths::roaming_app_data()?,
        local: crate::paths::local_app_data()?,
    })
}

/// Resolve the storage context for this process.
pub fn resolve(explicit: Option<&Path>) -> Result<StorageContext, StorageError> {
    let executable_dir = current_executable_dir()?;
    resolve_in(explicit, &executable_dir, appdata_roots())
}

/// Resolve with an injected executable directory and known folders, so tests never read the real
/// profile or the user's actual AppData.
pub fn resolve_in(
    explicit: Option<&Path>,
    executable_dir: &Path,
    appdata: Option<AppDataRoots>,
) -> Result<StorageContext, StorageError> {
    if let Some(requested) = explicit {
        // A relative path is resolved exactly once, here, and only the absolute result travels on.
        let root = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|error| StorageError::NotAbsolute(error.to_string()))?
                .join(requested)
        };
        ensure_writable(&root)?;
        return Ok(StorageContext {
            mode: StorageMode::Explicit,
            root: root.clone(),
            roaming: root.clone(),
            local: root,
            marker: None,
        });
    }

    let marker = executable_dir.join(PORTABLE_MARKER);
    if marker.is_file() {
        let root = executable_dir.join("data");
        ensure_writable(&root)?;
        return Ok(StorageContext {
            mode: StorageMode::Portable,
            root: root.clone(),
            roaming: root.clone(),
            local: root,
            marker: Some(marker),
        });
    }

    let roots = appdata.ok_or(StorageError::Unavailable)?;
    let roaming = roots.roaming.join(crate::PRODUCT_NAME);
    let local = roots.local.join(crate::PRODUCT_NAME);
    Ok(StorageContext {
        mode: StorageMode::AppData,
        root: roaming.clone(),
        roaming,
        local,
        marker: None,
    })
}

/// Create the root if needed and prove it is writable.
///
/// The probe file is the cheapest honest test: an access check alone would not catch a directory
/// that exists but cannot be written by this user.
fn ensure_writable(root: &Path) -> Result<(), StorageError> {
    std::fs::create_dir_all(root).map_err(|error| StorageError::Unwritable {
        path: root.to_path_buf(),
        reason: format!("the directory cannot be created: {error}"),
    })?;
    let probe = root.join(format!(".broiler-write-probe-{}", std::process::id()));
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(error) => Err(StorageError::Unwritable {
            path: root.to_path_buf(),
            reason: format!("a file cannot be written there: {error}"),
        }),
    }
}

/// Paths of an AppData configuration, described for diagnostics and migration.
pub fn appdata_of(roots: &AppDataRoots) -> StorageContext {
    let roaming = roots.roaming.join(crate::PRODUCT_NAME);
    let local = roots.local.join(crate::PRODUCT_NAME);
    StorageContext {
        mode: StorageMode::AppData,
        root: roaming.clone(),
        roaming,
        local,
        marker: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let mut directory = std::env::temp_dir();
        directory.push(format!("broiler-storage-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("scratch");
        directory
    }

    fn appdata(base: &Path) -> AppDataRoots {
        AppDataRoots {
            roaming: base.join("Roaming"),
            local: base.join("Local"),
        }
    }

    #[test]
    fn an_explicit_root_wins_over_a_marker() {
        let base = scratch("explicit");
        let executable = base.join("app");
        std::fs::create_dir_all(&executable).expect("app dir");
        std::fs::write(executable.join(PORTABLE_MARKER), "").expect("marker");

        let root = base.join("chosen");
        let context = resolve_in(Some(&root), &executable, Some(appdata(&base))).expect("context");

        assert_eq!(context.mode(), StorageMode::Explicit);
        assert_eq!(context.root(), root.as_path());
        assert_eq!(context.settings_file(), root.join("settings.toml"));
        assert_eq!(context.log_file(), root.join("logs").join("broiler.log"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_relative_explicit_root_is_resolved_once_to_an_absolute_path() {
        let base = scratch("relative");
        let context = resolve_in(Some(Path::new("data")), &base, None).expect("context");
        assert!(context.root().is_absolute());
        assert!(context.root().ends_with("data"));
        assert_eq!(context.mode(), StorageMode::Explicit);
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(std::env::current_dir().unwrap().join("data"));
    }

    #[test]
    fn the_marker_selects_executable_adjacent_data() {
        let base = scratch("marker");
        let executable = base.join("app");
        std::fs::create_dir_all(&executable).expect("app dir");
        std::fs::write(executable.join(PORTABLE_MARKER), "").expect("marker");

        let context = resolve_in(None, &executable, Some(appdata(&base))).expect("context");

        assert_eq!(context.mode(), StorageMode::Portable);
        assert_eq!(context.root(), executable.join("data").as_path());
        assert_eq!(context.marker(), Some(executable.join(PORTABLE_MARKER).as_path()));
        // Portable mode is one unified root: settings and logs never split.
        assert_eq!(context.roaming(), context.local());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn an_existing_data_directory_without_a_marker_is_not_portable() {
        let base = scratch("no-marker");
        let executable = base.join("app");
        std::fs::create_dir_all(executable.join("data")).expect("data dir");

        let context = resolve_in(None, &executable, Some(appdata(&base))).expect("context");

        assert_eq!(
            context.mode(),
            StorageMode::AppData,
            "an incidental data directory must not select portable mode"
        );
        assert_eq!(context.roaming(), base.join("Roaming").join("broiler"));
        assert_eq!(context.local(), base.join("Local").join("broiler"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_unmarked_fallback_splits_roaming_and_local_data() {
        let base = scratch("appdata");
        let context = resolve_in(None, &base, Some(appdata(&base))).expect("context");
        assert_eq!(context.mode(), StorageMode::AppData);
        assert_eq!(context.settings_file(), base.join("Roaming").join("broiler").join("settings.toml"));
        assert_eq!(context.log_file(), base.join("Local").join("broiler").join("logs").join("broiler.log"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn without_an_explicit_root_or_marker_and_without_known_folders_resolution_fails() {
        let base = scratch("no-appdata");
        assert_eq!(
            resolve_in(None, &base, None),
            Err(StorageError::Unavailable)
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_marker_beside_an_unwritable_root_fails_instead_of_falling_back() {
        let base = scratch("unwritable");
        let executable = base.join("app");
        std::fs::create_dir_all(&executable).expect("app dir");
        std::fs::write(executable.join(PORTABLE_MARKER), "").expect("marker");
        // A file where the data directory must be: create_dir_all cannot succeed.
        std::fs::write(executable.join("data"), "not a directory").expect("blocker");

        let error = resolve_in(None, &executable, Some(appdata(&base)))
            .expect_err("the root is unusable");
        match error {
            StorageError::Unwritable { path, .. } => assert_eq!(path, executable.join("data")),
            other => panic!("unexpected error: {other}"),
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn an_unusable_explicit_root_is_an_error_not_a_fallback() {
        let base = scratch("explicit-unwritable");
        let blocker = base.join("chosen");
        std::fs::write(&blocker, "not a directory").expect("blocker");

        let error = resolve_in(Some(&blocker), &base, Some(appdata(&base)))
            .expect_err("unusable explicit root");
        assert!(matches!(error, StorageError::Unwritable { .. }));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_working_directory_does_not_influence_marker_discovery() {
        let base = scratch("cwd-independent");
        let executable = base.join("app");
        std::fs::create_dir_all(&executable).expect("app dir");
        std::fs::write(executable.join(PORTABLE_MARKER), "").expect("marker");
        // A marker in the working directory must be irrelevant.
        std::fs::write(base.join(PORTABLE_MARKER), "").expect("decoy marker");

        let context = resolve_in(None, &executable, Some(appdata(&base))).expect("context");
        assert_eq!(context.root(), executable.join("data").as_path());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_description_carries_the_mode_and_every_root() {
        let base = scratch("describe");
        let executable = base.join("app");
        std::fs::create_dir_all(&executable).expect("app dir");
        std::fs::write(executable.join(PORTABLE_MARKER), "").expect("marker");
        let context = resolve_in(None, &executable, None).expect("context");
        let described = context.describe();
        assert_eq!(described["mode"], "portable");
        assert_eq!(
            described["root"],
            executable.join("data").display().to_string()
        );
        assert_eq!(
            described["marker"],
            executable.join(PORTABLE_MARKER).display().to_string()
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
