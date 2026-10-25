//! Profile schema: parsing, validation and diagnostics.
//!
//! A profile is one TOML file describing the controls for one named state. Parsing is strict on
//! purpose: unknown keys, duplicate identifiers, unsupported schema versions, empty profiles and
//! conflicting white-point mechanisms are errors with a file, line and column. Being lenient here
//! would mean silently applying a different profile than the user wrote.

use std::fmt;
use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item, Table, Value};

use super::value::{parse_control_value, ControlValue, ValueKind};

/// Schema version this build understands.
pub const SCHEMA_VERSION: u32 = 1;

/// Built-in icon names. An icon chooses artwork only and carries no behaviour.
pub const ICON_NAMES: &[&str] = &[
    "day",
    "night",
    "dawn",
    "dusk",
    "coding",
    "reading",
    "movie",
    "gaming",
    "work",
    "presentation",
    "photo",
    "eco",
    "focus",
    "monitor",
    "custom",
];

/// Standard simple MCCS preset values, used only when a device confirms the meaning.
pub const COLOR_PRESET_VALUES: &[(&str, u64)] = &[
    ("srgb", 0x01),
    ("native", 0x02),
    ("4000k", 0x03),
    ("5000k", 0x04),
    ("6500k", 0x05),
    ("7500k", 0x06),
    ("8200k", 0x07),
    ("9300k", 0x08),
    ("10000k", 0x09),
    ("11500k", 0x0A),
    ("user1", 0x0B),
    ("user2", 0x0C),
    ("user3", 0x0D),
];

/// Temperatures accepted as a named intent. A request vocabulary, not a promise about any device.
pub const KELVIN_VALUES: &[u32] = &[4000, 5000, 6500, 7500, 8200, 9300, 10000, 11500];

/// VCP codes that may carry a color temperature.
pub const VCP_COLOR_PRESET: u16 = 0x14;
pub const VCP_COLOR_REQUEST: u16 = 0x0C;

/// One diagnostic with an optional source location.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub file: PathBuf,
    pub line: Option<usize>,
    pub column: Option<usize>,
    pub message: String,
}

impl Diagnostic {
    pub fn new(file: &Path, location: Option<(usize, usize)>, message: impl Into<String>) -> Diagnostic {
        Diagnostic {
            file: file.to_path_buf(),
            line: location.map(|(line, _)| line),
            column: location.map(|(_, column)| column),
            message: message.into(),
        }
    }

    /// `path:line:column: message`, or `path: message` when no location is known.
    pub fn render(&self) -> String {
        match (self.line, self.column) {
            (Some(line), Some(column)) => {
                format!("{}:{}:{}: {}", self.file.display(), line, column, self.message)
            }
            _ => format!("{}: {}", self.file.display(), self.message),
        }
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.render())
    }
}

/// A set of diagnostics that prevented a profile from being accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileError {
    pub diagnostics: Vec<Diagnostic>,
}

impl ProfileError {
    fn single(diagnostic: Diagnostic) -> ProfileError {
        ProfileError {
            diagnostics: vec![diagnostic],
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

impl fmt::Display for ProfileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.render())
    }
}

impl std::error::Error for ProfileError {}

/// A named colour preset, or an explicit native value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColorPreset {
    Named(String),
    Native(u64),
}

impl ColorPreset {
    /// Native value for this preset, when the name has an established standard meaning.
    pub fn native_value(&self) -> Option<u64> {
        match self {
            ColorPreset::Named(name) => COLOR_PRESET_VALUES
                .iter()
                .find(|(known, _)| known == name)
                .map(|(_, value)| *value),
            ColorPreset::Native(value) => Some(*value),
        }
    }

    /// True when the profile may combine this preset with RGB gains.
    pub fn allows_gains(&self) -> bool {
        match self {
            ColorPreset::Named(name) => name.starts_with("user"),
            ColorPreset::Native(_) => false,
        }
    }
}

