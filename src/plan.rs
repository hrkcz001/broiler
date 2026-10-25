//! Apply planning.
//!
//! A plan is built before the first write, so a profile that cannot be applied as a whole is
//! refused while nothing has changed. Planning resolves every typed value against what the device
//! reports, keeps the requested value and the native target separate, and fixes the backend for a
//! colour-temperature operation before any traffic happens: choosing a second mechanism after an
//! uncertain write is exactly what must not happen.

use std::fmt;

use crate::config::profile::{ColorPreset, ColorTemperature, Profile, VCP_COLOR_PRESET, VCP_COLOR_REQUEST};
use crate::config::settings::UnsupportedPolicy;
use crate::config::value::{resolve, ControlValue, DeviceValueSupport, ResolveError};

/// One control the coordinator can touch. There is no generic VCP escape hatch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Control {
    Brightness,
    Contrast,
    ColorTemperature,
    ColorPreset,
    RgbRed,
    RgbGreen,
    RgbBlue,
    Volume,
    Mute,
}

impl Control {
    pub fn name(self) -> &'static str {
        match self {
            Control::Brightness => "brightness",
            Control::Contrast => "contrast",
            Control::ColorTemperature => "color_temperature",
            Control::ColorPreset => "color_preset",
            Control::RgbRed => "rgb_gain.red",
            Control::RgbGreen => "rgb_gain.green",
            Control::RgbBlue => "rgb_gain.blue",
            Control::Volume => "volume",
            Control::Mute => "mute",
        }
    }

    /// VCP code that carries this control, when it is carried by one.
    pub fn vcp_code(self) -> Option<u8> {
        match self {
            Control::Brightness => Some(0x10),
            Control::Contrast => Some(0x12),
            Control::ColorPreset => Some(0x14),
            Control::RgbRed => Some(0x16),
            Control::RgbGreen => Some(0x18),
            Control::RgbBlue => Some(0x1A),
            Control::Volume => Some(0x62),
            Control::Mute => Some(0x8D),
            Control::ColorTemperature => None,
        }
    }
}

impl fmt::Display for Control {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// Which mechanism performs a colour-temperature operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TemperatureBackend {
    /// `SetMonitorColorTemperature`, which is preferred when the exact value is supported.
    HighLevel,
    /// Validated preset on VCP `0x14`.
    Preset,
    /// Validated request value on VCP `0x0C`.
    Request,
}

impl TemperatureBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            TemperatureBackend::HighLevel => "dxva2-high-level",
            TemperatureBackend::Preset => "vcp-0x14",
            TemperatureBackend::Request => "vcp-0x0C",
        }
    }
}

/// Temperature capability evidence for one display.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TemperatureSupport {
    /// Kelvin values the high-level setter advertises.
    pub high_level: Vec<u32>,
    /// Accepted values of VCP `0x14`, with their standard labels when known.
    pub presets: Vec<u64>,
    /// Reported range of VCP `0x0C`.
    pub request_range: Option<(u64, u64)>,
    /// Validated request mapping: `kelvin = baseline + request * increment`.
    pub request_mapping: Option<RequestMapping>,
}

/// A validated linear mapping from a request value to Kelvin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestMapping {
    pub baseline: u32,
    pub increment: u32,
}

impl RequestMapping {
    /// Kelvin for a request value, or `None` when the result is not representable.
    pub fn kelvin_for(&self, request: u64) -> Option<u32> {
        if self.increment == 0 {
            return None;
        }
        let offset = u32::try_from(request).ok()?.checked_mul(self.increment)?;
        self.baseline.checked_add(offset)
    }

    /// Request value for a Kelvin temperature, when it lands exactly on the grid.
    pub fn request_for(&self, kelvin: u32) -> Option<u64> {
        if self.increment == 0 || kelvin < self.baseline {
            return None;
        }
        let delta = kelvin - self.baseline;
        (delta % self.increment == 0).then_some((delta / self.increment) as u64)
    }
}

/// What a display reports for each control.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceMetadata {
    pub brightness: DeviceValueSupport,
    pub contrast: DeviceValueSupport,
    pub volume: DeviceValueSupport,
    pub rgb_red: DeviceValueSupport,
    pub rgb_green: DeviceValueSupport,
    pub rgb_blue: DeviceValueSupport,
    pub color_preset: DeviceValueSupport,
    pub color_temperature: TemperatureSupport,
    pub mute: DeviceValueSupport,
}

