//! Startup decisions.
//!
//! The order below follows the documented lifecycle: monitors are discovered first, then the target
//! set is resolved (remembered selection or the required checkbox dialog), then the whole profile
//! directory is validated, and only then does the profile count choose between the setup window, the
//! single-profile one-shot run and residency.
//!
//! Keeping this as a pure function is deliberate: every branch is testable without a monitor, a
//! window or a daemon.

use std::fmt;

use crate::config::registry::{ProfileRegistry, RegistryError};
use crate::config::settings::Settings;

/// Why the selection dialog is required.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionReason {
    /// More than one controllable monitor and no remembered choice.
    MultipleMonitorsNoChoice,
    /// A remembered selection is no longer present, but other controllable monitors are.
    SavedSelectionMissing,
}

impl SelectionReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SelectionReason::MultipleMonitorsNoChoice => "multiple_monitors_no_choice",
            SelectionReason::SavedSelectionMissing => "saved_selection_missing",
        }
    }
}

/// What normal startup does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartupOutcome {
    /// No controllable external monitor: exit 0 silently, no dialog, no tray.
    ExitNoMonitors,
    /// A profile file failed validation: activation aborts and the diagnostics are shown.
    ExitInvalidProfiles { diagnostics: usize },
    /// The user confirmed an empty selection, or cancelled the initial dialog.
    ExitEmptySelection,
    /// Show the checkbox selection dialog, including at logon.
    ShowSelection {
        reason: SelectionReason,
        remember: bool,
    },
    /// Zero profiles: show the minimal setup path instead of inventing profiles.
    SetupWindow,
    /// Exactly one valid profile: apply it to the targets and exit without a tray icon.
    OneShot {
        targets: Vec<String>,
        profile: String,
    },
    /// More than one profile: stay resident, preserve current values, one tray icon per target.
    Resident {
        targets: Vec<String>,
        profiles: usize,
    },
}

impl StartupOutcome {
    /// Stable machine-readable name, used by the CLI and by `status`.
    pub fn as_str(&self) -> &'static str {
        match self {
            StartupOutcome::ExitNoMonitors => "no_controllable_monitors",
            StartupOutcome::ExitInvalidProfiles { .. } => "invalid_profiles",
            StartupOutcome::ExitEmptySelection => "empty_selection",
            StartupOutcome::ShowSelection { .. } => "selection_required",
            StartupOutcome::SetupWindow => "zero_profiles",
            StartupOutcome::OneShot { .. } => "one_shot_complete",
            StartupOutcome::Resident { .. } => "resident",
        }
    }

    /// Target identifiers this outcome would control, when it controls any.
    pub fn targets(&self) -> &[String] {
        match self {
            StartupOutcome::OneShot { targets, .. } | StartupOutcome::Resident { targets, .. } => {
                targets
            }
            _ => &[],
        }
    }
}

impl fmt::Display for StartupOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Inputs to the startup decision.
pub struct StartupState<'a> {
    /// Identifiers of controllable external monitors currently present.
    pub monitors: &'a [String],
    /// The whole profile directory, already validated as a unit.
    pub registry: Result<&'a ProfileRegistry, &'a RegistryError>,
    pub settings: &'a Settings,
    /// True when the user deliberately confirmed an empty selection.
    pub selection_confirmed_empty: bool,
}

/// Intersect the remembered identifiers with what is present, keeping the remembered order.
fn remembered_targets(remembered: &[String], monitors: &[String]) -> Vec<String> {
    remembered
        .iter()
        .filter(|id| monitors.iter().any(|present| present == *id))
        .cloned()
        .collect()
}

