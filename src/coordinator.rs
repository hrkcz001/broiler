//! The command coordinator.
//!
//! One coordinator per user session owns every hardware operation. Both the CLI and, later, the
//! tray issue the same typed commands here; the coordinator is the only place that decides what a
//! command means. It holds the settings, the profile registry and the selected monitors, and it
//! serialises access to the transport.
//!
//! The coordinator does not render anything and does not block on hardware outside the transport's
//! own bounded calls.

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::config::registry::ProfileRegistry;
use crate::config::settings::{self, Settings, UnsupportedPolicy};
use crate::config::value::{percent_to_native, ControlValue, DeviceValueSupport};
use crate::config::Profile;
use crate::ipc::{Failure, PROTOCOL_VERSION};
use crate::plan::{self, PlanError};
use crate::reconcile::{self, CurrentProfile, Evidence, HistoryHint, Observed};
use crate::transport::{ControlReading, MonitorSummary, Transport, TransportError};

/// Controls with an executable write path in this build, in the transport's own spelling.
///
/// The planner knows every control in the registry; this list is the narrower set the current
/// backend can actually send. A requested control outside it is reported as `not_implemented`
/// rather than silently dropped or misrepresented as a device limitation.
const WRITE_PATH_CONTROLS: &[&str] = &["brightness", "contrast", "color_temperature"];

fn build_writes(control: &str) -> bool {
    WRITE_PATH_CONTROLS.contains(&control)
}

/// Controls this build can read. Everything else a profile requests is recorded as unavailable for
/// observation, which makes a candidate containing it unverified instead of silently ignored.
const READABLE_CONTROLS: &[&str] = &["brightness", "contrast", "color_temperature"];

/// Controls a profile requests that this build cannot write at all.
fn unimplemented_controls(profile: &Profile) -> Vec<&'static str> {
    profile
        .requested_control_names()
        .into_iter()
        .filter(|name| !build_writes(name))
        .collect()
}

/// The part of a profile this build can execute.
///
/// Planning only sees controls that have a write path, so missing backend support can never be
/// confused with missing device support: the former is reported as `not_implemented`, the latter
/// comes back from the plan as `unsupported`.
fn writable_part(profile: &Profile) -> Profile {
    let mut restricted = profile.clone();
    let monitor = &mut restricted.monitor;
    if !build_writes("color_temperature") {
        monitor.color_temperature = None;
    }
    if !build_writes("color_preset") {
        monitor.color_preset = None;
    }
    if !build_writes("rgb_gain.red") {
        monitor.rgb_gain.red = None;
    }
    if !build_writes("rgb_gain.green") {
        monitor.rgb_gain.green = None;
    }
    if !build_writes("rgb_gain.blue") {
        monitor.rgb_gain.blue = None;
    }
    if !build_writes("contrast") {
        monitor.contrast = None;
    }
    if !build_writes("brightness") {
        monitor.brightness = None;
    }
    if !build_writes("volume") {
        monitor.volume = None;
    }
    if !build_writes("mute") {
        monitor.mute = None;
    }
    restricted
}

/// The single cause to report for a monitor, or for a whole apply, that did not fully succeed.
///
/// Ordered by how much the result can be trusted: an outcome-unknown write outranks a positive
/// `unsupported` reply, which outranks a build limitation.
const CAUSE_PRIORITY: &[&str] = &[
    "timeout",
    "transport_error",
    "protocol_error",
    "verification_failed",
    "unsupported",
    "blocked_by_mode",
    "not_implemented",
    "invalid_value",
];

fn dominant_cause(fields: &Value) -> &'static str {
    let Some(fields) = fields.as_array() else {
        return "not_applied";
    };
    for cause in CAUSE_PRIORITY {
        if fields.iter().any(|field| field["state"] == *cause) {
            return cause;
        }
    }
    "not_applied"
}

fn row_causes(rows: &Value) -> &'static str {
    let Some(rows) = rows.as_array() else {
        return "not_applied";
    };
    for cause in CAUSE_PRIORITY {
        if rows.iter().any(|row| row["cause"] == *cause) {
            return cause;
        }
    }
    "not_applied"
}

/// Map a planning failure onto the coordinator's typed failure.
fn plan_failure(monitor: Option<&str>, error: &PlanError) -> Failure {
    let kind = match error {
        PlanError::Unsupported { .. } => "unsupported",
        PlanError::Invalid { .. } => "invalid",
        PlanError::Conflict(_) => "invalid_config",
    };
    let message = match monitor {
        Some(monitor) => format!("{monitor}: {error}"),
        None => error.to_string(),
    };
    Failure::new(kind, message)
}

/// Why a command was refused.
fn invalid(message: impl Into<String>) -> Failure {
    Failure::new("invalid", message)
}

fn unknown_method(method: &str) -> Failure {
    Failure::new(
        "unknown_method",
        format!("`{method}` is not a coordinator method in this build"),
    )
}

fn transport_failure(error: TransportError) -> Failure {
    Failure::new(error.kind(), error.message().to_string())
}

fn string_param(params: &Value, key: &str) -> Result<String, Failure> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(|value| value.to_string())
        .ok_or_else(|| invalid(format!("`{key}` must be a string")))
}

fn control_param(params: &Value) -> Result<&'static str, Failure> {
    let control = string_param(params, "control")?;
    match control.as_str() {
        "brightness" => Ok("brightness"),
        "contrast" => Ok("contrast"),
        other => Err(Failure::new(
            "unsupported",
            format!("`{other}` is not a coordinator control in this build; brightness and contrast exist"),
        )),
    }
}

fn reading_json(reading: &ControlReading) -> Value {
    json!({
        "value": reading.current,
        "minimum": reading.minimum,
        "maximum": reading.maximum,
        "backend": reading.backend,
    })
}

/// One control value this coordinator has actually seen, with the moment it was seen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachedReading {
    pub current: u64,
    pub minimum: Option<u64>,
    pub maximum: Option<u64>,
    pub at: String,
}

/// Commands over one transport, plus the configuration they act on.
pub struct Coordinator<T: Transport> {
    transport: T,
    settings: Settings,
    registry: ProfileRegistry,
    settings_path: PathBuf,
    profiles_dir: Option<PathBuf>,
    /// Last fully applied profile per monitor. A profile id is not a claim about current hardware
    /// state; it only records what this coordinator last applied and verified.
    last_applied: std::collections::HashMap<String, String>,
    /// The last value this coordinator read or wrote per monitor and control.
    ///
    /// A UI surface reads this instead of the hardware, so opening the flyout, refreshing a tooltip or
    /// answering `status` never waits for the DDC mutex. An entry that is absent is simply unknown and
    /// is not shown, which is why no surface has to invent a value.
    readings: std::collections::HashMap<(String, String), CachedReading>,
    /// Monitors whose user-visible state changed since the last publication.
    dirty: Vec<String>,
    /// What the last command published, so a caller can see that a change was announced rather than
    /// applied silently. Production reads it only for the log; a test asserts on it.
    last_published: Vec<String>,
    /// The single current-state answer per monitor. Icon, tooltip, flyout and `status` all read this,
    /// so they cannot disagree with each other or with the last observation.
    current: std::collections::HashMap<String, CurrentProfile>,
    /// The persisted applied history, used only to break a tie between equally validated profiles.
    state: crate::state::State,
    /// Where the history is stored, when this coordinator owns a data root.
    state_path: Option<PathBuf>,
    /// The data root this coordinator owns. Every filesystem decision goes through it, and a client
    /// that resolved another root is refused rather than answered about data it never read.
    storage: Option<crate::storage::StorageContext>,
    shutdown_requested: bool,
    observed_at: String,
    /// The tray owner window, or 0 when this process has no user interface.
    ui_window: isize,
}

impl<T: Transport> Coordinator<T> {
    /// Record the window the UI thread owns, so a pipe request can ask it for a flyout.
    #[allow(dead_code)]
    pub fn set_ui_window(&mut self, window: isize) {
        self.ui_window = window;
    }

    /// The monitors this instance controls, in the order its tray icons use.
    ///
    /// A remembered selection keeps its order and drops displays that are gone; without a selection
    /// every present monitor is controlled. One implementation, because the pipe answers `ui.flyout`
    /// with slot indices that must match the icons.
    pub fn resident_targets(&mut self) -> Vec<String> {
        let present: Vec<String> = self
            .transport
            .monitors()
            .unwrap_or_default()
            .into_iter()
            .map(|monitor| monitor.id)
            .collect();
        if self.settings.monitor_ids.is_empty() {
            return present;
        }
        self.settings
            .monitor_ids
            .iter()
            .filter(|id| present.iter().any(|seen| seen == *id))
            .cloned()
            .collect()
    }

    pub fn new(transport: T, settings: Settings, registry: ProfileRegistry, settings_path: PathBuf) -> Coordinator<T> {
        Coordinator {
            transport,
            settings,
            registry,
            settings_path,
            profiles_dir: None,
            last_applied: std::collections::HashMap::new(),
            readings: std::collections::HashMap::new(),
            dirty: Vec::new(),
            last_published: Vec::new(),
            current: std::collections::HashMap::new(),
            state: crate::state::State::default(),
            state_path: None,
            storage: None,
            ui_window: 0,
            shutdown_requested: false,
            observed_at: crate::clock::iso_local(),
        }
    }

    /// Where the profile registry came from, so `reload` can re-read it.
    pub fn with_profiles_dir(mut self, directory: PathBuf) -> Coordinator<T> {
        self.profiles_dir = Some(directory);
        self
    }

    /// Attach the resolved data root this coordinator owns, and load the persisted history from it.
    pub fn with_storage(mut self, storage: crate::storage::StorageContext) -> Coordinator<T> {
        let (state, usable) = crate::state::load(&storage.state_file());
        if !usable {
            crate::log_error!(
                "the local state file {} could not be used; the applied history is treated as empty",
                storage.state_file().display()
            );
        }
        self.state = state;
        self.state_path = Some(storage.state_file());
        self.storage = Some(storage);
        self
    }

