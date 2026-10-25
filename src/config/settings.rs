//! Global settings: parsing, validation and comment-preserving saves.
//!
//! Settings are user-maintained TOML. Saves therefore keep the user's comments and key order, and
//! a file that no longer parses is never overwritten with defaults: a failed save must be visible
//! rather than silently destructive.

use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};

use toml_edit::{value, Array, DocumentMut, Item, Value};

use crate::logging::Level;

use super::profile::Diagnostic;

/// Settings schema version understood by this build.
pub const SETTINGS_SCHEMA_VERSION: u32 = 1;

/// What a left click on one tray icon does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeftClick {
    Cycle,
    Flyout,
}

impl LeftClick {
    pub fn as_str(self) -> &'static str {
        match self {
            LeftClick::Cycle => "cycle",
            LeftClick::Flyout => "flyout",
        }
    }

    pub fn parse(text: &str) -> Option<LeftClick> {
        match text {
            "cycle" => Some(LeftClick::Cycle),
            "flyout" => Some(LeftClick::Flyout),
            _ => None,
        }
    }
}

/// Policy for controls a display positively does not offer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnsupportedPolicy {
    Skip,
    Fail,
}

impl UnsupportedPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            UnsupportedPolicy::Skip => "skip",
            UnsupportedPolicy::Fail => "fail",
        }
    }

    pub fn parse(text: &str) -> Option<UnsupportedPolicy> {
        match text {
            "skip" => Some(UnsupportedPolicy::Skip),
            "fail" => Some(UnsupportedPolicy::Fail),
            _ => None,
        }
    }
}

/// Badge policy. `when_multiple` is the only v1 value; disabling multi-monitor identification is
/// not an option.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Badges {
    WhenMultiple,
}

/// Startup policy for the multi-profile case. Profiles are never applied automatically at logon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartupPolicy {
    Preserve,
}

/// Validated global settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub schema_version: u32,
    pub launch_at_login: bool,
    pub left_click: LeftClick,
    pub badges: Badges,
    pub remember: bool,
    pub monitor_ids: Vec<String>,
    pub profiles_startup: StartupPolicy,
    pub unsupported: UnsupportedPolicy,
    pub log_level: Level,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            schema_version: SETTINGS_SCHEMA_VERSION,
            launch_at_login: false,
            left_click: LeftClick::Flyout,
            badges: Badges::WhenMultiple,
            remember: false,
            monitor_ids: Vec::new(),
            profiles_startup: StartupPolicy::Preserve,
            unsupported: UnsupportedPolicy::Skip,
            log_level: Level::Warn,
        }
    }
}

/// Settings file problems.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsError {
    pub diagnostics: Vec<Diagnostic>,
}

impl SettingsError {
    fn single(path: &Path, message: impl Into<String>) -> SettingsError {
        SettingsError {
            diagnostics: vec![Diagnostic::new(path, None, message)],
        }
    }

    pub fn render(&self) -> String {
        self.diagnostics
            .iter()
            .map(Diagnostic::render)
            .collect::<Vec<String>>()
            .join("\n")
    }
}

impl fmt::Display for SettingsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.render())
    }
}

impl std::error::Error for SettingsError {}

fn as_bool(item: &Item) -> Option<bool> {
    item.as_value().and_then(Value::as_bool)
}

fn as_str(item: &Item) -> Option<String> {
    item.as_value()
        .and_then(Value::as_str)
        .map(|text| text.to_string())
}

fn as_string_array(item: &Item) -> Option<Vec<String>> {
    let array = item.as_value()?.as_array()?;
    let mut values = Vec::new();
    for entry in array.iter() {
        values.push(entry.as_str()?.to_string());
    }
    Some(values)
}

fn section<'a>(document: &'a DocumentMut, name: &str) -> Option<&'a Item> {
    document.get(name)
}

