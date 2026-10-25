//! Visual recall settings (issue #107).
//!
//! When enabled, the macOS adapter may capture a focused window into RAM, run on-device OCR
//! (Apple Vision), persist text + provenance, and optionally retain encrypted compressed JPEG
//! frames for the user-selected finite duration (`screen_frames` — invariant-2 exception).
//! Default is **off**.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// The Mac app's half (feature `desktop`): the cross-process settings file lock and the Apple
/// Vision OCR. The server build keeps the settings model and the in-process lock only.
#[cfg(feature = "desktop")]
mod desktop;
#[cfg(feature = "desktop")]
use desktop::SettingsFileLock;

static SETTINGS_WRITE_LOCK: Mutex<()> = Mutex::new(());
static SETTINGS_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Whether this build can OCR a stored frame (`visual_recall.rescan_frame`'s `ocr_available`):
/// Apple Vision, so the Mac app's build on macOS only.
#[cfg(feature = "desktop")]
pub const OCR_AVAILABLE: bool = desktop::OCR_AVAILABLE;
#[cfg(not(feature = "desktop"))]
pub const OCR_AVAILABLE: bool = false;

/// Existing installs and invalid retention values safely fall back to three days.
pub const DEFAULT_RETENTION_DAYS: u32 = 3;
/// Settings exposes one-day ticks through one week before the custom option.
pub const PRESET_RETENTION_DAYS: [u32; 7] = [1, 2, 3, 4, 5, 6, 7];
/// Custom retention is bounded to ten years. Automatic age expiry is always enabled.
pub const MAX_CUSTOM_RETENTION_DAYS: u32 = 3_650;
pub const DAY_MS: i64 = 24 * 60 * 60 * 1_000;

/// Validated finite retention for encrypted Visual Recall JPEG rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct RetentionPolicy {
    days: u32,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            days: DEFAULT_RETENTION_DAYS,
        }
    }
}

impl RetentionPolicy {
    pub fn try_days(days: u32) -> Result<Self, String> {
        if !(1..=MAX_CUSTOM_RETENTION_DAYS).contains(&days) {
            return Err(format!(
                "retention days must be between 1 and {MAX_CUSTOM_RETENTION_DAYS}"
            ));
        }
        Ok(Self { days })
    }

    pub fn days(self) -> u32 {
        self.days
    }

    /// Convert the bounded day count without relying on unchecked duration arithmetic.
    pub fn retain_ms(self) -> Result<i64, String> {
        i64::from(self.days)
            .checked_mul(DAY_MS)
            .ok_or_else(|| "visual recall retention is too large".to_string())
    }

    pub fn from_ms(self, now_ms: i64) -> Result<i64, String> {
        self.retain_ms()
            .map(|retain_ms| now_ms.saturating_sub(retain_ms))
    }
}

/// Persisted visual-recall preference. Default on (owner decision 2026-10-20); a saved
/// `enabled: false` survives updates.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Settings {
    /// Master switch. Off means no screen pixels are read and no OCR runs.
    pub enabled: bool,
    /// Automatic age expiry for JPEG rows. This can never represent an unbounded duration.
    pub retention: RetentionPolicy,
}

impl Default for Settings {
    fn default() -> Self {
        Self { enabled: true, retention: RetentionPolicy::default() }
    }
}

fn default_enabled() -> bool {
    true
}

impl<'de> serde::Deserialize<'de> for Settings {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        struct RawRetention {
            days: u32,
        }

        #[derive(serde::Deserialize)]
        struct StoredSettings {
            #[serde(default = "default_enabled")]
            enabled: bool,
            #[serde(default)]
            retention: Option<RawRetention>,
            // Source commit 83e7b1d stored this scalar. New writes omit it.
            #[serde(default)]
            retention_days: Option<u32>,
        }

        let stored = StoredSettings::deserialize(deserializer)?;
        let days = stored
            .retention
            .map(|retention| retention.days)
            .or(stored.retention_days)
            .unwrap_or(DEFAULT_RETENTION_DAYS);
        Ok(Self {
            enabled: stored.enabled,
            retention: RetentionPolicy::try_days(days).unwrap_or_default(),
        })
    }
}

/// Load the settings in force from `path` (default when missing or unreadable), checked against
/// the file's seal ([`crate::settings_seal`], audit 2026-10-20 M6): turning Visual recall on or
/// keeping screens longer by editing the file is not followed.
///
/// Only a missing file takes the shipped default (on); an unreadable or corrupt one reads as off.
pub fn load_settings(path: &Path) -> Settings {
    let off = || Settings { enabled: false, ..Settings::default() };
    let file: Settings = match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|_| off()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Settings::default(),
        Err(_) => off(),
    };
    crate::settings_seal::trusted(path, file, crate::settings_seal::now_ms())
}

