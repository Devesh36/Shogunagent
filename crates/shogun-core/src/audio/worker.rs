//! Ties capture → per-speaker VAD → ASR → sink into one pollable unit (§3).
//!
//! Kept sink-agnostic via `SegmentSink` so the pure pipeline is tested without a database, then
//! wired to `transcript_segments` at the call site. One `Vad` per speaker: mic and system audio
//! are independent streams and must not have their silence boundaries interleaved.

use std::sync::{Arc, RwLock};

use super::asr::Transcriber;
use super::capture::{AudioSource, Frame};
use super::vad::Vad;
use super::{Speaker, Utterance};
use crate::meeting::settings::{MeetingLanguage, Settings};

/// Where finished lines go. The desktop implements this over `transcript_segments`; tests collect
/// into a Vec.
pub trait SegmentSink {
    fn emit(
        &mut self,
        u: &Utterance,
        text: &str,
        confidence: f64,
        translation: Option<&str>,
    );
}

pub struct Worker<S: AudioSource, T: Transcriber> {
    source: S,
    asr: T,
    vad_me: Vad,
    vad_other: Vad,
    /// Wall-clock ms of the current step, set by the driver each poll.
    now: i64,
    /// Monotonic offset so utterances finalized in the same poll (or same ms) get distinct `ts`.
    line_seq: u32,
    /// Live meeting settings — read on each line so mode/lang changes apply to new utterances.
    live_settings: Option<Arc<RwLock<Settings>>>,
    /// A room session (issue #262 FR-MC-01): the microphone carries everyone, so the translation
    /// direction is decided by the line's language, not the capture channel.
    in_person: bool,
}

impl<S: AudioSource, T: Transcriber> Worker<S, T> {
    pub fn new(source: S, asr: T) -> Self {
        Worker {
            source,
            asr,
            vad_me: Vad::new(),
            vad_other: Vad::new(),
            now: 0,
            line_seq: 0,
            live_settings: None,
            in_person: false,
        }
    }

    pub fn with_live_settings(mut self, settings: Arc<RwLock<Settings>>) -> Self {
        self.live_settings = Some(settings);
        self
    }

    pub fn with_in_person(mut self, in_person: bool) -> Self {
        self.in_person = in_person;
        self
    }

    fn maybe_translate(&mut self, speaker: Speaker, pcm: &[f32], text: &str) -> Option<String> {
        let settings = self.live_settings.as_ref()?.read().ok()?;
        let me = settings.line_is_mine(self.in_person, speaker == Speaker::Me, text);
        let target = settings.translation_target(me)?;
        if target == MeetingLanguage::Auto {
            return None;
        }
        match target {
            MeetingLanguage::English => self
                .asr
                .translate_to_english(pcm)
                .into_iter()
                .next()
                .map(|s| s.text)
                .filter(|t| !t.is_empty() && t != text),
            MeetingLanguage::Japanese => {
                // EN→JA is async in the desktop sink (Select KK / latency). ASR shows immediately.
                let _ = text;
                None
            }
            MeetingLanguage::Auto => None,
        }
    }

    fn transcribe_cut(&mut self, speaker: Speaker, pcm: Vec<f32>, sink: &mut dyn SegmentSink) {
        let ts = self.now + self.line_seq as i64;
        self.line_seq += 1;
        let u = Utterance { speaker, started_at: ts, pcm: pcm.clone() };
        for seg in self.asr.transcribe(&u.pcm) {
            let text = seg.text.trim();
            if !text.is_empty() {
                let translation = self.maybe_translate(speaker, &pcm, text);
                sink.emit(&u, text, seg.confidence, translation.as_deref());
            }
        }
        // u (and its pcm) is dropped here — the waveform never outlives transcription.
    }

    pub fn capture_failures(&self) -> super::capture::CaptureFailures {
        self.source.capture_failures()
    }

    /// Drain all currently-available frames, transcribing any utterances they complete. Returns the
    /// number of frames consumed (0 means idle). Non-blocking.
    pub fn poll(&mut self, now: i64, sink: &mut dyn SegmentSink) -> usize {
        self.poll_with_frame_observer(now, sink, |_| {})
    }

