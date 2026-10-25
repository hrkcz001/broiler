//! Per-logon single-instance protection.
//!
//! One coordinator may own the hardware per logon session. The mutex name is derived from the same
//! logon-SID token as the endpoint, so a second `broiler start` in the same session detects the
//! existing owner instead of becoming a competing writer, while another terminal session is a
//! different logon SID and is unaffected.

use std::ffi::c_void;
use std::io;

use windows_sys::Win32::Foundation::{
    GetLastError, ERROR_ALREADY_EXISTS, HANDLE,
};
use windows_sys::Win32::System::Threading::CreateMutexW;

use crate::pipe::Endpoint;
use crate::win::process::Handle;

/// Owns the instance mutex for as long as it lives.
pub struct InstanceLock {
    _handle: Handle,
}

impl InstanceLock {
    /// Acquire the lock. The boolean is true when another process already holds it.
    pub fn acquire(endpoint: &Endpoint) -> io::Result<(InstanceLock, bool)> {
        let token = endpoint
            .name
            .rsplit('.')
            .next()
            .unwrap_or("unknown")
            .to_string();
        let name = format!("Local\\broiler.v1.instance.{token}");
        let mut wide: Vec<u16> = name.encode_utf16().collect();
        wide.push(0);

        let handle: HANDLE =
            unsafe { CreateMutexW(std::ptr::null_mut::<c_void>() as *const _, 0, wide.as_ptr()) };
        let handle = Handle::from_raw(handle);
        if !handle.is_valid() {
            let code = unsafe { GetLastError() };
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("CreateMutexW failed with Win32 error {code}"),
            ));
        }
        let already_running = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        Ok((InstanceLock { _handle: handle }, already_running))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipe::local_endpoint;

    #[test]
    fn the_second_acquisition_reports_an_existing_owner() {
        let endpoint = local_endpoint().expect("endpoint");
        let (first, already) = InstanceLock::acquire(&endpoint).expect("first lock");
        assert!(!already, "the first acquisition must not see an owner");
        let (_second, already) = InstanceLock::acquire(&endpoint).expect("second lock");
        assert!(already, "the second acquisition must see the first owner");
        drop(first);
    }
}
