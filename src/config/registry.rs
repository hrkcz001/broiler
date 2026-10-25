//! The profile registry: one directory, validated as a whole.
//!
//! Two rules matter more than convenience:
//!
//! * every `*.toml` file is parsed before anything is applied, so one invalid file aborts
//!   activation instead of quietly reducing a multi-profile folder to the one valid profile;
//! * identifiers are unique across the directory, and a reload keeps the last good registry rather
//!   than dropping the invalid file.

use std::fmt;
use std::path::{Path, PathBuf};

use super::profile::{self, Diagnostic, Profile};

/// Why a directory of profiles could not be accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistryError {
    pub diagnostics: Vec<Diagnostic>,
}

impl RegistryError {
    pub fn render(&self) -> String {
        self.diagnostics
            .iter()
            .map(Diagnostic::render)
            .collect::<Vec<String>>()
            .join("\n")
    }
}

impl fmt::Display for RegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.render())
    }
}

impl std::error::Error for RegistryError {}

impl From<profile::ProfileError> for RegistryError {
    fn from(error: profile::ProfileError) -> RegistryError {
        RegistryError {
            diagnostics: error.diagnostics,
        }
    }
}

/// Profiles in application order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProfileRegistry {
    profiles: Vec<Profile>,
}

impl ProfileRegistry {
    /// Build a registry from already parsed profiles, rejecting duplicate identifiers and sorting by
    /// `order` then `id`.
    pub fn new(mut profiles: Vec<Profile>) -> Result<ProfileRegistry, RegistryError> {
        let mut diagnostics = Vec::new();
        let mut seen: Vec<String> = Vec::new();
        for profile in &profiles {
            if seen.contains(&profile.id) {
                diagnostics.push(Diagnostic::new(
                    &profile.source,
                    None,
                    format!(
                        "duplicate profile id `{}`: identifiers must be unique across the directory",
                        profile.id
                    ),
                ));
            } else {
                seen.push(profile.id.clone());
            }
        }
        if !diagnostics.is_empty() {
            return Err(RegistryError { diagnostics });
        }
        profiles.sort_by(|left, right| {
            left.order
                .cmp(&right.order)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(ProfileRegistry { profiles })
    }

    pub fn profiles(&self) -> &[Profile] {
        &self.profiles
    }

    pub fn len(&self) -> usize {
        self.profiles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.profiles.is_empty()
    }

    pub fn find(&self, id: &str) -> Option<&Profile> {
        self.profiles.iter().find(|profile| profile.id == id)
    }

    /// Neighbouring profile in registry order, wrapping at the end.
    pub fn next_after(&self, id: &str) -> Option<&Profile> {
        if self.profiles.is_empty() {
            return None;
        }
        let index = self
            .profiles
            .iter()
            .position(|profile| profile.id == id)
            .map(|index| (index + 1) % self.profiles.len())
            .unwrap_or(0);
        self.profiles.get(index)
    }

    /// Load and validate every `*.toml` file in `directory`.
    ///
    /// A missing directory is an empty registry, not an error: zero profiles is a normal state with
    /// its own setup path. A directory that exists but contains an invalid profile is an error.
    pub fn load_dir(directory: &Path) -> Result<ProfileRegistry, RegistryError> {
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ProfileRegistry::default())
            }
            Err(error) => {
                return Err(RegistryError {
                    diagnostics: vec![Diagnostic::new(
                        directory,
                        None,
                        format!("cannot read the profiles directory: {error}"),
                    )],
                })
            }
        };

        let mut paths: Vec<PathBuf> = Vec::new();
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    return Err(RegistryError {
                        diagnostics: vec![Diagnostic::new(
                            directory,
                            None,
                            format!("cannot read a profiles directory entry: {error}"),
                        )],
                    })
                }
            };
            let path = entry.path();
            if is_profile_file(&path) {
                paths.push(path);
            }
        }
        paths.sort();

        let mut profiles = Vec::new();
        let mut diagnostics = Vec::new();
        for path in paths {
            match std::fs::read_to_string(&path) {
                Ok(text) => match profile::parse(&text, &path) {
                    Ok(parsed) => profiles.push(parsed),
                    Err(error) => diagnostics.extend(error.diagnostics),
                },
                Err(error) => diagnostics.push(Diagnostic::new(
                    &path,
                    None,
                    format!("cannot read the profile: {error}"),
                )),
            }
        }

        if !diagnostics.is_empty() {
            return Err(RegistryError { diagnostics });
        }
        ProfileRegistry::new(profiles)
    }
}