    /// The data root this coordinator owns, as the string clients are checked against.
    pub fn storage_root(&self) -> Option<String> {
        self.storage
            .as_ref()
            .map(|storage| storage.root().display().to_string())
    }

    pub fn shutdown_requested(&self) -> bool {
        self.shutdown_requested
    }

    /// Peak concurrent transport operations, when the transport reports it.
    pub fn transport_peak_concurrency(&self) -> Option<u32> {
        self.transport.peak_concurrency()
    }

    /// The profile this monitor was last verified against, when the values were actually applied by
    /// this process. `None` means the state is unknown or custom, and no icon may claim otherwise.
    pub fn applied_profile(&self, monitor: &str) -> Option<&str> {
        self.last_applied.get(monitor).map(String::as_str)
    }

    /// The display name of a profile id, as the user wrote it in the profile file.
    ///
    /// Every surface shows this rather than the file id: `night` is the file name, not something the
    /// user typed to see in a tooltip.
    pub fn profile_name(&self, id: &str) -> Option<&str> {
        self.registry
            .find(id)
            .map(|profile| profile.name.as_str())
    }

    /// The icon name of a profile id, for the tray and the flyout rows.
    pub fn profile_icon(&self, id: &str) -> Option<&str> {
        self.registry.find(id).map(|profile| profile.icon.as_str())
    }

    /// The single current-state answer for one monitor.
    ///
    /// Every surface reads this: the tray icon, the tooltip, the flyout marker and `status`. A monitor
    /// that was never observed is `checking`, which is the truth before the first read completes.
    pub fn current(&self, monitor: &str) -> CurrentProfile {
        self.current
            .get(monitor)
            .cloned()
            .unwrap_or_else(CurrentProfile::checking)
    }

