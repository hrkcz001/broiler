//! Backend boundary, fake backend and the apply executor.
//!
//! The executor is the only place that turns a plan into traffic. It keeps three things apart that
//! are easy to conflate: the value the user asked for, the native value that was sent, and the
//! value read back afterwards. A write is only reported as applied when the readback matches the
//! native target; a timeout is reported with an unknown hardware outcome and never replayed.
//!
//! Timing goes through [`Delay`], so the inter-transaction gap is asserted deterministically in
//! tests instead of being a sleep in CI.

use std::collections::HashMap;
use std::time::Duration;

use crate::plan::{ApplyPlan, Control, PlannedValue};

/// Minimum gap between two completed hardware transactions.
pub const INTER_TRANSACTION_GAP: Duration = Duration::from_millis(100);

/// Failure of one backend operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackendError {
    /// The channel failed; the hardware outcome is unknown.
    Transport(String),
    /// No response inside the bounded wait; the hardware outcome is unknown.
    Timeout,
    /// The device does not offer the control, or the current mode blocks it.
    Unsupported(String),
    /// A write completed but the readback disagreed.
    Verification { expected: u64, observed: u64 },
}

impl BackendError {
    pub fn message(&self) -> String {
        match self {
            BackendError::Transport(reason) => format!("transport error: {reason}"),
            BackendError::Timeout => String::from("no response within the bound; outcome unknown"),
            BackendError::Unsupported(reason) => format!("unsupported: {reason}"),
            BackendError::Verification { expected, observed } => format!(
                "readback mismatch: wrote {expected}, device reports {observed}"
            ),
        }
    }

    /// True when the backend cannot say whether the device changed.
    pub fn outcome_unknown(&self) -> bool {
        matches!(self, BackendError::Transport(_) | BackendError::Timeout)
    }
}

/// The transport a plan is executed against.
pub trait MonitorBackend {
    fn read(&mut self, monitor: &str, control: Control) -> Result<u64, BackendError>;
    fn write(&mut self, monitor: &str, control: Control, value: u64) -> Result<(), BackendError>;
}

/// Injected wait, so tests can assert timing without sleeping.
pub trait Delay {
    fn wait(&mut self, duration: Duration);
}

/// A delay that does nothing, for tests that only care about ordering.
pub struct NoDelay;

impl Delay for NoDelay {
    fn wait(&mut self, _duration: Duration) {}
}

/// A delay that records the total requested time.
#[derive(Default)]
pub struct RecordingDelay {
    pub total: Duration,
    pub calls: usize,
}

impl Delay for RecordingDelay {
    fn wait(&mut self, duration: Duration) {
        self.total += duration;
        self.calls += 1;
    }
}

/// Result of one planned field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FieldOutcome {
    /// Written and confirmed by a readback that matches the native value.
    Verified {
        requested: PlannedValue,
        native: u64,
        observed: u64,
    },
    /// Written or attempted, but not confirmed.
    Failed {
        requested: PlannedValue,
        native: u64,
        reason: String,
        outcome_unknown: bool,
    },
    /// Not written because the device does not offer it.
    Skipped { state: String, reason: String },
}

impl FieldOutcome {
    pub fn is_verified(&self) -> bool {
        matches!(self, FieldOutcome::Verified { .. })
    }
}

/// Overall result of applying one plan to one monitor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyStatus {
    /// Every planned field was verified and nothing was skipped.
    Applied,
    /// Some fields were verified; others were skipped or failed.
    Partial,
    /// Nothing was verified.
    NotApplied,
}

impl ApplyStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ApplyStatus::Applied => "applied",
            ApplyStatus::Partial => "partial",
            ApplyStatus::NotApplied => "not_applied",
        }
    }
}

/// Per-field results plus an overall status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyOutcome {
    pub status: ApplyStatus,
    pub fields: Vec<FieldOutcome>,
    pub stopped_early: bool,
}

