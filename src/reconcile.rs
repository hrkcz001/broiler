//! Read-only profile reconciliation.
//!
//! Under `profiles.startup = "preserve"` a resident instance writes nothing at logon, so it cannot
//! learn the current profile by applying one. It learns it by *reading*: once per monitor and control,
//! through the single serialized worker, it reads exactly the controls the loaded profiles request and
//! compares the device's native values with the native targets the planner itself would write.
//!
//! The result is evidence, not an operation:
//!
//! * `matched` — every requested field of one candidate was read and agrees;
//! * `applied` — this process applied that profile and verified the readback (a different provenance);
//! * `custom` — complete observations exclude every candidate;
//! * `unknown` — the observations cannot establish either, and saying "custom" would be a guess.
//!
//! Nothing here writes, and nothing here polls. `Checking` is the state before the first observation
//! of a monitor completes.

use std::collections::BTreeMap;

use crate::config::profile::{ColorTemperature, Profile};
use crate::config::settings::UnsupportedPolicy;

/// What one read of one control produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Observed {
    /// The device answered with a value and the range it reports for the control.
    Value {
        current: u64,
        minimum: Option<u64>,
        maximum: Option<u64>,
    },
    /// The device positively does not offer the control.
    Unsupported(String),
    /// This build has no read path for the control, so its value cannot be certified either way.
    Unavailable(String),
    /// The read failed or timed out; the hardware value is unknown.
    Failed(String),
}

impl Observed {
    pub fn value(&self) -> Option<u64> {
        match self {
            Observed::Value { current, .. } => Some(*current),
            _ => None,
        }
    }

    /// Why this field cannot participate in a match, when it cannot.
    fn unverified(&self) -> Option<String> {
        match self {
            Observed::Value { .. } => None,
            Observed::Unsupported(reason) | Observed::Unavailable(reason) | Observed::Failed(reason) => {
                Some(reason.clone())
            }
        }
    }
}

/// Everything one observation pass learned about one monitor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Evidence {
    pub monitor: String,
    /// ISO timestamp of the pass, so a surface can say how old the answer is.
    pub observed_at: String,
    fields: BTreeMap<String, Observed>,
}

impl Evidence {
    pub fn new(monitor: &str, observed_at: &str) -> Evidence {
        Evidence {
            monitor: monitor.to_string(),
            observed_at: observed_at.to_string(),
            fields: BTreeMap::new(),
        }
    }

    pub fn record(&mut self, control: &str, observed: Observed) {
        self.fields.insert(control.to_string(), observed);
    }

    pub fn get(&self, control: &str) -> Option<&Observed> {
        self.fields.get(control)
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// The device metadata the planner needs, built from what these reads reported.
    fn metadata(&self) -> crate::plan::DeviceMetadata {
        let mut metadata = crate::plan::DeviceMetadata::default();
        for (control, observed) in &self.fields {
            let support = match observed {
                Observed::Value {
                    minimum: Some(minimum),
                    maximum: Some(maximum),
                    ..
                } if maximum >= minimum => crate::config::value::DeviceValueSupport::Range {
                    minimum: *minimum,
                    maximum: *maximum,
                },
                Observed::Unsupported(_) => crate::config::value::DeviceValueSupport::Unsupported,
                // A value without a usable range, a failed read and a missing read path all leave the
                // metadata unknown, which is what stops a percentage from being mapped by guesswork.
                _ => crate::config::value::DeviceValueSupport::Unknown,
            };
            match control.as_str() {
                "brightness" => metadata.brightness = support,
                "contrast" => metadata.contrast = support,
                "volume" => metadata.volume = support,
                "rgb_gain.red" => metadata.rgb_red = support,
                "rgb_gain.green" => metadata.rgb_green = support,
                "rgb_gain.blue" => metadata.rgb_blue = support,
                "color_preset" => metadata.color_preset = support,
                "mute" => metadata.mute = support,
                _ => {}
            }
        }
        metadata
    }
}

/// The persisted fact a tie-break may use: the last profile this installation applied successfully,
/// and the content fingerprint it had at that moment.
///
/// It is history. It never establishes the current state on its own; it only decides between
/// candidates that the current reads already proved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryHint {
    pub profile_id: String,
    pub fingerprint: String,
}

