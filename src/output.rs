//! Console output that stays silent when there is no console.
//!
//! `broiler-tray.exe` is a GUI-subsystem executable. Windows does not give it a console,
//! so anything it prints would either be lost or, worse, force the operating system to
//! allocate one. The rule used here: attach to an *existing* parent console when standard
//! output is not usable, and otherwise print nothing at all. Standard handles are read
//! after that decision, so a redirected pipe keeps working.

use std::io::Write;

use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::System::Console::{
    AttachConsole, GetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
};

/// True when the process has a usable standard handle for the given stream.
fn handle_is_usable(std_handle: u32) -> bool {
    let handle = unsafe { GetStdHandle(std_handle) };
    !handle.is_null() && handle as isize != INVALID_HANDLE_VALUE as isize
}

/// Attach to the parent console when this process has no usable standard handles.
///
/// Returns true when at least one stream can be written to afterwards. A GUI-subsystem
/// process started from Explorer has no parent console and simply returns false.
pub fn attach_console_if_needed() -> bool {
    let out_usable = handle_is_usable(STD_OUTPUT_HANDLE);
    let err_usable = handle_is_usable(STD_ERROR_HANDLE);
    if out_usable || err_usable {
        return true;
    }

    let attached = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
    if attached == 0 {
        return false;
    }
    handle_is_usable(STD_OUTPUT_HANDLE) || handle_is_usable(STD_ERROR_HANDLE)
}

/// Write one line to stdout. Silently does nothing when stdout is unavailable.
pub fn line(text: &str) {
    let mut out = std::io::stdout();
    let _ = out.write_all(text.as_bytes());
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

/// Write one line to stderr. Silently does nothing when stderr is unavailable.
pub fn error_line(text: &str) {
    let mut err = std::io::stderr();
    let _ = err.write_all(text.as_bytes());
    let _ = err.write_all(b"\n");
    let _ = err.flush();
}
