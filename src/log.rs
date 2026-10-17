//! A small append-only log at ~/.local/state/plext/plext.log, so problems that
//! flash by in the UI (a skipped track, a dropped connection) can be read later.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_BYTES: u64 = 1024 * 1024;

static LOCK: Mutex<()> = Mutex::new(());

pub fn path() -> Option<PathBuf> {
    let base = dirs::state_dir().or_else(dirs::cache_dir)?;
    Some(base.join("plext").join("plext.log"))
}

/// Appends a timestamped line. Never fails loudly: logging must not break playback.
pub fn write(msg: &str) {
    if cfg!(test) {
        return;
    }
    let _guard = LOCK.lock();
    let Some(p) = path() else { return };
    if let Some(dir) = p.parent() {
        let _ = fs::create_dir_all(dir);
    }
    // Keep the log small: roll it over to plext.log.old at 1 MB.
    if fs::metadata(&p).map(|m| m.len() > MAX_BYTES).unwrap_or(false) {
        let _ = fs::rename(&p, p.with_extension("log.old"));
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&p) {
        let _ = writeln!(f, "{} {}", timestamp(), msg);
    }
}

/// UTC time as "2026-10-17 21:43:54Z".
fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}Z",
        y,
        m,
        d,
        rem / 3600,
        (rem / 60) % 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn timestamp_format() {
        let t = super::timestamp();
        assert_eq!(t.len(), 20, "{}", t);
        assert!(t.starts_with("20"), "{}", t);
    }
}