    /// Every control any loaded profile explicitly requests, in a stable order.
    ///
    /// This is the whole read set of an observation: nothing else is read, so an observation costs the
    /// union of what the profiles ask for and no speculative probing.
    fn requested_controls(&self) -> Vec<&'static str> {
        let mut controls: Vec<&'static str> = Vec::new();
        for profile in self.registry.profiles() {
            for control in profile.requested_control_names() {
                if !controls.contains(&control) {
                    controls.push(control);
                }
            }
        }
        controls
    }

    /// Read once, compare, and record what each monitor is currently in. Never writes.
    ///
    /// Each control of the union is read exactly once per monitor through the single serialized
    /// transport. The comparison is then done by [`reconcile::classify`], which uses the planner's own
    /// conversion, so the answer is `matched` — never `applied` — when the reads merely agree.
    pub fn reconcile(&mut self, monitors: &[String]) -> Vec<String> {
        // A monitor that is no longer present keeps no claim: an answer about a display someone
        // unplugged is not evidence, and a reconnect must start from a fresh observation rather than
        // from a snapshot of another connection generation.
        if let Ok(present) = self.transport.monitors() {
            let present: Vec<String> = present.into_iter().map(|monitor| monitor.id).collect();
            self.current
                .retain(|monitor, _| present.contains(monitor) || monitors.contains(monitor));
        }
        let controls = self.requested_controls();
        let mut observed: Vec<String> = Vec::new();
        for monitor in monitors {
            let mut evidence = Evidence::new(monitor, &crate::clock::iso_local());
            for control in &controls {
                if !READABLE_CONTROLS.contains(control) {
                    evidence.record(
                        control,
                        Observed::Unavailable(format!(
                            "this build has no read path for {control}"
                        )),
                    );
                    continue;
                }
                match self.transport.read(monitor, control) {
                    Ok(reading) => {
                        self.remember_reading(monitor, control, &reading);
                        evidence.record(
                            control,
                            Observed::Value {
                                current: reading.current,
                                minimum: reading.minimum,
                                maximum: reading.maximum,
                            },
                        );
                    }
                    // A positive refusal is evidence about the display, not about the channel.
                    Err(TransportError::Unsupported(message)) => {
                        evidence.record(control, Observed::Unsupported(message));
                    }
                    Err(error) => {
                        evidence.record(control, Observed::Failed(error.message().to_string()));
                    }
                }
            }

            let hint = self.state.applied.get(monitor).map(|record| HistoryHint {
                profile_id: record.profile_id.clone(),
                fingerprint: record.fingerprint.clone(),
            });
            let current = reconcile::classify(self.registry.profiles(), &evidence, hint.as_ref());
            crate::log_info!(
                "{monitor}: observation {} -> {} ({})",
                evidence.observed_at,
                current.state.as_str(),
                current.profile_id.as_deref().unwrap_or("-")
            );
            if let Some(reason) = &current.reason {
                crate::log_info!("{monitor}: {}", reason);
            }
            self.current.insert(monitor.clone(), current);
            observed.push(monitor.clone());
        }
        self.mark_changed(&observed);
        observed
    }

    /// Record a verified application: the hardware now demonstrably is this profile.
    fn note_applied(&mut self, monitor: &str, profile: &Profile) {
        let at = crate::clock::iso_local();
        self.last_applied
            .insert(monitor.to_string(), profile.id.clone());
        self.current
            .insert(monitor.to_string(), CurrentProfile::applied(&profile.id, &at));
        self.state
            .record_applied(monitor, &profile.id, &reconcile::fingerprint(profile), &at);
        self.persist_state();
    }

    /// Record a profile whose writes were all sent, at least one of them unverifiable.
    ///
    /// The profile is named and it becomes the base the next cycle starts from; the history is not
    /// touched, so a tie-breaker is still only ever decided by verified evidence.
    fn note_sent(&mut self, monitor: &str, profile: &Profile, reason: &str) {
        let at = crate::clock::iso_local();
        self.last_applied
            .insert(monitor.to_string(), profile.id.clone());
        self.current.insert(
            monitor.to_string(),
            CurrentProfile::applied_unverified(&profile.id, reason, &at),
        );
        self.state.forget(monitor);
        self.persist_state();
    }

    /// Drop any claim about a monitor after a write whose effect is not verified.
    ///
    /// A partial or unknown outcome means the hardware is in a state nobody observed. The old answer is
    /// invalidated immediately; the history entry is forgotten too, because it may no longer describe
    /// the display.
    fn note_unverified(&mut self, monitor: &str, reason: &str) {
        self.last_applied.remove(monitor);
        self.current
            .insert(monitor.to_string(), CurrentProfile::unknown(reason));
        self.state.forget(monitor);
        self.persist_state();
    }

    fn persist_state(&mut self) {
        let Some(path) = self.state_path.clone() else {
            return;
        };
        if let Err(error) = crate::state::save(&path, &self.state) {
            // History is a tie-break, so a failed write degrades one refinement and nothing else. It
            // is logged rather than silently ignored.
            crate::log_error!("cannot record the applied history in {}: {error}", path.display());
        }
    }

    /// The last value seen for one control, without touching the hardware.
    pub fn cached_reading(&self, monitor: &str, control: &str) -> Option<&CachedReading> {
        self.readings
            .get(&(monitor.to_string(), control.to_string()))
    }

    fn remember_reading(&mut self, monitor: &str, control: &str, reading: &ControlReading) {
        self.readings.insert(
            (monitor.to_string(), control.to_string()),
            CachedReading {
                current: reading.current,
                minimum: reading.minimum,
                maximum: reading.maximum,
                at: crate::clock::iso_local(),
            },
        );
    }

    /// Note that a monitor's user-visible state changed and will have to be published.
    ///
    /// The set is a queue of monitor ids, not a flag per surface: the tray icons, the tooltips, the
    /// open flyout and `status` all read the same coordinator state, so one notification serves all
    /// of them and a surface cannot be left showing an older answer.
    fn mark_changed(&mut self, monitors: &[String]) {
        for monitor in monitors {
            if !self.dirty.iter().any(|seen| seen == monitor) {
                self.dirty.push(monitor.clone());
            }
        }
    }

    /// Publish pending changes to the user interface and report which monitors changed.
    ///
    /// This is the single notification path. Every mutating command ends here — a CLI `profile apply`,
    /// a GUI click, a cycle, a reload, a settings write, a selection change or a finished
    /// reconciliation — so the tray cannot learn about one of them and miss another. A process with no
    /// user interface still gets the list back, which is how a test observes that a change was
    /// announced rather than applied silently.
    pub fn publish_pending(&mut self) -> Vec<String> {
        let changed = std::mem::take(&mut self.dirty);
        if changed.is_empty() || self.ui_window == 0 {
            return changed;
        }
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(
                self.ui_window as windows_sys::Win32::Foundation::HWND,
                crate::tray::STATE_CHANGED,
                0,
                0,
            );
        }
        changed
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn monitor_summaries(&mut self) -> Result<Vec<MonitorSummary>, TransportError> {
        self.transport.monitors()
    }

    pub fn read(&mut self, monitor: &str, control: &str) -> Result<ControlReading, TransportError> {
        self.transport.read(monitor, control)
    }

    /// Resolve a requested value and write it, verifying the readback.
    ///
    /// A successful direct change makes the monitor `Custom`: an older profile id must not stay
    /// labelled active merely because one of its values still happens to match.
    pub fn set(
        &mut self,
        monitor: &str,
        control: &str,
        value: ControlValue,
    ) -> Result<Value, Failure> {
        let reading = self
            .transport
            .read(monitor, control)
            .map_err(transport_failure)?;
        self.remember_reading(monitor, control, &reading);

        let native = match value {
            ControlValue::Percent(percent) => {
                let (Some(minimum), Some(maximum)) = (reading.minimum, reading.maximum) else {
                    return Err(invalid(
                        "the display reported no range for this control, so a percentage cannot be mapped",
                    ));
                };
                if maximum < minimum {
                    return Err(invalid("the display reported an inverted range for this control"));
                }
                percent_to_native(percent, minimum, maximum)
            }
            ControlValue::Native(native) => {
                if let (Some(minimum), Some(maximum)) = (reading.minimum, reading.maximum) {
                    if native < minimum || native > maximum {
                        return Err(invalid(format!(
                            "native value {native} is outside the reported range {minimum}..={maximum}; nothing was written"
                        )));
                    }
                }
                native
            }
        };

        let outcome = self
            .transport
            .write(monitor, control, native)
            .map_err(transport_failure)?;

        if !outcome.verified() {
            // The monitor is in an unverified state, so no profile is claimed for it either.
            self.last_applied.remove(monitor);
            self.mark_changed(std::slice::from_ref(&monitor.to_string()));
            return Err(Failure::new(
                "verification_failed",
                format!(
                    "{}: wrote {} and the device reports {}",
                    control, outcome.native_target, outcome.observed
                ),
            ));
        }

        self.last_applied.remove(monitor);
        // A direct change is not a profile any more, but only a read may decide what it is: the claim is
        // dropped here and the observation below replaces it with evidence.
        self.current.insert(
            monitor.to_string(),
            CurrentProfile::unknown("the value was changed directly; re-reading the display"),
        );
        self.state.forget(monitor);
        self.persist_state();
        // The readback is a fresh observation of this control, so the cache carries it with the range
        // the earlier read reported rather than losing that range.
        let (minimum, maximum) = self
            .cached_reading(monitor, control)
            .map(|reading| (reading.minimum, reading.maximum))
            .unwrap_or((None, None));
        let observed = CachedReading {
            current: outcome.observed,
            minimum,
            maximum,
            at: crate::clock::iso_local(),
        };
        self.readings
            .insert((monitor.to_string(), control.to_string()), observed);
        self.mark_changed(std::slice::from_ref(&monitor.to_string()));
        // Reconcile by reading, which is safe: one write has completed and verified, so no writer is in
        // doubt. This keeps a direct change from leaving a stale profile claim behind.
        self.reconcile(std::slice::from_ref(&monitor.to_string()));
        Ok(json!({
            "monitor": monitor,
            "control": control,
            "native_target": outcome.native_target,
            "observed": outcome.observed,
            "verified": true,
            "backend": outcome.backend,
        }))
    }

    fn persist_settings(&mut self) -> Result<(), Failure> {
        crate::config::settings::save(&self.settings_path, &self.settings)
            .map_err(|error| Failure::new("io_error", error.render()))
    }

    /// The transport back, so a test can inspect what the coordinator did to it.
    #[cfg(test)]
    pub fn into_transport(self) -> T {
        self.transport
    }

    /// The present monitors, for a surface that has to list them.
    pub fn monitors(&mut self) -> Result<Vec<crate::transport::MonitorSummary>, Failure> {
        self.transport.monitors().map_err(transport_failure)
    }

    /// Enumerate the present monitors and apply the documented startup decision.
    ///
    /// The resident loop and the CLI both need this answer before anything is written, so it is one
    /// method over `lifecycle::decide` rather than a reimplementation per entry point. An empty
    /// recorded selection is not a confirmation: only an explicit `monitors select --none` is, and
    /// that path clears `settings.monitor_ids` together with storing the remembered choice, so it is
    /// reported here through the same settings the decision reads.
    pub fn startup_outcome(&mut self) -> Result<crate::lifecycle::StartupOutcome, Failure> {
        let monitors: Vec<String> = self
            .transport
            .monitors()
            .map_err(transport_failure)?
            .into_iter()
            .map(|monitor| monitor.id)
            .collect();
        let state = crate::lifecycle::StartupState {
            monitors: &monitors,
            registry: Ok(&self.registry),
            settings: &self.settings,
            selection_confirmed_empty: false,
        };
        Ok(crate::lifecycle::decide(&state))
    }

    /// Which monitors a profile command targets. Broadcasts are never implicit.
    fn resolve_targets(&mut self, params: &Value) -> Result<Vec<String>, Failure> {
        if params
            .get("all_selected")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            if self.settings.monitor_ids.is_empty() {
                return Err(invalid(
                    "all_selected was requested but the controlled set is empty",
                ));
            }
            return Ok(self.settings.monitor_ids.clone());
        }
        if let Some(monitor) = params.get("monitor").and_then(Value::as_str) {
            return Ok(vec![monitor.to_string()]);
        }
        if let Some(list) = params.get("monitor_ids").and_then(Value::as_array) {
            if list.is_empty() {
                return Err(invalid("monitor_ids is empty; a target is required"));
            }
            let mut ids: Vec<String> = Vec::new();
            for entry in list {
                let id = entry
                    .as_str()
                    .ok_or_else(|| invalid("monitor_ids entries must be strings"))?;
                // A repeated target would write the same monitor twice in one apply.
                if !ids.iter().any(|seen| seen == id) {
                    ids.push(id.to_string());
                }
            }
            return Ok(ids);
        }
        Err(invalid(
            "a target is required: monitor <id>, monitor_ids or all_selected",
        ))
    }

    /// Read the device metadata one write path needs.
    ///
    /// Only the controls the profile actually requests are read: discovery stays bounded to the
    /// codes 0x10 and 0x12 that this build can write. An `unsupported` reply is positive evidence
    /// about the display; any other transport error aborts the preflight.
    fn read_metadata(
        &mut self,
        monitor: &str,
        controls: &[&str],
    ) -> Result<crate::plan::DeviceMetadata, TransportError> {
        let mut metadata = crate::plan::DeviceMetadata::default();
        for control in controls {
            let support = match self.transport.read(monitor, control) {
                Ok(reading) => {
                    self.remember_reading(monitor, control, &reading);
                    match (reading.minimum, reading.maximum) {
                        (Some(minimum), Some(maximum)) if maximum >= minimum => {
                            DeviceValueSupport::Range { minimum, maximum }
                        }
                        // A getter that reports no usable range cannot validate a percentage.
                        _ => DeviceValueSupport::Unknown,
                    }
                }
                Err(TransportError::Unsupported(_)) => DeviceValueSupport::Unsupported,
                Err(error) => return Err(error),
            };
            match *control {
                "brightness" => metadata.brightness = support,
                "contrast" => metadata.contrast = support,
                _ => {}
            }
        }
        if controls.iter().any(|control| *control == "color_temperature") {
            // The preset table comes from the display itself. Without it a native 0x14 value cannot be
            // validated, and writing one would be a guess about what the number means on this panel.
            if let Ok(Some(raw)) = self.transport.capabilities(monitor) {
                let parsed = crate::caps::parse(&crate::win::ddc::CapabilitiesRaw {
                    declared_length: raw.chars().count() as u32,
                    text: raw,
                });
                if let Some(values) = parsed.values_for(crate::win::ddc::VCP_COLOR_TEMPERATURE) {
                    let values: Vec<u64> = values.iter().map(|value| *value as u64).collect();
                    metadata.color_preset = DeviceValueSupport::Enumerated {
                        values: values.clone(),
                    };
                    metadata.color_temperature.presets = values;
                }
                if let Some(values) = parsed.values_for(crate::config::profile::VCP_COLOR_REQUEST as u8) {
                    // 0x0C is a plain integer request code, so only its advertised span is useful.
                    if let (Some(first), Some(last)) = (values.first(), values.last()) {
                        metadata.color_temperature.request_range =
                            Some((*first as u64, *last as u64));
                    }
                }
            }
        }
        Ok(metadata)
    }

    /// Apply a whole profile to every target, after planning all of them.
    ///
    /// Nothing is written until every target has been planned, so the strict policy cannot modify
    /// one monitor and then discover that another cannot be served. Execution then stops on a
    /// monitor as soon as a write leaves the hardware outcome uncertain; a timed-out write is
    /// reported, never replayed.
    fn apply_everywhere(&mut self, id: &str, targets: &[String]) -> Result<Value, Failure> {
        let profile = self
            .registry
            .find(id)
            .cloned()
            .ok_or_else(|| Failure::new("invalid", format!("unknown profile `{id}`")))?;

        let policy = self.settings.unsupported;

        // White-point conflicts are a property of the profile, so they are refused before any
        // monitor is even read.
        plan::check_conflicts(&profile).map_err(|error| plan_failure(None, &error))?;

        // A build limitation is not a device property: it is reported as such, and it is part of the
        // preflight under the strict policy.
        let unimplemented = unimplemented_controls(&profile);

        // Every target must exist before the first write; otherwise a typo would write half a set.
        let present: Vec<String> = self
            .transport
            .monitors()
            .map_err(transport_failure)?
            .into_iter()
            .map(|monitor| monitor.id)
            .collect();
        for monitor in targets {
            if !present.iter().any(|id| id == monitor) {
                return Err(Failure::new(
                    "unknown_monitor",
                    format!("monitor {monitor} is not present in this session"),
                ));
            }
        }

        if policy == UnsupportedPolicy::Fail && !unimplemented.is_empty() {
            return Err(Failure::new(
                "unsupported",
                format!(
                    "profiles.unsupported = fail: {} {} not written by this build; nothing was written",
                    unimplemented.join(", "),
                    if unimplemented.len() == 1 { "is" } else { "are" }
                ),
            ));
        }

        // Preflight: plan every target before executing any of them.
        let writable = writable_part(&profile);
        let requested = writable.requested_control_names();
        let mut prepared: Vec<(String, plan::ApplyPlan)> = Vec::new();
        for monitor in targets {
            let metadata = self
                .read_metadata(monitor, &requested)
                .map_err(transport_failure)?;
            let target_plan = plan::plan(&writable, &metadata, policy)
                .map_err(|error| plan_failure(Some(monitor), &error))?;
            prepared.push((monitor.clone(), target_plan));
        }

        // Execute. Each monitor is a transaction: once a write is outcome-unknown, the remaining
        // planned writes are not attempted.
        let mut results: Vec<Value> = Vec::new();
        for (monitor, target_plan) in prepared {
            let mut fields: Vec<Value> = Vec::new();
            let mut applied = 0usize;
            let mut stopped: Option<Failure> = None;

            for write in &target_plan.writes {
                let control = write.control.name();
                if let Some(failure) = &stopped {
                    fields.push(json!({
                        "control": control,
                        "status": "not_attempted",
                        "requested": write.requested.to_string(),
                        "native": write.native,
                        "state": "transaction_stopped",
                        "reason": format!(
                            "not attempted: the previous write on this monitor failed with {}",
                            failure.kind
                        ),
                    }));
                    continue;
                }

                match self.transport.write(&monitor, control, write.native) {
                    Ok(outcome) if outcome.verified() => {
                        applied += 1;
                        fields.push(json!({
                            "control": control,
                            "status": "applied",
                            "requested": write.requested.to_string(),
                            "native": outcome.native_target,
                            "observed": outcome.observed,
                            "backend": outcome.backend,
                        }));
                    }
                    Ok(outcome) if control == "color_temperature" && outcome.native_target != outcome.observed => {
                        // VCP 0x14 on this panel accepts a preset without reporting which one it is in:
                        // the write is acknowledged, the readback keeps its old value, and the colour
                        // visibly changes. Claiming a failed write would be wrong, and claiming a
                        // verified one would be a lie, so the field is recorded as sent but unverified
                        // and the rest of the profile still runs.
                        fields.push(json!({
                            "control": control,
                            "status": "applied_unverified",
                            "requested": write.requested.to_string(),
                            "native": outcome.native_target,
                            "observed": outcome.observed,
                            "backend": outcome.backend,
                            "reason": "the display does not report which preset it is in; the write was sent",
                        }));
                    }
                    Ok(outcome) => {
                        // A readback mismatch is a verification failure, not success.
                        let failure = Failure::new(
                            "verification_failed",
                            format!(
                                "{}: wrote {} and the device reports {}",
                                control, outcome.native_target, outcome.observed
                            ),
                        );
                        fields.push(json!({
                            "control": control,
                            "status": "failed",
                            "requested": write.requested.to_string(),
                            "native": outcome.native_target,
                            "observed": outcome.observed,
                            "state": "verification_failed",
                            "error": failure,
                        }));
                        stopped = Some(failure);
                    }
                    Err(TransportError::Unsupported(message)) => {
                        // Positive evidence from the device; the path is still healthy.
                        fields.push(json!({
                            "control": control,
                            "status": "skipped",
                            "requested": write.requested.to_string(),
                            "native": write.native,
                            "state": "unsupported",
                            "reason": message,
                        }));
                    }
                    Err(error) => {
                        let failure = transport_failure(error);
                        fields.push(json!({
                            "control": control,
                            "status": "failed",
                            "requested": write.requested.to_string(),
                            "native": write.native,
                            "state": failure.kind,
                            "error": failure,
                        }));
                        stopped = Some(failure);
                    }
                }
            }

            for skip in &target_plan.skipped {
                fields.push(json!({
                    "control": skip.control.name(),
                    "status": "skipped",
                    "state": skip.state,
                    "reason": skip.reason,
                }));
            }
            for control in &unimplemented {
                fields.push(json!({
                    "control": control,
                    "status": "skipped",
                    "state": "not_implemented",
                    "reason": format!("this build has no write path for {control}"),
                }));
            }

            let not_applied_fields = fields
                .iter()
                .filter(|field| field["status"] != "applied")
                .count();
            let unverified = fields
                .iter()
                .filter(|field| field["status"] == "applied_unverified")
                .count();
            let status = if applied > 0 && not_applied_fields == 0 {
                "applied"
            } else if applied > 0 && unverified > 0 && unverified + applied == fields.len() {
                // Every requested write was sent; at least one simply cannot be read back.
                "applied_unverified"
            } else if applied > 0 {
                "partial"
            } else {
                "not_applied"
            };

            if status == "applied" {
                // Every requested field was written and the readback matched, which is the one case
                // where the coordinator may claim the hardware is in this profile.
                self.note_applied(&monitor, &profile);
            } else if status == "applied_unverified" {
                self.note_sent(
                    &monitor,
                    &profile,
                    "every write was sent; one field cannot be read back on this display",
                );
            } else {
                // A partial or failed apply leaves the monitor in an unverified state; an older
                // profile id must not stay labelled active.
                self.note_unverified(
                    &monitor,
                    "the last apply did not complete, so the display state is unknown",
                );
            }
            // Whatever the outcome, this monitor's icon, tooltip and flyout row changed.
            self.mark_changed(std::slice::from_ref(&monitor));

            let mut row = json!({
                "monitor": monitor,
                "status": status,
                "fields": fields,
            });
            if status != "applied" {
                row["cause"] = json!(dominant_cause(&row["fields"]));
            }
            results.push(row);
        }

        let applied_targets = results
            .iter()
            .filter(|row| row["status"] == "applied")
            .count();
        let partial_targets = results
            .iter()
            .filter(|row| row["status"] == "partial")
            .count();
        let unverified_targets = results
            .iter()
            .filter(|row| row["status"] == "applied_unverified")
            .count();
        let status = if applied_targets == results.len() {
            "applied"
        } else if unverified_targets > 0 && unverified_targets + applied_targets == results.len() {
            "applied_unverified"
        } else if applied_targets > 0 || partial_targets > 0 {
            "partial"
        } else {
            "not_applied"
        };

        let mut result = json!({
            "profile": profile.id,
            "status": status,
            "monitors": results,
        });
        if status != "applied" {
            result["cause"] = json!(row_causes(&result["monitors"]));
        }
        Ok(result)
    }

    /// Dispatch one request. The returned value is the `result` field of the response.
    ///
    /// Every entry point goes through here — the pipe, the tray callbacks and the startup path — so
    /// the publication step below cannot be forgotten by one of them.
    pub fn handle(&mut self, method: &str, params: &Value) -> Result<Value, Failure> {
        let result = self.dispatch_command(method, params);
        self.last_published = self.publish_pending();
        result
    }

    /// The monitors the last command announced to the user interface.
    pub fn last_published(&self) -> &[String] {
        &self.last_published
    }

    fn dispatch_command(&mut self, method: &str, params: &Value) -> Result<Value, Failure> {
        match method {
            "ping" => Ok(json!({"protocol": PROTOCOL_VERSION})),
            "status" => {
                // `--refresh` asks for a fresh observation; an ordinary status answers from the cached
                // one, because a status command must never turn into a DDC read on a persistent loop.
                if params
                    .get("refresh")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    let targets = self.resident_targets();
                    self.reconcile(&targets);
                }
                let present = self.transport.monitors().map_err(transport_failure)?;
                let monitors = present
                    .iter()
                    .map(|monitor| {
                        let current = self.current(&monitor.id);
                        json!({
                            "id": monitor.id,
                            "friendly_name": monitor.friendly_name,
                            "connector": monitor.connector,
                            // One snapshot per monitor, the same one the icon, the tooltip and the
                            // flyout use.
                            "current": {
                                "state": current.state.as_str(),
                                "profile": current.profile_id(),
                                "profile_name": current
                                    .profile_id()
                                    .and_then(|id| self.profile_name(id)),
                                "reason": current.reason,
                                "observed_at": current.observed_at,
                            },
                        })
                    })
                    .collect::<Vec<Value>>();
                Ok(json!({
                    "running": true,
                    "protocol": PROTOCOL_VERSION,
                    "observed_at": self.observed_at,
                    "monitors": monitors,
                    "profiles": self.registry.len(),
                    "profile_ids": self.registry.profiles().iter().map(|profile| profile.id.clone()).collect::<Vec<String>>(),
                    "selection": self.settings.monitor_ids,
                    "settings_path": self.settings_path.display().to_string(),
                    "storage": self.storage.as_ref().map(|storage| storage.describe()),
                }))
            }
            "storage.info" => Ok(json!({
                "storage": self.storage.as_ref().map(|storage| storage.describe()),
                "settings_path": self.settings_path.display().to_string(),
                "profiles_dir": self.profiles_dir.as_ref().map(|path| path.display().to_string()),
            })),
            "monitors.list" => {
                let monitors = self.transport.monitors().map_err(transport_failure)?;
                Ok(json!({
                    "monitors": monitors.iter().map(|monitor| json!({
                        "id": monitor.id,
                        "friendly_name": monitor.friendly_name,
                        "connector": monitor.connector,
                        "primary": monitor.primary,
                    })).collect::<Vec<Value>>()
                }))
            }
            "parameters.get" => {
                let monitor = string_param(params, "monitor")?;
                let control = control_param(params)?;
                let reading = self
                    .transport
                    .read(&monitor, control)
                    .map_err(transport_failure)?;
                self.remember_reading(&monitor, control, &reading);
                Ok(json!({"control": control, "reading": reading_json(&reading)}))
            }
            "parameters.set" => {
                let monitor = string_param(params, "monitor")?;
                let control = control_param(params)?;
                let value = params
                    .get("value")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| invalid("`value` must be a non-negative integer"))?;
                let unit = params
                    .get("unit")
                    .and_then(Value::as_str)
                    .unwrap_or("native");
                let typed = match unit {
                    "native" => ControlValue::Native(value),
                    "percent" if value <= 100 => ControlValue::Percent(value as u32),
                    "percent" => {
                        return Err(invalid(format!(
                            "{value} is not a percentage; percentages range 0..=100"
                        )))
                    }
                    other => return Err(invalid(format!("unknown unit `{other}`"))),
                };
                self.set(&monitor, control, typed)
            }
            "ui.settings" => {
                if self.ui_window == 0 {
                    return Err(Failure::new(
                        "ui_unavailable",
                        "this process has no user interface to show its settings",
                    ));
                }
                unsafe {
                    windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(
                        self.ui_window as windows_sys::Win32::Foundation::HWND,
                        crate::tray::SETTINGS_OPEN,
                        0,
                        0,
                    );
                }
                Ok(json!({"opened": true}))
            }
            "ui.flyout" => {
                // The window belongs to the UI thread; a request from the pipe can only ask for it.
                // The slot index is the position in the resident target list, which is the same list
                // the tray icons were built from.
                if self.ui_window == 0 {
                    return Err(Failure::new(
                        "ui_unavailable",
                        "this process has no user interface to show a flyout",
                    ));
                }
                let targets = self.resident_targets();
                let requested = params.get("monitor").and_then(Value::as_str);
                let slot = match requested {
                    Some(id) => targets.iter().position(|target| target == id).ok_or_else(|| {
                        Failure::new(
                            "unknown_monitor",
                            format!("monitor {id} is not controlled by this instance"),
                        )
                    })?,
                    None => 0,
                };
                unsafe {
                    windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(
                        self.ui_window as windows_sys::Win32::Foundation::HWND,
                        crate::win::flyout::OPEN,
                        slot,
                        0,
                    );
                }
                Ok(json!({"opened": true, "monitor": targets.get(slot)}))
            }
            "profiles.list" => Ok(json!({
                "profiles": self.registry.profiles().iter().map(|profile| json!({
                    "id": profile.id,
                    "name": profile.name,
                    "icon": profile.icon,
                    "order": profile.order,
                    "controls": profile.requested_control_names(),
                })).collect::<Vec<Value>>()
            })),
            "profiles.validate" => Ok(json!({
                "valid": true,
                "count": self.registry.len(),
                "profiles": self.registry.profiles().iter().map(|profile| profile.id.clone()).collect::<Vec<String>>(),
            })),
            "settings.get" => match params.get("key").and_then(Value::as_str) {
                None => Ok(settings_json(&self.settings)),
                Some(key) => settings_value(&self.settings, key)
                    .ok_or_else(|| invalid(format!("unknown settings key `{key}`"))),
            },
            "settings.set" => {
                let key = string_param(params, "key")?;
                let value = string_param(params, "value")?;
                // Read the file that is on disk now, edit it in place to keep the user's comments,
                // validate the result, and replace the file atomically. A file that changed between
                // the read and the write is a stale edit and is refused rather than overwritten.
                let original = std::fs::read_to_string(&self.settings_path).unwrap_or_default();
                let updated = settings::set_key(&self.settings_path, &original, &key, &value)
                    .map_err(|error| Failure::new("invalid_config", error.render()))?;
                let parsed = settings::parse(&updated, &self.settings_path)
                    .map_err(|error| Failure::new("invalid_config", error.render()))?;
                settings::replace_text(&self.settings_path, &original, &updated)
                    .map_err(|error| Failure::new("invalid_config", error.render()))?;
                self.settings = parsed;
                // A setting the tray obeys takes effect now, so the surfaces that show it are told so:
                // a failed save returns above and never reaches this line.
                let targets = self.resident_targets();
                self.mark_changed(&targets);
                Ok(json!({"key": key, "value": settings_value(&self.settings, &key).unwrap_or(Value::Null)}))
            }
            "monitors.select" => {
                let requested = params
                    .get("monitor_ids")
                    .and_then(Value::as_array)
                    .ok_or_else(|| invalid("`monitor_ids` must be an array of identifiers"))?;
                let mut ids: Vec<String> = Vec::new();
                for entry in requested {
                    let id = entry
                        .as_str()
                        .ok_or_else(|| invalid("`monitor_ids` entries must be strings"))?;
                    if !self
                        .transport
                        .monitors()
                        .map_err(transport_failure)?
                        .iter()
                        .any(|monitor| monitor.id == id)
                    {
                        return Err(Failure::new(
                            "unknown_monitor",
                            format!("monitor {id} is not present in this session"),
                        ));
                    }
                    ids.push(id.to_string());
                }
                let remember = params
                    .get("remember")
                    .and_then(Value::as_bool)
                    .unwrap_or(self.settings.remember);
                if !remember && !ids.is_empty() {
                    self.settings.remember = false;
                    self.settings.monitor_ids = Vec::new();
                } else {
                    self.settings.remember = remember;
                    self.settings.monitor_ids = ids;
                }
                self.persist_settings()?;
                let targets = self.resident_targets();
                self.mark_changed(&targets);
                Ok(json!({
                    "remember": self.settings.remember,
                    "monitor_ids": self.settings.monitor_ids,
                }))
            }
            "profiles.reload" => {
                let Some(directory) = self.profiles_dir.clone() else {
                    return Err(invalid("this coordinator has no profiles directory configured"));
                };
                match ProfileRegistry::load_dir(&directory) {
                    Ok(registry) => {
                        self.registry = registry;
                        // Names and icons may have changed under every icon and row, and the set of
                        // candidate profiles changed, so a fresh observation replaces the old answer.
                        let targets = self.resident_targets();
                        self.reconcile(&targets);
                        self.mark_changed(&targets);
                        Ok(json!({
                            "reloaded": true,
                            "count": self.registry.len(),
                            "profiles": self.registry.profiles().iter().map(|profile| profile.id.clone()).collect::<Vec<String>>(),
                        }))
                    }
                    // Keep the last good registry: a reload must not silently reduce the set.
                    Err(error) => Err(Failure::new("invalid_config", error.render())),
                }
            }
            "profile.apply" => {
                let id = string_param(params, "id")?;
                let targets = self.resolve_targets(params)?;
                self.apply_everywhere(&id, &targets)
            }
            "profile.next" => {
                let monitor = string_param(params, "monitor")?;
                // Cycle starts after the profile the coordinator currently reports, whether that came
                // from an observation or from an apply of this process. With nothing known it starts at
                // the first profile in registry order.
                let current = self
                    .current
                    .get(&monitor)
                    .and_then(|current| current.profile_id().map(str::to_string))
                    .or_else(|| self.last_applied.get(&monitor).cloned());
                let next = match current.as_deref() {
                    Some(id) => self
                        .registry
                        .next_after(id)
                        .map(|profile| profile.id.clone()),
                    None => self.registry.profiles().first().map(|profile| profile.id.clone()),
                };
                let Some(next) = next else {
                    return Err(invalid("there is no profile to select"));
                };
                self.apply_everywhere(&next, &[monitor])
            }
            "startup.status" => {
                let outcome = self.startup_outcome()?;
                let monitors: Vec<String> = self
                    .transport
                    .monitors()
                    .map_err(transport_failure)?
                    .into_iter()
                    .map(|monitor| monitor.id)
                    .collect();
                Ok(json!({
                    "outcome": outcome.as_str(),
                    "targets": outcome.targets(),
                    "monitors": monitors,
                    "profiles": self.registry.profiles().iter().map(|profile| profile.id.clone()).collect::<Vec<String>>(),
                    "selection": self.settings.monitor_ids,
                }))
            }
            "icons.list" => Ok(json!({"icons": crate::config::profile::ICON_NAMES})),
            "monitors.validate_value" | "monitors.validate-value" => {
                let monitor = string_param(params, "monitor")?;
                let control = control_param(params)?;
                let value = params
                    .get("value")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| invalid("`value` must be a non-negative integer"))?;
                let confirmed = params
                    .get("confirm_write")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if !confirmed {
                    return Err(invalid(
                        "validate-value performs one supervised write and requires confirm_write",
                    ));
                }
                let outcome = self
                    .transport
                    .write(&monitor, control, value)
                    .map_err(transport_failure)?;
                Ok(json!({
                    "monitor": monitor,
                    "control": control,
                    "native_target": outcome.native_target,
                    "observed": outcome.observed,
                    "verified": outcome.verified(),
                    "backend": outcome.backend,
                    "evidence": "write_observed",
                }))
            }
            "paths.profiles" => Ok(json!({
                "profiles": self.profiles_dir.as_ref().map(|path| path.display().to_string()),
                "settings": self.settings_path.display().to_string(),
            })),
            "shutdown" => {
                self.shutdown_requested = true;
                Ok(json!({"stopping": true}))
            }
            other => Err(unknown_method(other)),
        }
    }
}