/// Persist settings to `path` (creates parent dirs when possible).
fn save_settings(path: &Path, settings: &Settings) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("create visual_recall settings directory: {e}"))?;
    }
    let json = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("visual_recall.json");
    let sequence = SETTINGS_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp_path = path.with_file_name(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        sequence
    ));
    std::fs::write(&temp_path, json)
        .map_err(|e| format!("save visual_recall settings temp file: {e}"))?;
    std::fs::rename(&temp_path, path).map_err(|e| {
        let _ = std::fs::remove_file(&temp_path);
        format!("replace visual_recall settings: {e}")
    })?;
    // Every write through here is the app's or an approved API write: seal it (M6).
    crate::settings_seal::seal(path, settings);
    Ok(())
}

/// Serialize read-modify-write updates from the desktop and local API surfaces.
///
/// The preference is a single document: updating `enabled` must never discard a
/// concurrently selected retention value (or vice versa).
pub fn update_settings(
    path: &Path,
    update: impl FnOnce(&mut Settings),
) -> Result<Settings, String> {
    update_settings_with_commit(path, update, |_| {})
}

/// Serialize a settings patch and its committed side effect across local processes.
///
/// `after_commit` runs while the settings file lock remains held. Once the atomic rename succeeds,
/// the mutation is committed and must not be reported as failed: callers keep follow-up work
/// infallible and log any cleanup failure themselves.
pub fn update_settings_with_commit(
    path: &Path,
    update: impl FnOnce(&mut Settings),
    after_commit: impl FnOnce(&Settings),
) -> Result<Settings, String> {
    try_update_settings_with_commit(
        path,
        |settings| {
            update(settings);
            Ok(())
        },
        after_commit,
    )
}

/// [`update_settings_with_commit`] whose patch may refuse after seeing the settings in force
/// (issue #782: an API write that would widen Visual recall, or a confirmed widening that no
/// longer matches its card). A refusal saves nothing and runs no `after_commit`; the decision
/// and the write happen under the same locks, so a concurrent change cannot slip between them.
pub fn try_update_settings_with_commit(
    path: &Path,
    update: impl FnOnce(&mut Settings) -> Result<(), String>,
    after_commit: impl FnOnce(&Settings),
) -> Result<Settings, String> {
    let _guard = SETTINGS_WRITE_LOCK
        .lock()
        .map_err(|_| "visual recall settings lock poisoned".to_string())?;
    #[cfg(feature = "desktop")]
    let _file_guard = SettingsFileLock::acquire(path)?;
    let mut settings = load_settings(path);
    update(&mut settings)?;
    save_settings(path, &settings)?;
    after_commit(&settings);
    Ok(settings)
}

/// Read settings and consume them while no local process can mutate the file.
pub fn with_locked_settings<T>(
    path: &Path,
    read: impl FnOnce(&Settings) -> T,
) -> Result<T, String> {
    let _guard = SETTINGS_WRITE_LOCK
        .lock()
        .map_err(|_| "visual recall settings lock poisoned".to_string())?;
    #[cfg(feature = "desktop")]
    let _file_guard = SettingsFileLock::acquire(path)?;
    Ok(read(&load_settings(path)))
}

/// Run a final capture commit only while Visual Recall remains enabled on disk.
///
/// Holding the same process/file locks as settings mutation makes disable + purge a strict barrier:
/// a capture already committing finishes before purge, while a capture arriving later is refused.
pub fn with_capture_enabled<T>(
    path: &Path,
    capture: impl FnOnce() -> T,
) -> Result<Option<T>, String> {
    with_locked_settings(path, |settings| settings.enabled.then(capture))
}

/// Fraction of the OCR text's distinct words that Accessibility text must contain for the OCR
/// copy to be redundant. Below it, the OCR text is kept as durable `screen_text` memory.
pub const AX_COVERAGE_MIN: f64 = 0.5;

