//! The memwatch log (docs/design.md §8.2): one line per alert, failed cycle
//! or state change, with the time in UTC.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Logger {
    /// `Box<dyn Write>`: "something you can write to", whether stderr or a file.
    out: Box<dyn Write>,
}

impl Logger {
    /// For the terminal: writes to stderr, which is unbuffered.
    pub fn stderr() -> Logger {
        Logger {
            out: Box::new(io::stderr()),
        }
    }

    /// For launchd: appends to the end of `path`. If the file is already over
    /// `max_bytes`, it first renames it to `<path>.old` (overwriting the previous
    /// one), so the log never takes more than ~2 × `max_bytes`.
    pub fn file(path: &Path, max_bytes: u64) -> io::Result<Logger> {
        if let Ok(metadata) = fs::metadata(path)
            && metadata.len() > max_bytes
        {
            let mut old = path.as_os_str().to_owned();
            old.push(".old");
            fs::rename(path, old)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Logger {
            out: Box::new(file),
        })
    }

    pub fn line(&mut self, message: &str) {
        // If the log cannot be written, there is no one to tell: it is ignored.
        let _ = writeln!(self.out, "{} {message}", utc(now_secs()));
    }
}

/// Seconds since 1970 (wall-clock time, NOT monotonic: only for display).
pub fn now_secs() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => elapsed.as_secs(),
        Err(_) => 0,
    }
}

/// "2026-11-17T21:40:03Z". No date crates: converts days since 1970 to
/// year/month/day with Howard Hinnant's `civil_from_days` algorithm.
pub fn utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rest = secs % 86_400;

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153; // March = 0
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + if month <= 2 { 1 } else { 0 };

    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_epoch() {
        assert_eq!(utc(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn a_leap_day() {
        assert_eq!(utc(951_782_400), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn a_known_instant() {
        assert_eq!(utc(1_700_000_000), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn the_file_is_rotated_when_too_big() {
        let dir = std::env::temp_dir().join(format!("memwatch-log-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("memwatch.log");
        fs::write(&path, "x".repeat(100)).unwrap();

        let mut logger = Logger::file(&path, 50).unwrap();
        logger.line("hello");
        drop(logger);

        assert_eq!(
            fs::read_to_string(dir.join("memwatch.log.old"))
                .unwrap()
                .len(),
            100
        );
        assert!(fs::read_to_string(&path).unwrap().ends_with("Z hello\n"));
        fs::remove_dir_all(&dir).unwrap();
    }
}
