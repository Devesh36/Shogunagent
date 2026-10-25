//! Content-free loudness summary of one capture, for diagnostics only (issue #974).
//!
//! A dictation that "captured" but transcribed nothing after sleep/wake can be a device that
//! delivers frames of a noise floor rather than exact zeros, which `nonzero_frames` counts as
//! healthy. This reduces a whole capture to a few coarse numbers: the peak and the overall RMS,
//! each as one of six wide dBFS buckets, and how many frames sat below a silence floor. Nothing
//! here can reconstruct audio or speech; no samples are kept.

use std::fmt;

/// A frame whose RMS is below this (−70 dBFS) counts as silent. Room noise through a working
/// microphone is usually well above it; exact digital silence and a dead input are below.
const SILENT_FRAME_RMS: f32 = 3.162e-4;

/// Running totals for one capture. `observe` per frame, then log [`CaptureEnergy::summary`].
#[derive(Debug, Clone, Default)]
pub struct CaptureEnergy {
    peak: f32,
    sum_squares: f64,
    samples: u64,
    silent_frames: u64,
}

impl CaptureEnergy {
    pub fn observe(&mut self, frame: &[f32]) {
        let mut frame_squares = 0.0_f64;
        let mut counted = 0_u64;
        for &sample in frame.iter().filter(|s| s.is_finite()) {
            self.peak = self.peak.max(sample.abs());
            frame_squares += f64::from(sample) * f64::from(sample);
            counted += 1;
        }
        if counted == 0 {
            return;
        }
        self.sum_squares += frame_squares;
        self.samples += counted;
        let frame_rms = (frame_squares / counted as f64).sqrt();
        if frame_rms < f64::from(SILENT_FRAME_RMS) {
            self.silent_frames += 1;
        }
    }

    pub fn summary(&self) -> EnergySummary {
        let rms = if self.samples == 0 {
            0.0
        } else {
            (self.sum_squares / self.samples as f64).sqrt() as f32
        };
        EnergySummary {
            peak: bucket(self.peak),
            rms: bucket(rms),
            silent_frames: self.silent_frames,
        }
    }
}

/// What the log line carries: two coarse buckets and a count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnergySummary {
    pub peak: &'static str,
    pub rms: &'static str,
    pub silent_frames: u64,
}

impl fmt::Display for EnergySummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "peak_dbfs={} rms_dbfs={} silent_frames={}",
            self.peak, self.rms, self.silent_frames
        )
    }
}

/// Six wide dBFS buckets for a linear amplitude. `none` is exact digital silence.
fn bucket(level: f32) -> &'static str {
    const BOUNDS: [(f32, &str); 5] = [
        (3.162e-5, "lt-90"),
        (3.162e-4, "-90..-70"),
        (3.162e-3, "-70..-50"),
        (3.162e-2, "-50..-30"),
        (3.162e-1, "-30..-10"),
    ];
    if level.is_nan() || level <= 0.0 {
        return "none";
    }
    BOUNDS
        .iter()
        .find(|(bound, _)| level < *bound)
        .map_or("ge-10", |(_, name)| name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_silence_and_no_frames_read_as_none() {
        let empty = CaptureEnergy::default().summary();
        assert_eq!(
            (empty.peak, empty.rms, empty.silent_frames),
            ("none", "none", 0)
        );
        let mut zeros = CaptureEnergy::default();
        zeros.observe(&[0.0; 320]);
        zeros.observe(&[]);
        assert_eq!(
            zeros.summary().to_string(),
            "peak_dbfs=none rms_dbfs=none silent_frames=1"
        );
    }

    #[test]
    fn a_noise_floor_is_told_apart_from_speech() {
        // What #948's log could not show: frames that are not zero, yet carry nothing.
        let mut floor = CaptureEnergy::default();
        for _ in 0..50 {
            floor.observe(&[1e-4, -1e-4, 1e-4, -1e-4]);
        }
        let floor = floor.summary();
        assert_eq!(
            (floor.peak, floor.rms, floor.silent_frames),
            ("-90..-70", "-90..-70", 50)
        );

        let mut speech = CaptureEnergy::default();
        for _ in 0..50 {
            speech.observe(&[0.2, -0.15, 0.1, -0.05]);
        }
        speech.observe(&[0.0; 4]);
        let speech = speech.summary();
        assert_eq!(
            (speech.peak, speech.rms, speech.silent_frames),
            ("-30..-10", "-30..-10", 1)
        );
    }

    /// The summary goes through `elog!`; the redactor must leave it readable.
    #[test]
    fn the_log_redactor_keeps_the_summary_intact() {
        for level in [0.0, 1e-5, 1e-4, 1e-3, 1e-2, 0.1, 0.9] {
            let mut energy = CaptureEnergy::default();
            energy.observe(&[level, -level]);
            let line = format!(
                "[voice] capture_complete session=3 frames=1 {}",
                energy.summary()
            );
            assert_eq!(crate::log_redact::scrub(&line), line);
        }
    }

    #[test]
    fn buckets_are_coarse_and_ignore_non_finite_samples() {
        assert_eq!(bucket(1.0), "ge-10");
        assert_eq!(bucket(0.05), "-30..-10");
        assert_eq!(bucket(0.005), "-50..-30");
        assert_eq!(bucket(f32::NAN), "none");
        let mut energy = CaptureEnergy::default();
        energy.observe(&[f32::NAN, f32::INFINITY]);
        assert_eq!(
            energy.summary().silent_frames,
            0,
            "no finite samples, no frame"
        );
        energy.observe(&[f32::NAN, 0.5]);
        assert_eq!(energy.summary().peak, "ge-10");
    }
}