/// Parse and validate the settings file.
pub fn parse(text: &str, path: &Path) -> Result<Settings, SettingsError> {
    let document: DocumentMut = text
        .parse()
        .map_err(|error: toml_edit::TomlError| SettingsError::single(path, format!("invalid TOML: {error}")))?;

    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    let mut complain = |message: String| diagnostics.push(Diagnostic::new(path, None, message));

    const TOP_LEVEL: &[&str] = &[
        "schema_version",
        "startup",
        "tray",
        "selection",
        "profiles",
        "logging",
    ];
    for (key, _) in document.iter() {
        if !TOP_LEVEL.contains(&key) {
            complain(format!("unknown settings key `{key}`"));
        }
    }

    let mut settings = Settings::default();

    match document.get("schema_version").and_then(|item| item.as_integer()) {
        Some(value) if value == SETTINGS_SCHEMA_VERSION as i64 => {}
        Some(other) => complain(format!(
            "unsupported schema_version {other}; this build understands {SETTINGS_SCHEMA_VERSION}"
        )),
        None => complain(String::from("schema_version = 1 is required")),
    }

    if let Some(item) = section(&document, "startup") {
        match item.as_table() {
            Some(table) => {
                for (key, _) in table.iter() {
                    if key != "launch_at_login" {
                        complain(format!("unknown settings key `startup.{key}`"));
                    }
                }
                if let Some(item) = table.get("launch_at_login") {
                    match as_bool(item) {
                        Some(value) => settings.launch_at_login = value,
                        None => complain(String::from("startup.launch_at_login must be true or false")),
                    }
                }
            }
            None => complain(String::from("[startup] must be a table")),
        }
    }

    if let Some(item) = section(&document, "tray") {
        match item.as_table() {
            Some(table) => {
                for (key, _) in table.iter() {
                    if key != "left_click" && key != "badges" {
                        complain(format!("unknown settings key `tray.{key}`"));
                    }
                }
                if let Some(item) = table.get("left_click") {
                    match as_str(item).as_deref().and_then(LeftClick::parse) {
                        Some(value) => settings.left_click = value,
                        None => complain(String::from(
                            "tray.left_click must be \"cycle\" or \"flyout\"",
                        )),
                    }
                }
                if let Some(item) = table.get("badges") {
                    match as_str(item).as_deref() {
                        Some("when_multiple") => settings.badges = Badges::WhenMultiple,
                        other => complain(format!(
                            "tray.badges accepts only \"when_multiple\"; got {other:?}"
                        )),
                    }
                }
            }
            None => complain(String::from("[tray] must be a table")),
        }
    }

    if let Some(item) = section(&document, "selection") {
        match item.as_table() {
            Some(table) => {
                for (key, _) in table.iter() {
                    if key != "remember" && key != "monitor_ids" {
                        complain(format!("unknown settings key `selection.{key}`"));
                    }
                }
                if let Some(item) = table.get("remember") {
                    match as_bool(item) {
                        Some(value) => settings.remember = value,
                        None => complain(String::from("selection.remember must be true or false")),
                    }
                }
                if let Some(item) = table.get("monitor_ids") {
                    match as_string_array(item) {
                        Some(values) => settings.monitor_ids = values,
                        None => complain(String::from(
                            "selection.monitor_ids must be an array of strings",
                        )),
                    }
                }
            }
            None => complain(String::from("[selection] must be a table")),
        }
    }

    if let Some(item) = section(&document, "profiles") {
        match item.as_table() {
            Some(table) => {
                for (key, _) in table.iter() {
                    if key != "startup" && key != "unsupported" {
                        complain(format!("unknown settings key `profiles.{key}`"));
                    }
                }
                if let Some(item) = table.get("startup") {
                    match as_str(item).as_deref() {
                        Some("preserve") => settings.profiles_startup = StartupPolicy::Preserve,
                        other => complain(format!(
                            "profiles.startup accepts only \"preserve\"; got {other:?}"
                        )),
                    }
                }
                if let Some(item) = table.get("unsupported") {
                    match as_str(item).as_deref().and_then(UnsupportedPolicy::parse) {
                        Some(value) => settings.unsupported = value,
                        None => complain(String::from(
                            "profiles.unsupported must be \"skip\" or \"fail\"",
                        )),
                    }
                }
            }
            None => complain(String::from("[profiles] must be a table")),
        }
    }

    if let Some(item) = section(&document, "logging") {
        match item.as_table() {
            Some(table) => {
                for (key, _) in table.iter() {
                    if key != "level" {
                        complain(format!("unknown settings key `logging.{key}`"));
                    }
                }
                if let Some(item) = table.get("level") {
                    match as_str(item).as_deref().and_then(Level::parse) {
                        Some(value) => settings.log_level = value,
                        None => complain(String::from(
                            "logging.level must be one of off, error, warn, info, debug",
                        )),
                    }
                }
            }
            None => complain(String::from("[logging] must be a table")),
        }
    }

    if !settings.remember && !settings.monitor_ids.is_empty() {
        complain(String::from(
            "selection.remember = false requires an empty selection.monitor_ids",
        ));
    }

    if !diagnostics.is_empty() {
        return Err(SettingsError { diagnostics });
    }
    Ok(settings)
}

