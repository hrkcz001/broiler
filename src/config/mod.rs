//! Configuration: typed values, profiles, the profile registry and global settings.
//!
//! All user-facing configuration is UTF-8 TOML. Parsing is strict; saving preserves comments and is
//! atomic. Nothing in this module touches hardware.

pub mod profile;
pub mod registry;
pub mod settings;
pub mod value;

pub use profile::{ColorPreset, ColorTemperature, Diagnostic, Profile, ProfileError};
pub use registry::{ProfileRegistry, RegistryError};
pub use settings::{LeftClick, Settings, SettingsError, UnsupportedPolicy};
pub use value::{ControlValue, DeviceValueSupport, ValueKind};
