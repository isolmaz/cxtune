//! A small rotating log file for diagnostics: `%LOCALAPPDATA%\cxtune\cxtune.log`.
//!
//! Lines are appended with a UTC timestamp. When the file would pass its size limit it is
//! renamed to `cxtune.log.1` (replacing the older one) and a new file is started. A program that
//! holds either file open without delete sharing blocks the rename; the current file is then
//! emptied instead. Either way the two files together stay within twice the limit. Logging never
//! fails the caller: a file that cannot be opened or written is skipped and tried again with the
//! next line.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Size at which the log file is rotated.
pub const DEFAULT_LIMIT: u64 = 256 * 1024;

pub struct Log {
    path: PathBuf,
    limit: u64,
    file: Option<File>,
    size: u64,
}

/// `%LOCALAPPDATA%\cxtune\cxtune.log`, or `None` if `LOCALAPPDATA` is not set.
pub fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(base).join("cxtune").join("cxtune.log"))
}

impl Log {
    /// A log that appends to `path` and rotates at `limit` bytes.
    pub fn open(path: PathBuf, limit: u64) -> Log {
        let mut log = Log {
            path,
            limit,
            file: None,
            size: 0,
        };
        log.reopen();
        log
    }

    /// Appends `message` as one line.
    pub fn line(&mut self, message: &str) {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let text = format!(
            "{} {}\n",
            timestamp(secs),
            message.replace(['\r', '\n'], " ")
        );
        let len = text.len() as u64;
        if self.size > 0 && self.size + len > self.limit {
            self.rotate();
        }
        if self.file.is_none() {
            self.reopen();
        }
        if let Some(file) = &mut self.file {
            match file.write_all(text.as_bytes()) {
                Ok(()) => self.size += len,
                Err(_) => self.file = None,
            }
        }
    }

    fn reopen(&mut self) {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .ok();
        self.size = self
            .file
            .as_ref()
            .and_then(|f| f.metadata().ok())
            .map_or(0, |m| m.len());
    }

    fn rotate(&mut self) {
        self.file = None;
        if std::fs::rename(&self.path, self.path.with_extension("log.1")).is_err() {
            let _ = File::create(&self.path);
        }
        self.reopen();
    }
}

/// `secs` since the Unix epoch as `YYYY-MM-DDTHH:MM:SSZ`.
fn timestamp(secs: u64) -> String {
    let (days, rest) = (secs / 86_400, secs % 86_400);
    let (hour, minute, second) = (rest / 3600, rest % 3600 / 60, rest % 60);
    // days since 1970-01-01 to a civil date (proleptic Gregorian calendar)
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::testdir::TempDir;

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap_or_default()
    }

    #[test]
    fn timestamps_are_utc_dates() {
        for (secs, text) in [
            (0, "1970-01-01T00:00:00Z"),
            (86_399, "1970-01-01T23:59:59Z"),
            (951_782_400, "2000-02-29T00:00:00Z"),
            (1_000_000_000, "2001-09-09T01:46:40Z"),
            (1_700_000_000, "2023-11-14T22:13:20Z"),
            (4_102_444_800, "2100-01-01T00:00:00Z"),
            (4_107_542_399, "2100-02-28T23:59:59Z"),
        ] {
            assert_eq!(timestamp(secs), text);
        }
    }

    #[test]
    fn lines_are_appended_with_a_timestamp_on_one_line_each() {
        let dir = TempDir::new("log-append");
        let path = dir.join("cxtune.log");
        let mut log = Log::open(path.clone(), DEFAULT_LIMIT);
        log.line("first");
        log.line("second\r\nstill second");
        drop(log);
        // a new log object continues the same file
        Log::open(path.clone(), DEFAULT_LIMIT).line("third");

        let text = read(&path);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        for (line, message) in lines.iter().zip(["first", "second  still second", "third"]) {
            let (stamp, rest) = line.split_once(' ').unwrap();
            assert_eq!(stamp.len(), 20, "{line}");
            assert!(stamp.ends_with('Z') && stamp.contains('T'), "{line}");
            assert_eq!(rest, message);
        }
    }

    #[test]
    fn the_log_rotates_and_stays_bounded() {
        let dir = TempDir::new("log-rotate");
        let path = dir.join("cxtune.log");
        let old = dir.join("cxtune.log.1");
        let limit = 400;
        let mut log = Log::open(path.clone(), limit);
        for i in 0..200 {
            log.line(&format!("message number {i:03}"));
            let total = path.metadata().unwrap().len() + old.metadata().map_or(0, |m| m.len());
            assert!(total <= 2 * limit, "{total} bytes after {i} lines");
        }
        drop(log);

        // only the two files exist, the newest line is at the end of the current one, and
        // the older file continues where the current one starts
        let mut names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["cxtune.log", "cxtune.log.1"]);
        let current = read(&path);
        let previous = read(&old);
        assert!(
            current
                .lines()
                .last()
                .unwrap()
                .ends_with("message number 199")
        );
        let number = |line: &str| -> usize { line.rsplit(' ').next().unwrap().parse().unwrap() };
        let first_current = number(current.lines().next().unwrap());
        let last_previous = number(previous.lines().last().unwrap());
        assert_eq!(first_current, last_previous + 1);
    }

    /// Opens `path` the way a viewer that follows the file does: others may read and write it,
    /// but not rename or delete it.
    fn hold_without_delete_sharing(path: &Path) -> File {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ_WRITE: u32 = 1 | 2;
        OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ_WRITE)
            .open(path)
            .unwrap()
    }

    #[test]
    fn the_log_stays_bounded_when_the_older_file_is_held_open() {
        let dir = TempDir::new("log-held-old");
        let path = dir.join("cxtune.log");
        let old = dir.join("cxtune.log.1");
        std::fs::write(
            &old, "older
",
        )
        .unwrap();
        let _held = hold_without_delete_sharing(&old);
        let limit = 400;
        let mut log = Log::open(path.clone(), limit);
        for i in 0..200 {
            log.line(&format!("message number {i:03}"));
            let size = path.metadata().unwrap().len();
            assert!(size <= limit, "{size} bytes after {i} lines");
        }
        assert_eq!(
            read(&old),
            "older
"
        );
        assert!(
            read(&path)
                .lines()
                .last()
                .unwrap()
                .ends_with("message number 199")
        );
    }

    #[test]
    fn the_log_stays_bounded_when_the_current_file_is_held_open() {
        let dir = TempDir::new("log-held-current");
        let path = dir.join("cxtune.log");
        let limit = 400;
        let mut log = Log::open(path.clone(), limit);
        log.line("first");
        let _held = hold_without_delete_sharing(&path);
        for i in 0..200 {
            log.line(&format!("message number {i:03}"));
            let size = path.metadata().unwrap().len();
            assert!(size <= limit, "{size} bytes after {i} lines");
        }
        assert!(
            read(&path)
                .lines()
                .last()
                .unwrap()
                .ends_with("message number 199")
        );
        assert!(!dir.join("cxtune.log.1").exists());
    }

    #[test]
    fn a_log_that_cannot_be_opened_does_not_fail_the_caller() {
        let dir = TempDir::new("log-broken");
        // a directory where the file should be
        let path = dir.join("cxtune.log");
        std::fs::create_dir(&path).unwrap();
        let mut log = Log::open(path.clone(), DEFAULT_LIMIT);
        log.line("goes nowhere");
        log.line("still nowhere");

        // once the obstacle is gone the next line is written
        std::fs::remove_dir(&path).unwrap();
        log.line("written");
        assert!(read(&path).ends_with("written\n"));
    }
}
