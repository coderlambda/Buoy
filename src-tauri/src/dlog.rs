//! Debug logging to /tmp/dt-debug.log, plus stderr. OPT-IN: silent unless DT_DEBUG=1, so it never
//! writes on the hot path in normal use (always-on logging flooded the file during a TUI's
//! constant redraws). Enable with DT_DEBUG=1 when diagnosing.

use std::io::Write;
use std::sync::atomic::{AtomicU8, Ordering};

const LOG_PATH: &str = "/tmp/dt-debug.log";

// 0 = uninitialized, 1 = off, 2 = on. Cached so we don't hit env on every call.
static ENABLED: AtomicU8 = AtomicU8::new(0);

pub fn enabled() -> bool {
    match ENABLED.load(Ordering::Relaxed) {
        1 => false,
        2 => true,
        _ => {
            let on = std::env::var("DT_DEBUG").as_deref() == Ok("1");
            ENABLED.store(if on { 2 } else { 1 }, Ordering::Relaxed);
            on
        }
    }
}

pub fn log(msg: &str) {
    if !enabled() {
        return;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let line = format!("{} [DT rs] {}\n", ts, msg);
    let _ = std::io::stderr().write_all(line.as_bytes());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(LOG_PATH) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// `dlog!("fmt", args...)` — formats and appends ONLY when logging is enabled, so the (possibly
/// expensive `{:?}`) formatting is skipped entirely on the hot path in normal use.
#[macro_export]
macro_rules! dlog {
    ($($arg:tt)*) => {
        if $crate::dlog::enabled() {
            $crate::dlog::log(&format!($($arg)*));
        }
    };
}