/// Whether this tick's Accessibility text already covers what OCR read from the same window.
///
/// Words are lowercase alphanumeric runs of at least 4 characters, compared as distinct tokens;
/// a word is covered when it occurs in the lowercased AX text (a substring test, so unspaced
/// scripts match too). Empty AX covers nothing; OCR with no such words has nothing to keep.
pub fn ax_covers_ocr(ax_text: &str, ocr_text: &str) -> bool {
    let ax = ax_text.to_lowercase();
    let ocr = ocr_text.to_lowercase();
    let words: std::collections::BTreeSet<&str> = ocr
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 4)
        .collect();
    if words.is_empty() {
        return true;
    }
    if ax.trim().is_empty() {
        return false;
    }
    let covered = words.iter().filter(|w| ax.contains(**w)).count();
    covered as f64 / words.len() as f64 >= AX_COVERAGE_MIN
}

/// One recognized text line and its box, normalized 0..1 in full-image coordinates, top-left origin.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct OcrLine {
    pub text: String,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Map a Vision `boundingBox` (normalized to the region of interest, bottom-left origin) to
/// full-image fractions with a top-left origin. `region` is `(x, y, w, h)`, top-left origin.
pub fn map_vision_box(region: [f64; 4], bb: [f64; 4]) -> [f64; 4] {
    let [rx, ry, rw, rh] = region;
    let [bx, by, bw, bh] = bb;
    // The ROI's own bottom edge in Vision space, then the box's bottom edge inside it.
    let bottom = (1.0 - ry - rh) + by * rh;
    let h = bh * rh;
    [rx + bx * rw, 1.0 - (bottom + h), bw * rw, h]
}

/// Re-OCR a stored JPEG via Apple Vision (the Mac app's build on macOS). Returns `None` when OCR
/// fails or is unavailable, as it always is in the server build.
///
/// `region` is the focused window as `(x, y, width, height)` fractions of the image, top-left
/// origin ([`shogun_memory::screen_frames::OcrRegion::normalized`]). Text outside it belongs to
/// other windows and is never read (#969).
#[cfg(feature = "desktop")]
pub fn ocr_jpeg_bytes(jpeg: &[u8], region: [f64; 4]) -> Option<String> {
    let lines = desktop::recognize_lines(jpeg, region)?;
    Some(lines.into_iter().map(|l| l.text).collect::<Vec<_>>().join("\n"))
}

#[cfg(not(feature = "desktop"))]
pub fn ocr_jpeg_bytes(_jpeg: &[u8], _region: [f64; 4]) -> Option<String> {
    None
}

/// OCR an encoded image held in memory (PNG or JPEG) via Apple Vision (macOS). The capture path
/// hands it a lossless crop of the focused window at native resolution, taken from the same
/// capture as the saved preview, so recognition does not suffer the preview's downscale and JPEG
/// loss. `region` is as in [`ocr_jpeg_bytes`].
#[cfg(feature = "desktop")]
pub fn ocr_image_bytes(image: &[u8], region: [f64; 4]) -> Option<String> {
    ocr_jpeg_bytes(image, region)
}

#[cfg(not(feature = "desktop"))]
pub fn ocr_image_bytes(_image: &[u8], _region: [f64; 4]) -> Option<String> {
    None
}

/// Like [`ocr_jpeg_bytes`] but keeps each line's box (read-only "View OCR text").
#[cfg(feature = "desktop")]
pub fn ocr_jpeg_lines(jpeg: &[u8], region: [f64; 4]) -> Option<Vec<OcrLine>> {
    desktop::recognize_lines(jpeg, region)
}