/// Decide what normal startup does. Pure: no I/O, no hardware, no UI.
pub fn decide(state: &StartupState<'_>) -> StartupOutcome {
    if state.monitors.is_empty() {
        return StartupOutcome::ExitNoMonitors;
    }

    if state.selection_confirmed_empty {
        return StartupOutcome::ExitEmptySelection;
    }

    let targets = if !state.settings.monitor_ids.is_empty() {
        let present = remembered_targets(&state.settings.monitor_ids, state.monitors);
        if present.is_empty() {
            // A remembered selection that is gone is never rebound to whatever is present.
            return StartupOutcome::ShowSelection {
                reason: SelectionReason::SavedSelectionMissing,
                remember: state.settings.remember,
            };
        }
        present
    } else if state.monitors.len() == 1 {
        vec![state.monitors[0].clone()]
    } else {
        return StartupOutcome::ShowSelection {
            reason: SelectionReason::MultipleMonitorsNoChoice,
            remember: state.settings.remember,
        };
    };

    let registry = match state.registry {
        Ok(registry) => registry,
        Err(error) => {
            return StartupOutcome::ExitInvalidProfiles {
                diagnostics: error.diagnostics.len(),
            }
        }
    };

    match registry.len() {
        0 => StartupOutcome::SetupWindow,
        1 => StartupOutcome::OneShot {
            targets,
            profile: registry.profiles()[0].id.clone(),
        },
        count => StartupOutcome::Resident {
            targets,
            profiles: count,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::profile::{parse as parse_profile, Profile};
    use crate::config::profile::Diagnostic;
    use crate::config::registry::RegistryError;
    use std::path::{Path, PathBuf};

    fn profile(id: &str, order: i64) -> Profile {
        let text = format!(
            "schema_version = 1\nid = \"{id}\"\nname = \"{id}\"\nicon = \"day\"\norder = {order}\n\n[monitor]\nbrightness = 40\n"
        );
        parse_profile(&text, Path::new("test.toml")).expect("profile")
    }

    fn registry(ids: &[&str]) -> ProfileRegistry {
        let profiles = ids
            .iter()
            .enumerate()
            .map(|(index, id)| profile(id, index as i64 * 10))
            .collect();
        ProfileRegistry::new(profiles).expect("registry")
    }

    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    fn invalid() -> RegistryError {
        RegistryError {
            diagnostics: vec![Diagnostic::new(
                &PathBuf::from("broken.toml"),
                Some((1, 1)),
                String::from("invalid"),
            )],
        }
    }

    fn decide_with(
        monitors: &[&str],
        registry: &ProfileRegistry,
        settings: &Settings,
    ) -> StartupOutcome {
        decide(&StartupState {
            monitors: &ids(monitors),
            registry: Ok(registry),
            settings,
            selection_confirmed_empty: false,
        })
    }

    #[test]
    fn zero_monitors_exits_silently_even_with_profiles() {
        let registry = registry(&["day", "night"]);
        let outcome = decide_with(&[], &registry, &Settings::default());
        assert_eq!(outcome, StartupOutcome::ExitNoMonitors);
        assert_eq!(outcome.as_str(), "no_controllable_monitors");
    }

    #[test]
    fn zero_monitors_wins_over_an_invalid_registry() {
        let error = invalid();
        let settings = Settings::default();
        let outcome = decide(&StartupState {
            monitors: &[],
            registry: Err(&error),
            settings: &settings,
            selection_confirmed_empty: false,
        });
        assert_eq!(outcome, StartupOutcome::ExitNoMonitors);
    }

    #[test]
    fn one_monitor_and_one_profile_is_a_one_shot_run() {
        let registry = registry(&["day"]);
        let outcome = decide_with(&["mon-a"], &registry, &Settings::default());
        assert_eq!(
            outcome,
            StartupOutcome::OneShot {
                targets: ids(&["mon-a"]),
                profile: String::from("day")
            }
        );
        assert_eq!(outcome.as_str(), "one_shot_complete");
    }

    #[test]
    fn one_monitor_and_many_profiles_is_resident() {
        let registry = registry(&["day", "night", "coding"]);
        let outcome = decide_with(&["mon-a"], &registry, &Settings::default());
        assert_eq!(
            outcome,
            StartupOutcome::Resident {
                targets: ids(&["mon-a"]),
                profiles: 3
            }
        );
    }

    #[test]
    fn many_monitors_without_a_saved_choice_require_the_dialog() {
        let registry = registry(&["day"]);
        let outcome = decide_with(&["mon-a", "mon-b"], &registry, &Settings::default());
        assert_eq!(
            outcome,
            StartupOutcome::ShowSelection {
                reason: SelectionReason::MultipleMonitorsNoChoice,
                remember: false
            }
        );
    }

    #[test]
    fn the_dialog_happens_before_profile_validation() {
        let error = invalid();
        let settings = Settings::default();
        let outcome = decide(&StartupState {
            monitors: &ids(&["mon-a", "mon-b"]),
            registry: Err(&error),
            settings: &settings,
            selection_confirmed_empty: false,
        });
        assert_eq!(
            outcome,
            StartupOutcome::ShowSelection {
                reason: SelectionReason::MultipleMonitorsNoChoice,
                remember: false
            }
        );
    }

    #[test]
    fn a_saved_selection_controls_only_the_present_members() {
        let registry = registry(&["day", "night"]);
        let mut settings = Settings::default();
        settings.remember = true;
        settings.monitor_ids = ids(&["mon-a", "mon-gone"]);

        let outcome = decide_with(&["mon-a", "mon-b"], &registry, &settings);
        assert_eq!(
            outcome,
            StartupOutcome::Resident {
                targets: ids(&["mon-a"]),
                profiles: 2
            }
        );
    }

    #[test]
    fn a_fully_missing_saved_selection_asks_again_instead_of_retargeting() {
        let registry = registry(&["day"]);
        let mut settings = Settings::default();
        settings.remember = true;
        settings.monitor_ids = ids(&["mon-gone"]);

        let outcome = decide_with(&["mon-a", "mon-b"], &registry, &settings);
        assert_eq!(
            outcome,
            StartupOutcome::ShowSelection {
                reason: SelectionReason::SavedSelectionMissing,
                remember: true
            }
        );
    }

    #[test]
    fn zero_profiles_with_monitors_shows_the_setup_path() {
        let registry = registry(&[]);
        let outcome = decide_with(&["mon-a"], &registry, &Settings::default());
        assert_eq!(outcome, StartupOutcome::SetupWindow);
        assert_eq!(outcome.as_str(), "zero_profiles");
    }

    #[test]
    fn an_invalid_profile_aborts_activation_instead_of_using_the_valid_ones() {
        let error = invalid();
        let settings = Settings::default();
        let outcome = decide(&StartupState {
            monitors: &ids(&["mon-a"]),
            registry: Err(&error),
            settings: &settings,
            selection_confirmed_empty: false,
        });
        assert_eq!(
            outcome,
            StartupOutcome::ExitInvalidProfiles { diagnostics: 1 }
        );
    }

    #[test]
    fn a_confirmed_empty_selection_exits_without_tray_icons() {
        let registry = registry(&["day", "night"]);
        let settings = Settings::default();
        let outcome = decide(&StartupState {
            monitors: &ids(&["mon-a", "mon-b"]),
            registry: Ok(&registry),
            settings: &settings,
            selection_confirmed_empty: true,
        });
        assert_eq!(outcome, StartupOutcome::ExitEmptySelection);
    }

    #[test]
    fn targets_are_exposed_only_for_controlling_outcomes() {
        assert!(StartupOutcome::ExitNoMonitors.targets().is_empty());
        assert!(StartupOutcome::SetupWindow.targets().is_empty());
        assert_eq!(
            StartupOutcome::OneShot {
                targets: ids(&["mon-a"]),
                profile: String::from("day")
            }
            .targets(),
            ids(&["mon-a"])
        );
    }
}