/// The evidence state of one monitor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CurrentState {
    /// No observation has completed yet.
    Checking,
    /// Reads prove the hardware matches this profile now. Nothing was written.
    Matched,
    /// This process applied this profile and verified the readback.
    Applied,
    /// Complete reads exclude every loaded profile.
    Custom,
    /// Reads are insufficient to certify a match or to exclude every candidate.
    Unknown,
}

impl CurrentState {
    /// The name used on the wire and in the UI.
    pub fn as_str(self) -> &'static str {
        match self {
            CurrentState::Checking => "checking",
            CurrentState::Matched => "matched",
            CurrentState::Applied => "applied_verified",
            CurrentState::Custom => "custom",
            CurrentState::Unknown => "unknown",
        }
    }
}

/// The one answer every surface shows for one monitor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CurrentProfile {
    pub state: CurrentState,
    /// Set only for `matched` and `applied`.
    pub profile_id: Option<String>,
    /// Why the state is what it is, for a tooltip or a status field: the field that could not be read,
    /// or the value that disagreed.
    pub reason: Option<String>,
    /// When the evidence was observed. `None` while checking.
    pub observed_at: Option<String>,
}

impl CurrentProfile {
    fn new(state: CurrentState) -> CurrentProfile {
        CurrentProfile {
            state,
            profile_id: None,
            reason: None,
            observed_at: None,
        }
    }

    pub fn checking() -> CurrentProfile {
        CurrentProfile::new(CurrentState::Checking)
    }

    pub fn unknown(reason: impl Into<String>) -> CurrentProfile {
        let mut profile = CurrentProfile::new(CurrentState::Unknown);
        profile.reason = Some(reason.into());
        profile
    }

    pub fn custom(reason: impl Into<String>) -> CurrentProfile {
        let mut profile = CurrentProfile::new(CurrentState::Custom);
        profile.reason = Some(reason.into());
        profile
    }

    pub fn matched(id: &str, observed_at: &str) -> CurrentProfile {
        CurrentProfile {
            state: CurrentState::Matched,
            profile_id: Some(id.to_string()),
            reason: None,
            observed_at: Some(observed_at.to_string()),
        }
    }

    pub fn applied(id: &str, observed_at: &str) -> CurrentProfile {
        CurrentProfile {
            state: CurrentState::Applied,
            profile_id: Some(id.to_string()),
            reason: None,
            observed_at: Some(observed_at.to_string()),
        }
    }

    /// The profile id this monitor is currently in, whichever way it was established.
    pub fn profile_id(&self) -> Option<&str> {
        self.profile_id.as_deref()
    }
}

/// One candidate's outcome against the evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Verdict {
    /// Every requested field was read and agrees.
    Match,
    /// A field was read and disagrees: this profile is positively not the current one.
    Excluded(String),
    /// A field could not be certified, so this profile can be neither confirmed nor excluded.
    Unverified(String),
}

/// A canonical rendering of the fields a profile requests, for the history fingerprint.
///
/// Only the hardware request participates: renaming a profile does not change what the monitor is in,
/// but editing a value does, which is exactly what makes a stored fingerprint worth comparing.
pub fn fingerprint(profile: &Profile) -> String {
    use sha2::{Digest, Sha256};

    let monitor = &profile.monitor;
    let mut canonical = String::new();
    for control in profile.requested_control_names() {
        canonical.push_str(control);
        canonical.push('=');
        match control {
            "brightness" => canonical.push_str(&render(monitor.brightness)),
            "contrast" => canonical.push_str(&render(monitor.contrast)),
            "volume" => canonical.push_str(&render(monitor.volume)),
            "rgb_gain.red" => canonical.push_str(&render(monitor.rgb_gain.red)),
            "rgb_gain.green" => canonical.push_str(&render(monitor.rgb_gain.green)),
            "rgb_gain.blue" => canonical.push_str(&render(monitor.rgb_gain.blue)),
            "mute" => canonical.push_str(if monitor.mute == Some(true) { "true" } else { "false" }),
            "color_temperature" => match monitor.color_temperature.as_ref() {
                Some(ColorTemperature::Kelvin(kelvin)) => canonical.push_str(&format!("k{kelvin}")),
                Some(ColorTemperature::Native { vcp, raw }) => {
                    canonical.push_str(&format!("v{vcp:#06X}:{raw}"))
                }
                None => canonical.push('-'),
            },
            "color_preset" => match monitor.color_preset.as_ref() {
                Some(preset) => canonical.push_str(&format!("{preset:?}")),
                None => canonical.push('-'),
            },
            _ => canonical.push('-'),
        }
        canonical.push('|');
    }
    let digest = Sha256::digest(canonical.as_bytes());
    digest.iter().take(8).map(|byte| format!("{byte:02x}")).collect()
}

