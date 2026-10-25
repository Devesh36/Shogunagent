//! The Mac app's half of Visual recall (feature `desktop`): the cross-process settings file
//! lock the app and the standalone faces share, and the Apple Vision OCR. The settings model and
//! the in-process lock stay in the parent module, where the server build reads them too.

use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use super::OcrLine;

/// Whether this build can OCR a stored frame: Apple Vision, so macOS only.
pub const OCR_AVAILABLE: bool = cfg!(target_os = "macos");

/// An `flock` on `.visual_recall.json.lock` beside the settings file, held for the length of a
/// settings read or write so the app and the faces serialize across processes.
pub(super) struct SettingsFileLock(std::fs::File);

impl SettingsFileLock {
    pub(super) fn acquire(path: &Path) -> Result<Self, String> {
        use std::fs::OpenOptions;

        let lock_path = settings_lock_path(path);
        if let Some(dir) = lock_path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("create visual_recall settings directory: {e}"))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| format!("open visual_recall settings lock: {e}"))?;
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if result != 0 {
            return Err(format!(
                "lock visual_recall settings: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(Self(file))
    }
}

impl Drop for SettingsFileLock {
    fn drop(&mut self) {
        let _ = unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

pub(super) fn settings_lock_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("visual_recall.json");
    path.with_file_name(format!(".{file_name}.lock"))
}

/// Run Vision text recognition on a stored JPEG, restricted to `region` (see
/// [`super::ocr_jpeg_bytes`]). Returns `(text, confidence, full-image box)` per non-empty line.
///
/// Vision and ImageIO autorelease the decoded pixel buffers, and the capture poller is a plain
/// thread with no pool: without this one, every OCR'd frame stayed in RAM for the life of the
/// process (about 4 GB after an hour).
#[cfg(target_os = "macos")]
pub(super) fn recognize_lines(jpeg: &[u8], region: [f64; 4]) -> Option<Vec<OcrLine>> {
    objc2::rc::autoreleasepool(|_| recognize_lines_in_pool(jpeg, region))
}

/// Off macOS there is no Vision: the feature-on build answers like the server build.
#[cfg(not(target_os = "macos"))]
pub(super) fn recognize_lines(_jpeg: &[u8], _region: [f64; 4]) -> Option<Vec<OcrLine>> {
    None
}

#[cfg(target_os = "macos")]
fn recognize_lines_in_pool(jpeg: &[u8], region: [f64; 4]) -> Option<Vec<OcrLine>> {
    use objc2::AnyThread;
    use objc2::runtime::AnyObject;
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};
    use objc2_foundation::{NSArray, NSData, NSDictionary};
    use objc2_vision::{
        VNImageRequestHandler, VNRecognizedTextObservation, VNRecognizeTextRequest, VNRequest,
        VNRequestTextRecognitionLevel,
    };

    use super::map_vision_box;

    let data = NSData::with_bytes(jpeg);
    let options = NSDictionary::<objc2_vision::VNImageOption, AnyObject>::new();
    let handler =
        VNImageRequestHandler::initWithData_options(VNImageRequestHandler::alloc(), &data, &options);
    let request = unsafe {
        let req = VNRecognizeTextRequest::init(VNRecognizeTextRequest::alloc());
        req.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
        req.setUsesLanguageCorrection(true);
        req.setAutomaticallyDetectsLanguage(true);
        // Vision's origin is the lower-left corner.
        let [x, y, width, height] = region;
        req.setRegionOfInterest(CGRect::new(
            CGPoint::new(x, 1.0 - y - height),
            CGSize::new(width, height),
        ));
        req
    };
    let requests = NSArray::from_slice(&[&*request as &VNRequest]);
    handler.performRequests_error(&requests).ok()?;
    let observations = request.results()?;
    let mut lines: Vec<OcrLine> = Vec::new();
    let mut conf_sum = 0.0f32;
    for obs in observations.iter() {
        let Some(text_obs) = obs.downcast_ref::<VNRecognizedTextObservation>() else {
            continue;
        };
        let candidates = text_obs.topCandidates(1);
        if let Some(best) = candidates.firstObject() {
            let line = best.string().to_string();
            if line.trim().is_empty() {
                continue;
            }
            conf_sum += best.confidence();
            let bb = unsafe { text_obs.boundingBox() };
            let [x, y, w, h] = map_vision_box(
                region,
                [bb.origin.x, bb.origin.y, bb.size.width, bb.size.height],
            );
            lines.push(OcrLine { text: line, x, y, w, h });
        }
    }
    if lines.is_empty() {
        return None;
    }
    let n = lines.len() as f32;
    eprintln!(
        "[visual_recall] ocr lines={} mean_conf={:.2} chars={}",
        lines.len(),
        conf_sum / n,
        lines.iter().map(|l| l.text.len()).sum::<usize>() + lines.len().saturating_sub(1)
    );
    Some(lines)
}