fn settings_json(settings: &Settings) -> Value {
    json!({
        "schema_version": settings.schema_version,
        "startup": {"launch_at_login": settings.launch_at_login},
        "tray": {
            "left_click": settings.left_click.as_str(),
            "badges": "when_multiple",
            "always_show_monitor_name": settings.always_show_monitor_name,
        },
        "selection": {"remember": settings.remember, "monitor_ids": settings.monitor_ids},
        "profiles": {"startup": "preserve", "unsupported": settings.unsupported.as_str()},
        "logging": {"level": settings.log_level.as_str()},
    })
}

fn settings_value(settings: &Settings, key: &str) -> Option<Value> {
    match key {
        "startup.launch_at_login" => Some(json!(settings.launch_at_login)),
        "tray.left_click" => Some(json!(settings.left_click.as_str())),
        "tray.badges" => Some(json!("when_multiple")),
        "tray.always_show_monitor_name" => Some(json!(settings.always_show_monitor_name)),
        "selection.remember" => Some(json!(settings.remember)),
        "selection.monitor_ids" => Some(json!(settings.monitor_ids)),
        "profiles.startup" => Some(json!("preserve")),
        "profiles.unsupported" => Some(json!(settings.unsupported.as_str())),
        "logging.level" => Some(json!(settings.log_level.as_str())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::FakeTransport;

    fn coordinator() -> Coordinator<FakeTransport> {
        let transport = FakeTransport::with_monitor("mon-a", "Panel").with_value("mon-a", "brightness", 40);
        Coordinator::new(
            transport,
            Settings::default(),
            ProfileRegistry::default(),
            PathBuf::from("C:\\test\\settings.toml"),
        )
    }

    #[test]
    fn ping_reports_the_protocol_version() {
        let mut coordinator = coordinator();
        let result = coordinator.handle("ping", &Value::Null).expect("ping");
        assert_eq!(result["protocol"], PROTOCOL_VERSION);
    }

    #[test]
    fn status_reports_runtime_state() {
        let mut coordinator = coordinator();
        let result = coordinator.handle("status", &Value::Null).expect("status");
        assert_eq!(result["running"], true);
        assert_eq!(result["profiles"], 0);
        assert!(result["monitors"].as_array().is_some());
    }

    #[test]
    fn percent_set_uses_the_reported_range() {
        let mut coordinator = coordinator();
        let result = coordinator
            .handle("parameters.set", &json!({"monitor": "mon-a", "control": "brightness", "value": 50, "unit": "percent"}))
            .expect("set");
        assert_eq!(result["native_target"], 50);
        assert_eq!(result["verified"], true);
    }

    #[test]
    fn native_set_is_bounds_checked_without_writing() {
        let mut coordinator = coordinator();
        let error = coordinator
            .handle("parameters.set", &json!({"monitor": "mon-a", "control": "brightness", "value": 200, "unit": "native"}))
            .expect_err("out of range");
        assert_eq!(error.kind, "invalid");
        assert!(coordinator.transport.writes.is_empty());
    }

    #[test]
    fn a_write_that_does_not_verify_is_a_failure_not_success() {
        let mut coordinator = coordinator();
        coordinator.transport.observe_offset = 2;
        let error = coordinator
            .handle("parameters.set", &json!({"monitor": "mon-a", "control": "brightness", "value": 40, "unit": "native"}))
            .expect_err("mismatch");
        assert_eq!(error.kind, "verification_failed");
    }

    #[test]
    fn unsupported_controls_and_unknown_methods_are_typed_errors() {
        let mut coordinator = coordinator();
        let control = coordinator
            .handle("parameters.get", &json!({"monitor": "mon-a", "control": "sharpness"}))
            .expect_err("unsupported");
        assert_eq!(control.kind, "unsupported");

        let method = coordinator
            .handle("set-vcp", &json!({"code": 0xD6, "value": 5}))
            .expect_err("unknown method");
        assert_eq!(method.kind, "unknown_method");
    }

    fn sample_profile(id: &str, order: i64, body: &str) -> crate::config::Profile {
        let text = format!(
            "schema_version = 1\nid = \"{id}\"\nname = \"{id}\"\nicon = \"day\"\norder = {order}\n\n[monitor]\n{body}\n"
        );
        crate::config::profile::parse(&text, std::path::Path::new("test.toml")).expect("profile")
    }

    fn scratch(name: &str) -> PathBuf {
        let mut directory = std::env::temp_dir();
        directory.push(format!("broiler-coordinator-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("scratch directory");
        directory
    }

    fn coordinator_with(
        transport: FakeTransport,
        profiles: Vec<crate::config::Profile>,
        directory: &std::path::Path,
    ) -> Coordinator<FakeTransport> {
        Coordinator::new(
            transport,
            Settings::default(),
            ProfileRegistry::new(profiles).expect("registry"),
            directory.join("settings.toml"),
        )
        .with_profiles_dir(directory.to_path_buf())
        // The history lives in the same disposable root, so a test never touches the user's state.
        .with_storage(crate::storage::StorageContext::for_test(directory))
    }

    #[test]
    fn selecting_a_monitor_persists_only_present_identifiers() {
        let directory = scratch("select");
        let transport = FakeTransport::with_monitor("mon-a", "Panel");
        let mut coordinator = coordinator_with(transport, Vec::new(), &directory);

        let result = coordinator
            .handle(
                "monitors.select",
                &json!({"monitor_ids": ["mon-a"], "remember": true}),
            )
            .expect("select");
        assert_eq!(result["remember"], true);
        assert_eq!(result["monitor_ids"][0], "mon-a");
        assert!(directory.join("settings.toml").exists());

        let error = coordinator
            .handle(
                "monitors.select",
                &json!({"monitor_ids": ["mon-ghost"], "remember": true}),
            )
            .expect_err("unknown monitor");
        assert_eq!(error.kind, "unknown_monitor");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn applying_a_profile_writes_supported_controls_and_marks_the_rest_partial() {
        let directory = scratch("apply");
        let transport = FakeTransport::with_monitor("mon-a", "Panel");
        let profile = sample_profile("day", 10, "brightness = 100\ncontrast = 80\nvolume = 20");
        let mut coordinator = coordinator_with(transport, vec![profile], &directory);

        let result = coordinator
            .handle("profile.apply", &json!({"id": "day", "monitor": "mon-a"}))
            .expect("apply");

        assert_eq!(result["status"], "partial");
        // Volume is not a device limitation here: this build has no write path for it.
        let volume = field_row(&result, "mon-a", "volume");
        assert_eq!(volume["status"], "skipped");
        assert_eq!(volume["state"], "not_implemented");
        let brightness = field_row(&result, "mon-a", "brightness");
        assert_eq!(brightness["status"], "applied");
        assert_eq!(brightness["observed"], 100);
        assert_eq!(brightness["native"], 100);
        assert_eq!(field_row(&result, "mon-a", "contrast")["status"], "applied");
        // Plan order puts contrast before brightness, and both were sent once.
        assert_eq!(
            coordinator.transport.writes,
            vec![
                (String::from("mon-a"), String::from("contrast"), 80),
                (String::from("mon-a"), String::from("brightness"), 100),
            ]
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn applying_to_all_selected_requires_a_selection() {
        let directory = scratch("all-selected");
        let transport = FakeTransport::with_monitor("mon-a", "Panel");
        let profile = sample_profile("day", 10, "brightness = 50");
        let mut coordinator = coordinator_with(transport, vec![profile], &directory);

        let error = coordinator
            .handle("profile.apply", &json!({"id": "day", "all_selected": true}))
            .expect_err("empty selection");
        assert_eq!(error.kind, "invalid");
        assert!(coordinator.transport.writes.is_empty());
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_native_temperature_value_is_validated_against_the_panels_own_list() {
        // The regression this guards: writing 8 to VCP 0x14 because a standard table says 8 means
        // something, on a panel whose own list may not even contain 8.
        let directory = scratch("temperature");
        let mut transport = FakeTransport::with_monitor("mon-a", "Panel")
            .with_value("mon-a", "brightness", 40)
            .with_value("mon-a", "contrast", 50);
        transport.capabilities = Some(String::from(
            "prot(monitor)type(lcd)vcp(020405080B0C101214(0506080B)D6(0104))mccs_ver(2.1)",
        ));
        let profiles = vec![sample_profile(
            "day",
            10,
            "brightness = 100
contrast = 50
color_temperature = { vcp = 0x14, raw = 8 }",
        )];
        let mut coordinator = coordinator_with(transport, profiles, &directory);

        let result = coordinator
            .handle(
                "profile.apply",
                &json!({"id": "day", "monitor": "mon-a", "wait": true}),
            )
            .expect("apply");
        assert_eq!(
            result["status"], "applied",
            "the panel lists 8, so the write is a validated one: {result}"
        );
        let temperature = result["monitors"][0]["fields"]
            .as_array()
            .expect("fields")
            .iter()
            .find(|field| field["control"] == "color_temperature")
            .cloned()
            .expect("a color_temperature field");
        assert_eq!(temperature["native"], 8);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn a_native_temperature_value_the_panel_does_not_list_is_refused_before_any_write() {
        let directory = scratch("temperature-refused");
        let mut transport = FakeTransport::with_monitor("mon-a", "Panel");
        transport.capabilities = Some(String::from(
            "prot(monitor)type(lcd)vcp(101214(0506080B))mccs_ver(2.1)",
        ));
        let profiles = vec![sample_profile(
            "day",
            10,
            "brightness = 40
color_temperature = { vcp = 0x14, raw = 9 }",
        )];
        let mut coordinator = coordinator_with(transport, profiles, &directory);

        // The panel lists 5, 6, 8 and 11. A value outside that list is not "unsupported hardware", it
        // is an invalid request, and it is refused in the preflight so nothing is written at all.
        let error = coordinator
            .handle(
                "profile.apply",
                &json!({"id": "day", "monitor": "mon-a", "wait": true}),
            )
            .expect_err("9 is not an accepted value on this panel");
        assert_eq!(error.kind, "invalid");
        assert!(error.message.contains("0x05"), "{}", error.message);
        assert!(error.message.contains("0x08"), "{}", error.message);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn profile_next_cycles_in_registry_order() {
        let directory = scratch("next");
        let transport = FakeTransport::with_monitor("mon-a", "Panel");
        let profiles = vec![
            sample_profile("day", 10, "brightness = 100"),
            sample_profile("night", 20, "brightness = 0"),
        ];
        let mut coordinator = coordinator_with(transport, profiles, &directory);

        let first = coordinator
            .handle("profile.next", &json!({"monitor": "mon-a"}))
            .expect("first");
        assert_eq!(first["profile"], "day");
        let second = coordinator
            .handle("profile.next", &json!({"monitor": "mon-a"}))
            .expect("second");
        assert_eq!(second["profile"], "night");
        let third = coordinator
            .handle("profile.next", &json!({"monitor": "mon-a"}))
            .expect("third");
        assert_eq!(third["profile"], "day");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn validate_value_needs_an_explicit_confirmation() {
        let directory = scratch("validate");
        let transport = FakeTransport::with_monitor("mon-a", "Panel");
        let mut coordinator = coordinator_with(transport, Vec::new(), &directory);

        let error = coordinator
            .handle(
                "monitors.validate_value",
                &json!({"monitor": "mon-a", "control": "brightness", "value": 5}),
            )
            .expect_err("no confirmation");
        assert_eq!(error.kind, "invalid");
        assert!(coordinator.transport.writes.is_empty());

        let result = coordinator
            .handle(
                "monitors.validate_value",
                &json!({"monitor": "mon-a", "control": "brightness", "value": 5, "confirm_write": true}),
            )
            .expect("confirmed");
        assert_eq!(result["verified"], true);
        assert_eq!(coordinator.transport.writes.len(), 1);
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn startup_status_reports_residency_for_two_profiles() {
        let directory = scratch("startup");
        let transport = FakeTransport::with_monitor("mon-a", "Panel");
        let profiles = vec![
            sample_profile("day", 10, "brightness = 100"),
            sample_profile("night", 20, "brightness = 0"),
        ];
        let mut coordinator = coordinator_with(transport, profiles, &directory);

        let result = coordinator
            .handle("startup.status", &Value::Null)
            .expect("startup");
        assert_eq!(result["outcome"], "resident");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn reload_keeps_the_last_good_registry_when_a_file_is_invalid() {
        let directory = scratch("reload");
        std::fs::write(
            directory.join("day.toml"),
            "schema_version = 1\nid = \"day\"\nname = \"Day\"\nicon = \"day\"\n\n[monitor]\nbrightness = 50\n",
        )
        .expect("write profile");
        let prefix_errors = ProfileRegistry::load_dir(&directory).expect("initial registry");
        let transport = FakeTransport::with_monitor("mon-a", "Panel");
        let mut coordinator = Coordinator::new(
            transport,
            Settings::default(),
            prefix_errors,
            directory.join("settings.toml"),
        )
        .with_profiles_dir(directory.clone());

        std::fs::write(directory.join("broken.toml"), "schema_version = 1\nid = \"x\"\n")
            .expect("write broken profile");
        let error = coordinator
            .handle("profiles.reload", &Value::Null)
            .expect_err("invalid file");
        assert_eq!(error.kind, "invalid_config");
        // The last good registry is still in service.
        let listed = coordinator
            .handle("profiles.list", &Value::Null)
            .expect("list");
        assert_eq!(listed["profiles"].as_array().map(|items| items.len()), Some(1));
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn shutdown_marks_the_coordinator() {
        let mut coordinator = coordinator();
        coordinator.handle("shutdown", &Value::Null).expect("shutdown");
        assert!(coordinator.shutdown_requested());
    }

    // -----------------------------------------------------------------------
    // P0: strict preflight, failure boundaries and multi-target aggregation
    // -----------------------------------------------------------------------

    use crate::config::settings::UnsupportedPolicy;
    use crate::transport::TransportError;

    fn strict() -> Settings {
        Settings {
            unsupported: UnsupportedPolicy::Fail,
            ..Settings::default()
        }
    }

    fn coordinator_with_settings(
        transport: FakeTransport,
        profiles: Vec<crate::config::Profile>,
        directory: &std::path::Path,
        settings: Settings,
    ) -> Coordinator<FakeTransport> {
        Coordinator::new(
            transport,
            settings,
            ProfileRegistry::new(profiles).expect("registry"),
            directory.join("settings.toml"),
        )
        .with_profiles_dir(directory.to_path_buf())
    }

    fn monitor_row<'a>(result: &'a Value, monitor: &str) -> &'a Value {
        result["monitors"]
            .as_array()
            .expect("monitors array")
            .iter()
            .find(|entry| entry["monitor"] == monitor)
            .expect("monitor row")
    }

    fn field_row<'a>(result: &'a Value, monitor: &str, control: &str) -> &'a Value {
        monitor_row(result, monitor)["fields"]
            .as_array()
            .expect("fields array")
            .iter()
            .find(|entry| entry["control"] == control)
            .expect("field row")
    }

    #[test]
    fn strict_policy_refuses_a_profile_with_an_unwritable_control_before_any_write() {
        let directory = scratch("strict-unwritable");
        let transport = FakeTransport::with_monitor("mon-a", "Panel");
        let profile = sample_profile("night", 10, "brightness = 0\nvolume = 20");
        let mut coordinator =
            coordinator_with_settings(transport, vec![profile], &directory, strict());

        let error = coordinator
            .handle("profile.apply", &json!({"id": "night", "monitor": "mon-a"}))
            .expect_err("strict policy refuses the unwritable control");

        assert_eq!(error.kind, "unsupported");
        assert!(coordinator.transport.writes.is_empty(), "nothing may be written");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn strict_policy_writes_nothing_on_either_target_when_one_target_lacks_a_control() {
        let directory = scratch("strict-targets");
        let transport = FakeTransport::with_monitor("mon-a", "Panel")
            .add_monitor("mon-b", "Second")
            .without_control("mon-b", "contrast");
        let profile = sample_profile("day", 10, "brightness = 100\ncontrast = 80");
        let mut coordinator =
            coordinator_with_settings(transport, vec![profile], &directory, strict());

        let error = coordinator
            .handle(
                "profile.apply",
                &json!({"id": "day", "monitor_ids": ["mon-a", "mon-b"]}),
            )
            .expect_err("one target cannot provide the requested control");

        assert_eq!(error.kind, "unsupported");
        assert!(
            coordinator.transport.writes.is_empty(),
            "the healthy target must not be written when preflight fails anywhere"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn skip_policy_applies_the_writable_fields_and_names_what_it_skipped() {
        let directory = scratch("skip-unwritable");
        let transport = FakeTransport::with_monitor("mon-a", "Panel");
        let profile = sample_profile("day", 10, "brightness = 40\nvolume = 20");
        let mut coordinator = coordinator_with(transport, vec![profile], &directory);

        let result = coordinator
            .handle("profile.apply", &json!({"id": "day", "monitor": "mon-a"}))
            .expect("apply");

        assert_eq!(result["status"], "partial");
        assert_eq!(field_row(&result, "mon-a", "brightness")["status"], "applied");
        let volume = field_row(&result, "mon-a", "volume");
        assert_eq!(volume["status"], "skipped");
        assert_eq!(volume["state"], "not_implemented");
        assert_eq!(coordinator.transport.writes.len(), 1);
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_timeout_stops_the_rest_of_that_monitors_transaction() {
        let directory = scratch("timeout-stop");
        let transport = FakeTransport::with_monitor("mon-a", "Panel").fail_write(
            "mon-a",
            "contrast",
            TransportError::Timeout(String::from(
                "the DDC worker did not answer; the hardware outcome is unknown",
            )),
        );
        let profile = sample_profile("day", 10, "contrast = 70\nbrightness = 50");
        let mut coordinator = coordinator_with(transport, vec![profile], &directory);

        let result = coordinator
            .handle("profile.apply", &json!({"id": "day", "monitor": "mon-a"}))
            .expect("apply reports an outcome rather than aborting");

        assert_eq!(result["status"], "not_applied");
        assert_eq!(result["cause"], "timeout");
        assert_eq!(field_row(&result, "mon-a", "contrast")["status"], "failed");
        assert_eq!(
            field_row(&result, "mon-a", "brightness")["status"],
            "not_attempted"
        );
        assert!(
            coordinator.transport.writes.is_empty(),
            "no write may follow an outcome-unknown write in the same transaction"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn one_failed_target_can_never_turn_a_mixed_result_into_applied() {
        let directory = scratch("mixed");
        let transport = FakeTransport::with_monitor("mon-a", "Panel")
            .add_monitor("mon-b", "Second")
            .fail_write(
                "mon-b",
                "brightness",
                TransportError::Timeout(String::from("no answer from the worker")),
            );
        let profile = sample_profile("day", 10, "brightness = 60");
        let mut coordinator = coordinator_with(transport, vec![profile], &directory);

        let result = coordinator
            .handle(
                "profile.apply",
                &json!({"id": "day", "monitor_ids": ["mon-a", "mon-b"]}),
            )
            .expect("apply");

        assert_eq!(result["status"], "partial");
        assert_eq!(result["cause"], "timeout");
        assert_eq!(monitor_row(&result, "mon-a")["status"], "applied");
        assert_eq!(monitor_row(&result, "mon-b")["status"], "not_applied");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn every_target_failing_is_not_applied_with_a_typed_cause() {
        let directory = scratch("all-failed");
        let transport = FakeTransport::with_monitor("mon-a", "Panel").fail_write(
            "mon-a",
            "brightness",
            TransportError::Transport(String::from("the pipe closed")),
        );
        let profile = sample_profile("day", 10, "brightness = 60");
        let mut coordinator = coordinator_with(transport, vec![profile], &directory);

        let result = coordinator
            .handle("profile.apply", &json!({"id": "day", "monitor": "mon-a"}))
            .expect("apply");

        assert_eq!(result["status"], "not_applied");
        assert_eq!(result["cause"], "transport_error");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn an_unknown_target_is_refused_before_planning() {
        let directory = scratch("unknown-target");
        let transport = FakeTransport::with_monitor("mon-a", "Panel");
        let profile = sample_profile("day", 10, "brightness = 60");
        let mut coordinator = coordinator_with(transport, vec![profile], &directory);

        let error = coordinator
            .handle("profile.apply", &json!({"id": "day", "monitor": "mon-ghost"}))
            .expect_err("target is not present");

        assert_eq!(error.kind, "unknown_monitor");
        assert!(coordinator.transport.writes.is_empty());
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_direct_set_clears_the_applied_profile_marker() {
        let directory = scratch("direct-set");
        let transport = FakeTransport::with_monitor("mon-a", "Panel");
        let profile = sample_profile("day", 10, "brightness = 60");
        let mut coordinator = coordinator_with(transport, vec![profile], &directory);

        coordinator
            .handle("profile.apply", &json!({"id": "day", "monitor": "mon-a"}))
            .expect("apply");
        assert_eq!(
            coordinator.last_applied.get("mon-a").map(String::as_str),
            Some("day")
        );

        coordinator
            .handle(
                "parameters.set",
                &json!({"monitor": "mon-a", "control": "brightness", "value": 40, "unit": "percent"}),
            )
            .expect("direct set");

        assert!(
            !coordinator.last_applied.contains_key("mon-a"),
            "a direct change makes the monitor Custom, not still on the old profile"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn settings_set_writes_the_selected_root_and_keeps_the_user_comments() {
        let directory = scratch("settings-set");
        let file = directory.join("settings.toml");
        std::fs::write(
            &file,
            "# written by hand\nschema_version = 1\n\n[tray]\nleft_click = \"flyout\"\n",
        )
        .expect("settings");

        let transport = FakeTransport::with_monitor("mon-a", "Panel");
        let mut coordinator = coordinator_with(transport, Vec::new(), &directory);

        let result = coordinator
            .handle(
                "settings.set",
                &json!({"key": "tray.left_click", "value": "cycle"}),
            )
            .expect("set");

        assert_eq!(result["value"], "cycle");
        let text = std::fs::read_to_string(&file).expect("read");
        assert!(text.starts_with("# written by hand\n"), "{text}");
        assert!(text.contains("left_click = \"cycle\""));
        assert_eq!(
            coordinator.settings().left_click,
            crate::config::settings::LeftClick::Cycle
        );
        // The write went into the resolved root, not somewhere else.
        assert!(!directory.join("settings.toml.tmp").exists());
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn settings_set_refuses_a_file_it_cannot_parse_and_leaves_it_untouched() {
        let directory = scratch("settings-broken");
        let file = directory.join("settings.toml");
        std::fs::write(&file, "schema_version = \n").expect("broken settings");

        let transport = FakeTransport::with_monitor("mon-a", "Panel");
        let mut coordinator = coordinator_with(transport, Vec::new(), &directory);

        let error = coordinator
            .handle(
                "settings.set",
                &json!({"key": "tray.left_click", "value": "cycle"}),
            )
            .expect_err("unparseable settings are not overwritten");

        assert_eq!(error.kind, "invalid_config");
        assert_eq!(
            std::fs::read_to_string(&file).expect("read"),
            "schema_version = \n"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    // -----------------------------------------------------------------------
    // R2: read-only preserve reconciliation
    // -----------------------------------------------------------------------

    #[test]
    fn an_observation_writes_nothing_and_reports_matched_not_applied() {
        let directory = scratch("reconcile-read-only");
        // The display already reports 20, which is what the `night` profile asks for.
        let transport =
            FakeTransport::with_monitor("mon-a", "Panel").with_value("mon-a", "brightness", 20);
        let profiles = vec![
            sample_profile("day", 10, "brightness = 80"),
            sample_profile("night", 20, "brightness = 20"),
        ];
        let mut coordinator = coordinator_with(transport, profiles, &directory);

        assert_eq!(
            coordinator.current("mon-a").state,
            crate::reconcile::CurrentState::Checking,
            "nothing is known before the first observation"
        );

        let observed = coordinator.reconcile(&[String::from("mon-a")]);
        assert_eq!(observed, vec![String::from("mon-a")]);

        let current = coordinator.current("mon-a");
        assert_eq!(current.state, crate::reconcile::CurrentState::Matched);
        assert_eq!(current.profile_id(), Some("night"));
        assert!(current.observed_at.is_some());
        // A read-only match is not an application, and it wrote nothing.
        assert_eq!(current.state.as_str(), "matched");
        assert!(
            coordinator.transport.writes.is_empty(),
            "an observation must never write to the hardware"
        );
        // The same answer every surface reads: the icon comes from the matched profile.
        assert_eq!(coordinator.profile_icon("night"), Some("day"));
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_status_refresh_observes_once_and_an_ordinary_status_does_not_read_hardware() {
        let directory = scratch("status-refresh");
        let transport =
            FakeTransport::with_monitor("mon-a", "Panel").with_value("mon-a", "brightness", 80);
        let profiles = vec![sample_profile("day", 10, "brightness = 80")];
        let mut coordinator = coordinator_with(transport, profiles, &directory);

        // Without refresh the cached snapshot is all that is consulted.
        let status = coordinator.handle("status", &json!({"refresh": false})).expect("status");
        assert_eq!(status["monitors"][0]["current"]["state"], "checking");

        let status = coordinator.handle("status", &json!({"refresh": true})).expect("status");
        assert_eq!(status["monitors"][0]["current"]["state"], "matched");
        assert_eq!(status["monitors"][0]["current"]["profile"], "day");
        assert_eq!(status["monitors"][0]["current"]["profile_name"], "day");
        assert!(coordinator.transport.writes.is_empty());
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_cycle_starts_after_the_observed_profile_and_not_at_the_first_one() {
        let directory = scratch("next-after-observed");
        // The hardware is in `night`; a cycle must go to `day`, not back to `night`.
        let transport =
            FakeTransport::with_monitor("mon-a", "Panel").with_value("mon-a", "brightness", 20);
        let profiles = vec![
            sample_profile("day", 10, "brightness = 80"),
            sample_profile("night", 20, "brightness = 20"),
        ];
        let mut coordinator = coordinator_with(transport, profiles, &directory);
        coordinator.reconcile(&[String::from("mon-a")]);
        assert_eq!(coordinator.current("mon-a").profile_id(), Some("night"));

        let result = coordinator
            .handle("profile.next", &json!({"monitor": "mon-a"}))
            .expect("cycle");
        assert_eq!(result["monitors"][0]["monitor"], "mon-a");
        assert_eq!(result["profile"], "day");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn an_unsupported_read_leaves_the_monitor_unknown_and_claims_no_profile() {
        let directory = scratch("reconcile-unsupported");
        // Contrast is part of the profile but the display does not offer it.
        let transport = FakeTransport::with_monitor("mon-a", "Panel")
            .with_value("mon-a", "brightness", 40)
            .without_control("mon-a", "contrast");
        let profiles = vec![sample_profile("day", 10, "brightness = 40\ncontrast = 70")];
        let mut coordinator = coordinator_with(transport, profiles, &directory);

        coordinator.reconcile(&[String::from("mon-a")]);
        let current = coordinator.current("mon-a");
        assert_eq!(current.state, crate::reconcile::CurrentState::Unknown);
        assert_eq!(current.profile_id(), None);
        assert!(current.reason.as_deref().unwrap_or("").contains("contrast"));
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn the_persisted_history_breaks_a_tie_and_a_failed_apply_drops_it() {
        let directory = scratch("reconcile-history");
        // Two profiles with identical requests: reads alone cannot tell them apart.
        let transport =
            FakeTransport::with_monitor("mon-a", "Panel").with_value("mon-a", "brightness", 50);
        let profiles = vec![
            sample_profile("alpha", 10, "brightness = 50"),
            sample_profile("bravo", 20, "brightness = 50"),
        ];
        let mut coordinator = coordinator_with(transport, profiles, &directory);

        // Without history, registry order decides.
        coordinator.reconcile(&[String::from("mon-a")]);
        assert_eq!(coordinator.current("mon-a").profile_id(), Some("alpha"));

        // Applying the second one records the history and the verified state.
        coordinator
            .handle("profile.apply", &json!({"id": "bravo", "monitor": "mon-a"}))
            .expect("apply");
        assert_eq!(coordinator.current("mon-a").state, crate::reconcile::CurrentState::Applied);
        assert!(directory.join("state.json").exists(), "the history is persisted");

        // A later observation prefers the profile the history names.
        coordinator.reconcile(&[String::from("mon-a")]);
        assert_eq!(coordinator.current("mon-a").profile_id(), Some("bravo"));

        // A failed apply leaves nothing verified, and the recorded history goes with it.
        coordinator.transport.failing_writes.push((
            String::from("mon-a"),
            String::from("brightness"),
            TransportError::Timeout(String::from("no answer")),
        ));
        coordinator
            .handle("profile.apply", &json!({"id": "bravo", "monitor": "mon-a"}))
            .expect("apply reports a failure rather than aborting");
        assert_eq!(coordinator.current("mon-a").state, crate::reconcile::CurrentState::Unknown);
        let text = std::fs::read_to_string(directory.join("state.json")).expect("state");
        assert!(!text.contains("bravo"), "{text}");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_monitor_that_was_unplugged_loses_its_claim() {
        let directory = scratch("reconcile-reconnect");
        let transport =
            FakeTransport::with_monitor("mon-a", "Panel").with_value("mon-a", "brightness", 40);
        let profiles = vec![sample_profile("day", 10, "brightness = 40")];
        let mut coordinator = coordinator_with(transport, profiles, &directory);
        coordinator.reconcile(&[String::from("mon-a")]);
        assert_eq!(coordinator.current("mon-a").profile_id(), Some("day"));

        // The display disappears: a later observation must not keep answering about it.
        coordinator.transport.unplug("mon-a");
        coordinator.reconcile(&[]);
        assert_eq!(
            coordinator.current("mon-a").state,
            crate::reconcile::CurrentState::Checking,
            "a stale claim about a disconnected display must not survive"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn strict_policy_refuses_conflicting_white_point_controls_before_any_write() {
        let directory = scratch("strict-conflict");
        let transport = FakeTransport::with_monitor("mon-a", "Panel");
        let mut profile = sample_profile("day", 10, "brightness = 40");
        profile.monitor.color_temperature =
            Some(crate::config::profile::ColorTemperature::Kelvin(6500));
        profile.monitor.rgb_gain.red = Some(crate::config::value::ControlValue::Percent(100));
        let mut coordinator =
            coordinator_with_settings(transport, vec![profile], &directory, strict());

        let error = coordinator
            .handle("profile.apply", &json!({"id": "day", "monitor": "mon-a"}))
            .expect_err("conflicting white-point mechanisms");

        assert!(coordinator.transport.writes.is_empty());
        assert!(error.message.contains("RGB gains"), "{}", error.message);
        let _ = std::fs::remove_dir_all(&directory);
    }
}