fn render(value: Option<crate::config::value::ControlValue>) -> String {
    match value {
        Some(value) => value.to_string(),
        None => String::from("-"),
    }
}

/// Judge one candidate profile against one evidence set.
///
/// The native target is not re-derived here: the planner produces it with the same metadata and the
/// same rounding the apply path uses, so a profile that would be written as value N is compared with
/// the device's N. Any field the planner refuses or skips makes the candidate unverified rather than
/// falsely mismatched.
fn judge(profile: &Profile, evidence: &Evidence) -> Verdict {
    let requested = profile.requested_control_names();
    if requested.is_empty() {
        // Nothing to compare: an empty request would otherwise match every monitor that offers
        // nothing, which is a vacuous claim rather than evidence.
        return Verdict::Unverified(String::from(
            "the profile requests no hardware control, so it cannot be certified",
        ));
    }

    let metadata = evidence.metadata();
    let plan = match crate::plan::plan(profile, &metadata, UnsupportedPolicy::Skip) {
        Ok(plan) => plan,
        Err(error) => return Verdict::Unverified(format!("{error}")),
    };
    if let Some(skipped) = plan.skipped.first() {
        return Verdict::Unverified(format!("{}: {}", skipped.control, skipped.reason));
    }

    let mut disagreement: Option<String> = None;
    let mut unverified: Option<String> = None;
    for control in &requested {
        let Some(planned) = plan
            .writes
            .iter()
            .find(|write| write.control.name() == *control)
        else {
            unverified.get_or_insert_with(|| {
                format!("{control}: this build cannot plan a value for it")
            });
            continue;
        };
        let observed = match evidence.get(control) {
            Some(observed) => observed,
            None => {
                unverified.get_or_insert_with(|| format!("{control}: not read in this observation"));
                continue;
            }
        };
        if let Some(reason) = observed.unverified() {
            unverified.get_or_insert_with(|| format!("{control}: {reason}"));
            continue;
        }
        if observed.value() != Some(planned.native) {
            disagreement.get_or_insert_with(|| {
                format!(
                    "{control}: the display reports {}, the profile asks for {}",
                    observed.value().unwrap_or_default(),
                    planned.native
                )
            });
        }
    }

    // A positive disagreement excludes the profile even if another field was merely unreadable: the
    // monitor demonstrably is not in this profile. An unreadable field alone proves nothing.
    if let Some(reason) = disagreement {
        return Verdict::Excluded(reason);
    }
    if let Some(reason) = unverified {
        return Verdict::Unverified(reason);
    }
    Verdict::Match
}

