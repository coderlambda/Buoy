//! Debug logging to /tmp/dt-debug.log (same file the Electron build used), plus stderr.
//! Always on; set DT_DEBUG=0 to silence. Line-appended and self-timestamped (millis since epoch
//! — we can't assume a clock crate, so a monotonic-ish counter from SystemTime is fine here).

use std::io::Write;

const LOG_PATH: &str = "/tmp/dt-debug.log";

pub fn log(msg: &str) {
    if std::env::var("DT_DEBUG").as_deref() == Ok("0") {
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

/// `dlog!("fmt", args...)` — formats then appends.
#[macro_export]
macro_rules! dlog {
    ($($arg:tt)*) => {
        $crate::dlog::log(&format!($($arg)*))
    };
}
