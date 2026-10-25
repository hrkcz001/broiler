//! Minimal read-only registry access.
//!
//! Only used to read the EDID blob that Windows already cached under the monitor's device
//! enumeration key. Nothing is written, and no value is trusted for anything except identity
//! evidence: a registry copy of an EDID can outlive the physical connection.

use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE,
};

use super::wide::to_wide;

struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        unsafe { RegCloseKey(self.0) };
    }
}

fn open(path: &str) -> Option<Key> {
    let wide = to_wide(path);
    let mut handle: HKEY = std::ptr::null_mut();
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            wide.as_ptr(),
            0,
            KEY_QUERY_VALUE,
            &mut handle,
        )
    };
    if status != ERROR_SUCCESS || handle.is_null() {
        return None;
    }
    Some(Key(handle))
}

/// Read a `REG_BINARY` value. Returns `None` when the key, the value or its type is missing.
pub fn read_binary(key_path: &str, value_name: &str) -> Option<Vec<u8>> {
    let key = open(key_path)?;
    let name = to_wide(value_name);

    let mut size: u32 = 0;
    let status = unsafe {
        RegQueryValueExW(
            key.0,
            name.as_ptr(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if status != ERROR_SUCCESS || size == 0 || size > 64 * 1024 {
        return None;
    }

    let mut buffer = vec![0u8; size as usize];
    let mut value_type: u32 = 0;
    let status = unsafe {
        RegQueryValueExW(
            key.0,
            name.as_ptr(),
            std::ptr::null(),
            &mut value_type,
            buffer.as_mut_ptr(),
            &mut size,
        )
    };
    // REG_BINARY == 3, REG_NONE == 0. Some drivers store EDID as REG_NONE.
    if status != ERROR_SUCCESS || (value_type != 3 && value_type != 0) {
        return None;
    }
    buffer.truncate(size as usize);
    Some(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_key_is_not_an_error_value() {
        assert!(read_binary(
            "SYSTEM\\CurrentControlSet\\Enum\\DISPLAY\\BROILER-NOT-A-DEVICE\\0",
            "EDID"
        )
        .is_none());
    }

    #[test]
    fn real_enum_key_without_the_value_returns_none() {
        let path = "SYSTEM\\CurrentControlSet\\Enum\\DISPLAY";
        assert!(read_binary(path, "EDID").is_none());
    }
}
