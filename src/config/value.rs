//! Typed parameter values.
//!
//! Three kinds of value exist and are never interchangeable:
//!
//! * **percent** — a fraction of the monitor's own usable range, continuous controls only;
//! * **native** — one exact payload value for the control, validated against what the device
//!   reports (a range or an enumerated set) and never normalized, clamped or reinterpreted;
//! * **named intent** — a requested standard meaning such as `6500k` or `srgb`, resolved to one
//!   native value by the backend.
//!
//! TOML has no distinct hexadecimal runtime type, so `0x40` and `64` are the same integer. For a
//! continuous control a bare integer therefore means percent, even in hex; a native value always
//! requires the `raw` tag.

use std::fmt;

use toml_edit::{Item, Value};

/// A value written in a profile for one control.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlValue {
    /// 0..=100 percent of the device's reported usable range.
    Percent(u32),
    /// Exact native payload. Not universally one byte: standard non-table values can be 16 bits.
    Native(u64),
}

impl fmt::Display for ControlValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ControlValue::Percent(percent) => write!(formatter, "{percent}%"),
            ControlValue::Native(value) => write!(formatter, "native {value} (0x{value:X})"),
        }
    }
}

/// What a bare integer means for this control.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueKind {
    /// Continuous controls: a bare integer is a percentage.
    Continuous,
    /// Enumerated controls: a preset index is a choice, so a bare integer is the native value.
    Discrete,
}

/// Why a written value could not be interpreted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// The field is neither an integer, a string nor a table.
    NotAValue,
    /// A percentage outside 0..=100.
    PercentOutOfRange(i64),
    /// `percent` and `raw` in one table, or a unit key next to another unit key.
    MixedUnits,
    /// A single-key table with an unknown unit name.
    UnknownUnit(String),
    /// A native value that is negative or does not fit the payload model.
    NativeOutOfRange(i64),
    /// An empty table, which specifies nothing.
    EmptyTable,
    /// A string where a value was expected, for a control that has no named vocabulary.
    UnexpectedString(String),
    /// A boolean or float for an integer field.
    WrongType,
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::NotAValue => write!(formatter, "expected an integer or a value table"),
            ParseError::PercentOutOfRange(value) => {
                write!(formatter, "percentage {value} is outside 0..=100")
            }
            ParseError::MixedUnits => write!(
                formatter,
                "exactly one unit is allowed: use either percent or raw, never both"
            ),
            ParseError::UnknownUnit(unit) => write!(
                formatter,
                "unknown unit `{unit}`: allowed unit keys are percent, raw, kelvin and vcp"
            ),
            ParseError::NativeOutOfRange(value) => {
                write!(formatter, "native value {value} is not a valid payload")
            }
            ParseError::EmptyTable => write!(formatter, "value table is empty"),
            ParseError::UnexpectedString(value) => {
                write!(formatter, "`{value}` is not a named value for this control")
            }
            ParseError::WrongType => write!(formatter, "expected an integer, not a boolean or float"),
        }
    }
}

fn integer_of(value: &Value) -> Option<i64> {
    value.as_integer()
}

fn percent_from(raw: i64) -> Result<ControlValue, ParseError> {
    if !(0..=100).contains(&raw) {
        return Err(ParseError::PercentOutOfRange(raw));
    }
    Ok(ControlValue::Percent(raw as u32))
}

fn native_from(raw: i64) -> Result<ControlValue, ParseError> {
    if raw < 0 {
        return Err(ParseError::NativeOutOfRange(raw));
    }
    Ok(ControlValue::Native(raw as u64))
}