/// Load settings, falling back to defaults when the file does not exist.
pub fn load(path: &Path) -> Result<Settings, SettingsError> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text, path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(error) => Err(SettingsError::single(
            path,
            format!("cannot read the settings file: {error}"),
        )),
    }
}

fn set_string(table: &mut toml_edit::Table, key: &str, text: &str) {
    table[key] = value(text);
}

fn apply(document: &mut DocumentMut, settings: &Settings) {
    document["schema_version"] = value(settings.schema_version as i64);

    if !document.contains_key("startup") {
        document["startup"] = Item::Table(toml_edit::Table::new());
    }
    document["startup"]["launch_at_login"] = value(settings.launch_at_login);

    if !document.contains_key("tray") {
        document["tray"] = Item::Table(toml_edit::Table::new());
    }
    if let Some(table) = document["tray"].as_table_mut() {
        set_string(table, "left_click", settings.left_click.as_str());
        set_string(table, "badges", "when_multiple");
    }

    if !document.contains_key("selection") {
        document["selection"] = Item::Table(toml_edit::Table::new());
    }
    if let Some(table) = document["selection"].as_table_mut() {
        table["remember"] = value(settings.remember);
        let mut array = Array::new();
        for id in &settings.monitor_ids {
            array.push(id.as_str());
        }
        table["monitor_ids"] = value(array);
    }

    if !document.contains_key("profiles") {
        document["profiles"] = Item::Table(toml_edit::Table::new());
    }
    if let Some(table) = document["profiles"].as_table_mut() {
        set_string(table, "startup", "preserve");
        set_string(table, "unsupported", settings.unsupported.as_str());
    }

    if !document.contains_key("logging") {
        document["logging"] = Item::Table(toml_edit::Table::new());
    }
    if let Some(table) = document["logging"].as_table_mut() {
        set_string(table, "level", settings.log_level.as_str());
    }
}

/// Save settings next to `path`, preserving comments and key order of an existing file.
///
/// The write is atomic: the new content goes to a temporary file in the same directory and is then
/// renamed over the target, so an interrupted save cannot leave a half-written settings file. If the
/// existing file does not parse, nothing is written.
pub fn save(path: &Path, settings: &Settings) -> Result<(), SettingsError> {
    let existing = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(SettingsError::single(
                path,
                format!("cannot read the settings file: {error}"),
            ))
        }
    };

    let mut document: DocumentMut = if existing.trim().is_empty() {
        DocumentMut::new()
    } else {
        existing
            .parse()
            .map_err(|error: toml_edit::TomlError| {
                SettingsError::single(
                    path,
                    format!(
                        "refusing to overwrite a settings file that does not parse: {error}"
                    ),
                )
            })?
    };

    apply(&mut document, settings);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            SettingsError::single(path, format!("cannot create the settings directory: {error}"))
        })?;
    }

    let temporary = temporary_path(path);
    {
        let mut file = std::fs::File::create(&temporary).map_err(|error| {
            SettingsError::single(path, format!("cannot create a temporary settings file: {error}"))
        })?;
        file.write_all(document.to_string().as_bytes())
            .map_err(|error| {
                SettingsError::single(path, format!("cannot write settings: {error}"))
            })?;
        file.sync_all().ok();
    }
    std::fs::rename(&temporary, path).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        SettingsError::single(path, format!("cannot replace the settings file: {error}"))
    })
}