    /// Like [`Self::poll`], additionally observing each capture frame before VAD consumes it.
    ///
    /// Observers can derive ephemeral presentation state such as a recording waveform. They receive
    /// only the in-memory samples already headed to ASR and must not retain them.
    pub fn poll_with_frame_observer(
        &mut self,
        now: i64,
        sink: &mut dyn SegmentSink,
        mut observe: impl FnMut(&[f32]),
    ) -> usize {
        self.set_clock(now);
        let mut consumed = 0;
        while let Some(Frame { speaker, samples }) = self.source.try_recv() {
            consumed += 1;
            observe(&samples);
            let vad = match speaker {
                Speaker::Me => &mut self.vad_me,
                Speaker::Other => &mut self.vad_other,
            };
            for cut in vad.push(&samples) {
                self.transcribe_cut(speaker, cut, sink);
            }
        }
        consumed
    }

    /// Give back the ASR after the worker is done, so a caller that reloads the same model every
    /// session (push-to-talk) can keep the loaded weights alive instead of paying the load again.
    /// The source is dropped (its device is already released by `stop`); only the transcriber, which
    /// owns nothing session-specific but its weights, comes back. Meeting lanes never call this — a
    /// `Whisper` lives exactly one meeting there — so it changes no existing behaviour.
    pub fn into_asr(self) -> T {
        self.asr
    }

    /// Advance the driver clock. The line offset only disambiguates utterances within one clock
    /// reading — without the reset it grew for the Worker's whole life, stamping late-meeting
    /// lines seconds ahead of the wall clock (and onto the search spine out of order).
    fn set_clock(&mut self, now: i64) {
        if now != self.now {
            self.line_seq = 0;
        }
        self.now = now;
    }