impl Default for DeviceValueSupport {
    fn default() -> DeviceValueSupport {
        DeviceValueSupport::Unknown
    }
}

fn support_of(metadata: &DeviceMetadata, control: Control) -> DeviceValueSupport {
    match control {
        Control::Brightness => metadata.brightness.clone(),
        Control::Contrast => metadata.contrast.clone(),
        Control::Volume => metadata.volume.clone(),
        Control::RgbRed => metadata.rgb_red.clone(),
        Control::RgbGreen => metadata.rgb_green.clone(),
        Control::RgbBlue => metadata.rgb_blue.clone(),
        Control::ColorPreset => metadata.color_preset.clone(),
        Control::ColorTemperature => DeviceValueSupport::Unknown,
        Control::Mute => metadata.mute.clone(),
    }
}

/// The value as the user wrote it, kept next to the resolved native target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlannedValue {
    Percent(u32),
    Native(u64),
    Kelvin(u32),
    Named(String),
    Boolean(bool),
}

impl fmt::Display for PlannedValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlannedValue::Percent(value) => write!(formatter, "{value}%"),
            PlannedValue::Native(value) => write!(formatter, "{value} (0x{value:X})"),
            PlannedValue::Kelvin(value) => write!(formatter, "{value} K"),
            PlannedValue::Named(value) => write!(formatter, "{value}"),
            PlannedValue::Boolean(value) => write!(formatter, "{value}"),
        }
    }
}

/// One planned write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedWrite {
    pub control: Control,
    pub requested: PlannedValue,
    /// Exact native payload that will be sent.
    pub native: u64,
    /// Chosen mechanism, fixed before the first write.
    pub backend: Option<TemperatureBackend>,
}

/// A field that will not be written, with the state that explains why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkippedField {
    pub control: Control,
    pub state: String,
    pub reason: String,
}

/// The plan for one monitor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApplyPlan {
    pub writes: Vec<PlannedWrite>,
    pub skipped: Vec<SkippedField>,
}

impl ApplyPlan {
    pub fn is_empty(&self) -> bool {
        self.writes.is_empty()
    }
}

/// Why planning failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanError {
    /// The strict policy refuses a control the display does not offer.
    Unsupported {
        control: Control,
        reason: String,
    },
    /// A written value does not fit the device or the schema.
    Invalid {
        control: Control,
        reason: String,
    },
    /// The profile requests conflicting white-point mechanisms.
    Conflict(String),
}

impl fmt::Display for PlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanError::Unsupported { control, reason } => {
                write!(formatter, "{control}: {reason}")
            }
            PlanError::Invalid { control, reason } => write!(formatter, "{control}: {reason}"),
            PlanError::Conflict(reason) => formatter.write_str(reason),
        }
    }
}

fn state_of(error: &ResolveError) -> &'static str {
    match error {
        ResolveError::Unsupported => "unsupported",
        ResolveError::BlockedByMode => "blocked_by_mode",
        ResolveError::PercentOnEnumerated
        | ResolveError::PercentOnUnknownMetadata
        | ResolveError::NativeOutOfRange { .. }
        | ResolveError::NativeNotInSet { .. } => "invalid_value",
    }
}

fn plan_continuous(
    control: Control,
    value: ControlValue,
    metadata: &DeviceMetadata,
    policy: UnsupportedPolicy,
    plan: &mut ApplyPlan,
) -> Result<(), PlanError> {
    let support = support_of(metadata, control);
    match resolve(value, &support) {
        Ok(native) => {
            plan.writes.push(PlannedWrite {
                control,
                requested: match value {
                    ControlValue::Percent(percent) => PlannedValue::Percent(percent),
                    ControlValue::Native(raw) => PlannedValue::Native(raw),
                },
                native,
                backend: None,
            });
            Ok(())
        }
        Err(error @ (ResolveError::Unsupported | ResolveError::BlockedByMode)) => {
            match policy {
                UnsupportedPolicy::Skip => {
                    plan.skipped.push(SkippedField {
                        control,
                        state: String::from(state_of(&error)),
                        reason: error.to_string(),
                    });
                    Ok(())
                }
                UnsupportedPolicy::Fail => Err(PlanError::Unsupported {
                    control,
                    reason: error.to_string(),
                }),
            }
        }
        Err(error) => Err(PlanError::Invalid {
            control,
            reason: error.to_string(),
        }),
    }
}