/// Execute a plan against one monitor, verifying every write by readback.
pub fn execute(
    plan: &ApplyPlan,
    backend: &mut dyn MonitorBackend,
    monitor: &str,
    delay: &mut dyn Delay,
) -> ApplyOutcome {
    let mut fields: Vec<FieldOutcome> = Vec::new();
    let mut stopped_early = false;

    for skipped in &plan.skipped {
        fields.push(FieldOutcome::Skipped {
            state: skipped.state.clone(),
            reason: format!("{}: {}", skipped.control, skipped.reason),
        });
    }

    for (index, write) in plan.writes.iter().enumerate() {
        if index > 0 {
            delay.wait(INTER_TRANSACTION_GAP);
        }

        if stopped_early {
            fields.push(FieldOutcome::Failed {
                requested: write.requested.clone(),
                native: write.native,
                reason: String::from("not attempted: an earlier operation on this monitor failed"),
                outcome_unknown: false,
            });
            continue;
        }

        match backend.write(monitor, write.control, write.native) {
            Ok(()) => match backend.read(monitor, write.control) {
                Ok(observed) if observed == write.native => fields.push(FieldOutcome::Verified {
                    requested: write.requested.clone(),
                    native: write.native,
                    observed,
                }),
                Ok(observed) => {
                    fields.push(FieldOutcome::Failed {
                        requested: write.requested.clone(),
                        native: write.native,
                        reason: BackendError::Verification {
                            expected: write.native,
                            observed,
                        }
                        .message(),
                        outcome_unknown: false,
                    });
                    stopped_early = true;
                }
                Err(error) => {
                    let unknown = error.outcome_unknown();
                    fields.push(FieldOutcome::Failed {
                        requested: write.requested.clone(),
                        native: write.native,
                        reason: error.message(),
                        outcome_unknown: unknown,
                    });
                    stopped_early = true;
                }
            },
            Err(error) => {
                let unknown = error.outcome_unknown();
                fields.push(FieldOutcome::Failed {
                    requested: write.requested.clone(),
                    native: write.native,
                    reason: error.message(),
                    outcome_unknown: unknown,
                });
                // A timeout or transport failure leaves the device state unknown; nothing else is
                // sent to this monitor in this operation.
                if unknown {
                    stopped_early = true;
                }
            }
        }
    }

    let verified = fields.iter().filter(|field| field.is_verified()).count();
    let status = if verified == 0 {
        ApplyStatus::NotApplied
    } else if verified == plan.writes.len() && plan.skipped.is_empty() {
        ApplyStatus::Applied
    } else {
        ApplyStatus::Partial
    };

    ApplyOutcome {
        status,
        fields,
        stopped_early,
    }
}

/// Deterministic backend used by tests and by any code path that must not touch hardware.
#[derive(Default)]
pub struct FakeBackend {
    values: HashMap<(String, Control), u64>,
    /// Every write that reached the backend, in order.
    pub writes: Vec<(String, Control, u64)>,
    /// Failures to return on the next writes, in order.
    pub scripted_write_failures: Vec<BackendError>,
    /// Failures to return on the next reads, in order.
    pub scripted_read_failures: Vec<BackendError>,
    /// Offset added to every reported value, to simulate a device that does not store what it was
    /// sent.
    pub read_offset: i64,
    /// When set, writes are recorded but not stored, which makes readback disagree.
    pub ignore_writes: bool,
}

impl FakeBackend {
    pub fn new() -> FakeBackend {
        FakeBackend::default()
    }

    pub fn with_value(mut self, monitor: &str, control: Control, value: u64) -> FakeBackend {
        self.values.insert((monitor.to_string(), control), value);
        self
    }

    pub fn stored(&self, monitor: &str, control: Control) -> Option<u64> {
        self.values
            .get(&(monitor.to_string(), control))
            .copied()
    }
}

impl MonitorBackend for FakeBackend {
    fn read(&mut self, monitor: &str, control: Control) -> Result<u64, BackendError> {
        if !self.scripted_read_failures.is_empty() {
            return Err(self.scripted_read_failures.remove(0));
        }
        let value = self
            .values
            .get(&(monitor.to_string(), control))
            .copied()
            .unwrap_or(0);
        let adjusted = (value as i64 + self.read_offset).max(0) as u64;
        Ok(adjusted)
    }

