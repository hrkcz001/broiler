//! Native Windows access.
//!
//! Everything here is a narrow safe wrapper around `windows-sys` calls: handles are owned by
//! RAII types, reading a wide string always stops at the first NUL, and no function in this
//! module performs a monitor write. Writes arrive in a later gate, behind one serialized
//! executor.

pub mod ddc;
pub mod edid;
pub mod process;
pub mod registry;
pub mod topology;
pub mod flyout;
pub mod settings;
pub mod ui;
pub mod wide;