/// Decide what one monitor is currently in, from reads alone.
///
/// Several complete matches are resolved by the persisted history — only when that profile is among
/// the current matches *and* its content fingerprint still matches — and otherwise by registry order,
/// so the answer is deterministic and never depends on iteration order.
pub fn classify(
    profiles: &[Profile],
    evidence: &Evidence,
    history: Option<&HistoryHint>,
) -> CurrentProfile {
    // Every outcome except `checking` is evidence as of a moment, so every one of them carries the
    // timestamp: a surface may say "custom, as of 09:12" and must not imply continuous monitoring.
    let stamped = |mut current: CurrentProfile| {
        current.observed_at = Some(evidence.observed_at.clone());
        current
    };

    if profiles.is_empty() {
        return stamped(CurrentProfile::unknown(
            "no profiles are loaded, so there is nothing to compare",
        ));
    }
    if evidence.is_empty() {
        return stamped(CurrentProfile::unknown(
            "the monitor was not read in this observation",
        ));
    }

    let mut matches: Vec<&Profile> = Vec::new();
    let mut unverified: Vec<(String, String)> = Vec::new();
    let mut excluded: Vec<(String, String)> = Vec::new();

    for profile in profiles {
        match judge(profile, evidence) {
            Verdict::Match => matches.push(profile),
            Verdict::Excluded(reason) => excluded.push((profile.id.clone(), reason)),
            Verdict::Unverified(reason) => unverified.push((profile.id.clone(), reason)),
        }
    }

    if matches.len() == 1 {
        return CurrentProfile::matched(&matches[0].id, &evidence.observed_at);
    }

    if matches.len() > 1 {
        if let Some(hint) = history {
            if let Some(winner) = matches.iter().find(|profile| {
                profile.id == hint.profile_id && fingerprint(profile) == hint.fingerprint
            }) {
                return CurrentProfile::matched(&winner.id, &evidence.observed_at);
            }
        }
        // Registry order is `order` then `id`, and `profiles` is already sorted that way.
        return CurrentProfile::matched(&matches[0].id, &evidence.observed_at);
    }

    if unverified.is_empty() {
        // Every candidate was read completely and disagreed: this is a positive Custom.
        let detail = excluded
            .first()
            .map(|(id, reason)| format!("{id}: {reason}"))
            .unwrap_or_else(|| String::from("no candidate matched"));
        return stamped(CurrentProfile::custom(format!(
            "no loaded profile matches the display ({detail})"
        )));
    }

    let (id, reason) = &unverified[0];
    stamped(CurrentProfile::unknown(format!("{id}: {reason}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::profile::parse as parse_profile;
    use std::path::Path;

    fn profile(text: &str) -> Profile {
        parse_profile(text, Path::new("test.toml")).expect("profile")
    }

    fn simple(id: &str, order: i64, body: &str) -> Profile {
        profile(&format!(
            "schema_version = 1\nid = \"{id}\"\nname = \"{id}\"\nicon = \"day\"\norder = {order}\n\n[monitor]\n{body}\n"
        ))
    }

    fn evidence(monitor: &str, fields: &[(&str, Observed)]) -> Evidence {
        let mut evidence = Evidence::new(monitor, "2026-10-25T09:00:00");
        for (control, observed) in fields {
            evidence.record(control, observed.clone());
        }
        evidence
    }

    fn value(current: u64, minimum: u64, maximum: u64) -> Observed {
        Observed::Value {
            current,
            minimum: Some(minimum),
            maximum: Some(maximum),
        }
    }

    #[test]
    fn a_percent_profile_matches_the_native_value_the_planner_would_write() {
        let day = simple("day", 10, "brightness = 40");
        // 40 % of a 0..=100 display is 40, and the planner's own mapping is what is compared.
        let reads = evidence("mon-a", &[("brightness", value(40, 0, 100))]);
        let current = classify(&[day], &reads, None);
        assert_eq!(current.state, CurrentState::Matched);
        assert_eq!(current.profile_id(), Some("day"));
        assert_eq!(current.observed_at.as_deref(), Some("2026-10-25T09:00:00"));
    }

    #[test]
    fn a_nonzero_range_is_mapped_with_the_planners_rounding() {
        // 50 % of 20..=80 is 50; 33 % is 20 + round(0.33 * 60) = 40; 1 % is 20 + round(0.6) = 21.
        let mid = simple("mid", 10, "brightness = 50");
        let odd = simple("odd", 10, "brightness = 33");
        let low = simple("low", 10, "brightness = 1");
        let reads = evidence("mon-a", &[("brightness", value(50, 20, 80))]);
        assert_eq!(classify(&[mid], &reads, None).profile_id(), Some("mid"));
        let reads = evidence("mon-a", &[("brightness", value(40, 20, 80))]);
        assert_eq!(classify(&[odd], &reads, None).profile_id(), Some("odd"));
        let reads = evidence("mon-a", &[("brightness", value(21, 20, 80))]);
        assert_eq!(classify(&[low.clone()], &reads, None).profile_id(), Some("low"));
        // One count off is a mismatch, not a tolerance.
        let reads = evidence("mon-a", &[("brightness", value(22, 20, 80))]);
        assert_eq!(classify(&[low], &reads, None).state, CurrentState::Custom);
    }

    #[test]
    fn a_raw_profile_matches_only_the_exact_integer() {
        let fixed = simple("fixed", 10, "brightness = { raw = 37 }");
        let reads = evidence(
            "mon-a",
            &[(
                "brightness",
                Observed::Value {
                    current: 37,
                    minimum: None,
                    maximum: None,
                },
            )],
        );
        assert_eq!(classify(&[fixed.clone()], &reads, None).profile_id(), Some("fixed"));

        let reads = evidence(
            "mon-a",
            &[(
                "brightness",
                Observed::Value {
                    current: 38,
                    minimum: None,
                    maximum: None,
                },
            )],
        );
        assert_eq!(classify(&[fixed], &reads, None).state, CurrentState::Custom);
    }

    #[test]
    fn every_requested_field_must_agree() {
        let both = simple("both", 10, "brightness = 40\ncontrast = 70");
        let reads = evidence(
            "mon-a",
            &[("brightness", value(40, 0, 100)), ("contrast", value(70, 0, 100))],
        );
        assert_eq!(classify(&[both.clone()], &reads, None).profile_id(), Some("both"));

        // Brightness alone agrees; contrast disagrees. The profile is excluded, not matched.
        let reads = evidence(
            "mon-a",
            &[("brightness", value(40, 0, 100)), ("contrast", value(71, 0, 100))],
        );
        let current = classify(&[both], &reads, None);
        assert_eq!(current.state, CurrentState::Custom);
        assert!(current.reason.expect("reason").contains("contrast"));
    }

    #[test]
    fn an_unread_field_makes_a_candidate_unverified_rather_than_custom() {
        let both = simple("both", 10, "brightness = 40\ncontrast = 70");
        // Contrast was not read at all: brightness alone must not certify or exclude the profile.
        let reads = evidence("mon-a", &[("brightness", value(40, 0, 100))]);
        let current = classify(&[both], &reads, None);
        assert_eq!(current.state, CurrentState::Unknown);
        assert!(current.reason.as_deref().expect("reason").contains("contrast"));
        assert_eq!(current.profile_id(), None);
    }

    #[test]
    fn an_unsupported_or_failed_read_is_unknown_not_custom() {
        let both = simple("both", 10, "brightness = 40\ncontrast = 70");
        for observed in [
            Observed::Unsupported(String::from("contrast is not offered by mon-a")),
            Observed::Failed(String::from("the read timed out")),
            Observed::Unavailable(String::from("this build cannot read color_temperature")),
        ] {
            let reads = evidence("mon-a", &[("brightness", value(40, 0, 100)), ("contrast", observed)]);
            let current = classify(&[both.clone()], &reads, None);
            assert_eq!(current.state, CurrentState::Unknown, "{current:?}");
        }
    }

    #[test]
    fn a_temperature_profile_is_never_certified_by_brightness_alone() {
        let warm = profile(
            "schema_version = 1\nid = \"warm\"\nname = \"Warm\"\nicon = \"day\"\norder = 10\n\n[monitor]\nbrightness = 40\ncolor_temperature = \"6500k\"\n",
        );
        let reads = evidence(
            "mon-a",
            &[
                ("brightness", value(40, 0, 100)),
                (
                    "color_temperature",
                    Observed::Unavailable(String::from(
                        "this build has no read path for color_temperature",
                    )),
                ),
            ],
        );
        let current = classify(&[warm], &reads, None);
        assert_eq!(current.state, CurrentState::Unknown);
    }

    #[test]
    fn identical_candidates_are_decided_by_history_then_by_order() {
        let first = simple("alpha", 10, "brightness = 40");
        let second = simple("bravo", 20, "brightness = 40");
        let reads = evidence("mon-a", &[("brightness", value(40, 0, 100))]);

        // No history: registry order decides, deterministically.
        assert_eq!(
            classify(&[first.clone(), second.clone()], &reads, None).profile_id(),
            Some("alpha")
        );
        // History names the second candidate and its fingerprint still matches.
        let hint = HistoryHint {
            profile_id: String::from("bravo"),
            fingerprint: fingerprint(&second),
        };
        assert_eq!(
            classify(&[first.clone(), second.clone()], &reads, Some(&hint)).profile_id(),
            Some("bravo")
        );
        // The stored fingerprint no longer matches the edited profile, so history loses its vote.
        let stale = HistoryHint {
            profile_id: String::from("bravo"),
            fingerprint: String::from("0000000000000000"),
        };
        assert_eq!(
            classify(&[first.clone(), second.clone()], &reads, Some(&stale)).profile_id(),
            Some("alpha")
        );
        // History naming a profile that is not among the current matches is ignored.
        let third = simple("charlie", 30, "brightness = 90");
        let hint = HistoryHint {
            profile_id: String::from("charlie"),
            fingerprint: fingerprint(&third),
        };
        assert_eq!(
            classify(&[first, second, third], &reads, Some(&hint)).profile_id(),
            Some("alpha")
        );
    }

    #[test]
    fn a_fingerprint_changes_when_a_value_changes_but_not_when_a_name_does() {
        let base = simple("day", 10, "brightness = 40");
        let renamed = profile(
            "schema_version = 1\nid = \"day\"\nname = \"Sunlight\"\nicon = \"night\"\norder = 10\n\n[monitor]\nbrightness = 40\n",
        );
        let edited = simple("day", 10, "brightness = 41");
        assert_eq!(fingerprint(&base), fingerprint(&renamed));
        assert_ne!(fingerprint(&base), fingerprint(&edited));
    }

    #[test]
    fn no_candidate_at_all_is_custom_only_with_complete_reads() {
        let day = simple("day", 10, "brightness = 40");
        let complete = evidence("mon-a", &[("brightness", value(90, 0, 100))]);
        let current = classify(&[day.clone()], &complete, None);
        assert_eq!(current.state, CurrentState::Custom);
        assert_eq!(current.profile_id(), None);

        // The same disagreement, but the read could not be trusted: Unknown, never Custom.
        let incomplete = evidence(
            "mon-a",
            &[("brightness", Observed::Failed(String::from("no answer")))],
        );
        assert_eq!(classify(&[day], &incomplete, None).state, CurrentState::Unknown);
    }

    #[test]
    fn nothing_is_decided_before_the_first_observation() {
        assert_eq!(CurrentProfile::checking().state, CurrentState::Checking);
        assert_eq!(CurrentProfile::checking().profile_id(), None);
        // An observation that read nothing is not evidence of anything.
        let day = simple("day", 10, "brightness = 40");
        let empty = Evidence::new("mon-a", "2026-10-25T09:00:00");
        assert_eq!(classify(&[day], &empty, None).state, CurrentState::Unknown);
        assert_eq!(classify(&[], &empty, None).state, CurrentState::Unknown);
    }

    #[test]
    fn a_profile_without_hardware_fields_is_not_a_vacuous_match() {
        // The schema forbids this, but a value built in code must not certify itself.
        let mut hollow = simple("hollow", 10, "brightness = 40");
        hollow.monitor.brightness = None;
        let reads = evidence("mon-a", &[("brightness", value(40, 0, 100))]);
        assert_eq!(classify(&[hollow], &reads, None).state, CurrentState::Unknown);
    }

    #[test]
    fn state_names_are_the_wire_names() {
        assert_eq!(CurrentState::Checking.as_str(), "checking");
        assert_eq!(CurrentState::Matched.as_str(), "matched");
        assert_eq!(CurrentState::Applied.as_str(), "applied_verified");
        assert_eq!(CurrentState::Custom.as_str(), "custom");
        assert_eq!(CurrentState::Unknown.as_str(), "unknown");
    }
}
