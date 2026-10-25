//! User capture pause, shared by desktop and Memory API. It never pauses connector sync.
//! A pause commit waits for in-flight screen readers/writers to finish. The same lock is held
//! through AX/OCR ingestion and cache publication, including across processes on macOS.

use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

static LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Status {
    pub paused: bool,
    pub resume_at_ms: Option<i64>,
}

impl Status {
    pub fn effective(self, now_ms: i64) -> Self {
        if !self.paused || self.resume_at_ms.is_some_and(|until| now_ms >= until) {
            Self::default()
        } else {
            self
        }
    }

    pub fn validate(self, now_ms: i64) -> Result<Self, String> {
        if let Some(until) = self.resume_at_ms {
            if !self.paused || until <= now_ms || until > now_ms.saturating_add(93_600_000) {
                return Err("Resume time must be within the next 26 hours of a pause.".into());
            }
        }
        Ok(self)
    }

    /// When capture runs again under this setting: now when it is not paused, at the resume time
    /// of a timed pause, and never (`None`) for a pause that waits to be resumed.
    fn resumes_at(self, now_ms: i64) -> Option<i64> {
        let status = self.effective(now_ms);
        if status.paused {
            status.resume_at_ms
        } else {
            Some(now_ms)
        }
    }

    /// Whether replacing `self` with `requested` starts capture again sooner (issue #897):
    /// ending a pause, giving a pause that waits to be resumed a resume time, or bringing a timed
    /// pause's resume time forward. Pausing, or keeping a pause at least as long, does not.
    /// An API caller may only do the second kind on its own; the first widens what SHOGUN reads
    /// from the screen and waits for the user's approval in the app, like an exclusion removal
    /// (#657).
    pub fn resumes_sooner(self, requested: Status, now_ms: i64) -> bool {
        match (self.resumes_at(now_ms), requested.resumes_at(now_ms)) {
            (_, None) => false,
            (None, Some(_)) => true,
            (Some(current), Some(next)) => next < current,
        }
    }

    /// Whether this is still the pause an approval to end it was requested against (issue #897):
    /// paused now, with the same resume time (`None` = until resumed). Confirming a request after
    /// the pause changed (a timed pause ran out, or the user resumed and set another) fails
    /// closed instead of ending a pause the request never saw.
    pub fn is_pause_until(self, resume_at_ms: Option<i64>, now_ms: i64) -> bool {
        let status = self.effective(now_ms);
        status.paused && status.resume_at_ms == resume_at_ms
    }
}

pub struct Guard {
    _process: MutexGuard<'static, ()>,
    #[cfg(unix)]
    file: std::fs::File,
}

#[cfg(unix)]
impl Drop for Guard {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// Lock a capture mutex whose value is a marker or is only ever replaced whole, recovering it if a
/// panic poisoned it (issue #783). The supervised capture tick catches panics and restarts; a
/// poisoned lock that stayed fatal would stop capture, pause/resume and every selection reader
/// until relaunch, which for a login item can be days. Such a value is never half-written, so the
/// data behind a poisoned lock is still the last complete one.
pub fn lock_recovering<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn acquire(path: &Path) -> Result<Guard, String> {
    // The lock guards `()`: poisoning carries no state to distrust.
    let process = lock_recovering(&LOCK);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|_| "Cannot open capture settings.".to_string())?;
    }
    #[cfg(unix)]
    let file = {
        use std::os::fd::AsRawFd;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path.with_extension("lock"))
            .map_err(|_| "Cannot lock capture settings.".to_string())?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err("Cannot lock capture settings.".into());
        }
        file
    };
    Ok(Guard {
        _process: process,
        #[cfg(unix)]
        file,
    })
}