/// Replace the settings file with already-validated text, atomically.
///
/// `expected` is what the caller read before editing. If the file on disk is no longer that text,
/// somebody else changed it in the meantime and this update would silently undo their edit, so it
/// is refused. The replacement itself is a same-directory temporary file plus a rename, so a crash
/// leaves either the old file or the new one, never a truncated mix.
pub fn replace_text(path: &Path, expected: &str, updated: &str) -> Result<(), SettingsError> {
    let current = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(SettingsError::single(
                path,
                format!("cannot read the settings file: {error}"),
            ))
        }
    };
    if current != expected {
        return Err(SettingsError::single(
            path,
            "the settings file changed while this update was being prepared; nothing was written",
        ));
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            SettingsError::single(path, format!("cannot create the settings directory: {error}"))
        })?;
    }

    let temporary = temporary_path(path);
    {
        let mut file = std::fs::File::create(&temporary).map_err(|error| {
            SettingsError::single(path, format!("cannot create a temporary settings file: {error}"))
        })?;
        file.write_all(updated.as_bytes())
            .map_err(|error| SettingsError::single(path, format!("cannot write settings: {error}")))?;
        file.sync_all().ok();
    }
    std::fs::rename(&temporary, path).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        SettingsError::single(path, format!("cannot replace the settings file: {error}"))
    })
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|name| name.to_os_string()).unwrap_or_default();
    name.push(".tmp");
    path.with_file_name(name)
}