fn plan_color_preset(
    preset: &ColorPreset,
    metadata: &DeviceMetadata,
    policy: UnsupportedPolicy,
    plan: &mut ApplyPlan,
) -> Result<(), PlanError> {
    let control = Control::ColorPreset;
    let native = match preset.native_value() {
        Some(value) => value,
        None => {
            return Err(PlanError::Invalid {
                control,
                reason: format!("no standard native value is known for `{preset:?}`"),
            })
        }
    };

    match resolve(ControlValue::Native(native), &metadata.color_preset) {
        Ok(value) => {
            plan.writes.push(PlannedWrite {
                control,
                requested: match preset {
                    ColorPreset::Named(name) => PlannedValue::Named(name.clone()),
                    ColorPreset::Native(raw) => PlannedValue::Native(*raw),
                },
                native: value,
                backend: None,
            });
            Ok(())
        }
        Err(error @ (ResolveError::Unsupported | ResolveError::BlockedByMode)) => match policy {
            UnsupportedPolicy::Skip => {
                plan.skipped.push(SkippedField {
                    control,
                    state: String::from(state_of(&error)),
                    reason: error.to_string(),
                });
                Ok(())
            }
            UnsupportedPolicy::Fail => Err(PlanError::Unsupported {
                control,
                reason: error.to_string(),
            }),
        },
        Err(error) => Err(PlanError::Invalid {
            control,
            reason: error.to_string(),
        }),
    }
}

fn preset_for_kelvin(kelvin: u32) -> Option<u64> {
    crate::config::profile::COLOR_PRESET_VALUES
        .iter()
        .find(|(name, _)| name == &format!("{kelvin}k"))
        .map(|(_, value)| *value)
}

fn plan_color_temperature(
    temperature: &ColorTemperature,
    metadata: &DeviceMetadata,
    policy: UnsupportedPolicy,
    plan: &mut ApplyPlan,
) -> Result<(), PlanError> {
    let control = Control::ColorTemperature;
    let support = &metadata.color_temperature;

    let unsupported = |plan: &mut ApplyPlan, reason: String| match policy {
        UnsupportedPolicy::Skip => {
            plan.skipped.push(SkippedField {
                control,
                state: String::from("unsupported"),
                reason,
            });
            Ok(())
        }
        UnsupportedPolicy::Fail => Err(PlanError::Unsupported { control, reason }),
    };

    match temperature {
        ColorTemperature::Kelvin(kelvin) => {
            if support.high_level.contains(kelvin) {
                plan.writes.push(PlannedWrite {
                    control,
                    requested: PlannedValue::Kelvin(*kelvin),
                    native: *kelvin as u64,
                    backend: Some(TemperatureBackend::HighLevel),
                });
                return Ok(());
            }

            if let Some(preset) = preset_for_kelvin(*kelvin) {
                if let DeviceValueSupport::Enumerated { values } = &metadata.color_preset {
                    if values.contains(&preset) {
                        plan.writes.push(PlannedWrite {
                            control,
                            requested: PlannedValue::Kelvin(*kelvin),
                            native: preset,
                            backend: Some(TemperatureBackend::Preset),
                        });
                        return Ok(());
                    }
                }
            }

            if let (Some(mapping), Some((minimum, maximum))) =
                (support.request_mapping, support.request_range)
            {
                if let Some(request) = mapping.request_for(*kelvin) {
                    if request >= minimum && request <= maximum {
                        plan.writes.push(PlannedWrite {
                            control,
                            requested: PlannedValue::Kelvin(*kelvin),
                            native: request,
                            backend: Some(TemperatureBackend::Request),
                        });
                        return Ok(());
                    }
                }
            }

            unsupported(
                plan,
                format!(
                    "no validated mechanism offers {kelvin} K on this display: high-level temperature, a matching \
                     0x14 preset and a validated 0x0C mapping are all absent"
                ),
            )
        }
        ColorTemperature::Native { vcp, raw } => match *vcp {
            VCP_COLOR_PRESET => {
                match resolve(ControlValue::Native(*raw), &metadata.color_preset) {
                    Ok(value) => {
                        plan.writes.push(PlannedWrite {
                            control,
                            requested: PlannedValue::Native(*raw),
                            native: value,
                            backend: Some(TemperatureBackend::Preset),
                        });
                        Ok(())
                    }
                    Err(error @ (ResolveError::Unsupported | ResolveError::BlockedByMode)) => {
                        match policy {
                            UnsupportedPolicy::Skip => {
                                plan.skipped.push(SkippedField {
                                    control,
                                    state: String::from(state_of(&error)),
                                    reason: error.to_string(),
                                });
                                Ok(())
                            }
                            UnsupportedPolicy::Fail => Err(PlanError::Unsupported {
                                control,
                                reason: error.to_string(),
                            }),
                        }
                    }
                    Err(error) => Err(PlanError::Invalid {
                        control,
                        reason: error.to_string(),
                    }),
                }
            }
            VCP_COLOR_REQUEST => match support.request_range {
                Some((minimum, maximum)) if *raw >= minimum && *raw <= maximum => {
                    plan.writes.push(PlannedWrite {
                        control,
                        requested: PlannedValue::Native(*raw),
                        native: *raw,
                        backend: Some(TemperatureBackend::Request),
                    });
                    Ok(())
                }
                Some((minimum, maximum)) => Err(PlanError::Invalid {
                    control,
                    reason: format!(
                        "native request {raw} is outside the reported range {minimum}..={maximum}"
                    ),
                }),
                None => unsupported(plan, String::from(
                    "the display did not report a range for the 0x0C temperature request",
                )),
            },
            other => Err(PlanError::Invalid {
                control,
                reason: format!("only vcp 0x14 and 0x0C carry a temperature, not {other:#04X}"),
            }),
        },
    }
}