/// The single white-point mechanism a profile may request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColorTemperature {
    /// Requested actual temperature in Kelvin.
    Kelvin(u32),
    /// Exact native value for the enumerated preset code or the temperature request code.
    Native { vcp: u16, raw: u64 },
}

/// RGB gain channels, each independently optional.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RgbGain {
    pub red: Option<ControlValue>,
    pub green: Option<ControlValue>,
    pub blue: Option<ControlValue>,
}

impl RgbGain {
    pub fn is_empty(&self) -> bool {
        self.red.is_none() && self.green.is_none() && self.blue.is_none()
    }

    pub fn any(&self) -> bool {
        !self.is_empty()
    }
}

/// Controls requested by one profile. Omitted controls are left untouched.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MonitorSettings {
    pub brightness: Option<ControlValue>,
    pub contrast: Option<ControlValue>,
    pub color_temperature: Option<ColorTemperature>,
    pub color_preset: Option<ColorPreset>,
    pub rgb_gain: RgbGain,
    pub volume: Option<ControlValue>,
    pub mute: Option<bool>,
}

impl MonitorSettings {
    /// True when the profile requests nothing at all, which the schema forbids.
    pub fn is_empty(&self) -> bool {
        self.brightness.is_none()
            && self.contrast.is_none()
            && self.color_temperature.is_none()
            && self.color_preset.is_none()
            && self.rgb_gain.is_empty()
            && self.volume.is_none()
            && self.mute.is_none()
    }
}

/// A validated profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    pub schema_version: u32,
    pub id: String,
    pub name: String,
    pub icon: String,
    pub order: i64,
    pub monitor: MonitorSettings,
    pub source: PathBuf,
}

impl Profile {
    /// Controls in the order the coordinator applies them.
    pub fn requested_control_names(&self) -> Vec<&'static str> {
        let mut names = Vec::new();
        if self.monitor.color_temperature.is_some() {
            names.push("color_temperature");
        }
        if self.monitor.color_preset.is_some() {
            names.push("color_preset");
        }
        if self.monitor.rgb_gain.red.is_some() {
            names.push("rgb_gain.red");
        }
        if self.monitor.rgb_gain.green.is_some() {
            names.push("rgb_gain.green");
        }
        if self.monitor.rgb_gain.blue.is_some() {
            names.push("rgb_gain.blue");
        }
        if self.monitor.contrast.is_some() {
            names.push("contrast");
        }
        if self.monitor.brightness.is_some() {
            names.push("brightness");
        }
        if self.monitor.volume.is_some() {
            names.push("volume");
        }
        if self.monitor.mute.is_some() {
            names.push("mute");
        }
        names
    }
}

