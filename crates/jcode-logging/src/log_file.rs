use chrono::{DateTime, Days, Local, NaiveDate};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MIB: u64 = 1024 * 1024;
const SEGMENT_BYTES: u64 = 64 * MIB;
const TOTAL_BYTES: u64 = 256 * MIB;
const RETAIN_DAYS: Days = Days::new(6); // today plus six previous dates
const CHECK_BYTES: u64 = MIB;
const CHECK_INTERVAL: Duration = Duration::from_secs(60);

pub(super) struct LogFile {
    dir: PathBuf,
    day: NaiveDate,
    file: File,
    bytes_until_check: u64,
    next_check: Instant,
}

impl LogFile {
    pub(super) fn open(dir: PathBuf, day: NaiveDate) -> io::Result<Self> {
        let file = open_append(&daily_path(&dir, day))?;
        Ok(Self {
            bytes_until_check: check_budget(&file)?,
            next_check: Instant::now() + CHECK_INTERVAL,
            dir,
            day,
            file,
        })
    }

    pub(super) fn append(&mut self, now: DateTime<Local>, bytes: &[u8]) -> io::Result<()> {
        let day = now.date_naive();
        if day != self.day
            || bytes.len() as u64 > self.bytes_until_check
            || Instant::now() >= self.next_check
        {
            let result = self.refresh(now, day != self.day, bytes.len() as u64);
            if let Err(err) = result {
                // Keep the open file usable even if maintenance fails. Avoid
                // printing another maintenance error on every subsequent log.
                eprintln!("jcode logger rotation failed: {err}");
                self.day = day;
                self.bytes_until_check = CHECK_BYTES;
                self.next_check = Instant::now() + CHECK_INTERVAL;
            }
        }
        self.file.write_all(bytes)?;
        self.bytes_until_check = self.bytes_until_check.saturating_sub(bytes.len() as u64);
        Ok(())
    }

    fn refresh(
        &mut self,
        now: DateTime<Local>,
        day_changed: bool,
        incoming: u64,
    ) -> io::Result<()> {
        let day = now.date_naive();
        let path = daily_path(&self.dir, day);
        let mut file = open_append(&path)?;
        let mut len = file.metadata()?.len();
        let needs_rotation = |len: u64| len > 0 && len.saturating_add(incoming) > SEGMENT_BYTES;
        if (day_changed || needs_rotation(len))
            && let Some(_lock) = rotation_lock(&self.dir)?
        {
            // Only maintenance takes this cross-process lock. Appenders open
            // the current name at bounded byte/time intervals, so a peer can
            // append to the just-rotated segment until its next check.
            file = open_append(&path)?;
            len = file.metadata()?.len();
            if needs_rotation(len) {
                let mut serial = now.timestamp_micros().unsigned_abs();
                let archive = loop {
                    let candidate = path.with_extension(format!("log.{serial:020}"));
                    if !candidate.try_exists()? {
                        break candidate;
                    }
                    serial += 1;
                };
                drop(file);
                fs::rename(&path, archive)?;
                file = open_append(&path)?;
                len = file.metadata()?.len();
            }
            prune(&self.dir, day, SEGMENT_BYTES.saturating_sub(len));
        }
        self.file = file;
        self.day = day;
        self.bytes_until_check = SEGMENT_BYTES.saturating_sub(len).min(CHECK_BYTES);
        self.next_check = Instant::now() + CHECK_INTERVAL;
        Ok(())
    }
}

pub(super) fn daily_path(dir: &Path, day: NaiveDate) -> PathBuf {
    dir.join(format!("jcode-{day}.log"))
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn check_budget(file: &File) -> io::Result<u64> {
    Ok(SEGMENT_BYTES
        .saturating_sub(file.metadata()?.len())
        .min(CHECK_BYTES))
}

fn rotation_lock(dir: &Path) -> io::Result<Option<File>> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join(".rotation.lock"))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(err)) => Err(err),
    }
}

pub(super) fn cleanup(dir: &Path, day: NaiveDate) {
    let Ok(Some(_lock)) = rotation_lock(dir) else {
        return;
    };
    let len = fs::metadata(daily_path(dir, day)).map_or(0, |m| m.len());
    // Let the first append rotate a pre-existing oversized active file before
    // counting it against the budget for the rest of the log family.
    if len > SEGMENT_BYTES {
        return;
    }
    prune(dir, day, SEGMENT_BYTES.saturating_sub(len));
}

