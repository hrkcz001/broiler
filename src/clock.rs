//! Local timestamps for logs and evidence records.
//!
//! Timestamps are informational evidence, never ordering keys: the coordinator sequences
//! operations itself. Formatting is fixed-width so log lines and JSON fields sort visually.

use windows_sys::Win32::Foundation::SYSTEMTIME;
use windows_sys::Win32::System::SystemInformation::GetLocalTime;

fn local_time() -> SYSTEMTIME {
    let mut st = SYSTEMTIME {
        wYear: 0,
        wMonth: 0,
        wDayOfWeek: 0,
        wDay: 0,
        wHour: 0,
        wMinute: 0,
        wSecond: 0,
        wMilliseconds: 0,
    };
    unsafe { GetLocalTime(&mut st) };
    st
}

/// `YYYY-MM-DD HH:MM:SS.mmm` in local time, without a UTC offset.
pub fn timestamp() -> String {
    let st = local_time();
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond, st.wMilliseconds
    )
}

/// Local wall-clock timestamp in ISO-8601 shape for evidence fields.
///
/// No UTC offset is appended: the value is local time and claiming `Z` or a fixed offset
/// would be a lie whenever the machine's time zone changes.
pub fn iso_local() -> String {
    let st = local_time();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond, st.wMilliseconds
    )
}

/// Milliseconds since an arbitrary monotonic start point of this process.
pub fn elapsed_ms_since(start: std::time::Instant) -> u64 {
    start.elapsed().as_millis().min(u64::MAX as u128) as u64
}
