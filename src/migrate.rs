//! Explicit, copy-only migration of an older AppData configuration into the selected data root.
//!
//! The rules are deliberately narrow: the migration only ever copies, it never deletes or moves the
//! source, it never overwrites a destination that already holds different content, it validates
//! before it publishes, and it records what it did so an interrupted run can be recovered or
//! retried. It performs no hardware write, starts no coordinator, changes no startup registration
//! and never applies a profile.
//!
//! Publication is journalled: every file is staged inside the destination filesystem, the journal
//! records the intended entry and its content hash, and the entry is then published with a single
//! rename. A failed or killed run leaves enough information to undo the files it published.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::storage::{appdata_of, AppDataRoots, StorageContext, StorageMode};

/// Schema version of the journal and of the import record.
pub const MIGRATION_SCHEMA_VERSION: u32 = 1;

/// Schema version of `state.json` this build understands.
const STATE_SCHEMA_VERSION: u64 = 1;

const JOURNAL_NAME: &str = ".migration-journal.json";
const RECORD_NAME: &str = "migration.json";

/// What the destination already holds for one source file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DestinationState {
    /// Nothing there: the file will be published.
    Absent,
    /// Byte-identical file: left untouched, nothing to do.
    Identical,
    /// Different content: a conflict that stops the whole migration.
    Different,
}

impl DestinationState {
    pub fn as_str(self) -> &'static str {
        match self {
            DestinationState::Absent => "absent",
            DestinationState::Identical => "identical",
            DestinationState::Different => "different",
        }
    }
}

/// One file the migration considers.
#[derive(Clone, Debug)]
pub struct Item {
    /// Path relative to the data root, using forward slashes.
    pub relative: String,
    pub source: PathBuf,
    pub destination: PathBuf,
    pub state: DestinationState,
}

/// Result of planning or performing a migration.
#[derive(Clone, Debug)]
pub struct Report {
    pub source_root: PathBuf,
    pub destination_root: PathBuf,
    /// True when the selected root is AppData itself, so there is nothing to migrate.
    pub unnecessary: bool,
    pub items: Vec<Item>,
    /// Files deliberately not migrated, with the reason.
    pub skipped: Vec<(String, String)>,
    pub dry_run: bool,
    pub committed: bool,
    pub written: Vec<String>,
    /// Journal entries rolled back or cleaned up before this run.
    pub recovered: Vec<String>,
}

impl Report {
    pub fn conflicts(&self) -> Vec<&Item> {
        self.items
            .iter()
            .filter(|item| item.state == DestinationState::Different)
            .collect()
    }

    pub fn describe(&self) -> serde_json::Value {
        serde_json::json!({
            "source": self.source_root.display().to_string(),
            "destination": self.destination_root.display().to_string(),
            "unnecessary": self.unnecessary,
            "dry_run": self.dry_run,
            "committed": self.committed,
            "items": self.items.iter().map(|item| serde_json::json!({
                "path": item.relative,
                "source": item.source.display().to_string(),
                "destination": item.destination.display().to_string(),
                "state": item.state.as_str(),
            })).collect::<Vec<serde_json::Value>>(),
            "skipped": self.skipped.iter().map(|(path, reason)| serde_json::json!({
                "path": path,
                "reason": reason,
            })).collect::<Vec<serde_json::Value>>(),
            "written": self.written,
            "recovered": self.recovered,
        })
    }
}

/// Why a migration could not be planned or performed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MigrateError {
    /// The source does not parse, so copying it would only spread a broken file.
    InvalidSource { path: String, reason: String },
    /// The destination already holds different content.
    Conflict { paths: Vec<String> },
    /// A filesystem operation failed; anything published in this run was rolled back.
    Write { path: String, reason: String },
    /// The journal itself could not be read or written.
    Journal(String),
}

impl std::fmt::Display for MigrateError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MigrateError::InvalidSource { path, reason } => {
                write!(formatter, "the existing configuration cannot be validated: {path}: {reason}")
            }
            MigrateError::Conflict { paths } => write!(
                formatter,
                "the destination already holds different content for: {}; nothing was written. \
                 Move or remove those files yourself, or choose another --config-dir",
                paths.join(", ")
            ),
            MigrateError::Write { path, reason } => {
                write!(formatter, "cannot write {path}: {reason}; nothing was committed")
            }
            MigrateError::Journal(reason) => write!(formatter, "migration journal: {reason}"),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct JournalEntry {
    relative: String,
    source: String,
    staged: String,
    published: bool,
    sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Journal {
    schema_version: u32,
    /// `prepared`, `committing`, `complete` or `rolled_back`.
    phase: String,
    source_root: String,
    entries: Vec<JournalEntry>,
}

/// Record of the import decision taken for this data root.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportRecord {
    pub schema_version: u32,
    /// `completed` or `declined`.
    pub appdata_import: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
}

