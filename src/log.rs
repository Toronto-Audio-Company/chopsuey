//! chopsuey's log file: `~/Library/Logs/chopsuey/chopsuey.log` (Console.app
//! shows it under Log Reports). A plugin's stderr goes nowhere a user can
//! find, so warnings, failures and panics (with a backtrace) are written
//! here, for users to send with a bug report.
//!
//! Never call this from the audio thread: it takes a lock and writes a file.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

/// The log is moved to `chopsuey.log.1` once it reaches this size, so it
/// never grows without bound (one old file is kept).
const MAX_BYTES: u64 = 1_000_000;

static LOCK: Mutex<()> = Mutex::new(());

/// Writes one line to the log: `logln!("couldn't read {}: {e}", path.display())`.
/// Debug builds also print it to stderr.
#[macro_export]
macro_rules! logln {
    ($($arg:tt)*) => { $crate::log::write(&format!($($arg)*)) };
}

pub fn dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Logs/chopsuey"))
}

pub fn path() -> Option<PathBuf> {
    Some(dir()?.join("chopsuey.log"))
}

/// Appends `message` with a UTC timestamp, the process id and the thread.
/// Failures are ignored: logging must never be the thing that breaks.
pub fn write(message: &str) {
    let thread = std::thread::current();
    let line = format!(
        "{} [{}] {} {}: {}\n",
        timestamp(),
        std::process::id(),
        env!("CARGO_PKG_VERSION"),
        thread.name().unwrap_or("?"),
        message.trim_end()
    );
    if cfg!(debug_assertions) || cfg!(test) || std::env::var_os("CHOPSUEY_LOG_STDERR").is_some() {
        eprint!("{line}");
    }
    if cfg!(test) {
        return; // tests don't write to the user's log
    }
    let Some(path) = path() else { return };
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if std::fs::metadata(&path).is_ok_and(|m| m.len() >= MAX_BYTES) {
        let _ = std::fs::rename(&path, path.with_extension("log.1"));
    }
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = file.write_all(line.as_bytes());
    }
}

/// "2026-11-25 20:41:07Z", from the system clock.
fn timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs()) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Days since 1970-01-01 to a (year, month, day) date (Howard Hinnant's
/// algorithm), so the log needs no date crate.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(20_782), (2026, 11, 25));
    }
}