    /// Stop capture and flush the final utterance on each speaker before the buffers are dropped.
    pub fn stop(&mut self, now: i64, sink: &mut dyn SegmentSink) {
        self.set_clock(now);
        self.source.stop();
        if let Some(cut) = self.vad_me.flush() {
            self.transcribe_cut(Speaker::Me, cut, sink);
        }
        if let Some(cut) = self.vad_other.flush() {
            self.transcribe_cut(Speaker::Other, cut, sink);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::asr::FakeTranscriber;
    use crate::audio::capture::FakeSource;
    use crate::audio::SAMPLE_RATE;

    #[derive(Default)]
    struct VecSink {
        lines: Vec<(Speaker, String, f64)>,
    }
    impl SegmentSink for VecSink {
        fn emit(
            &mut self,
            u: &Utterance,
            text: &str,
            confidence: f64,
            _translation: Option<&str>,
        ) {
            self.lines.push((u.speaker, text.to_string(), confidence));
        }
    }

    fn tone(n: usize) -> Vec<f32> {
        (0..n).map(|i| 0.3 * ((i as f32) * 0.3).sin()).collect()
    }

    #[test]
    fn speech_then_silence_produces_a_line_via_asr() {
        let frames = vec![
            Frame { speaker: Speaker::Me, samples: tone(SAMPLE_RATE as usize) },
            Frame { speaker: Speaker::Me, samples: vec![0.0; SAMPLE_RATE as usize] },
        ];
        let mut w = Worker::new(FakeSource::new(frames), FakeTranscriber::default());
        let mut sink = VecSink::default();
        w.poll(1_000, &mut sink);
        assert_eq!(sink.lines.len(), 1);
        assert_eq!(sink.lines[0].0, Speaker::Me);
        assert!(sink.lines[0].1.starts_with("utterance-"));
    }

    #[test]
    fn mic_and_system_are_segmented_independently() {
        let frames = vec![
            Frame { speaker: Speaker::Me, samples: tone(SAMPLE_RATE as usize) },
            Frame { speaker: Speaker::Other, samples: tone(SAMPLE_RATE as usize) },
            Frame { speaker: Speaker::Me, samples: vec![0.0; SAMPLE_RATE as usize] },
            Frame { speaker: Speaker::Other, samples: vec![0.0; SAMPLE_RATE as usize] },
        ];
        let mut w = Worker::new(FakeSource::new(frames), FakeTranscriber::default());
        let mut sink = VecSink::default();
        w.poll(1_000, &mut sink);
        let speakers: Vec<Speaker> = sink.lines.iter().map(|l| l.0).collect();
        assert!(speakers.contains(&Speaker::Me) && speakers.contains(&Speaker::Other));
    }

    #[test]
    fn frame_observer_sees_every_frame_before_vad() {
        let frames = vec![
            Frame {
                speaker: Speaker::Me,
                samples: vec![0.1, 0.2],
            },
            Frame {
                speaker: Speaker::Other,
                samples: vec![0.3],
            },
        ];
        let mut worker = Worker::new(FakeSource::new(frames), FakeTranscriber::default());
        let mut sink = VecSink::default();
        let mut observed = Vec::new();

        assert_eq!(
            worker.poll_with_frame_observer(1_000, &mut sink, |samples| {
                observed.push(samples.to_vec());
            }),
            2
        );
        assert_eq!(observed, vec![vec![0.1, 0.2], vec![0.3]]);
    }

    #[test]
    fn stop_flushes_trailing_speech() {
        let frames = vec![Frame { speaker: Speaker::Me, samples: tone(SAMPLE_RATE as usize) }];
        let mut w = Worker::new(FakeSource::new(frames), FakeTranscriber::default());
        let mut sink = VecSink::default();
        w.poll(1_000, &mut sink); // no trailing silence yet → nothing emitted
        assert_eq!(sink.lines.len(), 0);
        w.stop(2_000, &mut sink); // flush emits the buffered utterance
        assert_eq!(sink.lines.len(), 1);
    }

    struct TsSink {
        stamps: Vec<i64>,
    }
    impl SegmentSink for TsSink {
        fn emit(&mut self, u: &Utterance, _t: &str, _c: f64, _tr: Option<&str>) {
            self.stamps.push(u.started_at);
        }
    }

    #[test]
    fn line_offset_resets_when_the_clock_advances() {
        // The per-line offset only disambiguates within one clock reading. It must not
        // accumulate for the worker's lifetime and stamp late lines ahead of the wall clock.
        let frames = vec![
            Frame { speaker: Speaker::Me, samples: tone(SAMPLE_RATE as usize) },
            Frame { speaker: Speaker::Me, samples: vec![0.0; SAMPLE_RATE as usize] },
            Frame { speaker: Speaker::Me, samples: tone(SAMPLE_RATE as usize) },
            Frame { speaker: Speaker::Me, samples: vec![0.0; SAMPLE_RATE as usize] },
            Frame { speaker: Speaker::Me, samples: tone(SAMPLE_RATE as usize) },
        ];
        let mut w = Worker::new(FakeSource::new(frames), FakeTranscriber::default());
        let mut sink = TsSink { stamps: Vec::new() };
        w.poll(1_000, &mut sink); // two utterances in one poll → 1_000, 1_001
        w.stop(2_000, &mut sink); // flush at a later clock → exactly 2_000, not 2_002
        assert_eq!(sink.stamps, vec![1_000, 1_001, 2_000]);
    }

    #[test]
    fn into_asr_returns_the_transcriber_for_reuse() {
        let frames = vec![
            Frame { speaker: Speaker::Me, samples: tone(SAMPLE_RATE as usize) },
            Frame { speaker: Speaker::Me, samples: vec![0.0; SAMPLE_RATE as usize] },
        ];
        let mut w = Worker::new(FakeSource::new(frames), FakeTranscriber::default());
        let mut sink = VecSink::default();
        w.poll(1_000, &mut sink); // speech then silence → one transcribe call
        let asr = w.into_asr();
        assert_eq!(asr.calls, 1, "the reused transcriber carries its state across the handoff");
    }
}