/// Map a byte offset from `toml_edit` to a 1-based line and column.
fn location(source: &str, offset: usize) -> (usize, usize) {
    let mut line = 1usize;
    let mut column = 1usize;
    for (index, character) in source.char_indices() {
        if index >= offset {
            break;
        }
        if character == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    (line, column)
}

fn item_location(source: &str, item: &Item) -> Option<(usize, usize)> {
    match item {
        Item::Value(value) => value.span().map(|span| location(source, span.start)),
        _ => None,
    }
}

/// Record one diagnostic for a key or value, with its location when known.
fn complain(
    diagnostics: &mut Vec<Diagnostic>,
    file: &Path,
    source: &str,
    item: Option<&Item>,
    message: String,
) {
    let located = item.and_then(|item| item_location(source, item));
    diagnostics.push(Diagnostic::new(file, located, message));
}

fn table_location(source: &str, table: &Table) -> Option<(usize, usize)> {
    table.span().map(|span| location(source, span.start))
}

fn key_location(source: &str, table: &Table, key: &str) -> Option<(usize, usize)> {
    table
        .key(key)
        .and_then(|key| key.span())
        .map(|span| location(source, span.start))
        .or_else(|| table_location(source, table))
}

fn integer_value(item: &Item) -> Option<i64> {
    item.as_value().and_then(Value::as_integer)
}

fn string_value(item: &Item) -> Option<String> {
    item.as_value()
        .and_then(Value::as_str)
        .map(|text| text.to_string())
}

fn parse_kelvin_name(text: &str) -> Option<u32> {
    let trimmed = text.trim();
    let lower = trimmed.to_ascii_lowercase();
    let digits = lower.strip_suffix('k')?;
    let kelvin: u32 = digits.trim().parse().ok()?;
    KELVIN_VALUES.contains(&kelvin).then_some(kelvin)
}

fn parse_color_temperature(
    item: &Item,
    source: &str,
    file: &Path,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<ColorTemperature> {
    let complain = |diagnostics: &mut Vec<Diagnostic>, message: &str| {
        diagnostics.push(Diagnostic::new(file, item_location(source, item), message));
    };

    if let Some(text) = string_value(item) {
        return match parse_kelvin_name(&text) {
            Some(kelvin) => Some(ColorTemperature::Kelvin(kelvin)),
            None => {
                complain(
                    diagnostics,
                    &format!(
                        "`{text}` is not an accepted temperature; use one of {}",
                        KELVIN_VALUES
                            .iter()
                            .map(|value| format!("{value}k"))
                            .collect::<Vec<String>>()
                            .join(", ")
                    ),
                );
                None
            }
        };
    }

    let table = match item {
        Item::Value(Value::InlineTable(table)) => table.iter().map(|(key, value)| (key.to_string(), value.clone())).collect::<Vec<_>>(),
        _ => {
            complain(
                diagnostics,
                "color_temperature accepts a Kelvin string, { kelvin = N }, { vcp = 0x14, raw = N } or { vcp = 0x0C, raw = N }; a bare integer is ambiguous",
            );
            return None;
        }
    };

    let keys: Vec<&str> = table.iter().map(|(key, _)| key.as_str()).collect();
    let has_kelvin = keys.contains(&"kelvin");
    let has_vcp = keys.contains(&"vcp");
    let has_raw = keys.contains(&"raw");
    let has_percent = keys.contains(&"percent");

    if has_percent {
        complain(diagnostics, "a temperature is never a percentage");
        return None;
    }

    if has_kelvin {
        if table.len() != 1 {
            complain(
                diagnostics,
                "{ kelvin = N } must not be combined with another unit",
            );
            return None;
        }
        let raw = table[0].1.as_integer();
        let kelvin = raw.and_then(|value| u32::try_from(value).ok());
        return match kelvin {
            Some(value) if KELVIN_VALUES.contains(&value) => Some(ColorTemperature::Kelvin(value)),
            Some(value) => {
                complain(
                    diagnostics,
                    &format!(
                        "{value} K is not an accepted temperature; accepted values are {}",
                        KELVIN_VALUES
                            .iter()
                            .map(|item| item.to_string())
                            .collect::<Vec<String>>()
                            .join(", ")
                    ),
                );
                None
            }
            None => {
                complain(diagnostics, "{ kelvin = N } requires a positive integer");
                None
            }
        };
    }

    if has_vcp && has_raw {
        if table.len() != 2 {
            complain(
                diagnostics,
                "{ vcp = C, raw = N } accepts exactly those two keys",
            );
            return None;
        }
        let vcp_raw = table
            .iter()
            .find(|(key, _)| key == "vcp")
            .and_then(|(_, value)| value.as_integer());
        let value_raw = table
            .iter()
            .find(|(key, _)| key == "raw")
            .and_then(|(_, value)| value.as_integer());
        let (Some(vcp_raw), Some(value_raw)) = (vcp_raw, value_raw) else {
            complain(diagnostics, "vcp and raw must both be integers");
            return None;
        };
        let vcp = match u16::try_from(vcp_raw) {
            Ok(code) => code,
            Err(_) => {
                complain(diagnostics, "the vcp code must be a non-negative integer");
                return None;
            }
        };
        if vcp != VCP_COLOR_PRESET && vcp != VCP_COLOR_REQUEST {
            complain(
                diagnostics,
                &format!(
                    "color_temperature accepts only vcp 0x14 (preset) or 0x0C (request), not {vcp:#04X}"
                ),
            );
            return None;
        }
        if value_raw < 0 {
            complain(diagnostics, "raw must not be negative");
            return None;
        }
        return Some(ColorTemperature::Native {
            vcp,
            raw: value_raw as u64,
        });
    }

    complain(
        diagnostics,
        "color_temperature requires { kelvin = N }, { vcp = 0x14, raw = N } or { vcp = 0x0C, raw = N }",
    );
    None
}

fn parse_color_preset(
    item: &Item,
    source: &str,
    file: &Path,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<ColorPreset> {
    if let Some(text) = string_value(item) {
        if COLOR_PRESET_VALUES.iter().any(|(name, _)| *name == text) {
            return Some(ColorPreset::Named(text));
        }
        diagnostics.push(Diagnostic::new(
            file,
            item_location(source, item),
            format!(
                "`{text}` is not a known colour preset; known names are {}",
                COLOR_PRESET_VALUES
                    .iter()
                    .map(|(name, _)| *name)
                    .collect::<Vec<&str>>()
                    .join(", ")
            ),
        ));
        return None;
    }

    match parse_control_value(item, ValueKind::Discrete) {
        Ok(ControlValue::Native(value)) => Some(ColorPreset::Native(value)),
        Ok(ControlValue::Percent(_)) => {
            diagnostics.push(Diagnostic::new(
                file,
                item_location(source, item),
                "a colour preset is a choice, not a percentage",
            ));
            None
        }
        Err(error) => {
            diagnostics.push(Diagnostic::new(
                file,
                item_location(source, item),
                format!("color_preset: {error}"),
            ));
            None
        }
    }
}

/// Parse and validate one profile file.
pub fn parse(source: &str, path: &Path) -> Result<Profile, ProfileError> {
    let document: DocumentMut = match source.parse() {
        Ok(document) => document,
        Err(error) => {
            let offset = error.span().map(|span| span.start).unwrap_or(0);
            return Err(ProfileError::single(Diagnostic::new(
                path,
                Some(location(source, offset)),
                format!("invalid TOML: {error}"),
            )));
        }
    };

    let mut diagnostics: Vec<Diagnostic> = Vec::new();

    const TOP_LEVEL: &[&str] = &["schema_version", "id", "name", "icon", "order", "monitor"];
    for (key, _) in document.iter() {
        if !TOP_LEVEL.contains(&key) {
            complain(&mut diagnostics, path, source, 
                document.get(key),
                format!("unknown key `{key}` in a profile"),
            );
        }
    }

    let schema_version = document
        .get("schema_version")
        .and_then(integer_value)
        .and_then(|value| u32::try_from(value).ok());
    match schema_version {
        Some(SCHEMA_VERSION) => {}
        Some(other) => complain(&mut diagnostics, path, source, 
            document.get("schema_version"),
            format!("unsupported schema_version {other}; this build understands {SCHEMA_VERSION}"),
        ),
        None => complain(&mut diagnostics, path, source, 
            document.get("schema_version"),
            String::from("schema_version = 1 is required"),
        ),
    }

    let id = document.get("id").and_then(string_value).unwrap_or_default();
    if id.is_empty() {
        complain(&mut diagnostics, path, source, document.get("id"), String::from("id is required"));
    } else if !is_valid_id(&id) {
        complain(&mut diagnostics, path, source, 
            document.get("id"),
            format!("id `{id}` must be a lowercase ASCII slug of letters, digits and hyphens"),
        );
    }

    let name = document.get("name").and_then(string_value).unwrap_or_default();
    if name.trim().is_empty() {
        complain(&mut diagnostics, path, source, document.get("name"), String::from("name must be a non-empty label"));
    }

    let icon = document.get("icon").and_then(string_value).unwrap_or_default();
    if icon.is_empty() {
        complain(&mut diagnostics, path, source, document.get("icon"), String::from("icon is required"));
    } else if !ICON_NAMES.contains(&icon.as_str()) {
        complain(&mut diagnostics, path, source, 
            document.get("icon"),
            format!(
                "unknown icon `{icon}`; built-in icons are {}",
                ICON_NAMES.join(", ")
            ),
        );
    }

    let order = match document.get("order") {
        None => 100,
        Some(item) => match integer_value(item) {
            Some(value) => value,
            None => {
                complain(&mut diagnostics, path, source, document.get("order"), String::from("order must be an integer"));
                100
            }
        },
    };

    let mut monitor = MonitorSettings::default();
    match document.get("monitor") {
        None => complain(&mut diagnostics, path, source, None, String::from("[monitor] is required")),
        Some(Item::Table(table)) => {
            const MONITOR_KEYS: &[&str] = &[
                "brightness",
                "contrast",
                "color_temperature",
                "color_preset",
                "rgb_gain",
                "volume",
                "mute",
            ];
            for (key, _) in table.iter() {
                if !MONITOR_KEYS.contains(&key) {
                    complain(&mut diagnostics, path, source, 
                        table.get(key),
                        format!("unknown key `monitor.{key}`"),
                    );
                }
            }

            for (field, slot) in [
                ("brightness", &mut monitor.brightness),
                ("contrast", &mut monitor.contrast),
                ("volume", &mut monitor.volume),
            ] {
                if let Some(item) = table.get(field) {
                    match parse_control_value(item, ValueKind::Continuous) {
                        Ok(value) => *slot = Some(value),
                        Err(error) => diagnostics.push(Diagnostic::new(
                            path,
                            item_location(source, item),
                            format!("{field}: {error}"),
                        )),
                    }
                }
            }

            if let Some(item) = table.get("color_temperature") {
                monitor.color_temperature = parse_color_temperature(item, source, path, &mut diagnostics);
            }
            if let Some(item) = table.get("color_preset") {
                monitor.color_preset = parse_color_preset(item, source, path, &mut diagnostics);
            }

            if let Some(item) = table.get("mute") {
                match item.as_value().and_then(Value::as_bool) {
                    Some(value) => monitor.mute = Some(value),
                    None => diagnostics.push(Diagnostic::new(
                        path,
                        item_location(source, item),
                        String::from("mute must be true or false"),
                    )),
                }
            }

            match table.get("rgb_gain") {
                None => {}
                Some(Item::Table(gains)) => {
                    for (key, _) in gains.iter() {
                        if !["red", "green", "blue"].contains(&key) {
                            diagnostics.push(Diagnostic::new(
                                path,
                                key_location(source, gains, key),
                                format!("unknown key `monitor.rgb_gain.{key}`"),
                            ));
                        }
                    }
                    for (field, slot) in [
                        ("red", &mut monitor.rgb_gain.red),
                        ("green", &mut monitor.rgb_gain.green),
                        ("blue", &mut monitor.rgb_gain.blue),
                    ] {
                        if let Some(item) = gains.get(field) {
                            match parse_control_value(item, ValueKind::Continuous) {
                                Ok(value) => *slot = Some(value),
                                Err(error) => diagnostics.push(Diagnostic::new(
                                    path,
                                    item_location(source, item),
                                    format!("rgb_gain.{field}: {error}"),
                                )),
                            }
                        }
                    }
                }
                Some(other) => diagnostics.push(Diagnostic::new(
                    path,
                    item_location(source, other),
                    String::from("[monitor.rgb_gain] must be a table"),
                )),
            }
        }
        Some(other) => diagnostics.push(Diagnostic::new(
            path,
            item_location(source, other),
            String::from("[monitor] must be a table"),
        )),
    }

    if monitor.is_empty() && diagnostics.is_empty() {
        complain(&mut diagnostics, path, source, 
            document.get("monitor"),
            String::from("a profile must request at least one control"),
        );
    }

    if monitor.color_temperature.is_some() && monitor.color_preset.is_some() {
        complain(&mut diagnostics, path, source, 
            document.get("monitor"),
            String::from(
                "color_temperature and color_preset are alternative white-point mechanisms; use one",
            ),
        );
    }
    if monitor.color_temperature.is_some() && monitor.rgb_gain.any() {
        complain(&mut diagnostics, path, source, 
            document.get("monitor"),
            String::from("color_temperature cannot be combined with RGB gains"),
        );
    }
    if let Some(preset) = monitor.color_preset.as_ref() {
        if monitor.rgb_gain.any() && !preset.allows_gains() {
            complain(&mut diagnostics, path, source, 
                document.get("monitor"),
                String::from(
                    "RGB gains may accompany only a user preset (user1, user2, user3); this preset may ignore or overwrite them",
                ),
            );
        }
    }

    if !diagnostics.is_empty() {
        return Err(ProfileError { diagnostics });
    }

    Ok(Profile {
        schema_version: SCHEMA_VERSION,
        id,
        name,
        icon,
        order,
        monitor,
        source: path.to_path_buf(),
    })
}

/// Lowercase ASCII slug: letters, digits and inner hyphens.
pub fn is_valid_id(id: &str) -> bool {
    if id.is_empty() || id.len() > 64 {
        return false;
    }
    let bytes = id.as_bytes();
    if !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit() {
        return false;
    }
    if !bytes[bytes.len() - 1].is_ascii_lowercase() && !bytes[bytes.len() - 1].is_ascii_digit() {
        return false;
    }
    bytes
        .iter()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn path() -> PathBuf {
        PathBuf::from("C:\\\\profiles\\\\test.toml")
    }

    fn parse_ok(text: &str) -> Profile {
        parse(text, &path()).expect("profile should parse")
    }

    fn parse_err(text: &str) -> ProfileError {
        parse(text, &path()).expect_err("profile should be rejected")
    }

    const MINIMAL: &str = "schema_version = 1\nid = \"night\"\nname = \"Night\"\nicon = \"night\"\norder = 20\n\n[monitor]\nbrightness = 15\n";

    #[test]
    fn parses_a_minimal_profile() {
        let profile = parse_ok(MINIMAL);
        assert_eq!(profile.id, "night");
        assert_eq!(profile.name, "Night");
        assert_eq!(profile.icon, "night");
        assert_eq!(profile.order, 20);
        assert_eq!(profile.monitor.brightness, Some(ControlValue::Percent(15)));
        assert!(profile.monitor.contrast.is_none());
    }

    #[test]
    fn order_defaults_to_one_hundred() {
        let text = MINIMAL.replace("order = 20\n", "");
        assert_eq!(parse_ok(&text).order, 100);
    }

    #[test]
    fn every_shipped_example_parses() {
        // The examples in the repository are part of the contract; two of them are reference-only
        // files that deliberately contain conflicting alternatives, so only the real ones are used.
        for (name, text) in [
            (
                "day",
                "schema_version = 1\nid = \"day\"\nname = \"Day\"\nicon = \"day\"\norder = 10\n\n[monitor]\nbrightness = 65\n",
            ),
            ("night", MINIMAL),
        ] {
            let profile = parse_ok(text);
            assert_eq!(profile.id, name);
        }
    }

    #[test]
    fn unknown_top_level_key_is_an_error_with_a_location() {
        let text = MINIMAL.replace("schema_version = 1", "schema_version = 1\nvolume_boost = 5");
        let error = parse_err(&text);
        let rendered = error.render();
        assert!(rendered.contains("unknown key `volume_boost`"), "{rendered}");
        assert!(rendered.contains("test.toml:"), "{rendered}");
    }

    #[test]
    fn unknown_monitor_key_is_an_error() {
        let text = MINIMAL.replace("brightness = 15", "brightness = 15\nsharpness = 4");
        let rendered = parse_err(&text).render();
        assert!(rendered.contains("unknown key `monitor.sharpness`"), "{rendered}");

        // A key written after the table header belongs to that table, not to the top level.
        let text = format!("{MINIMAL}volume_boost = 5\n");
        assert!(parse_err(&text).render().contains("unknown key `monitor.volume_boost`"));
    }

    #[test]
    fn unsupported_schema_version_is_rejected() {
        let text = MINIMAL.replace("schema_version = 1", "schema_version = 2");
        let rendered = parse_err(&text).render();
        assert!(rendered.contains("unsupported schema_version 2"), "{rendered}");
    }

    #[test]
    fn missing_schema_version_is_rejected() {
        let text = MINIMAL.replace("schema_version = 1\n", "");
        assert!(parse_err(&text).render().contains("schema_version = 1 is required"));
    }

    #[test]
    fn invalid_ids_are_rejected() {
        for id in ["Night", "night_2", "-night", "night ", "night/two", ""] {
            let text = MINIMAL.replace("id = \"night\"", &format!("id = \"{id}\""));
            assert!(parse(&text, &path()).is_err(), "id `{id}` should be rejected");
        }
        assert!(is_valid_id("night-2"));
    }

    #[test]
    fn unknown_icons_are_rejected() {
        let text = MINIMAL.replace("icon = \"night\"", "icon = \"moon\"");
        assert!(parse_err(&text).render().contains("unknown icon `moon`"));
    }

    #[test]
    fn empty_profiles_are_rejected() {
        let text = "schema_version = 1\nid = \"empty\"\nname = \"Empty\"\nicon = \"day\"\n\n[monitor]\n";
        assert!(parse_err(text)
            .render()
            .contains("must request at least one control"));
    }

    #[test]
    fn kelvin_intent_accepts_a_string_and_a_table() {
        for value in ["\"6500k\"", "{ kelvin = 6500 }"] {
            let text = format!(
                "{}\ncolor_temperature = {value}\n",
                MINIMAL.replace("brightness = 15", "")
                    .replace("[monitor]\n\n", "[monitor]\n")
            );
            let profile = parse_ok(&text);
            assert_eq!(profile.monitor.color_temperature, Some(ColorTemperature::Kelvin(6500)));
        }
    }

    #[test]
    fn native_temperature_forms_carry_the_code_and_value_separately() {
        let preset = MINIMAL.replace(
            "brightness = 15",
            "color_temperature = { vcp = 0x14, raw = 5 }",
        );
        assert_eq!(
            parse_ok(&preset).monitor.color_temperature,
            Some(ColorTemperature::Native {
                vcp: VCP_COLOR_PRESET,
                raw: 5
            })
        );

        let request = MINIMAL.replace(
            "brightness = 15",
            "color_temperature = { vcp = 0x0C, raw = 0x23 }",
        );
        assert_eq!(
            parse_ok(&request).monitor.color_temperature,
            Some(ColorTemperature::Native {
                vcp: VCP_COLOR_REQUEST,
                raw: 0x23
            })
        );
    }

    #[test]
    fn bare_integer_temperature_is_rejected_as_ambiguous() {
        let text = MINIMAL.replace("brightness = 15", "color_temperature = 6500");
        assert!(parse_err(&text)
            .render()
            .contains("a bare integer is ambiguous"));
    }

    #[test]
    fn temperature_rejects_percentages_and_foreign_vcp_codes() {
        let percent = MINIMAL.replace("brightness = 15", "color_temperature = { percent = 50 }");
        assert!(parse_err(&percent).render().contains("never a percentage"));

        let wrong_code = MINIMAL.replace(
            "brightness = 15",
            "color_temperature = { vcp = 0x10, raw = 5 }",
        );
        assert!(parse_err(&wrong_code)
            .render()
            .contains("only vcp 0x14 (preset) or 0x0C (request)"));
    }

    #[test]
    fn unsupported_kelvin_values_are_rejected() {
        let text = MINIMAL.replace("brightness = 15", "color_temperature = \"1234k\"");
        assert!(parse_err(&text).render().contains("not an accepted temperature"));
    }

    #[test]
    fn white_point_mechanisms_cannot_be_combined() {
        let both = MINIMAL.replace(
            "brightness = 15",
            "color_temperature = \"6500k\"\ncolor_preset = \"srgb\"",
        );
        assert!(parse_err(&both)
            .render()
            .contains("alternative white-point mechanisms"));

        let gains = MINIMAL.replace(
            "brightness = 15",
            "color_temperature = \"6500k\"\n[monitor.rgb_gain]\nred = 100",
        );
        assert!(parse_err(&gains)
            .render()
            .contains("cannot be combined with RGB gains"));
    }

    #[test]
    fn gains_require_a_user_preset() {
        let text = MINIMAL.replace(
            "brightness = 15",
            "color_preset = \"6500k\"\n[monitor.rgb_gain]\nred = 100",
        );
        assert!(parse_err(&text)
            .render()
            .contains("only a user preset"));

        let user = MINIMAL.replace(
            "brightness = 15",
            "color_preset = \"user1\"\n[monitor.rgb_gain]\nred = 100",
        );
        assert!(parse_ok(&user).monitor.rgb_gain.red.is_some());
    }

    #[test]
    fn color_preset_accepts_names_and_native_values() {
        for (written, expected) in [
            ("\"srgb\"", ColorPreset::Named(String::from("srgb"))),
            ("0x0B", ColorPreset::Native(0x0B)),
            ("{ raw = 0x0B }", ColorPreset::Native(0x0B)),
        ] {
            let text = MINIMAL.replace("brightness = 15", &format!("color_preset = {written}"));
            assert_eq!(parse_ok(&text).monitor.color_preset, Some(expected));
        }
    }

    #[test]
    fn color_preset_rejects_percentages_and_unknown_names() {
        let percent = MINIMAL.replace("brightness = 15", "color_preset = { percent = 40 }");
        assert!(parse_err(&percent).render().contains("choice, not a percentage"));

        let name = MINIMAL.replace("brightness = 15", "color_preset = \"sepia\"");
        assert!(parse_err(&name).render().contains("not a known colour preset"));
    }

    #[test]
    fn native_gain_values_above_one_hundred_are_preserved() {
        let text = MINIMAL.replace("brightness = 15", "brightness = { raw = 0xFF }");
        assert_eq!(parse_ok(&text).monitor.brightness, Some(ControlValue::Native(255)));
    }

    #[test]
    fn repository_examples_parse_with_the_current_schema() {
        // The shipped examples are part of the user contract; drift between them and the parser is a
        // bug in whichever changed last.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
        for name in [
            "day.toml",
            "night.toml",
            "full-profile.toml",
            "color-temperature.toml",
        ] {
            let path = root.join(name);
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
            if let Err(error) = parse(&text, &path) {
                panic!("{}: {}", path.display(), error.render());
            }
        }
    }

    #[test]
    fn malformed_toml_reports_its_location() {
        let error = parse_err("schema_version = 1\nid = \"a\"\n[monitor\n");
        assert!(error.render().contains("invalid TOML"));
    }

    #[test]
    fn control_order_matches_the_application_order() {
        let text = "schema_version = 1\nid = \"full\"\nname = \"Full\"\nicon = \"coding\"\n\n[monitor]\nbrightness = 35\ncontrast = 70\nvolume = 20\nmute = false\n\n[monitor.rgb_gain]\nred = 100\n";
        let profile = parse_ok(text);
        assert_eq!(
            profile.requested_control_names(),
            vec!["rgb_gain.red", "contrast", "brightness", "volume", "mute"]
        );
    }
}