/// Interpret one profile field.
pub fn parse_control_value(item: &Item, kind: ValueKind) -> Result<ControlValue, ParseError> {
    match item {
        Item::Value(Value::Integer(integer)) => {
            let raw = *integer.value();
            match kind {
                ValueKind::Continuous => percent_from(raw),
                ValueKind::Discrete => native_from(raw),
            }
        }
        Item::Value(Value::String(text)) => Err(ParseError::UnexpectedString(text.value().clone())),
        Item::Value(Value::Boolean(_)) | Item::Value(Value::Float(_)) => Err(ParseError::WrongType),
        Item::Value(Value::InlineTable(table)) => {
            if table.is_empty() {
                return Err(ParseError::EmptyTable);
            }
            let mut percent: Option<i64> = None;
            let mut raw: Option<i64> = None;
            for (key, value) in table.iter() {
                let integer = integer_of(value).ok_or(ParseError::NotAValue)?;
                match key {
                    "percent" => percent = Some(integer),
                    "raw" => raw = Some(integer),
                    other => return Err(ParseError::UnknownUnit(other.to_string())),
                }
            }
            match (percent, raw) {
                (Some(_), Some(_)) => Err(ParseError::MixedUnits),
                (Some(value), None) => percent_from(value),
                (None, Some(value)) => native_from(value),
                (None, None) => Err(ParseError::EmptyTable),
            }
        }
        Item::Table(table) => {
            if table.is_empty() {
                return Err(ParseError::EmptyTable);
            }
            let mut percent: Option<i64> = None;
            let mut raw: Option<i64> = None;
            for (key, item) in table.iter() {
                let value = item.as_value().ok_or(ParseError::NotAValue)?;
                let integer = integer_of(value).ok_or(ParseError::NotAValue)?;
                match key {
                    "percent" => percent = Some(integer),
                    "raw" => raw = Some(integer),
                    other => return Err(ParseError::UnknownUnit(other.to_string())),
                }
            }
            match (percent, raw) {
                (Some(_), Some(_)) => Err(ParseError::MixedUnits),
                (Some(value), None) => percent_from(value),
                (None, Some(value)) => native_from(value),
                (None, None) => Err(ParseError::EmptyTable),
            }
        }
        _ => Err(ParseError::NotAValue),
    }
}

/// What the device reports about one control's acceptable values.
///
/// The classification is a property of the monitor discovered at runtime, never something the
/// configuration author declares. `0x14` is enumerated on one display and could be a range on
/// another, and the same `{ raw = N }` syntax has to work in both cases.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceValueSupport {
    /// Continuous control with the device's own minimum and maximum.
    Range { minimum: u64, maximum: u64 },
    /// Enumerated control with an explicit set of accepted values.
    Enumerated { values: Vec<u64> },
    /// Positively not offered by this display.
    Unsupported,
    /// Present but unavailable in the current picture or HDR mode.
    BlockedByMode,
    /// No reliable evidence either way: native values may be forwarded exactly, percentages may not
    /// be mapped.
    Unknown,
}

/// Why a value could not be resolved against device metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolveError {
    /// A preset index is a choice, not a point on a percentage scale.
    PercentOnEnumerated,
    /// A percentage cannot be mapped without a reported range.
    PercentOnUnknownMetadata,
    /// The native value is outside the reported range. Never clamped.
    NativeOutOfRange { value: u64, minimum: u64, maximum: u64 },
    /// The native value is not one of the values the device accepts.
    NativeNotInSet { value: u64, values: Vec<u64> },
    Unsupported,
    BlockedByMode,
}

impl fmt::Display for ResolveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResolveError::PercentOnEnumerated => write!(
                formatter,
                "this control is a choice list; percentages do not apply, use a native value"
            ),
            ResolveError::PercentOnUnknownMetadata => write!(
                formatter,
                "the device did not report a range for this control, so a percentage cannot be mapped"
            ),
            ResolveError::NativeOutOfRange {
                value,
                minimum,
                maximum,
            } => write!(
                formatter,
                "native value {value} is outside the reported range {minimum}..={maximum}"
            ),
            ResolveError::NativeNotInSet { value, values } => {
                let listed: Vec<String> = values.iter().map(|item| format!("{item:#04X}")).collect();
                write!(
                    formatter,
                    "native value {value:#04X} is not one of the accepted values: {}",
                    listed.join(", ")
                )
            }
            ResolveError::Unsupported => write!(formatter, "the display does not offer this control"),
            ResolveError::BlockedByMode => write!(
                formatter,
                "the control is unavailable in the current picture or HDR mode"
            ),
        }
    }
}