fn prune(dir: &Path, day: NaiveDate, reserve: u64) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let cutoff = day.checked_sub_days(RETAIN_DAYS).unwrap_or(NaiveDate::MIN);
    let mut candidates = Vec::new();
    let mut used = reserve;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some((date, archive, desktop)) = parse_name(name) else {
            continue;
        };
        let Ok(metadata) = fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let expired = if desktop {
            // Legacy Desktop logs belong to a different writer, which may
            // still append to a file named for its launch date.
            metadata
                .modified()
                .ok()
                .and_then(|time| time.elapsed().ok())
                .is_some_and(|age| age > Duration::from_secs(7 * 24 * 60 * 60))
        } else {
            date < cutoff
        };
        if expired {
            remove(&entry.path());
        } else if !desktop {
            used = used.saturating_add(metadata.len());
            if date != day || archive.is_some() {
                candidates.push((
                    date,
                    archive.unwrap_or(u64::MAX),
                    metadata.len(),
                    entry.path(),
                ));
            }
        }
    }
    if used > TOTAL_BYTES {
        candidates.sort_unstable();
        for (_, _, len, path) in candidates {
            if remove(&path) {
                used = used.saturating_sub(len);
                if used <= TOTAL_BYTES {
                    break;
                }
            }
        }
    }
}

fn parse_name(name: &str) -> Option<(NaiveDate, Option<u64>, bool)> {
    let (rest, desktop) = match name.strip_prefix("jcode-desktop-") {
        Some(rest) => (rest, true),
        None => (name.strip_prefix("jcode-")?, false),
    };
    let (date, suffix) = rest.split_once(".log")?;
    let date = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    let archive = if suffix.is_empty() {
        None
    } else if !desktop {
        Some(suffix.strip_prefix('.')?.parse().ok()?)
    } else {
        return None;
    };
    Some((date, archive, desktop))
}

fn remove(path: &Path) -> bool {
    match fs::remove_file(path) {
        Ok(()) => true,
        Err(err) if err.kind() == io::ErrorKind::NotFound => true,
        Err(err) => {
            eprintln!("jcode logger cleanup failed: {err}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::io::Read;

    #[test]
    fn appends_rotate_full_segments_and_reopen_on_the_next_day() {
        let now = Local.with_ymd_and_hms(2026, 11, 22, 12, 0, 0).unwrap();
        let dir = std::env::temp_dir().join(format!(
            "jcode-log-rotate-{}-{}",
            std::process::id(),
            Local::now().timestamp_nanos_opt().unwrap(),
        ));
        fs::create_dir(&dir).unwrap();
        let day = now.date_naive();
        let today = daily_path(&dir, day);
        let mut first = LogFile::open(dir.clone(), day).unwrap();
        let mut second = LogFile::open(dir.clone(), day).unwrap();
        first.append(now, b"before\n").unwrap();
        first.file.set_len(SEGMENT_BYTES).unwrap(); // sparse fixture
        first.bytes_until_check = 0;

        first.append(now, b"current A\n").unwrap();
        second.next_check = Instant::now();
        second.append(now, b"current B\n").unwrap();
        assert_eq!(fs::read(&today).unwrap(), b"current A\ncurrent B\n");
        let archives: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .and_then(parse_name)
                    .is_some_and(|(_, serial, _)| serial.is_some())
            })
            .collect();
        assert_eq!(archives.len(), 1);
        let mut archived = File::open(archives[0].path()).unwrap();
        assert_eq!(archived.metadata().unwrap().len(), SEGMENT_BYTES);
        let mut prefix = [0; 7];
        archived.read_exact(&mut prefix).unwrap();
        assert_eq!(&prefix, b"before\n");

        let tomorrow = now.checked_add_days(Days::new(1)).unwrap();
        first.append(tomorrow, b"daily\n").unwrap();
        assert_eq!(
            fs::read(daily_path(&dir, tomorrow.date_naive())).unwrap(),
            b"daily\n"
        );
        assert_eq!(fs::read(&today).unwrap(), b"current A\ncurrent B\n");
        drop((archived, first, second));
        fs::remove_dir_all(dir).unwrap();
    }
}