/// True for files that should be treated as profiles.
///
/// Editor temporary files and unrelated extensions are ignored, because a half-written swap file
/// must not abort activation and is not a profile the user wrote.
pub fn is_profile_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return path
            .extension()
            .map(|extension| extension.eq_ignore_ascii_case("toml"))
            .unwrap_or(false);
    };
    if name.starts_with('.') || name.starts_with('~') || name.ends_with('~') {
        return false;
    }
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".tmp") || lower.ends_with(".swp") || lower.ends_with(".bak") {
        return false;
    }
    if !path
        .extension()
        .map(|extension| extension.eq_ignore_ascii_case("toml"))
        .unwrap_or(false)
    {
        return false;
    }
    // "day.toml.tmp" style names survive the extension check because the extension is "tmp".
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let mut directory = std::env::temp_dir();
        directory.push(format!("broiler-registry-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("scratch directory");
        directory
    }

    fn profile_text(id: &str, order: i64, brightness: u8) -> String {
        format!(
            "schema_version = 1\nid = \"{id}\"\nname = \"{id}\"\nicon = \"day\"\norder = {order}\n\n[monitor]\nbrightness = {brightness}\n"
        )
    }

    fn write(directory: &Path, name: &str, text: &str) {
        std::fs::write(directory.join(name), text).expect("write profile");
    }

    #[test]
    fn missing_directory_is_an_empty_registry() {
        let directory = scratch("missing");
        let missing = directory.join("does-not-exist");
        let registry = ProfileRegistry::load_dir(&missing).expect("empty registry");
        assert!(registry.is_empty());
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn empty_directory_is_an_empty_registry() {
        let directory = scratch("empty");
        let registry = ProfileRegistry::load_dir(&directory).expect("empty registry");
        assert_eq!(registry.len(), 0);
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn profiles_are_ordered_by_order_then_id() {
        let directory = scratch("order");
        write(&directory, "b.toml", &profile_text("bravo", 20, 20));
        write(&directory, "a.toml", &profile_text("alpha", 20, 10));
        write(&directory, "c.toml", &profile_text("charlie", 10, 30));

        let registry = ProfileRegistry::load_dir(&directory).expect("registry");
        let ids: Vec<&str> = registry.profiles().iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["charlie", "alpha", "bravo"]);
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn duplicate_ids_are_rejected() {
        let directory = scratch("duplicate");
        write(&directory, "one.toml", &profile_text("day", 10, 10));
        write(&directory, "two.toml", &profile_text("day", 20, 20));

        let error = ProfileRegistry::load_dir(&directory).expect_err("duplicate id");
        assert!(error.render().contains("duplicate profile id `day`"));
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn one_invalid_file_aborts_the_whole_directory() {
        let directory = scratch("invalid");
        write(&directory, "day.toml", &profile_text("day", 10, 10));
        write(&directory, "broken.toml", "schema_version = 1\nid = \"broken\"\n[monitor]\nbrightness = 200\n");

        let error = ProfileRegistry::load_dir(&directory).expect_err("invalid profile");
        let rendered = error.render();
        assert!(rendered.contains("broken.toml"), "{rendered}");
        assert!(rendered.contains("outside 0..=100"), "{rendered}");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn temporary_and_unrelated_files_are_ignored() {
        let directory = scratch("ignored");
        write(&directory, "day.toml", &profile_text("day", 10, 10));
        write(&directory, ".day.toml.swp", "garbage");
        write(&directory, "day.toml.tmp", "garbage");
        write(&directory, "notes.txt", "garbage");
        write(&directory, "~day.toml", "garbage");

        let registry = ProfileRegistry::load_dir(&directory).expect("registry");
        assert_eq!(registry.len(), 1);
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn next_after_wraps_in_registry_order() {
        let directory = scratch("next");
        write(&directory, "a.toml", &profile_text("alpha", 10, 10));
        write(&directory, "b.toml", &profile_text("bravo", 20, 20));

        let registry = ProfileRegistry::load_dir(&directory).expect("registry");
        assert_eq!(registry.next_after("alpha").map(|p| p.id.as_str()), Some("bravo"));
        assert_eq!(registry.next_after("bravo").map(|p| p.id.as_str()), Some("alpha"));
        assert_eq!(registry.next_after("unknown").map(|p| p.id.as_str()), Some("alpha"));
        let _ = std::fs::remove_dir_all(&directory);
    }
}
