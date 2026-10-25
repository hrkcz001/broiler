//! broiler: a native Windows DDC/CI monitor controller.
//!
//! Two executables share this crate:
//!
//! * `broiler.exe` — console CLI.
//! * `broiler-tray.exe` — GUI-subsystem user-session process that owns the tray,
//!   the native windows and the coordinator. Its `--internal-ddc-worker` mode is the
//!   isolated process that performs the blocking Dxva2 calls.
//!
//! No async runtime, no service, no elevation, no WebView. Hardware writes exist only
//! behind the single coordinator and its child worker.

pub mod backend;
pub mod caps;
pub mod cli;
pub mod clock;
pub mod config;
pub mod coordinator;
pub mod daemon;
pub mod discovery;
pub mod instance;
pub mod ipc;
pub mod pipe;
pub mod lifecycle;
pub mod logging;
pub mod migrate;
pub mod model;
pub mod output;
pub mod packaging;
pub mod paths;
pub mod plan;
pub mod runtime;
pub mod storage;
pub mod supervisor;
pub mod transport;
pub mod win;
pub mod worker;

/// Product name as used in user-visible strings.
pub const PRODUCT_NAME: &str = "broiler";

/// Version of this build, from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