/// The pause setting as it is on disk right now, read without the capture lock.
///
/// For **reporting** only: a label, a menu state, the reason a row is empty. [`Guard::save`]
/// replaces the file by rename, so a reader sees the old setting or the new one, never half of
/// one. Taking the lock just to report waited for whatever held it, and the capture poller holds
/// it for a whole tick (about a second in a browser): a main-thread caller such as the notch's
/// action row then froze the panel until the tick ended (#783).
///
/// Anything that reads the screen or writes captured text still goes through [`acquire`], which
/// is what makes a pause wait for in-flight readers.
///
/// The file is checked against its seal ([`crate::settings_seal`], audit 2026-10-20 M6): ending
/// or shortening a pause by editing (or deleting) the file is not followed.
pub fn read_status(path: &Path, now_ms: i64) -> Result<Status, String> {
    let file = match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str::<Status>(&text)
            .map_err(|_| "Capture settings need repair. Resume capture to reset them.".to_string())?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Status::default(),
        Err(_) => return Err("Cannot read capture settings.".into()),
    };
    Ok(crate::settings_seal::trusted(path, file, now_ms).effective(now_ms))
}

impl Guard {
    pub fn status(&self, path: &Path, now_ms: i64) -> Result<Status, String> {
        read_status(path, now_ms)
    }

