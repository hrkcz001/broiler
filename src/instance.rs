//! Per-logon single-instance protection.
//!
//! One coordinator may own the hardware per logon session. The mutex name is derived from the whole
//! endpoint name, which carries the logon-SID token, so a second `broiler start` in the same session
//! detects the existing owner instead of becoming a competing writer, while another terminal session
//! is a different logon SID and is unaffected.
//!
//! Deriving the mutex from the complete endpoint name, rather than from one parsed segment of it,
//! keeps the invariant exact: one endpoint, one owner, and no two different endpoints sharing a lock.

use std::ffi::c_void;
use std::io;

use sha2::{Digest, Sha256};
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

/// The mutex that guards one endpoint. A hash keeps the name short enough for the kernel object
/// namespace while staying a pure function of the endpoint.
fn mutex_name(endpoint: &Endpoint) -> String {
    let digest = Sha256::digest(endpoint.name.as_bytes());
    let token: String = digest.iter().take(8).map(|byte| format!("{byte:02x}")).collect();
    format!("Local\\broiler.v1.instance.{token}")
}

impl InstanceLock {
    /// Acquire the lock. The boolean is true when another process already holds it.
    pub fn acquire(endpoint: &Endpoint) -> io::Result<(InstanceLock, bool)> {
        let name = mutex_name(endpoint);
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
    use crate::pipe::{fixture, local_endpoint};

    /// A fixture endpoint: its own pipe name and therefore its own mutex, so a resident instance the
    /// developer happens to be running cannot answer this test's questions.
    fn isolated(label: &str) -> Endpoint {
        let name = fixture::namespace(label);
        fixture::with_namespace(&name, || local_endpoint().expect("endpoint"))
    }

    #[test]
    fn the_second_acquisition_reports_an_existing_owner() {
        let endpoint = isolated("instance");
        let (first, already) = InstanceLock::acquire(&endpoint).expect("first lock");
        assert!(!already, "the first acquisition must not see an owner");
        let (_second, already) = InstanceLock::acquire(&endpoint).expect("second lock");
        assert!(already, "the second acquisition must see the first owner");
        drop(first);
    }

    #[test]
    fn two_endpoints_do_not_share_one_lock() {
        // The defect this guards: a mutex named from one parsed segment of the endpoint let two
        // different endpoints collide, so a fixture could look like the running owner.
        let first = isolated("instance-a");
        let second = isolated("instance-b");
        assert_ne!(mutex_name(&first), mutex_name(&second));
        let (_held, occupied) = InstanceLock::acquire(&first).expect("first");
        assert!(!occupied);
        let (_other, occupied) = InstanceLock::acquire(&second).expect("second");
        assert!(!occupied, "an unrelated endpoint must not appear to own this one");
    }

    #[test]
    fn a_fixture_endpoint_is_never_the_production_one() {
        let production = local_endpoint().expect("production endpoint");
        let fixture_endpoint = isolated("instance-production");
        assert_ne!(production.name, fixture_endpoint.name);
        assert!(fixture_endpoint.name.contains(".fixture."));
        assert_ne!(mutex_name(&production), mutex_name(&fixture_endpoint));
    }
}
