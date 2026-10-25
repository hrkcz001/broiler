//! Bounded file logger.
//!
//! One append-only text file per user session in `%LOCALAPPDATA%\broiler\logs`. The file is
//! capped at [`MAX_LOG_BYTES`] and rotated once to `broiler.log.1` when it would exceed the
//! cap, so a long-running or crashing process cannot fill the disk. No telemetry, no network
//! output, no unbounded line buffering.
//!
//! The logger is deliberately synchronous and tiny: log writes happen on the calling thread
//! and never block a hardware transaction. Configuration of the level arrives with the
//! settings module in a later gate; `init` takes the level explicitly so that tests and the
//! tray can select it without reading a config file.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::Foundation::SYSTEMTIME;
use windows_sys::Win32::System::SystemInformation::GetLocalTime;

/// Hard cap for a single log file before rotation.
pub const MAX_LOG_BYTES: u64 = 1024 * 1024;

/// Maximum bytes of a single formatted log line, message included.
const MAX_LINE_BYTES: usize = 4096;

/// Severity levels, ordered from quietest to most verbose.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Off,
    Error,
    Warn,
    Info,
    Debug,
}

impl Level {
    /// Parse the level names used by the settings file.
    pub fn parse(text: &str) -> Option<Level> {
        match text.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Level::Off),
            "error" => Some(Level::Error),
            "warn" => Some(Level::Warn),
            "info" => Some(Level::Info),
            "debug" => Some(Level::Debug),
            _ => None,
        }
    }

    /// Stable name for files and diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Off => "off",
            Level::Error => "error",
            Level::Warn => "warn",
            Level::Info => "info",
            Level::Debug => "debug",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Level::Off => "off",
            Level::Error => "error",
            Level::Warn => "warn",
            Level::Info => "info",
            Level::Debug => "debug",
        }
    }
}

struct Sink {
    level: Level,
    path: PathBuf,
    file: Option<File>,
    size: u64,
}

impl Sink {
    fn open(level: Level, path: PathBuf) -> Sink {
        let mut sink = Sink {
            level,
            path,
            file: None,
            size: 0,
        };
        if level == Level::Off {
            return sink;
        }

        if let Some(parent) = sink.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        sink.rotate_if_needed(0);
        sink.reopen();
        sink
    }

    fn backup_path(path: &Path) -> PathBuf {
        let mut name = path.as_os_str().to_os_string();
        name.push(".1");
        PathBuf::from(name)
    }

    fn rotate_if_needed(&mut self, additional: u64) {
        let current = fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        if current + additional <= MAX_LOG_BYTES {
            self.size = current;
            return;
        }
        self.file = None;
        let _ = fs::rename(&self.path, Sink::backup_path(&self.path));
        self.size = 0;
        self.reopen();
        if self.file.is_none() {
            // A failed rotation must not lose the ability to log: truncate instead.
            if let Ok(file) = File::create(&self.path) {
                self.file = Some(file);
                self.size = 0;
            }
        }
    }

    fn reopen(&mut self) {
        if self.file.is_some() {
            return;
        }
        if let Ok(file) = OpenOptions::new().create(true).append(true).open(&self.path) {
            self.file = Some(file);
            self.size = fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        }
    }

    fn write_line(&mut self, level: Level, message: &str) {
        if self.level == Level::Off || level > self.level {
            return;
        }
        self.reopen();

        let mut line = format!("{} [{}] {}\n", timestamp(), level.label(), message);
        if line.len() > MAX_LINE_BYTES {
            // Keep the head of an oversized message rather than dropping the record.
            let mut cut = MAX_LINE_BYTES - 1;
            while cut > 0 && !line.is_char_boundary(cut) {
                cut -= 1;
            }
            line.truncate(cut);
            line.push('\n');
        }

        let bytes = line.as_bytes();
        if self.size + bytes.len() as u64 > MAX_LOG_BYTES {
            self.rotate_if_needed(bytes.len() as u64);
        }
        if let Some(file) = self.file.as_mut() {
            if file.write_all(bytes).is_ok() {
                self.size += bytes.len() as u64;
            }
        }
    }
}

static SINK: OnceLock<Mutex<Sink>> = OnceLock::new();

fn timestamp() -> String {
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
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond, st.wMilliseconds
    )
}

/// Install the logger. Later calls are ignored, so the first initialisation wins.
pub fn init(level: Level, path: PathBuf) {
    let _ = SINK.set(Mutex::new(Sink::open(level, path)));
}

/// Path currently written to, when the logger was initialised.
pub fn current_path() -> Option<PathBuf> {
    SINK.get().and_then(|sink| sink.lock().ok().map(|s| s.path.clone()))
}

/// Record one message at the given level.
pub fn log(level: Level, message: &str) {
    if let Some(sink) = SINK.get() {
        if let Ok(mut sink) = sink.lock() {
            sink.write_line(level, message);
        }
    }
}

#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {
        $crate::logging::log($crate::logging::Level::Error, &format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {
        $crate::logging::log($crate::logging::Level::Warn, &format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        $crate::logging::log($crate::logging::Level::Info, &format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_debug {
    ($($arg:tt)*) => {
        $crate::logging::log($crate::logging::Level::Debug, &format!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(name: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("broiler-logtest-{}-{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch directory");
        dir
    }

    #[test]
    fn level_names_round_trip() {
        for level in [Level::Off, Level::Error, Level::Warn, Level::Info, Level::Debug] {
            assert_eq!(Level::parse(level.as_str()), Some(level));
        }
        assert_eq!(Level::parse("verbose"), None);
    }

    #[test]
    fn level_filtering_is_ordered() {
        assert!(Level::Error < Level::Warn);
        assert!(Level::Warn < Level::Info);
        assert!(Level::Info < Level::Debug);
    }

    #[test]
    fn rotation_keeps_file_bounded() {
        let dir = scratch_dir("rotate");
        let path = dir.join("broiler.log");
        let mut sink = Sink::open(Level::Debug, path.clone());
        let chunk = "x".repeat(1024);
        for _ in 0..(MAX_LOG_BYTES / 1024 + 64) {
            sink.write_line(Level::Info, &chunk);
        }
        drop(sink);

        let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        assert!(size <= MAX_LOG_BYTES, "log file exceeded cap: {size}");
        assert!(Sink::backup_path(&path).exists(), "rotated file missing");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_line_is_truncated_not_dropped() {
        let dir = scratch_dir("truncate");
        let path = dir.join("broiler.log");
        let mut sink = Sink::open(Level::Debug, path.clone());
        sink.write_line(Level::Warn, &"y".repeat(MAX_LINE_BYTES * 4));
        drop(sink);

        let text = fs::read_to_string(&path).expect("log file");
        assert!(text.contains("[warn]"));
        assert!(text.len() <= MAX_LINE_BYTES + 32);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn off_level_writes_nothing() {
        let dir = scratch_dir("off");
        let path = dir.join("broiler.log");
        let mut sink = Sink::open(Level::Off, path.clone());
        sink.write_line(Level::Error, "must not appear");
        drop(sink);
        assert!(!path.exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