    pub fn save(&self, path: &Path, next: Status, now_ms: i64) -> Result<Status, String> {
        use std::io::Write;
        let next = next.validate(now_ms)?;
        let json = serde_json::to_vec(&next)
            .map_err(|_| "Cannot serialize capture settings.".to_string())?;
        let temp = path.with_extension(format!("{}.tmp", std::process::id()));
        let mut file = std::fs::File::create(&temp)
            .map_err(|_| "Cannot save capture settings.".to_string())?;
        file.write_all(&json)
            .and_then(|()| file.sync_all())
            .map_err(|_| "Cannot save capture settings.".to_string())?;
        drop(file);
        std::fs::rename(&temp, path).map_err(|_| "Cannot replace capture settings.".to_string())?;
        // The app's or an approved API write: seal it (M6).
        crate::settings_seal::seal(path, &next);
        Ok(next)
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|time| i64::try_from(time.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pause_state_is_readable_while_capture_holds_the_lock() {
        let dir = std::env::temp_dir().join(format!(
            "shogun-capture-read-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("capture_control.json");
        // No file yet: not paused.
        assert_eq!(read_status(&path, 1_000), Ok(Status::default()));

        let guard = acquire(&path).unwrap();
        guard
            .save(&path, Status { paused: true, resume_at_ms: Some(60_000) }, 1_000)
            .unwrap();
        // The lock is still held, as it is for a whole poller tick. A reader on another thread
        // gets the answer at once instead of waiting for it.
        let (tx, rx) = std::sync::mpsc::channel();
        let reader_path = path.clone();
        std::thread::spawn(move || {
            let _ = tx.send(read_status(&reader_path, 2_000));
        });
        let seen = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the read did not wait for the lock");
        assert_eq!(seen, Ok(Status { paused: true, resume_at_ms: Some(60_000) }));
        // A timed pause that has run out reads as not paused, as it does under the lock.
        assert_eq!(read_status(&path, 60_000), Ok(Status::default()));
        assert_eq!(guard.status(&path, 2_000), read_status(&path, 2_000));
        drop(guard);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A panic while holding capture control must not leave it unavailable for the rest of the
    /// process (issue #783): the supervised tick restarts, and pause/resume must keep working.
    #[test]
    fn capture_control_survives_a_panic_while_held() {
        let dir = std::env::temp_dir().join(format!(
            "shogun-capture-control-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("capture_control.json");
        let panicked = std::thread::spawn({
            let path = path.clone();
            move || {
                let _guard = acquire(&path).unwrap();
                panic!("tick panicked while holding capture control");
            }
        })
        .join();
        assert!(panicked.is_err());
        assert!(LOCK.is_poisoned(), "the panic poisoned the process lock");
        assert!(acquire(&path).is_ok(), "capture control is still available after the panic");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lock_recovering_returns_the_last_complete_value() {
        let shared = std::sync::Arc::new(Mutex::new(vec!["com.example.Secret".to_string()]));
        let poisoner = shared.clone();
        let _ = std::thread::spawn(move || {
            let _held = poisoner.lock().unwrap();
            panic!("poison");
        })
        .join();
        assert!(shared.is_poisoned());
        assert_eq!(*lock_recovering(&shared), vec!["com.example.Secret".to_string()]);
    }

    /// Issue #897: what an API caller may change on its own is what keeps capture paused at
    /// least as long; anything that starts it again sooner needs the user.
    #[test]
    fn only_a_change_that_starts_capture_sooner_resumes_it() {
        let running = Status::default();
        let until_resumed = Status { paused: true, resume_at_ms: None };
        let until = |ms| Status { paused: true, resume_at_ms: Some(ms) };
        let now = 1_000;

        // Pausing, or keeping a pause at least as long, never resumes anything.
        assert!(!running.resumes_sooner(until_resumed, now));
        assert!(!running.resumes_sooner(until(60_000), now));
        assert!(!running.resumes_sooner(running, now));
        assert!(!until_resumed.resumes_sooner(until_resumed, now));
        assert!(!until(60_000).resumes_sooner(until(60_000), now));
        assert!(!until(60_000).resumes_sooner(until(90_000), now));
        assert!(!until(60_000).resumes_sooner(until_resumed, now));

        // Ending a pause, or making it end earlier, does.
        assert!(until_resumed.resumes_sooner(running, now));
        assert!(until_resumed.resumes_sooner(until(2_000), now));
        assert!(until(60_000).resumes_sooner(running, now));
        assert!(until(60_000).resumes_sooner(until(30_000), now));

        // A timed pause that already ran out is capture running: resuming it changes nothing.
        assert!(!until(500).resumes_sooner(running, now));
    }

    #[test]
    fn an_unpause_request_matches_only_the_pause_it_saw() {
        let running = Status::default();
        let until_resumed = Status { paused: true, resume_at_ms: None };
        let until = |ms| Status { paused: true, resume_at_ms: Some(ms) };
        let now = 1_000;

        assert!(until_resumed.is_pause_until(None, now));
        assert!(until(60_000).is_pause_until(Some(60_000), now));
        // A different pause, capture running, or a timed pause that ran out: not the same pause.
        assert!(!until(90_000).is_pause_until(Some(60_000), now));
        assert!(!until(60_000).is_pause_until(None, now));
        assert!(!until_resumed.is_pause_until(Some(60_000), now));
        assert!(!running.is_pause_until(None, now));
        assert!(!until(500).is_pause_until(Some(500), now));
    }

    #[test]
    fn timed_pause_expires_at_boundary_and_manual_pause_survives() {
        let timed = Status {
            paused: true,
            resume_at_ms: Some(900_000),
        };
        assert!(timed.effective(899_999).paused);
        assert!(!timed.effective(900_000).paused);
        assert!(
            Status {
                paused: true,
                resume_at_ms: None
            }
            .effective(i64::MAX)
            .paused
        );
        assert!(Status {
            paused: false,
            resume_at_ms: Some(100)
        }
        .validate(0)
        .is_err());
        assert!(Status {
            paused: true,
            resume_at_ms: Some(93_600_001)
        }
        .validate(0)
        .is_err());
    }

    #[test]
    fn pause_commit_waits_for_inflight_capture_and_blocks_later_work() {
        let path = std::env::temp_dir().join(format!(
            "shogun-capture-control-{}.json",
            std::process::id()
        ));
        let capture = acquire(&path).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let writer_path = path.clone();
        let writer = std::thread::spawn(move || {
            acquire(&writer_path)
                .unwrap()
                .save(
                    &writer_path,
                    Status {
                        paused: true,
                        resume_at_ms: None,
                    },
                    0,
                )
                .unwrap();
            tx.send(()).unwrap();
        });
        assert!(rx
            .recv_timeout(std::time::Duration::from_millis(25))
            .is_err());
        drop(capture);
        rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        writer.join().unwrap();
        let guard = acquire(&path).unwrap();
        assert!(guard.status(&path, 100).unwrap().paused);
        std::fs::write(&path, "broken").unwrap();
        assert!(guard.status(&path, 100).is_err());
        drop(guard);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("lock"));
    }
}