/// Apply one `settings set <key> <value>` update to the file text.
///
/// Returns the updated document text. Unknown keys and values outside the contract are rejected
/// instead of being written, so a typo cannot create a second, competing setting.
pub fn set_key(path: &Path, text: &str, key: &str, raw_value: &str) -> Result<String, SettingsError> {
    let mut document: DocumentMut = if text.trim().is_empty() {
        DocumentMut::new()
    } else {
        text.parse().map_err(|error: toml_edit::TomlError| {
            SettingsError::single(path, format!("invalid TOML: {error}"))
        })?
    };

    // A file that does not exist yet still needs the schema marker, or the result could not be
    // validated when it is read back.
    if !document.contains_key("schema_version") {
        document["schema_version"] = value(SETTINGS_SCHEMA_VERSION as i64);
    }

    let parsed_bool = || match raw_value {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(SettingsError::single(
            path,
            format!("`{other}` is not a boolean; use true or false"),
        )),
    };

    match key {
        "startup.launch_at_login" => {
            let value_bool = parsed_bool()?;
            if !document.contains_key("startup") {
                document["startup"] = Item::Table(toml_edit::Table::new());
            }
            document["startup"]["launch_at_login"] = value(value_bool);
        }
        "tray.left_click" => {
            let parsed = LeftClick::parse(raw_value).ok_or_else(|| {
                SettingsError::single(path, String::from("tray.left_click must be cycle or flyout"))
            })?;
            if !document.contains_key("tray") {
                document["tray"] = Item::Table(toml_edit::Table::new());
            }
            if let Some(table) = document["tray"].as_table_mut() {
                set_string(table, "left_click", parsed.as_str());
            }
        }
        "profiles.unsupported" => {
            let parsed = UnsupportedPolicy::parse(raw_value).ok_or_else(|| {
                SettingsError::single(path, String::from("profiles.unsupported must be skip or fail"))
            })?;
            if !document.contains_key("profiles") {
                document["profiles"] = Item::Table(toml_edit::Table::new());
            }
            if let Some(table) = document["profiles"].as_table_mut() {
                set_string(table, "unsupported", parsed.as_str());
            }
        }
        "logging.level" => {
            let parsed = Level::parse(raw_value).ok_or_else(|| {
                SettingsError::single(
                    path,
                    String::from("logging.level must be off, error, warn, info or debug"),
                )
            })?;
            if !document.contains_key("logging") {
                document["logging"] = Item::Table(toml_edit::Table::new());
            }
            if let Some(table) = document["logging"].as_table_mut() {
                set_string(table, "level", parsed.as_str());
            }
        }
        "selection.remember" => {
            let value_bool = parsed_bool()?;
            if !document.contains_key("selection") {
                document["selection"] = Item::Table(toml_edit::Table::new());
            }
            if let Some(table) = document["selection"].as_table_mut() {
                table["remember"] = value(value_bool);
                if !value_bool {
                    table["monitor_ids"] = value(Array::new());
                }
            }
        }
        other => {
            return Err(SettingsError::single(
                path,
                format!(
                    "`{other}` is not a mutable setting; mutable keys are startup.launch_at_login, tray.left_click, profiles.unsupported, logging.level and selection.remember"
                ),
            ))
        }
    }

    Ok(document.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path() -> PathBuf {
        PathBuf::from("C:\\\\settings.toml")
    }

    const SHIPPED: &str = "schema_version = 1\n\n[startup]\nlaunch_at_login = false\n\n[tray]\nleft_click = \"flyout\"\nbadges = \"when_multiple\"\n\n[selection]\nremember = false\nmonitor_ids = []\n\n[profiles]\nstartup = \"preserve\"\nunsupported = \"skip\"\n\n[logging]\nlevel = \"warn\"\n";

    fn scratch(name: &str) -> PathBuf {
        let mut directory = std::env::temp_dir();
        directory.push(format!("broiler-settings-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("scratch");
        directory
    }

    #[test]
    fn an_update_replaces_the_file_atomically_and_keeps_the_user_comments() {
        let directory = scratch("replace");
        let file = directory.join("settings.toml");
        let original = format!("# written by hand\n{SHIPPED}");
        std::fs::write(&file, &original).expect("settings");

        let updated = set_key(&file, &original, "tray.left_click", "cycle").expect("edit");
        replace_text(&file, &original, &updated).expect("replace");

        let on_disk = std::fs::read_to_string(&file).expect("read");
        assert!(on_disk.starts_with("# written by hand\n"));
        assert!(on_disk.contains("left_click = \"cycle\""));
        assert_eq!(parse(&on_disk, &file).expect("parse").left_click, LeftClick::Cycle);
        // No temporary file survives a successful replacement.
        assert!(!file.with_file_name("settings.toml.tmp").exists());
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn an_update_that_lost_a_race_with_another_editor_is_refused() {
        let directory = scratch("stale");
        let file = directory.join("settings.toml");
        std::fs::write(&file, SHIPPED).expect("settings");
        let expected = SHIPPED.to_string();

        // Somebody else edits the file after the caller read it.
        let changed = SHIPPED.replace("level = \"warn\"", "level = \"info\"");
        std::fs::write(&file, &changed).expect("external edit");

        let updated = set_key(&file, &expected, "tray.left_click", "cycle").expect("edit");
        let error = replace_text(&file, &expected, &updated).expect_err("stale edit");

        assert!(error.render().contains("changed while this update was being prepared"));
        assert_eq!(std::fs::read_to_string(&file).expect("read"), changed);
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn parses_the_shipped_settings_file() {
        let settings = parse(SHIPPED, &path()).expect("settings");
        assert_eq!(settings, Settings::default());
    }

    #[test]
    fn missing_file_yields_defaults() {
        let mut directory = std::env::temp_dir();
        directory.push(format!("broiler-settings-missing-{}", std::process::id()));
        let settings = load(&directory.join("settings.toml")).expect("defaults");
        assert_eq!(settings, Settings::default());
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let text = format!("{SHIPPED}\n[telemetry]\nenabled = true\n");
        let error = parse(&text, &path()).expect_err("unknown section");
        assert!(error.render().contains("unknown settings key `telemetry`"));

        let nested = SHIPPED.replace("level = \"warn\"", "level = \"warn\"\nrotate = 3");
        let error = parse(&nested, &path()).expect_err("unknown nested key");
        assert!(error.render().contains("unknown settings key `logging.rotate`"));
    }

    #[test]
    fn invalid_enumerations_are_rejected() {
        for (needle, replacement, expected) in [
            ("left_click = \"flyout\"", "left_click = \"double\"", "tray.left_click"),
            ("badges = \"when_multiple\"", "badges = \"never\"", "tray.badges"),
            ("unsupported = \"skip\"", "unsupported = \"ignore\"", "profiles.unsupported"),
            ("startup = \"preserve\"", "startup = \"apply\"", "profiles.startup"),
            ("level = \"warn\"", "level = \"trace\"", "logging.level"),
        ] {
            let text = SHIPPED.replace(needle, replacement);
            let error = parse(&text, &path()).expect_err("invalid value");
            assert!(error.render().contains(expected), "{}", error.render());
        }
    }

    #[test]
    fn remember_false_forbids_stored_ids() {
        let text = SHIPPED.replace("monitor_ids = []", "monitor_ids = [\"mon-aa\"]");
        let error = parse(&text, &path()).expect_err("inconsistent selection");
        assert!(error.render().contains("requires an empty selection.monitor_ids"));
    }

    #[test]
    fn remember_true_allows_stored_ids() {
        let text = SHIPPED
            .replace("remember = false", "remember = true")
            .replace("monitor_ids = []", "monitor_ids = [\"mon-aa\", \"mon-bb\"]");
        let settings = parse(&text, &path()).expect("settings");
        assert!(settings.remember);
        assert_eq!(settings.monitor_ids, vec!["mon-aa", "mon-bb"]);
    }

    #[test]
    fn saving_preserves_comments_and_key_order() {
        let mut directory = std::env::temp_dir();
        directory.push(format!("broiler-settings-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("scratch");
        let file = directory.join("settings.toml");

        let commented = "# keep this comment\nschema_version = 1\n\n[tray]\n# click behaviour\nleft_click = \"flyout\"\nbadges = \"when_multiple\"\n";
        std::fs::write(&file, commented).expect("write");

        let mut settings = load(&file).expect("settings");
        settings.left_click = LeftClick::Cycle;
        save(&file, &settings).expect("save");

        let text = std::fs::read_to_string(&file).expect("read");
        assert!(text.contains("# keep this comment"), "{text}");
        assert!(text.contains("# click behaviour"), "{text}");
        assert!(text.contains("left_click = \"cycle\""), "{text}");
        assert_eq!(load(&file).expect("reload").left_click, LeftClick::Cycle);
        assert!(!directory.join("settings.toml.tmp").exists());
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn saving_refuses_to_overwrite_a_file_that_does_not_parse() {
        let mut directory = std::env::temp_dir();
        directory.push(format!("broiler-settings-broken-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("scratch");
        let file = directory.join("settings.toml");
        std::fs::write(&file, "schema_version = \n").expect("write");

        let error = save(&file, &Settings::default()).expect_err("refuse");
        assert!(error.render().contains("does not parse"));
        assert_eq!(std::fs::read_to_string(&file).expect("read"), "schema_version = \n");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn set_key_updates_only_the_named_setting() {
        let updated = set_key(&path(), SHIPPED, "tray.left_click", "cycle").expect("update");
        assert!(updated.contains("left_click = \"cycle\""));
        assert!(updated.contains("level = \"warn\""));
        let settings = parse(&updated, &path()).expect("parse");
        assert_eq!(settings.left_click, LeftClick::Cycle);
    }

    #[test]
    fn set_key_on_a_missing_file_writes_the_schema_marker() {
        let updated = set_key(&path(), "", "tray.left_click", "cycle").expect("update");
        assert!(updated.contains("schema_version = 1"), "{updated}");
        let settings = parse(&updated, &path()).expect("parse");
        assert_eq!(settings.left_click, LeftClick::Cycle);
    }

    #[test]
    fn set_key_rejects_unknown_keys_and_values() {
        assert!(set_key(&path(), SHIPPED, "tray.badge_policy", "never")
            .expect_err("unknown key")
            .render()
            .contains("not a mutable setting"));
        assert!(set_key(&path(), SHIPPED, "logging.level", "verbose")
            .expect_err("bad value")
            .render()
            .contains("logging.level must be"));
    }

    #[test]
    fn set_key_clears_stored_ids_when_remember_is_disabled() {
        let text = SHIPPED
            .replace("remember = false", "remember = true")
            .replace("monitor_ids = []", "monitor_ids = [\"mon-aa\"]");
        let updated = set_key(&path(), &text, "selection.remember", "false").expect("update");
        let settings = parse(&updated, &path()).expect("parse");
        assert!(!settings.remember);
        assert!(settings.monitor_ids.is_empty());
    }
}