/// Refuse a profile that asks for two white-point mechanisms at once.
///
/// This is a property of the profile, not of a display, so it is checked once per apply before any
/// monitor is planned. The schema rejects these combinations too; the check exists for values built
/// in code and for a future schema version.
pub fn check_conflicts(profile: &Profile) -> Result<(), PlanError> {
    let monitor = &profile.monitor;
    if monitor.color_temperature.is_some() && monitor.color_preset.is_some() {
        return Err(PlanError::Conflict(String::from(
            "color_temperature and color_preset cannot both be requested",
        )));
    }
    if monitor.color_temperature.is_some() && monitor.rgb_gain.any() {
        return Err(PlanError::Conflict(String::from(
            "color_temperature cannot be combined with RGB gains",
        )));
    }
    if let Some(preset) = monitor.color_preset.as_ref() {
        if monitor.rgb_gain.any() && !preset.allows_gains() {
            return Err(PlanError::Conflict(String::from(
                "RGB gains may accompany only a user preset",
            )));
        }
    }
    Ok(())
}

/// Build the plan for one profile against one display.
///
/// Order follows the coordinator contract: white point first (temperature or a user preset), then
/// the gains that depend on it, then contrast, brightness, volume and mute.
pub fn plan(
    profile: &Profile,
    metadata: &DeviceMetadata,
    policy: UnsupportedPolicy,
) -> Result<ApplyPlan, PlanError> {
    check_conflicts(profile)?;

    let monitor = &profile.monitor;
    let mut plan = ApplyPlan::default();

    if let Some(temperature) = monitor.color_temperature.as_ref() {
        plan_color_temperature(temperature, metadata, policy, &mut plan)?;
    }
    if let Some(preset) = monitor.color_preset.as_ref() {
        plan_color_preset(preset, metadata, policy, &mut plan)?;
    }
    for (control, value) in [
        (Control::RgbRed, monitor.rgb_gain.red),
        (Control::RgbGreen, monitor.rgb_gain.green),
        (Control::RgbBlue, monitor.rgb_gain.blue),
    ] {
        if let Some(value) = value {
            plan_continuous(control, value, metadata, policy, &mut plan)?;
        }
    }
    if let Some(value) = monitor.contrast {
        plan_continuous(Control::Contrast, value, metadata, policy, &mut plan)?;
    }
    if let Some(value) = monitor.brightness {
        plan_continuous(Control::Brightness, value, metadata, policy, &mut plan)?;
    }
    if let Some(value) = monitor.volume {
        plan_continuous(Control::Volume, value, metadata, policy, &mut plan)?;
    }
    if let Some(mute) = monitor.mute {
        let native = if mute { 0x01 } else { 0x02 };
        match resolve(ControlValue::Native(native), &metadata.mute) {
            Ok(value) => plan.writes.push(PlannedWrite {
                control: Control::Mute,
                requested: PlannedValue::Boolean(mute),
                native: value,
                backend: None,
            }),
            Err(error @ (ResolveError::Unsupported | ResolveError::BlockedByMode)) => {
                match policy {
                    UnsupportedPolicy::Skip => plan.skipped.push(SkippedField {
                        control: Control::Mute,
                        state: String::from(state_of(&error)),
                        reason: error.to_string(),
                    }),
                    UnsupportedPolicy::Fail => {
                        return Err(PlanError::Unsupported {
                            control: Control::Mute,
                            reason: error.to_string(),
                        })
                    }
                }
            }
            Err(error) => {
                return Err(PlanError::Invalid {
                    control: Control::Mute,
                    reason: error.to_string(),
                })
            }
        }
    }

    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::profile::parse as parse_profile;
    use crate::config::value::DeviceValueSupport;
    use std::path::Path;

    fn profile(text: &str) -> Profile {
        let source = format!(
            "schema_version = 1\nid = \"test\"\nname = \"Test\"\nicon = \"day\"\n\n[monitor]\n{text}\n"
        );
        parse_profile(&source, Path::new("test.toml")).expect("profile")
    }

    fn range(minimum: u64, maximum: u64) -> DeviceValueSupport {
        DeviceValueSupport::Range { minimum, maximum }
    }

    fn metadata() -> DeviceMetadata {
        DeviceMetadata {
            brightness: range(0, 100),
            contrast: range(0, 100),
            volume: range(0, 100),
            rgb_red: range(0, 100),
            rgb_green: range(0, 100),
            rgb_blue: range(0, 100),
            color_preset: DeviceValueSupport::Enumerated {
                values: vec![0x01, 0x02, 0x05, 0x08, 0x0B],
            },
            color_temperature: TemperatureSupport::default(),
            mute: DeviceValueSupport::Unsupported,
        }
    }

    #[test]
    fn percent_values_resolve_against_the_device_range() {
        let plan = super::plan(&profile("brightness = 50"), &metadata(), UnsupportedPolicy::Skip)
            .expect("plan");
        assert_eq!(plan.writes.len(), 1);
        assert_eq!(plan.writes[0].control, Control::Brightness);
        assert_eq!(plan.writes[0].requested, PlannedValue::Percent(50));
        assert_eq!(plan.writes[0].native, 50);
    }

    #[test]
    fn a_nonzero_minimum_is_respected() {
        let mut metadata = metadata();
        metadata.brightness = range(20, 100);
        let plan = super::plan(&profile("brightness = 0"), &metadata, UnsupportedPolicy::Skip).expect("plan");
        assert_eq!(plan.writes[0].native, 20);
    }

    #[test]
    fn native_values_above_one_hundred_are_forwarded_exactly() {
        let mut metadata = metadata();
        metadata.contrast = range(0, 1000);
        let plan = super::plan(&profile("contrast = { raw = 0x2BC }"), &metadata, UnsupportedPolicy::Skip)
            .expect("plan");
        assert_eq!(plan.writes[0].native, 0x2BC);
        assert_eq!(plan.writes[0].requested, PlannedValue::Native(0x2BC));
    }

    #[test]
    fn ordering_follows_white_point_then_gains_then_contrast_brightness_volume_mute() {
        let text = "color_preset = \"user1\"\nbrightness = 35\ncontrast = 70\nvolume = 20\n[monitor.rgb_gain]\nred = 100\n";
        let plan = super::plan(&profile(text), &metadata(), UnsupportedPolicy::Skip).expect("plan");
        let controls: Vec<Control> = plan.writes.iter().map(|write| write.control).collect();
        assert_eq!(
            controls,
            vec![
                Control::ColorPreset,
                Control::RgbRed,
                Control::Contrast,
                Control::Brightness,
                Control::Volume
            ]
        );
    }

    #[test]
    fn unsupported_controls_are_skipped_under_skip_and_refused_under_fail() {
        let text = "brightness = 30\nmute = true";
        let skipped = super::plan(&profile(text), &metadata(), UnsupportedPolicy::Skip).expect("plan");
        assert_eq!(skipped.writes.len(), 1);
        assert_eq!(skipped.skipped.len(), 1);
        assert_eq!(skipped.skipped[0].control, Control::Mute);
        assert_eq!(skipped.skipped[0].state, "unsupported");

        let error = super::plan(&profile(text), &metadata(), UnsupportedPolicy::Fail).expect_err("fail");
        assert!(matches!(error, PlanError::Unsupported { control: Control::Mute, .. }));
    }

    #[test]
    fn out_of_range_native_values_are_errors_under_both_policies() {
        let error = super::plan(
            &profile("brightness = { raw = 200 }"),
            &metadata(),
            UnsupportedPolicy::Skip,
        )
        .expect_err("out of range");
        assert!(matches!(error, PlanError::Invalid { .. }));
    }

    #[test]
    fn percentages_are_refused_for_an_enumerated_preset() {
        // The schema refuses a percentage before planning is reached; the resolver refuses the same
        // combination for any caller that builds a value in code.
        let error = crate::config::profile::parse(
            "schema_version = 1\nid = \"t\"\nname = \"T\"\nicon = \"day\"\n\n[monitor]\ncolor_preset = { percent = 40 }\n",
            std::path::Path::new("t.toml"),
        )
        .expect_err("schema refuses a percentage for a preset");
        assert!(error.render().contains("choice, not a percentage"));

        assert_eq!(
            resolve(
                ControlValue::Percent(40),
                &DeviceValueSupport::Enumerated { values: vec![0x05] }
            ),
            Err(ResolveError::PercentOnEnumerated)
        );
    }

    #[test]
    fn named_presets_map_to_standard_values_and_must_be_accepted() {
        let plan = super::plan(&profile("color_preset = \"6500k\""), &metadata(), UnsupportedPolicy::Skip)
            .expect("plan");
        assert_eq!(plan.writes[0].native, 0x05);
        assert_eq!(plan.writes[0].requested, PlannedValue::Named(String::from("6500k")));

        let error = super::plan(&profile("color_preset = \"7500k\""), &metadata(), UnsupportedPolicy::Fail)
            .expect_err("preset not accepted by this device");
        assert!(matches!(error, PlanError::Invalid { .. }));
    }

    #[test]
    fn kelvin_uses_the_high_level_setter_when_the_exact_value_is_supported() {
        let mut metadata = metadata();
        metadata.color_temperature.high_level = vec![6500];
        let plan = super::plan(
            &profile("color_temperature = \"6500k\""),
            &metadata,
            UnsupportedPolicy::Skip,
        )
        .expect("plan");
        assert_eq!(plan.writes[0].backend, Some(TemperatureBackend::HighLevel));
        assert_eq!(plan.writes[0].native, 6500);
    }

    #[test]
    fn kelvin_falls_back_to_a_matching_preset_only_when_the_device_accepts_it() {
        let mut metadata = metadata();
        metadata.color_temperature.high_level = vec![];
        let plan = super::plan(
            &profile("color_temperature = \"6500k\""),
            &metadata,
            UnsupportedPolicy::Skip,
        )
        .expect("plan");
        assert_eq!(plan.writes[0].backend, Some(TemperatureBackend::Preset));
        assert_eq!(plan.writes[0].native, 0x05);
    }

    #[test]
    fn kelvin_uses_a_validated_request_mapping_when_no_preset_matches() {
        let mut metadata = metadata();
        metadata.color_preset = DeviceValueSupport::Enumerated { values: vec![0x01] };
        metadata.color_temperature.request_range = Some((10, 60));
        metadata.color_temperature.request_mapping = Some(RequestMapping {
            baseline: 3000,
            increment: 100,
        });
        let plan = super::plan(
            &profile("color_temperature = \"6500k\""),
            &metadata,
            UnsupportedPolicy::Skip,
        )
        .expect("plan");
        assert_eq!(plan.writes[0].backend, Some(TemperatureBackend::Request));
        assert_eq!(plan.writes[0].native, 35);
    }

    #[test]
    fn kelvin_fails_when_nothing_validated_can_produce_it() {
        let mut metadata = metadata();
        // No matching 6500 K preset on this display, and 6500 is not on the request grid.
        metadata.color_preset = DeviceValueSupport::Enumerated { values: vec![0x01, 0x02] };
        metadata.color_temperature.request_mapping = Some(RequestMapping {
            baseline: 3000,
            increment: 250,
        });
        metadata.color_temperature.request_range = Some((0, 10));
        let error = super::plan(
            &profile("color_temperature = \"6500k\""),
            &metadata,
            UnsupportedPolicy::Skip,
        )
        .expect("plan");
        // 6500 K is not on the 3000 + n*250 grid, so the field is skipped rather than rounded.
        assert!(error.writes.is_empty());
        assert_eq!(error.skipped[0].control, Control::ColorTemperature);
    }

    #[test]
    fn native_preset_temperature_is_validated_against_the_enumerated_set() {
        let accepted = super::plan(
            &profile("color_temperature = { vcp = 0x14, raw = 5 }"),
            &metadata(),
            UnsupportedPolicy::Skip,
        )
        .expect("plan");
        assert_eq!(accepted.writes[0].native, 5);
        assert_eq!(accepted.writes[0].backend, Some(TemperatureBackend::Preset));

        let rejected = super::plan(
            &profile("color_temperature = { vcp = 0x14, raw = 6 }"),
            &metadata(),
            UnsupportedPolicy::Skip,
        )
        .expect_err("6 is not in the set");
        assert!(matches!(rejected, PlanError::Invalid { .. }));
    }

    #[test]
    fn native_request_temperature_is_validated_against_the_reported_range() {
        let mut metadata = metadata();
        metadata.color_temperature.request_range = Some((0, 100));
        let plan = super::plan(
            &profile("color_temperature = { vcp = 0x0C, raw = 0x23 }"),
            &metadata,
            UnsupportedPolicy::Skip,
        )
        .expect("plan");
        assert_eq!(plan.writes[0].native, 0x23);
        assert_eq!(plan.writes[0].backend, Some(TemperatureBackend::Request));

        let error = super::plan(
            &profile("color_temperature = { vcp = 0x0C, raw = 200 }"),
            &metadata,
            UnsupportedPolicy::Skip,
        )
        .expect_err("outside the range");
        assert!(matches!(error, PlanError::Invalid { .. }));
    }

    #[test]
    fn a_request_temperature_without_a_reported_range_is_unsupported() {
        let plan = super::plan(
            &profile("color_temperature = { vcp = 0x0C, raw = 35 }"),
            &metadata(),
            UnsupportedPolicy::Skip,
        )
        .expect("plan");
        assert!(plan.writes.is_empty());
        assert_eq!(plan.skipped[0].state, "unsupported");
    }

    #[test]
    fn mute_uses_the_standard_audio_encoding_only_when_confirmed() {
        let mut metadata = metadata();
        metadata.mute = DeviceValueSupport::Enumerated { values: vec![0x01, 0x02] };
        let plan = super::plan(&profile("mute = true"), &metadata, UnsupportedPolicy::Skip).expect("plan");
        assert_eq!(plan.writes[0].native, 0x01);

        let plan = super::plan(&profile("mute = false"), &metadata, UnsupportedPolicy::Skip).expect("plan");
        assert_eq!(plan.writes[0].native, 0x02);
    }

    #[test]
    fn conflicting_white_point_requests_are_refused_before_planning() {
        // The schema refuses the same combination, so the planner is checked directly with a
        // profile that a future schema version could produce.
        let mut profile = profile("brightness = 40");
        profile.monitor.color_preset = Some(ColorPreset::Named(String::from("srgb")));
        profile.monitor.rgb_gain.red = Some(ControlValue::Percent(100));
        let error = super::plan(&profile, &metadata(), UnsupportedPolicy::Skip).expect_err("conflict");
        assert!(matches!(error, PlanError::Conflict(_)));
    }

    #[test]
    fn request_mapping_is_only_valid_on_its_grid() {
        let mapping = RequestMapping {
            baseline: 3000,
            increment: 50,
        };
        assert_eq!(mapping.request_for(6500), Some(70));
        assert_eq!(mapping.request_for(6525), None);
        assert_eq!(mapping.kelvin_for(70), Some(6500));
        assert_eq!(mapping.request_for(2000), None);
    }
}