fn hash(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn journal_path(context: &StorageContext) -> PathBuf {
    context.root().join(JOURNAL_NAME)
}

fn record_path(context: &StorageContext) -> PathBuf {
    context.root().join(RECORD_NAME)
}

/// The import decision already recorded for this root, when there is one.
pub fn recorded_decision(context: &StorageContext) -> Option<ImportRecord> {
    let text = std::fs::read_to_string(record_path(context)).ok()?;
    serde_json::from_str(&text).ok()
}

/// True when a portable first launch has an older AppData configuration to decide about.
///
/// Only portable mode asks: an explicit `--config-dir` is a deliberate choice, including the
/// temporary roots tests and alternate deployments use, and it must not depend on the user's real
/// AppData directory.
pub fn pending_import(context: &StorageContext, appdata: &AppDataRoots) -> bool {
    if context.mode() != StorageMode::Portable {
        return false;
    }
    if context.settings_file().exists() || recorded_decision(context).is_some() {
        return false;
    }
    let source = appdata_of(appdata);
    source.settings_file().exists() || has_profiles(&source.profiles_dir())
}

fn has_profiles(directory: &Path) -> bool {
    std::fs::read_dir(directory)
        .map(|entries| {
            entries.flatten().any(|entry| {
                entry.path().is_file()
                    && entry
                        .path()
                        .extension()
                        .and_then(|extension| extension.to_str())
                        .map(|extension| extension.eq_ignore_ascii_case("toml"))
                        .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

fn sorted_files(directory: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(directory)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.is_file())
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
}

fn destination_state(source: &Path, destination: &Path) -> Result<DestinationState, MigrateError> {
    let source_bytes = std::fs::read(source).map_err(|error| MigrateError::Write {
        path: source.display().to_string(),
        reason: error.to_string(),
    })?;
    match std::fs::read(destination) {
        Ok(existing) if existing == source_bytes => Ok(DestinationState::Identical),
        Ok(_) => Ok(DestinationState::Different),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(DestinationState::Absent),
        Err(error) => Err(MigrateError::Write {
            path: destination.display().to_string(),
            reason: error.to_string(),
        }),
    }
}

/// Validate one source file the same way it would be validated in place.
fn validate_source(relative: &str, source: &Path) -> Result<(), MigrateError> {
    let invalid = |reason: String| MigrateError::InvalidSource {
        path: source.display().to_string(),
        reason,
    };
    if relative == "state.json" {
        let text = std::fs::read_to_string(source).map_err(|error| invalid(error.to_string()))?;
        let parsed: serde_json::Value =
            serde_json::from_str(&text).map_err(|error| invalid(error.to_string()))?;
        let schema = parsed
            .get("schema_version")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        if schema != STATE_SCHEMA_VERSION {
            return Err(invalid(format!(
                "state schema version {schema} is not {STATE_SCHEMA_VERSION}"
            )));
        }
        return Ok(());
    }
    if relative == "settings.toml" {
        let text = std::fs::read_to_string(source).map_err(|error| invalid(error.to_string()))?;
        crate::config::settings::parse(&text, source).map_err(|error| invalid(error.render()))?;
        return Ok(());
    }
    if relative.starts_with("profiles/") {
        let text = std::fs::read_to_string(source).map_err(|error| invalid(error.to_string()))?;
        crate::config::profile::parse(&text, source).map_err(|error| invalid(error.render()))?;
        return Ok(());
    }
    Ok(())
}

/// Plan the migration without touching anything.
pub fn plan(context: &StorageContext, appdata: &AppDataRoots) -> Result<Report, MigrateError> {
    let source = appdata_of(appdata);
    let mut report = Report {
        source_root: source.root().to_path_buf(),
        destination_root: context.root().to_path_buf(),
        unnecessary: context.mode() == StorageMode::AppData,
        items: Vec::new(),
        skipped: Vec::new(),
        dry_run: false,
        committed: false,
        written: Vec::new(),
        recovered: Vec::new(),
    };
    if report.unnecessary {
        return Ok(report);
    }

    let mut candidates: Vec<(String, PathBuf)> = Vec::new();
    let settings = source.settings_file();
    if settings.is_file() {
        candidates.push((String::from("settings.toml"), settings));
    }
    let profile_source = source.profiles_dir();
    for file in sorted_files(&profile_source) {
        let name = match file.file_name().and_then(|name| name.to_str()) {
            Some(name) => name.to_string(),
            None => continue,
        };
        if !name.to_ascii_lowercase().ends_with(".toml") {
            report.skipped.push((
                format!("profiles/{name}"),
                String::from("not a profile file: only .toml files are profiles"),
            ));
            continue;
        }
        candidates.push((format!("profiles/{name}"), file));
    }
    let state = source.state_file();
    if state.is_file() {
        candidates.push((String::from("state.json"), state));
    }

    // Transient artefacts are regenerated rather than copied: a stale capability cache or an
    // unbounded log is not persistent identity evidence.
    for (path, reason) in [
        (
            String::from("capabilities.json"),
            "regenerated cache, not persistent evidence",
        ),
        (
            String::from("logs/"),
            "logs are bounded and regenerated; old logs are not copied",
        ),
        (
            String::from("*.tmp"),
            "temporary editor files are not configuration",
        ),
    ] {
        let present = match path.as_str() {
            "capabilities.json" => source.capabilities_file().exists(),
            "logs/" => source.log_dir().exists(),
            _ => true,
        };
        if present {
            report.skipped.push((path, String::from(reason)));
        }
    }

    for (relative, path) in candidates {
        validate_source(&relative, &path)?;
        let destination = context
            .root()
            .join(relative.replace('/', &std::path::MAIN_SEPARATOR.to_string()));
        let state = destination_state(&path, &destination)?;
        report.items.push(Item {
            relative,
            source: path,
            destination,
            state,
        });
    }

    Ok(report)
}

/// Recover from a journal left by an interrupted run.
///
/// Published files are removed only when their content still matches what the journal recorded, so
/// a file the user has since edited is never deleted. Staged files are always removed.
pub fn recover(context: &StorageContext) -> Result<Vec<String>, MigrateError> {
    let path = journal_path(context);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(MigrateError::Journal(error.to_string())),
    };
    let journal: Journal = serde_json::from_str(&text)
        .map_err(|error| MigrateError::Journal(format!("unreadable journal: {error}")))?;

    let mut recovered = Vec::new();
    for entry in &journal.entries {
        let staged = context.root().join(&entry.staged);
        if staged.exists() {
            std::fs::remove_file(&staged)
                .map_err(|error| MigrateError::Journal(format!("cannot remove {}: {error}", staged.display())))?;
            recovered.push(format!("{}: removed staged copy", entry.relative));
        }
        if entry.published && journal.phase != "complete" {
            let destination = context.root().join(&entry.relative);
            if destination.exists() {
                let current = std::fs::read(&destination).map_err(|error| {
                    MigrateError::Journal(format!("cannot read {}: {error}", destination.display()))
                })?;
                if hash(&current) == entry.sha256 {
                    std::fs::remove_file(&destination).map_err(|error| {
                        MigrateError::Journal(format!(
                            "cannot roll back {}: {error}",
                            destination.display()
                        ))
                    })?;
                    recovered.push(format!("{}: rolled back", entry.relative));
                } else {
                    recovered.push(format!(
                        "{}: left in place because its content changed after publication",
                        entry.relative
                    ));
                }
            }
        }
    }
    std::fs::remove_file(&path)
        .map_err(|error| MigrateError::Journal(format!("cannot clear the journal: {error}")))?;
    Ok(recovered)
}

/// Perform the migration, or only describe it when `dry_run` is set.
pub fn migrate(
    context: &StorageContext,
    appdata: &AppDataRoots,
    dry_run: bool,
) -> Result<Report, MigrateError> {
    // A previous run that died part-way is undone before anything new is planned.
    let recovered = if dry_run { Vec::new() } else { recover(context)? };

    let mut report = plan(context, appdata)?;
    report.dry_run = dry_run;
    report.recovered = recovered;
    if report.unnecessary || dry_run {
        return Ok(report);
    }

    let conflicts: Vec<String> = report
        .items
        .iter()
        .filter(|item| item.state == DestinationState::Different)
        .map(|item| item.relative.clone())
        .collect();
    if !conflicts.is_empty() {
        return Err(MigrateError::Conflict { paths: conflicts });
    }

    let pending: Vec<&Item> = report
        .items
        .iter()
        .filter(|item| item.state == DestinationState::Absent)
        .collect();
    if pending.is_empty() {
        // Nothing to do: identical destinations or an empty source.
        write_record(context, "completed", Some(&report.source_root))?;
        report.committed = true;
        return Ok(report);
    }

    for item in &pending {
        if let Some(parent) = item.destination.parent() {
            std::fs::create_dir_all(parent).map_err(|error| MigrateError::Write {
                path: parent.display().to_string(),
                reason: error.to_string(),
            })?;
        }
    }

    let mut journal = Journal {
        schema_version: MIGRATION_SCHEMA_VERSION,
        phase: String::from("prepared"),
        source_root: report.source_root.display().to_string(),
        entries: pending
            .iter()
            .map(|item| JournalEntry {
                relative: item.relative.clone(),
                source: item.source.display().to_string(),
                staged: staged_name(&item.relative),
                published: false,
                sha256: String::new(),
            })
            .collect(),
    };
    write_journal(context, &journal)?;

    let outcome = publish_all(context, &mut journal);
    match outcome {
        Ok(()) => {
            journal.phase = String::from("complete");
            write_journal(context, &journal)?;
            let _ = std::fs::remove_file(journal_path(context));
            write_record(context, "completed", Some(&report.source_root))?;
            report.committed = true;
            report.written = journal
                .entries
                .iter()
                .filter(|entry| entry.published)
                .map(|entry| entry.relative.clone())
                .collect();
            Ok(report)
        }
        Err(error) => {
            // Undo whatever this run published; the source is untouched either way.
            journal.phase = String::from("rolled_back");
            let _ = write_journal(context, &journal);
            let _ = recover(context);
            Err(error)
        }
    }
}

fn staged_name(relative: &str) -> String {
    format!(
        ".broiler-migrate-{}-{}",
        std::process::id(),
        relative.replace('/', "_")
    )
}

fn publish_all(context: &StorageContext, journal: &mut Journal) -> Result<(), MigrateError> {
    for index in 0..journal.entries.len() {
        let relative = journal.entries[index].relative.clone();
        let source = PathBuf::from(&journal.entries[index].source);
        let staged = context.root().join(&journal.entries[index].staged);
        let destination = context.root().join(&relative);

        let bytes = std::fs::read(&source).map_err(|error| MigrateError::Write {
            path: source.display().to_string(),
            reason: error.to_string(),
        })?;
        let digest = hash(&bytes);

        std::fs::write(&staged, &bytes).map_err(|error| MigrateError::Write {
            path: staged.display().to_string(),
            reason: error.to_string(),
        })?;
        journal.entries[index].sha256 = digest.clone();
        journal.phase = String::from("committing");
        write_journal(context, journal)?;

        // Never overwrite: a destination that appeared since planning is a conflict, not a target.
        if destination.exists() {
            let existing = std::fs::read(&destination).map_err(|error| MigrateError::Write {
                path: destination.display().to_string(),
                reason: error.to_string(),
            })?;
            if hash(&existing) != digest {
                return Err(MigrateError::Conflict {
                    paths: vec![relative],
                });
            }
            // Identical content appeared meanwhile: keep it and drop the staged copy.
            let _ = std::fs::remove_file(&staged);
            continue;
        }

        std::fs::rename(&staged, &destination).map_err(|error| MigrateError::Write {
            path: destination.display().to_string(),
            reason: error.to_string(),
        })?;
        journal.entries[index].published = true;
        write_journal(context, journal)?;

        // Byte-for-byte equality is the completion condition.
        let published = std::fs::read(&destination).map_err(|error| MigrateError::Write {
            path: destination.display().to_string(),
            reason: error.to_string(),
        })?;
        if hash(&published) != digest {
            return Err(MigrateError::Write {
                path: destination.display().to_string(),
                reason: String::from("the published file does not match the source"),
            });
        }
    }
    Ok(())
}

fn write_journal(context: &StorageContext, journal: &Journal) -> Result<(), MigrateError> {
    let text = serde_json::to_string_pretty(journal)
        .map_err(|error| MigrateError::Journal(error.to_string()))?;
    std::fs::write(journal_path(context), text)
        .map_err(|error| MigrateError::Journal(error.to_string()))
}

fn write_record(
    context: &StorageContext,
    decision: &str,
    source: Option<&Path>,
) -> Result<(), MigrateError> {
    let record = ImportRecord {
        schema_version: MIGRATION_SCHEMA_VERSION,
        appdata_import: decision.to_string(),
        source_root: source.map(|path| path.display().to_string()),
        completed_at: Some(crate::clock::iso_local()),
    };
    let text = serde_json::to_string_pretty(&record)
        .map_err(|error| MigrateError::Write {
            path: record_path(context).display().to_string(),
            reason: error.to_string(),
        })?;
    std::fs::write(record_path(context), text).map_err(|error| MigrateError::Write {
        path: record_path(context).display().to_string(),
        reason: error.to_string(),
    })
}

/// Record a deliberate decision not to import the older configuration.
pub fn decline(context: &StorageContext, appdata: &AppDataRoots) -> Result<ImportRecord, MigrateError> {
    if context.mode() == StorageMode::AppData {
        return Err(MigrateError::Write {
            path: context.root().display().to_string(),
            reason: String::from("this installation already uses AppData; there is nothing to decline"),
        });
    }
    if let Some(record) = recorded_decision(context) {
        if record.appdata_import == "completed" {
            return Err(MigrateError::Write {
                path: record_path(context).display().to_string(),
                reason: String::from("an import already completed for this data root"),
            });
        }
    }
    let source = appdata_of(appdata);
    write_record(context, "declined", Some(&source.root()))?;
    Ok(recorded_decision(context).unwrap_or(ImportRecord {
        schema_version: MIGRATION_SCHEMA_VERSION,
        appdata_import: String::from("declined"),
        source_root: None,
        completed_at: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{resolve_in, AppDataRoots};

    const SETTINGS: &str = "# keep this comment\nschema_version = 1\n\n[tray]\nleft_click = \"cycle\"\n";
    const PROFILE: &str = "# night profile\nschema_version = 1\nid = \"night\"\nname = \"Night\"\nicon = \"night\"\n\n[monitor]\nbrightness = 0\n";

    struct Fixture {
        base: PathBuf,
        appdata: AppDataRoots,
        context: StorageContext,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    fn fixture(name: &str) -> Fixture {
        let mut base = std::env::temp_dir();
        base.push(format!("broiler-migrate-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("scratch");

        let appdata = AppDataRoots {
            roaming: base.join("Roaming"),
            local: base.join("Local"),
        };
        let executable = base.join("app");
        std::fs::create_dir_all(&executable).expect("app dir");
        std::fs::write(executable.join(crate::storage::PORTABLE_MARKER), "").expect("marker");
        let context = resolve_in(None, &executable, Some(appdata.clone())).expect("context");
        Fixture {
            base,
            appdata,
            context,
        }
    }

    fn source_path(fixture: &Fixture, relative: &str) -> PathBuf {
        let source = appdata_of(&fixture.appdata);
        // AppData splits roaming configuration from local state; the fixture mirrors that.
        let base = if relative == "state.json"
            || relative == "capabilities.json"
            || relative.starts_with("logs/")
        {
            source.local()
        } else {
            source.root()
        };
        base.join(relative.replace('/', &std::path::MAIN_SEPARATOR.to_string()))
    }

    fn write_source(fixture: &Fixture, relative: &str, text: &str) -> PathBuf {
        let path = source_path(fixture, relative);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("source dir");
        std::fs::write(&path, text).expect("source file");
        path
    }

    fn populated(fixture: &Fixture) {
        write_source(fixture, "settings.toml", SETTINGS);
        write_source(fixture, "profiles/night.toml", PROFILE);
    }

    fn source_bytes(fixture: &Fixture, relative: &str) -> Vec<u8> {
        let source = appdata_of(&fixture.appdata);
        std::fs::read(
            source
                .root()
                .join(relative.replace('/', &std::path::MAIN_SEPARATOR.to_string())),
        )
        .expect("source bytes")
    }

    #[test]
    fn an_appdata_installation_reports_that_migration_is_unnecessary() {
        let base = std::env::temp_dir().join(format!("broiler-unnecessary-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("scratch");
        let roots = AppDataRoots {
            roaming: base.join("Roaming"),
            local: base.join("Local"),
        };
        let context = resolve_in(None, &base, Some(roots.clone())).expect("context");
        let report = migrate(&context, &roots, true).expect("report");
        assert!(report.unnecessary);
        assert!(report.items.is_empty());
        assert!(report.written.is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_dry_run_lists_work_and_changes_nothing() {
        let fixture = fixture("dry-run");
        populated(&fixture);

        let report = migrate(&fixture.context, &fixture.appdata, true).expect("dry run");

        assert!(report.dry_run);
        assert!(!report.committed);
        assert_eq!(report.items.len(), 2);
        assert!(report
            .items
            .iter()
            .all(|item| item.state == DestinationState::Absent));
        assert!(!fixture.context.settings_file().exists());
        assert!(!fixture.context.profiles_dir().exists());
        assert!(!fixture.context.import_record().exists());
        assert!(!fixture.context.root().join(JOURNAL_NAME).exists());
    }

    #[test]
    fn an_empty_destination_is_filled_and_the_source_is_preserved() {
        let fixture = fixture("empty");
        populated(&fixture);
        let settings_before = source_bytes(&fixture, "settings.toml");
        let profile_before = source_bytes(&fixture, "profiles/night.toml");

        let report = migrate(&fixture.context, &fixture.appdata, false).expect("migrate");

        assert!(report.committed);
        assert_eq!(report.written.len(), 2);
        assert_eq!(
            std::fs::read_to_string(fixture.context.settings_file()).expect("settings"),
            SETTINGS,
            "comments must survive byte for byte"
        );
        assert_eq!(
            std::fs::read_to_string(fixture.context.profiles_dir().join("night.toml"))
                .expect("profile"),
            PROFILE
        );
        // The originals are still there, unchanged.
        assert_eq!(source_bytes(&fixture, "settings.toml"), settings_before);
        assert_eq!(source_bytes(&fixture, "profiles/night.toml"), profile_before);
        // The decision is recorded in the selected root.
        let record = recorded_decision(&fixture.context).expect("record");
        assert_eq!(record.appdata_import, "completed");
        // No journal is left behind.
        assert!(!fixture.context.root().join(JOURNAL_NAME).exists());
        // No stray staged files.
        assert!(std::fs::read_dir(fixture.context.root())
            .expect("root")
            .flatten()
            .all(|entry| !entry.file_name().to_string_lossy().starts_with(".broiler-migrate-")));
    }

    #[test]
    fn a_second_run_is_idempotent_and_rewrites_nothing() {
        let fixture = fixture("rerun");
        populated(&fixture);
        migrate(&fixture.context, &fixture.appdata, false).expect("first");
        let destination = fixture.context.settings_file();
        let modified = std::fs::metadata(&destination)
            .and_then(|meta| meta.modified())
            .expect("modified");

        let report = migrate(&fixture.context, &fixture.appdata, false).expect("second");

        assert!(report.committed);
        assert!(report.written.is_empty(), "identical files are left alone");
        assert!(report
            .items
            .iter()
            .all(|item| item.state == DestinationState::Identical));
        assert_eq!(
            std::fs::metadata(&destination)
                .and_then(|meta| meta.modified())
                .expect("modified"),
            modified
        );
    }

    #[test]
    fn a_conflicting_destination_stops_everything() {
        let fixture = fixture("conflict");
        populated(&fixture);
        std::fs::create_dir_all(fixture.context.profiles_dir()).expect("profiles");
        std::fs::write(fixture.context.profiles_dir().join("night.toml"), "different\n")
            .expect("conflict");

        let error = migrate(&fixture.context, &fixture.appdata, false).expect_err("conflict");

        match error {
            MigrateError::Conflict { paths } => assert_eq!(paths, vec![String::from("profiles/night.toml")]),
            other => panic!("unexpected error: {other}"),
        }
        // Not even the non-conflicting settings file was published.
        assert!(!fixture.context.settings_file().exists());
        assert_eq!(
            std::fs::read_to_string(fixture.context.profiles_dir().join("night.toml"))
                .expect("conflict file"),
            "different\n"
        );
    }

    #[test]
    fn an_invalid_source_file_stops_everything() {
        let fixture = fixture("invalid");
        write_source(&fixture, "settings.toml", SETTINGS);
        write_source(&fixture, "profiles/broken.toml", "schema_version = 1\nid = \"x\"\n");

        let error = migrate(&fixture.context, &fixture.appdata, false).expect_err("invalid");

        match error {
            MigrateError::InvalidSource { path, .. } => assert!(path.ends_with("broken.toml")),
            other => panic!("unexpected error: {other}"),
        }
        assert!(!fixture.context.settings_file().exists());
    }

    #[test]
    fn an_incompatible_state_schema_is_refused() {
        let fixture = fixture("state");
        write_source(&fixture, "settings.toml", SETTINGS);
        write_source(&fixture, "state.json", "{\"schema_version\": 99}\n");

        let error = migrate(&fixture.context, &fixture.appdata, false).expect_err("schema");

        assert!(matches!(error, MigrateError::InvalidSource { .. }));
        assert!(!fixture.context.state_file().exists());
    }

    #[test]
    fn transient_artefacts_are_reported_as_skipped() {
        let fixture = fixture("skipped");
        populated(&fixture);
        write_source(&fixture, "state.json", "{\"schema_version\": 1}\n");
        write_source(&fixture, "capabilities.json", "{}\n");
        write_source(&fixture, "logs/broiler.log", "old log\n");

        let report = migrate(&fixture.context, &fixture.appdata, true).expect("plan");

        let paths: Vec<&str> = report.skipped.iter().map(|(path, _)| path.as_str()).collect();
        assert!(paths.contains(&"capabilities.json"));
        assert!(paths.contains(&"logs/"));
        assert!(report.items.iter().any(|item| item.relative == "state.json"));
        assert!(!fixture.context.log_dir().exists());
    }

    #[test]
    fn a_failed_publish_rolls_back_the_files_it_already_wrote() {
        let fixture = fixture("rollback");
        populated(&fixture);
        std::fs::create_dir_all(fixture.context.root()).expect("root");

        // Publish through the real journalled path, with the second source removed between planning
        // and publication: the first file is already published when that fails.
        let mut journal = Journal {
            schema_version: MIGRATION_SCHEMA_VERSION,
            phase: String::from("prepared"),
            source_root: appdata_of(&fixture.appdata).root().display().to_string(),
            entries: vec![
                JournalEntry {
                    relative: String::from("settings.toml"),
                    source: appdata_of(&fixture.appdata).settings_file().display().to_string(),
                    staged: staged_name("settings.toml"),
                    published: false,
                    sha256: String::new(),
                },
                JournalEntry {
                    relative: String::from("profiles/gone.toml"),
                    source: fixture
                        .base
                        .join("does-not-exist.toml")
                        .display()
                        .to_string(),
                    staged: staged_name("profiles/gone.toml"),
                    published: false,
                    sha256: String::new(),
                },
            ],
        };
        write_journal(&fixture.context, &journal).expect("journal");

        let error = publish_all(&fixture.context, &mut journal).expect_err("publish failure");
        assert!(matches!(error, MigrateError::Write { .. }), "{error}");
        // The first entry really was published before the failure.
        assert!(fixture.context.settings_file().exists());

        // Rollback is the recovery path the migrate caller uses.
        let recovered = recover(&fixture.context).expect("recover");
        assert!(recovered.iter().any(|line| line.contains("settings.toml: rolled back")));
        assert!(!fixture.context.settings_file().exists());
        assert!(!fixture.context.root().join(JOURNAL_NAME).exists());
        assert_eq!(source_bytes(&fixture, "settings.toml"), SETTINGS.as_bytes());
    }

    #[test]
    fn a_blocked_destination_directory_stops_the_commit_before_any_write() {
        let fixture = fixture("blocked");
        populated(&fixture);
        std::fs::create_dir_all(fixture.context.root()).expect("root");
        // `profiles` exists as a file, so the destination directory cannot be created.
        std::fs::write(fixture.context.profiles_dir(), "blocker").expect("blocker");

        let error = migrate(&fixture.context, &fixture.appdata, false).expect_err("write failure");

        assert!(matches!(error, MigrateError::Write { .. }), "{error}");
        assert!(!fixture.context.settings_file().exists());
        assert!(!fixture.context.root().join(JOURNAL_NAME).exists());
        assert_eq!(source_bytes(&fixture, "settings.toml"), SETTINGS.as_bytes());
    }

    #[test]
    fn an_interrupted_run_is_recovered_from_the_journal() {
        let fixture = fixture("journal");
        populated(&fixture);

        // Simulate a run that staged one file, published it, and then died.
        let published = fixture.context.root().join("settings.toml");
        std::fs::create_dir_all(fixture.context.root()).expect("root");
        std::fs::write(&published, SETTINGS).expect("published");
        let staged = fixture.context.root().join(".broiler-migrate-1-profiles_night.toml");
        std::fs::write(&staged, PROFILE).expect("staged");
        let journal = Journal {
            schema_version: MIGRATION_SCHEMA_VERSION,
            phase: String::from("committing"),
            source_root: appdata_of(&fixture.appdata).root().display().to_string(),
            entries: vec![
                JournalEntry {
                    relative: String::from("settings.toml"),
                    source: appdata_of(&fixture.appdata).settings_file().display().to_string(),
                    staged: String::from(".broiler-migrate-1-settings.toml"),
                    published: true,
                    sha256: hash(SETTINGS.as_bytes()),
                },
                JournalEntry {
                    relative: String::from("profiles/night.toml"),
                    source: appdata_of(&fixture.appdata)
                        .profiles_dir()
                        .join("night.toml")
                        .display()
                        .to_string(),
                    staged: String::from(".broiler-migrate-1-profiles_night.toml"),
                    published: false,
                    sha256: hash(PROFILE.as_bytes()),
                },
            ],
        };
        write_journal(&fixture.context, &journal).expect("journal");

        let recovered = recover(&fixture.context).expect("recover");

        assert_eq!(recovered.len(), 2);
        assert!(!published.exists(), "the published file is rolled back");
        assert!(!staged.exists(), "the staged file is removed");
        assert!(!fixture.context.root().join(JOURNAL_NAME).exists());
        assert_eq!(source_bytes(&fixture, "settings.toml"), SETTINGS.as_bytes());

        // And the retry then succeeds on its own.
        let report = migrate(&fixture.context, &fixture.appdata, false).expect("retry");
        assert!(report.committed);
        assert_eq!(report.written.len(), 2);
    }

    #[test]
    fn recovery_never_deletes_a_file_that_changed_after_publication() {
        let fixture = fixture("journal-edited");
        populated(&fixture);
        std::fs::create_dir_all(fixture.context.root()).expect("root");
        std::fs::write(fixture.context.settings_file(), "user edited\n").expect("edited");
        let journal = Journal {
            schema_version: MIGRATION_SCHEMA_VERSION,
            phase: String::from("committing"),
            source_root: String::from("x"),
            entries: vec![JournalEntry {
                relative: String::from("settings.toml"),
                source: String::from("x"),
                staged: String::from(".broiler-migrate-1-settings.toml"),
                published: true,
                sha256: hash(SETTINGS.as_bytes()),
            }],
        };
        write_journal(&fixture.context, &journal).expect("journal");

        let recovered = recover(&fixture.context).expect("recover");

        assert_eq!(
            std::fs::read_to_string(fixture.context.settings_file()).expect("kept"),
            "user edited\n"
        );
        assert!(recovered[0].contains("changed after publication"));
    }

    #[test]
    fn a_decline_is_recorded_and_silences_the_first_launch_question() {
        let fixture = fixture("decline");
        populated(&fixture);

        assert!(pending_import(&fixture.context, &fixture.appdata));
        let record = decline(&fixture.context, &fixture.appdata).expect("decline");
        assert_eq!(record.appdata_import, "declined");
        assert!(!pending_import(&fixture.context, &fixture.appdata));
        // Nothing was copied.
        assert!(!fixture.context.settings_file().exists());
        // Declining twice is harmless, declining after an import is not.
        assert!(decline(&fixture.context, &fixture.appdata).is_ok());
        migrate(&fixture.context, &fixture.appdata, false).expect("migrate");
        assert!(decline(&fixture.context, &fixture.appdata).is_err());
    }

    #[test]
    fn an_explicit_root_never_asks_about_appdata() {
        let fixture = fixture("explicit");
        populated(&fixture);
        let explicit = fixture.base.join("chosen");
        let context =
            resolve_in(Some(&explicit), &fixture.base.join("app"), Some(fixture.appdata.clone()))
                .expect("context");
        assert!(!pending_import(&context, &fixture.appdata));
    }

    #[test]
    fn an_empty_appdata_source_does_not_ask_anything() {
        let fixture = fixture("empty-source");
        assert!(!pending_import(&fixture.context, &fixture.appdata));
        let report = migrate(&fixture.context, &fixture.appdata, false).expect("migrate");
        assert!(report.committed);
        assert!(report.items.is_empty());
    }
}