    fn write(&mut self, monitor: &str, control: Control, value: u64) -> Result<(), BackendError> {
        self.writes.push((monitor.to_string(), control, value));
        if !self.scripted_write_failures.is_empty() {
            return Err(self.scripted_write_failures.remove(0));
        }
        if !self.ignore_writes {
            self.values.insert((monitor.to_string(), control), value);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{PlannedWrite, SkippedField};

    fn plan_with(writes: Vec<(Control, u64)>) -> ApplyPlan {
        ApplyPlan {
            writes: writes
                .into_iter()
                .map(|(control, native)| PlannedWrite {
                    control,
                    requested: PlannedValue::Native(native),
                    native,
                    backend: None,
                })
                .collect(),
            skipped: Vec::new(),
        }
    }

    #[test]
    fn a_single_verified_write_reports_applied() {
        let plan = plan_with(vec![(Control::Brightness, 40)]);
        let mut backend = FakeBackend::new();
        let outcome = execute(&plan, &mut backend, "mon-a", &mut NoDelay);
        assert_eq!(outcome.status, ApplyStatus::Applied);
        assert!(outcome.fields[0].is_verified());
        assert_eq!(backend.writes.len(), 1);
    }

    #[test]
    fn a_mismatching_readback_is_not_success() {
        let plan = plan_with(vec![(Control::Brightness, 40)]);
        let mut backend = FakeBackend::new();
        backend.ignore_writes = true;
        let outcome = execute(&plan, &mut backend, "mon-a", &mut NoDelay);
        assert_eq!(outcome.status, ApplyStatus::NotApplied);
        match &outcome.fields[0] {
            FieldOutcome::Failed { reason, outcome_unknown, .. } => {
                assert!(reason.contains("readback mismatch"), "{reason}");
                assert!(!outcome_unknown);
            }
            other => panic!("unexpected outcome {other:?}"),
        }
    }

    #[test]
    fn a_scripted_offset_device_fails_verification() {
        let plan = plan_with(vec![(Control::Contrast, 70)]);
        let mut backend = FakeBackend::new();
        backend.read_offset = 1;
        let outcome = execute(&plan, &mut backend, "mon-a", &mut NoDelay);
        assert_eq!(outcome.status, ApplyStatus::NotApplied);
    }

    #[test]
    fn a_timeout_is_reported_as_an_unknown_outcome_and_stops_the_monitor() {
        let plan = plan_with(vec![(Control::Brightness, 10), (Control::Contrast, 20)]);
        let mut backend = FakeBackend::new();
        backend.scripted_write_failures.push(BackendError::Timeout);
        let outcome = execute(&plan, &mut backend, "mon-a", &mut NoDelay);

        assert_eq!(outcome.status, ApplyStatus::NotApplied);
        assert!(outcome.stopped_early);
        match &outcome.fields[0] {
            FieldOutcome::Failed { outcome_unknown, .. } => assert!(outcome_unknown),
            other => panic!("unexpected outcome {other:?}"),
        }
        // The second field is reported as not attempted, and no second write reached the device.
        assert_eq!(backend.writes.len(), 1);
        assert!(matches!(outcome.fields[1], FieldOutcome::Failed { .. }));
    }

    #[test]
    fn a_partial_plan_reports_partial_and_keeps_both_field_states() {
        let mut plan = plan_with(vec![(Control::Brightness, 25)]);
        plan.skipped.push(SkippedField {
            control: Control::Mute,
            state: String::from("unsupported"),
            reason: String::from("the display does not offer this control"),
        });
        let mut backend = FakeBackend::new();
        let outcome = execute(&plan, &mut backend, "mon-a", &mut NoDelay);

        assert_eq!(outcome.status, ApplyStatus::Partial);
        assert!(outcome.fields.iter().any(FieldOutcome::is_verified));
        assert!(outcome
            .fields
            .iter()
            .any(|field| matches!(field, FieldOutcome::Skipped { .. })));
    }

    #[test]
    fn transactions_are_separated_by_the_configured_gap() {
        let plan = plan_with(vec![
            (Control::Brightness, 10),
            (Control::Contrast, 20),
            (Control::Volume, 30),
        ]);
        let mut backend = FakeBackend::new();
        let mut delay = RecordingDelay::default();
        let outcome = execute(&plan, &mut backend, "mon-a", &mut delay);

        assert_eq!(outcome.status, ApplyStatus::Applied);
        assert_eq!(delay.calls, 2);
        assert_eq!(delay.total, INTER_TRANSACTION_GAP * 2);
    }

    #[test]
    fn an_empty_plan_reports_not_applied() {
        let plan = ApplyPlan::default();
        let mut backend = FakeBackend::new();
        let outcome = execute(&plan, &mut backend, "mon-a", &mut NoDelay);
        assert_eq!(outcome.status, ApplyStatus::NotApplied);
        assert!(backend.writes.is_empty());
    }

    #[test]
    fn values_are_stored_per_monitor() {
        let plan = plan_with(vec![(Control::Brightness, 55)]);
        let mut backend = FakeBackend::new();
        execute(&plan, &mut backend, "mon-a", &mut NoDelay);
        assert_eq!(backend.stored("mon-a", Control::Brightness), Some(55));
        assert_eq!(backend.stored("mon-b", Control::Brightness), None);
    }
}