#[cfg(not(feature = "desktop"))]
pub fn ocr_jpeg_lines(_jpeg: &[u8], _region: [f64; 4]) -> Option<Vec<OcrLine>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vision_box_maps_to_top_left_full_image() {
        // Whole frame: Vision's bottom-left y=0.8,h=0.1 is top-left y=0.1.
        let b = map_vision_box([0.0, 0.0, 1.0, 1.0], [0.1, 0.8, 0.2, 0.1]);
        assert!((b[1] - 0.1).abs() < 1e-9 && (b[0] - 0.1).abs() < 1e-9);
        // Window in the right-bottom quarter: a box filling the ROI maps to the ROI.
        let b = map_vision_box([0.5, 0.5, 0.5, 0.5], [0.0, 0.0, 1.0, 1.0]);
        assert_eq!(b, [0.5, 0.5, 0.5, 0.5]);
        // A box at the ROI's top edge lands at the ROI's top.
        let b = map_vision_box([0.5, 0.5, 0.5, 0.5], [0.0, 0.9, 1.0, 0.1]);
        assert!((b[1] - 0.5).abs() < 1e-9 && (b[3] - 0.05).abs() < 1e-9);
    }

    use std::sync::{Arc, Barrier};

    #[test]
    fn ax_coverage_rule() {
        let ocr = "Quarterly roadmap review with Maya about invoice totals";
        // Empty AX covers nothing.
        assert!(!ax_covers_ocr("", ocr));
        assert!(!ax_covers_ocr("   \n", ocr));
        // Full and case-insensitive coverage.
        assert!(ax_covers_ocr("QUARTERLY ROADMAP REVIEW with maya about INVOICE totals", ocr));
        // 8 distinct words of 4+ characters: exactly half is covered, under half is not.
        assert!(ax_covers_ocr("quarterly roadmap review with", ocr));
        assert!(!ax_covers_ocr("quarterly roadmap review", ocr));
        // Short words and punctuation do not count; nothing to keep means covered.
        assert!(ax_covers_ocr("x", "ok to go, a b c 12"));
        // Unspaced scripts match by substring.
        assert!(ax_covers_ocr("今日の議事録を確認します", "議事録を確認します"));
        assert!(!ax_covers_ocr("unrelated words entirely", "議事録を確認します"));
    }

    #[test]
    fn a_refused_patch_saves_nothing_and_skips_the_commit_hook() {
        let dir = std::env::temp_dir().join(format!("vr-try-update-{}", std::process::id()));
        let path = dir.join("visual_recall.json");
        let _ = std::fs::remove_dir_all(&dir);
        update_settings(&path, |settings| {
            settings.enabled = false;
            settings.retention = RetentionPolicy::try_days(5).unwrap();
        })
        .unwrap();
        let mut committed = false;
        let refused = try_update_settings_with_commit(
            &path,
            |settings| {
                settings.enabled = true;
                Err("invalid visual recall request".to_string())
            },
            |_| committed = true,
        );
        assert_eq!(refused, Err("invalid visual recall request".to_string()));
        assert!(!committed);
        let on_disk = load_settings(&path);
        assert!(!on_disk.enabled);
        assert_eq!(on_disk.retention.days(), 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_is_on() {
        let settings = Settings::default();
        assert!(settings.enabled);
        assert_eq!(settings.retention.days(), DEFAULT_RETENTION_DAYS);
    }

    #[test]
    fn a_corrupt_file_reads_as_off() {
        let dir = std::env::temp_dir().join(format!("shogun-vr-corrupt-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("visual_recall.json");
        std::fs::write(&path, "{\"enabled\":tr").unwrap();
        assert!(!load_settings(&path).enabled);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn absent_json_field_reads_as_on() {
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert!(s.enabled);
        assert_eq!(s.retention.days(), DEFAULT_RETENTION_DAYS);
    }

    #[test]
    fn legacy_retention_days_migrates_without_changing_enabled() {
        let settings: Settings =
            serde_json::from_str(r#"{"enabled":true,"retention_days":7}"#).unwrap();
        assert!(settings.enabled);
        assert_eq!(settings.retention.days(), 7);
    }

    #[test]
    fn invalid_retention_falls_back_to_three_days() {
        let settings: Settings =
            serde_json::from_str(r#"{"enabled":true,"retention":{"days":0}}"#).unwrap();
        assert!(settings.enabled);
        assert_eq!(settings.retention.days(), DEFAULT_RETENTION_DAYS);
    }

    #[test]
    fn custom_retention_boundaries_are_checked() {
        assert!(RetentionPolicy::try_days(1).is_ok());
        assert!(RetentionPolicy::try_days(MAX_CUSTOM_RETENTION_DAYS).is_ok());
        assert!(RetentionPolicy::try_days(0).is_err());
        assert!(RetentionPolicy::try_days(MAX_CUSTOM_RETENTION_DAYS + 1).is_err());
    }

    #[test]
    fn retention_conversion_is_exact_and_finite() {
        let retention = RetentionPolicy::try_days(7).unwrap();
        assert_eq!(retention.retain_ms(), Ok(7 * DAY_MS));
        assert_eq!(retention.from_ms(10 * DAY_MS), Ok(3 * DAY_MS));
    }

    #[test]
    fn concurrent_field_updates_preserve_both_values_and_valid_json() {
        let path = std::env::temp_dir().join(format!(
            "shogun-visual-recall-settings-{}-{}.json",
            std::process::id(),
            SETTINGS_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let start = Arc::new(Barrier::new(3));
        let enabled_path = path.clone();
        let enabled_start = start.clone();
        let enabled = std::thread::spawn(move || {
            enabled_start.wait();
            update_settings(&enabled_path, |settings| settings.enabled = true)
        });
        let retention_path = path.clone();
        let retention_start = start.clone();
        let retention = std::thread::spawn(move || {
            retention_start.wait();
            update_settings(&retention_path, |settings| {
                settings.retention = RetentionPolicy::try_days(30).unwrap();
            })
        });

        start.wait();
        enabled.join().unwrap().unwrap();
        retention.join().unwrap().unwrap();

        let json = std::fs::read_to_string(&path).unwrap();
        let settings: Settings = serde_json::from_str(&json).unwrap();
        assert!(settings.enabled);
        assert_eq!(settings.retention.days(), 30);
        #[cfg(feature = "desktop")]
        let _ = std::fs::remove_file(desktop::settings_lock_path(&path));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn final_capture_gate_refuses_work_after_disable_commits() {
        let path = std::env::temp_dir().join(format!(
            "shogun-visual-recall-capture-gate-{}-{}.json",
            std::process::id(),
            SETTINGS_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        update_settings(&path, |settings| settings.enabled = true).unwrap();
        assert_eq!(with_capture_enabled(&path, || 7).unwrap(), Some(7));

        update_settings(&path, |settings| settings.enabled = false).unwrap();
        assert_eq!(with_capture_enabled(&path, || 9).unwrap(), None);

        #[cfg(feature = "desktop")]
        let _ = std::fs::remove_file(desktop::settings_lock_path(&path));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn disable_purge_finishes_after_an_inflight_capture_commit() {
        use std::sync::atomic::AtomicBool;

        let path = std::env::temp_dir().join(format!(
            "shogun-visual-recall-disable-barrier-{}-{}.json",
            std::process::id(),
            SETTINGS_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        update_settings(&path, |settings| settings.enabled = true).unwrap();
        let entered_capture = Arc::new(Barrier::new(2));
        let frame_present = Arc::new(AtomicBool::new(false));
        let capture_path = path.clone();
        let capture_entered = entered_capture.clone();
        let capture_frame = frame_present.clone();
        let capture = std::thread::spawn(move || {
            with_capture_enabled(&capture_path, || {
                capture_entered.wait();
                capture_frame.store(true, Ordering::SeqCst);
            })
            .unwrap();
        });

        entered_capture.wait();
        let purge_frame = frame_present.clone();
        update_settings_with_commit(
            &path,
            |settings| settings.enabled = false,
            move |_| purge_frame.store(false, Ordering::SeqCst),
        )
        .unwrap();
        capture.join().unwrap();

        assert!(!frame_present.load(Ordering::SeqCst));
        #[cfg(feature = "desktop")]
        let _ = std::fs::remove_file(desktop::settings_lock_path(&path));
        let _ = std::fs::remove_file(path);
    }

    #[cfg(feature = "desktop")]
    #[test]
    fn cross_process_settings_update_child() {
        let Ok(path) = std::env::var("SHOGUN_VR_SETTINGS_TEST_PATH") else {
            return;
        };
        let operation = std::env::var("SHOGUN_VR_SETTINGS_TEST_OPERATION").unwrap();
        update_settings(Path::new(&path), |settings| {
            std::thread::sleep(std::time::Duration::from_millis(150));
            match operation.as_str() {
                "enabled" => settings.enabled = true,
                "retention" => settings.retention = RetentionPolicy::try_days(30).unwrap(),
                _ => panic!("unexpected child operation"),
            }
        })
        .unwrap();
    }

    #[cfg(feature = "desktop")]
    #[test]
    fn cross_process_updates_are_serialized() {
        use std::process::Command;

        let path = std::env::temp_dir().join(format!(
            "shogun-visual-recall-process-settings-{}-{}.json",
            std::process::id(),
            SETTINGS_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let executable = std::env::current_exe().unwrap();
        let spawn = |operation: &str| {
            Command::new(&executable)
                .args([
                    "--exact",
                    "capture::visual_recall::tests::cross_process_settings_update_child",
                ])
                .env("SHOGUN_VR_SETTINGS_TEST_PATH", &path)
                .env("SHOGUN_VR_SETTINGS_TEST_OPERATION", operation)
                .spawn()
                .unwrap()
        };
        let mut enabled = spawn("enabled");
        let mut retention = spawn("retention");
        assert!(enabled.wait().unwrap().success());
        assert!(retention.wait().unwrap().success());

        let json = std::fs::read_to_string(&path).unwrap();
        let settings: Settings = serde_json::from_str(&json).unwrap();
        assert!(settings.enabled);
        assert_eq!(settings.retention.days(), 30);
        let _ = std::fs::remove_file(desktop::settings_lock_path(&path));
        let _ = std::fs::remove_file(path);
    }
}