/// Map a percentage onto the device's own minimum and maximum, rounding half up.
///
/// The minimum is never assumed to be zero: high-level Dxva2 getters report their own minimum and
/// it varies between models.
pub fn percent_to_native(percent: u32, minimum: u64, maximum: u64) -> u64 {
    if maximum <= minimum {
        return minimum;
    }
    let span = (maximum - minimum) as u128;
    let offset = (percent as u128 * span + 50) / 100;
    minimum + offset as u64
}

/// Resolve a written value to the exact native value that will be sent.
pub fn resolve(value: ControlValue, support: &DeviceValueSupport) -> Result<u64, ResolveError> {
    match (value, support) {
        (_, DeviceValueSupport::Unsupported) => Err(ResolveError::Unsupported),
        (_, DeviceValueSupport::BlockedByMode) => Err(ResolveError::BlockedByMode),
        (ControlValue::Native(raw), DeviceValueSupport::Enumerated { values }) => {
            if values.contains(&raw) {
                Ok(raw)
            } else {
                Err(ResolveError::NativeNotInSet {
                    value: raw,
                    values: values.clone(),
                })
            }
        }
        (ControlValue::Native(raw), DeviceValueSupport::Range { minimum, maximum }) => {
            if raw >= *minimum && raw <= *maximum {
                Ok(raw)
            } else {
                Err(ResolveError::NativeOutOfRange {
                    value: raw,
                    minimum: *minimum,
                    maximum: *maximum,
                })
            }
        }
        (ControlValue::Native(raw), DeviceValueSupport::Unknown) => Ok(raw),
        (ControlValue::Percent(_), DeviceValueSupport::Enumerated { .. }) => {
            Err(ResolveError::PercentOnEnumerated)
        }
        (ControlValue::Percent(_), DeviceValueSupport::Unknown) => {
            Err(ResolveError::PercentOnUnknownMetadata)
        }
        (ControlValue::Percent(percent), DeviceValueSupport::Range { minimum, maximum }) => {
            Ok(percent_to_native(percent, *minimum, *maximum))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use toml_edit::DocumentMut;

    fn field(text: &str) -> Item {
        let document: DocumentMut = format!("value = {text}").parse().expect("valid toml");
        document["value"].clone()
    }

    #[test]
    fn bare_integer_is_percent_for_continuous_controls() {
        assert_eq!(
            parse_control_value(&field("35"), ValueKind::Continuous),
            Ok(ControlValue::Percent(35))
        );
    }

    #[test]
    fn hexadecimal_bare_integer_is_still_percent() {
        // 0x40 is 64, and 64 percent is what a continuous control means by it.
        assert_eq!(
            parse_control_value(&field("0x40"), ValueKind::Continuous),
            Ok(ControlValue::Percent(64))
        );
    }

    #[test]
    fn bare_integer_is_native_for_discrete_controls() {
        assert_eq!(
            parse_control_value(&field("0x0B"), ValueKind::Discrete),
            Ok(ControlValue::Native(11))
        );
    }

    #[test]
    fn raw_table_is_native_and_hex_equals_decimal() {
        assert_eq!(
            parse_control_value(&field("{ raw = 0xFF }"), ValueKind::Continuous),
            Ok(ControlValue::Native(255))
        );
        assert_eq!(
            parse_control_value(&field("{ raw = 255 }"), ValueKind::Continuous),
            parse_control_value(&field("{ raw = 0xFF }"), ValueKind::Continuous)
        );
    }

    #[test]
    fn native_values_above_255_are_preserved_exactly() {
        assert_eq!(
            parse_control_value(&field("{ raw = 0xFFFF }"), ValueKind::Continuous),
            Ok(ControlValue::Native(65535))
        );
    }

    #[test]
    fn percent_table_is_equivalent_to_a_bare_integer() {
        assert_eq!(
            parse_control_value(&field("{ percent = 35 }"), ValueKind::Continuous),
            Ok(ControlValue::Percent(35))
        );
    }

    #[test]
    fn percent_and_raw_together_are_rejected() {
        assert_eq!(
            parse_control_value(&field("{ percent = 35, raw = 0x40 }"), ValueKind::Continuous),
            Err(ParseError::MixedUnits)
        );
    }

    #[test]
    fn percentages_outside_zero_to_hundred_are_rejected() {
        assert_eq!(
            parse_control_value(&field("101"), ValueKind::Continuous),
            Err(ParseError::PercentOutOfRange(101))
        );
        assert_eq!(
            parse_control_value(&field("-1"), ValueKind::Continuous),
            Err(ParseError::PercentOutOfRange(-1))
        );
    }

    #[test]
    fn unknown_units_and_wrong_types_are_rejected() {
        assert_eq!(
            parse_control_value(&field("{ native = 5 }"), ValueKind::Continuous),
            Err(ParseError::UnknownUnit(String::from("native")))
        );
        assert_eq!(
            parse_control_value(&field("false"), ValueKind::Continuous),
            Err(ParseError::WrongType)
        );
        assert_eq!(
            parse_control_value(&field("{ }"), ValueKind::Continuous),
            Err(ParseError::EmptyTable)
        );
    }

    #[test]
    fn percent_mapping_respects_a_nonzero_minimum() {
        assert_eq!(percent_to_native(0, 20, 100), 20);
        assert_eq!(percent_to_native(100, 20, 100), 100);
        assert_eq!(percent_to_native(50, 20, 100), 60);
    }

    #[test]
    fn percent_mapping_rounds_half_up() {
        // 1..=3 span two: 50 percent is exactly 2, 25 percent is 1.5 -> 2.
        assert_eq!(percent_to_native(25, 1, 3), 2);
        assert_eq!(percent_to_native(75, 1, 3), 3);
        assert_eq!(percent_to_native(50, 1, 3), 2);
    }

    #[test]
    fn percent_mapping_handles_a_degenerate_range() {
        assert_eq!(percent_to_native(50, 7, 7), 7);
    }

    #[test]
    fn native_values_are_validated_against_a_range_without_clamping() {
        let support = DeviceValueSupport::Range {
            minimum: 0,
            maximum: 100,
        };
        assert_eq!(resolve(ControlValue::Native(100), &support), Ok(100));
        assert_eq!(
            resolve(ControlValue::Native(101), &support),
            Err(ResolveError::NativeOutOfRange {
                value: 101,
                minimum: 0,
                maximum: 100
            })
        );
    }

    #[test]
    fn native_values_are_validated_against_an_enumerated_set() {
        let support = DeviceValueSupport::Enumerated {
            values: vec![0x01, 0x02, 0x05, 0x08],
        };
        assert_eq!(resolve(ControlValue::Native(5), &support), Ok(5));
        assert!(matches!(
            resolve(ControlValue::Native(6), &support),
            Err(ResolveError::NativeNotInSet { .. })
        ));
    }

    #[test]
    fn percentages_are_rejected_for_enumerated_and_unknown_controls() {
        assert_eq!(
            resolve(
                ControlValue::Percent(50),
                &DeviceValueSupport::Enumerated { values: vec![1, 2] }
            ),
            Err(ResolveError::PercentOnEnumerated)
        );
        assert_eq!(
            resolve(ControlValue::Percent(50), &DeviceValueSupport::Unknown),
            Err(ResolveError::PercentOnUnknownMetadata)
        );
    }

    #[test]
    fn native_values_are_forwarded_when_metadata_is_unknown() {
        assert_eq!(
            resolve(ControlValue::Native(0x2345), &DeviceValueSupport::Unknown),
            Ok(0x2345)
        );
    }

    #[test]
    fn unsupported_and_blocked_metadata_are_distinct_from_unknown() {
        assert_eq!(
            resolve(ControlValue::Native(1), &DeviceValueSupport::Unsupported),
            Err(ResolveError::Unsupported)
        );
        assert_eq!(
            resolve(ControlValue::Native(1), &DeviceValueSupport::BlockedByMode),
            Err(ResolveError::BlockedByMode)
        );
    }
}
