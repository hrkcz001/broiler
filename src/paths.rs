//! Configuration and log locations.
//!
//! Roaming and local AppData are resolved through the Windows known-folder API, not
//! through the current directory or the executable's directory, so a moved executable
//! does not silently change where settings live.

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;

use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::UI::Shell::{SHGetKnownFolderPath, FOLDERID_LocalAppData, FOLDERID_RoamingAppData};

const KF_FLAG_DEFAULT: u32 = 0;

fn known_folder(id: &GUID) -> Option<PathBuf> {
    let mut raw: *mut u16 = std::ptr::null_mut();
    let result = unsafe {
        SHGetKnownFolderPath(
            id as *const GUID,
            KF_FLAG_DEFAULT,
            std::ptr::null_mut::<std::ffi::c_void>() as HANDLE,
            &mut raw,
        )
    };
    if result != 0 || raw.is_null() {
        return None;
    }

    let path = unsafe {
        let mut len = 0usize;
        while *raw.add(len) != 0 {
            len += 1;
        }
        let slice = std::slice::from_raw_parts(raw, len);
        let owned = OsString::from_wide(slice);
        CoTaskMemFree(raw as *mut std::ffi::c_void);
        PathBuf::from(owned)
    };
    Some(path)
}

fn env_path(name: &str) -> Option<PathBuf> {
    match std::env::var_os(name) {
        Some(value) if !value.is_empty() => Some(PathBuf::from(value)),
        _ => None,
    }
}

/// Roaming AppData, which holds settings and profiles.
pub fn roaming_app_data() -> Option<PathBuf> {
    known_folder(&FOLDERID_RoamingAppData).or_else(|| env_path("APPDATA"))
}

/// Local AppData, which holds state, capability evidence and logs.
pub fn local_app_data() -> Option<PathBuf> {
    known_folder(&FOLDERID_LocalAppData).or_else(|| env_path("LOCALAPPDATA"))
}

/// `%APPDATA%\broiler`
pub fn config_root() -> Option<PathBuf> {
    roaming_app_data().map(|base| base.join(crate::PRODUCT_NAME))
}

/// `%APPDATA%\broiler\profiles`
pub fn profiles_dir() -> Option<PathBuf> {
    config_root().map(|base| base.join("profiles"))
}

/// `%APPDATA%\broiler\settings.toml`
pub fn settings_file() -> Option<PathBuf> {
    config_root().map(|base| base.join("settings.toml"))
}

/// `%LOCALAPPDATA%\broiler`
pub fn local_root() -> Option<PathBuf> {
    local_app_data().map(|base| base.join(crate::PRODUCT_NAME))
}

/// `%LOCALAPPDATA%\broiler\logs`
pub fn log_dir() -> Option<PathBuf> {
    local_root().map(|base| base.join("logs"))
}

/// `%LOCALAPPDATA%\broiler\logs\broiler.log`
pub fn log_file() -> Option<PathBuf> {
    log_dir().map(|base| base.join("broiler.log"))
}
