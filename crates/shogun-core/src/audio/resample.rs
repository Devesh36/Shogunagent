//! Bring any capture stream to the one rate the VAD and ASR agree on: 16 kHz mono f32 (§3).
//!
//! Linear interpolation, deliberately dependency-free — ASR is robust to the mild aliasing this
//! introduces, and it keeps the pure-logic layer unit-testable on CI. Swap in a polyphase
//! resampler (rubato) later behind the same signature if measurement shows it matters.

use super::SAMPLE_RATE;

/// Downmix to mono by averaging interleaved channels.
pub fn to_mono(interleaved: &[f32], channels: u16) -> Vec<f32> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    let ch = channels as usize;
    interleaved
        .chunks(ch)
        .map(|frame| frame.iter().sum::<f32>() / ch as f32)
        .collect()
}

/// Streaming [`to_16k_mono`] for a capture callback: the fractional read position and the last
/// sample carry across calls, so the output count tracks wall time exactly instead of each
/// chunk being rounded on its own (issue #1005: 512-frame 48 kHz callbacks drifted ~+0.2%, about
/// seven seconds an hour, and every timestamp derived from the sample count drifted with them).
///
/// The position is kept in exact integer units of 1/16 000 input sample, so it cannot drift
/// even over days.
#[derive(Debug, Clone)]
pub struct Resampler {
    in_rate: u32,
    /// Position of the next output sample relative to the start of the next input chunk, in
    /// 1/`SAMPLE_RATE` input samples. Never below `-SAMPLE_RATE` (one sample back).
    phase: i64,
    /// The previous chunk's last sample, for an output that falls between two chunks.
    previous: Option<f32>,
}

impl Resampler {
    pub fn new(in_rate: u32) -> Self {
        Self {
            in_rate: in_rate.max(1),
            phase: 0,
            previous: None,
        }
    }

    /// Resample the next mono chunk of the stream.
    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        if self.in_rate == SAMPLE_RATE {
            return input.to_vec();
        }
        let Some(&last) = input.last() else {
            return Vec::new();
        };
        let den = i64::from(SAMPLE_RATE);
        let step = i64::from(self.in_rate);
        let len = input.len() as i64;
        let capacity = input.len() as u64 * u64::from(SAMPLE_RATE) / u64::from(self.in_rate);
        let mut out = Vec::with_capacity(capacity as usize + 1);
        let mut phase = self.phase;
        loop {
            let index = phase.div_euclid(den);
            let frac = phase.rem_euclid(den);
            // Interpolating needs the sample after `index`; at the chunk's end, wait for the next.
            if index >= len || (frac > 0 && index + 1 >= len) {
                break;
            }
            let a = if index < 0 {
                self.previous.unwrap_or(input[0])
            } else {
                input[index as usize]
            };
            let sample = if frac == 0 {
                a
            } else {
                let b = input[(index + 1) as usize];
                a + (b - a) * (frac as f32 / den as f32)
            };
            out.push(sample);
            phase += step;
        }
        self.phase = phase - len * den;
        self.previous = Some(last);
        out
    }
}

/// Resample mono `input` from `in_rate` to 16 kHz by linear interpolation. One-shot: a capture
/// stream uses [`Resampler`], which keeps chunk boundaries from accumulating rounding.
pub fn to_16k_mono(input: &[f32], in_rate: u32) -> Vec<f32> {
    if in_rate == SAMPLE_RATE || input.is_empty() {
        return input.to_vec();
    }
    let ratio = SAMPLE_RATE as f64 / in_rate as f64;
    let out_len = ((input.len() as f64) * ratio).round() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src = i as f64 / ratio;
        let j = src.floor() as usize;
        let frac = (src - j as f64) as f32;
        let a = input[j.min(input.len() - 1)];
        let b = input[(j + 1).min(input.len() - 1)];
        out.push(a + (b - a) * frac);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mono_passthrough() {
        assert_eq!(to_mono(&[1.0, 2.0, 3.0], 1), vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn stereo_averages_to_mono() {
        // L,R,L,R → averaged
        assert_eq!(to_mono(&[0.0, 2.0, 4.0, 8.0], 2), vec![1.0, 6.0]);
    }

    #[test]
    fn same_rate_is_passthrough() {
        assert_eq!(to_16k_mono(&[1.0, 2.0], 16_000), vec![1.0, 2.0]);
    }

    /// Issue #1005: one simulated hour of 48 kHz audio in 512-frame callbacks stays within one
    /// output sample of wall time. Per-chunk rounding gained ~0.2% (about 7 s an hour).
    #[test]
    fn streaming_resampler_does_not_drift_over_an_hour() {
        for (rate, frames) in [(48_000_u32, 512_usize), (44_100, 512), (48_000, 441), (8_000, 160)] {
            let mut resampler = Resampler::new(rate);
            let chunk = vec![0.25_f32; frames];
            let total_in = rate as u64 * 3600;
            let mut fed = 0_u64;
            let mut produced = 0_u64;
            while fed < total_in {
                let n = (total_in - fed).min(frames as u64) as usize;
                produced += resampler.process(&chunk[..n]).len() as u64;
                fed += n as u64;
            }
            let wall = total_in * SAMPLE_RATE as u64 / rate as u64;
            assert!(
                produced.abs_diff(wall) <= 1,
                "{rate} Hz / {frames}: produced {produced}, wall time {wall}"
            );
            // Rounding per chunk instead of over the whole stream drifts measurably at 48 kHz / 512.
            if (rate, frames) == (48_000, 512) {
                let rounded = (total_in / frames as u64) * to_16k_mono(&chunk, rate).len() as u64;
                assert!(rounded.abs_diff(wall) > SAMPLE_RATE as u64, "the regression is real");
            }
        }
    }

    #[test]
    fn streaming_resampler_matches_one_shot_and_interpolates_across_chunks() {
        let ramp: Vec<f32> = (0..960).map(|i| i as f32 / 960.0).collect();
        let whole = to_16k_mono(&ramp, 48_000);
        let mut resampler = Resampler::new(48_000);
        let mut pieces = Vec::new();
        for chunk in ramp.chunks(7) {
            pieces.extend(resampler.process(chunk));
        }
        assert_eq!(pieces.len(), 320);
        for (a, b) in whole.iter().zip(&pieces) {
            assert!((a - b).abs() < 1e-5, "{a} vs {b}");
        }
        // Upsampling reads one sample back across the boundary.
        let mut up = Resampler::new(8_000);
        let mut out = up.process(&[0.0, 1.0]);
        out.extend(up.process(&[2.0]));
        assert_eq!(out, vec![0.0, 0.5, 1.0, 1.5, 2.0]);
        assert_eq!(Resampler::new(16_000).process(&[0.5, 0.25]), vec![0.5, 0.25]);
        assert!(Resampler::new(48_000).process(&[]).is_empty());
    }

    #[test]
    fn downsampling_halves_length_from_32k() {
        let input = vec![0.5_f32; 320]; // 320 @ 32k → ~160 @ 16k
        let out = to_16k_mono(&input, 32_000);
        assert!((out.len() as i64 - 160).abs() <= 1, "unexpected length {}", out.len());
        assert!(out.iter().all(|&s| (s - 0.5).abs() < 1e-6));
    }
}
