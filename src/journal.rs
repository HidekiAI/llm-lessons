//! Journal-grade logging to stderr, off by default.
//!
//! Each step that touches a DB file reports what it did (path, version, counts, decision), so a
//! run can be retraced from stderr alone. It is off by default because the Claude Code hook's
//! output is read by Claude Code; it is turned on with `--verbose` or a non-empty `LESSONS_LOG`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Called once from `main` before any command runs.
pub fn init(verbose_flag: bool) {
    let env_requested: bool =
        std::env::var_os("LESSONS_LOG").is_some_and(|value| !value.is_empty());
    ENABLED.store(verbose_flag || env_requested, Ordering::Relaxed); // set once before any reader, so no ordering is needed
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Milliseconds since the Unix epoch, so log lines from the CLI and the hook can be interleaved by time.
pub fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since_epoch| since_epoch.as_millis()) // a clock before 1970 logs 0 rather than failing the command
}

/// `journal!("open {}: ...", path.display())` writes one timestamped line to stderr when enabled.
#[macro_export]
macro_rules! journal {
    ($($arg:tt)*) => {
        if $crate::journal::enabled() {
            eprintln!("lessons [{}] {}", $crate::journal::unix_millis(), format_args!($($arg)*));
        }
    };
}
