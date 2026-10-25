//! Microphone capture via cpal. Speaker = `Me`. cpal's `Stream` is `!Send` on macOS (the CoreAudio
//! handle is bound to the thread that built it), while `AudioSource` must be `Send` so the worker
//! can own it across threads. So the stream lives on its own dedicated thread that builds it, plays
//! it, and parks until stopped; `Mic` itself holds only `Send` handles — the receiving end of the
//! sample channel and a stop flag. Resampled 16 kHz mono frames arrive over the channel; the worker
//! polls `try_recv`.

use super::super::{resample, Speaker};
use super::samples::{channel, SampleReceiver, SampleSender};
use super::{AudioSource, CaptureFailures, CaptureStartGate, DefaultInputFollow, Frame};
use crate::audio::mic_owner::MicOwnership;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
pub use shogun_microphone::{input_device_choices, input_device_names, InputDeviceChoice};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::TryRecvError;
use std::sync::Arc;

pub const CAPTURE_START_CANCELLED: &str = "capture start cancelled";

pub struct Mic {
    rx: SampleReceiver,
    stop: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Mic {
    /// Open the default input device. Err (permission denied, no device, unsupported config) →
    /// caller degrades to notes-only (meeting.rs), never crashes. Blocks only until the capture
    /// thread has built and started the stream (or reported why it could not).
    pub fn open() -> Result<Self, String> {
        Self::open_with_device(None)
    }

    /// Open a named input device, or the current macOS default when no name is selected.
    /// A missing selected device is an error rather than a silent fallback: changing the input
    /// source without the user's knowledge is surprising and can capture the wrong microphone.
    pub fn open_with_device(selected_device: Option<&str>) -> Result<Self, String> {
        Self::open_with_device_and_gate(selected_device, None, false, None)
    }

    /// Meeting live capture: bounded ingress, with observable loss instead of an audio backlog.
    pub fn open_with_device_bounded(selected_device: Option<&str>) -> Result<Self, String> {
        Self::open_with_device_and_gate(selected_device, None, true, None)
    }

    /// The meeting's microphone (#934): while dictation holds the mic, `owner` silences the
    /// user's voice **at capture time**, in the audio callback. Silencing where frames are read
    /// instead would follow the reader's lag: a meeting worker busy transcribing can fall
    /// seconds behind, so dictated words captured mid-hold would be read after it and land in the
    /// notes, while real speech captured before the hold would be zeroed. `bounded` as in
    /// [`Self::open_with_device_bounded`].
    pub fn open_meeting(
        selected_device: Option<&str>,
        bounded: bool,
        owner: &'static MicOwnership,
    ) -> Result<Self, String> {
        Self::open_with_device_and_gate(selected_device, None, bounded, Some(owner))
    }

    /// Open a microphone only if `start_gate` has not been cancelled. The gate is held across
    /// `Stream::play` so a release cannot race activation, but stays Pending afterwards: dictation
    /// commits the start only with its ready cue, once frames are arriving (#951).
    pub fn open_with_device_guarded(
        selected_device: Option<&str>,
        start_gate: CaptureStartGate,
    ) -> Result<Self, String> {
        Self::open_with_device_and_gate(selected_device, Some(start_gate), false, None)
    }

    fn open_with_device_and_gate(
        selected_device: Option<&str>,
        start_gate: Option<CaptureStartGate>,
        bounded: bool,
        held: Option<&'static MicOwnership>,
    ) -> Result<Self, String> {
        let selected_device = selected_device.map(str::to_owned);
        let (sample_tx, rx) = channel(bounded);
        // The capture thread reports the outcome of building the stream back here so `open` stays
        // fallible without moving the `!Send` stream across the boundary.
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let failed = Arc::new(AtomicBool::new(false));
        let failed_thread = Arc::clone(&failed);
        // Set by the error callback when cpal pauses the stream because the device's sample rate
        // changed (an AirPods call switch does this); the capture thread rebuilds it.
        let invalidated = Arc::new(AtomicBool::new(false));
        // Meetings need the same terminal device failure signal as guarded dictation. Release
        // a disconnected device promptly; generic buffer/timestamp errors remain recoverable.
        let track_terminal = start_gate.is_some() || held.is_some();
        // Only a meeting microphone on the system default follows a new default (issue #1005).
        // An explicitly selected device is never switched away from.
        let follow_default = held.is_some() && selected_device.is_none();
        // Shared so a reopened stream keeps feeding the same channel and sample offsets.
        let sample_tx = Arc::new(sample_tx);

        let thread = std::thread::spawn(move || {
            let stream = match build_stream(
                Arc::clone(&sample_tx),
                selected_device.clone(),
                start_gate.as_ref(),
                held,
                Arc::clone(&failed_thread),
                Arc::clone(&invalidated),
                track_terminal,
            ) {
                Ok(s) => {
                    let _ = ready_tx.send(Ok(()));
                    s
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            if follow_default {
                hold_following_default(
                    stream,
                    &sample_tx,
                    held,
                    &stop_thread,
                    &failed_thread,
                    &invalidated,
                    track_terminal,
                );
            } else {
                hold_stream(stream, &stop_thread, &failed_thread, &invalidated, || {
                    build_stream(
                        Arc::clone(&sample_tx),
                        selected_device.clone(),
                        None,
                        held,
                        Arc::clone(&failed_thread),
                        Arc::clone(&invalidated),
                        track_terminal,
                    )
                });
            }
        });

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Mic {
                rx,
                stop,
                failed,
                thread: Some(thread),
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("mic capture thread exited before reporting readiness".into()),
        }
    }
}

/// Keep the `!Send` stream on its capture thread. A device failure must release it even while
/// the shortcut remains held; the next hold then opens a fresh stream on the selected device.
/// An invalidated stream (paused by cpal after a sample-rate change) is rebuilt with `reopen` on
/// the same device, at its new rate.
fn hold_stream<S>(
    mut stream: S,
    stop: &AtomicBool,
    failed: &AtomicBool,
    invalidated: &AtomicBool,
    mut reopen: impl FnMut() -> Result<S, String>,
) {
    while !stop.load(Ordering::Relaxed) && !failed.load(Ordering::Acquire) {
        std::thread::sleep(std::time::Duration::from_millis(50));
        if !invalidated.swap(false, Ordering::AcqRel) || stop.load(Ordering::Relaxed) {
            continue;
        }
        crate::elog!("[audio] microphone_stream_invalidated; reopening");
        drop(stream);
        // ponytail: one attempt, no retry. Add a short backoff if DeviceBusy shows up mid-switch.
        match reopen() {
            Ok(next) => stream = next,
            Err(_) => {
                crate::elog!("[audio] microphone_reopen_failed");
                failed.store(true, Ordering::Release);
                return;
            }
        }
    }
    drop(stream);
}

/// The default input's name, read on the capture thread. Kept in RAM only, never logged.
fn default_input_name(host: &cpal::Host) -> Option<String> {
    let description = host.default_input_device()?.description().ok()?;
    Some(description.name().to_owned())
}

/// [`hold_stream`] for a meeting microphone on the system default: once a second, check whether
/// macOS moved the default input and, when the change is stable, reopen on the new default
/// (issue #1005). Same channel, same capture-time silencing (#934). A reopen that fails ends
/// capture as a device failure, the same visible outcome as a disconnected microphone. An
/// invalidated stream is reopened at once, as in [`hold_stream`].
fn hold_following_default(
    mut stream: cpal::Stream,
    sample_tx: &Arc<SampleSender>,
    held: Option<&'static MicOwnership>,
    stop: &AtomicBool,
    failed: &Arc<AtomicBool>,
    invalidated: &Arc<AtomicBool>,
    track_terminal: bool,
) {
    const CHECK_EVERY: std::time::Duration = std::time::Duration::from_secs(1);
    let host = cpal::default_host();
    let mut follow = DefaultInputFollow::new(default_input_name(&host));
    let mut checked_at = std::time::Instant::now();
    while !stop.load(Ordering::Relaxed) && !failed.load(Ordering::Acquire) {
        std::thread::sleep(std::time::Duration::from_millis(50));
        let stream_invalidated = invalidated.swap(false, Ordering::AcqRel);
        if !stream_invalidated {
            if checked_at.elapsed() < CHECK_EVERY {
                continue;
            }
            checked_at = std::time::Instant::now();
            if !follow.observe(default_input_name(&host).as_deref()) {
                continue;
            }
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if stream_invalidated {
            crate::elog!("[audio] microphone_stream_invalidated; reopening meeting microphone");
        } else {
            crate::elog!("[audio] default_input_changed; reopening meeting microphone");
        }
        drop(stream);
        match build_stream(
            Arc::clone(sample_tx),
            None,
            None,
            held,
            Arc::clone(failed),
            Arc::clone(invalidated),
            track_terminal,
        ) {
            Ok(next) => stream = next,
            Err(_) => {
                crate::elog!("[audio] default_input_reopen_failed");
                failed.store(true, Ordering::Release);
                return;
            }
        }
    }
    drop(stream);
}

fn push_resampled<T>(
    data: &[T],
    channels: u16,
    resampler: &mut resample::Resampler,
    sample_tx: &SampleSender,
    held: Option<&MicOwnership>,
) where
    T: cpal::Sample,
    f32: cpal::FromSample<T>,
{
    let samples: Vec<f32> = data.iter().copied().map(cpal::Sample::to_sample).collect();
    let mono = resample::to_mono(&samples, channels);
    // Stateful: chunk boundaries must not round the sample count away from wall time (#1005).
    let mut f16k = resampler.process(&mono);
    // Same length, zero content: the meeting's timeline stays aligned (#934).
    if held.is_some_and(MicOwnership::meeting_mic_silenced) {
        f16k.fill(0.0);
    }
    sample_tx.send(f16k);
}

fn build_stream_for<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: u16,
    input_rate: u32,
    sample_tx: Arc<SampleSender>,
    held: Option<&'static MicOwnership>,
    err_fn: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<cpal::Stream, String>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    let mut resampler = resample::Resampler::new(input_rate);
    device
        .build_input_stream(
            *config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                push_resampled(data, channels, &mut resampler, &sample_tx, held);
            },
            err_fn,
            None,
        )
        .map_err(|e| e.to_string())
}

fn stream_error_handler(
    failed: Arc<AtomicBool>,
    invalidated: Arc<AtomicBool>,
    track_terminal: bool,
) -> impl FnMut(cpal::Error) + Send + 'static {
    let mut backend_logged = false;
    let mut device_logged = false;
    move |error| {
        let (kind, first) = match error.kind() {
            cpal::ErrorKind::DeviceNotAvailable if track_terminal => {
                ("device_unavailable", !failed.swap(true, Ordering::AcqRel))
            }
            cpal::ErrorKind::DeviceNotAvailable => {
                let first = !device_logged;
                device_logged = true;
                ("device_unavailable", first)
            }
            // cpal has paused the stream (sample-rate change); the capture thread rebuilds it.
            cpal::ErrorKind::StreamInvalidated => {
                invalidated.store(true, Ordering::Release);
                ("stream_invalidated", false)
            }
            // Xruns and other per-buffer errors: CoreAudio keeps streaming.
            _ => {
                let first = !backend_logged;
                backend_logged = true;
                ("backend", first)
            }
        };
        if first {
            crate::elog!("[audio] microphone_stream_error kind={kind} detail={error}");
        }
    }
}

/// Resolve the input device to capture from: the named selection, or the current macOS default
/// when nothing is selected. A selected-but-missing device is an error, as in
/// [`Mic::open_with_device`].
fn open_input_device(
    host: &cpal::Host,
    selected_device: Option<&str>,
) -> Result<cpal::Device, String> {
    let Some(selected_device) = selected_device else {
        return host
            .default_input_device()
            .ok_or_else(|| "no input device".to_string());
    };

    let devices = host
        .input_devices()
        .map_err(|error| format!("could not list microphone devices: {error}"))?
        .filter_map(|device| {
            let name = device.description().ok()?.name().to_owned();
            Some((name, device))
        });
    super::find_named_device(devices, selected_device)
}

/// Build and start the selected input stream, sending resampled 16 kHz mono frames on `sample_tx`.
/// Must run on the thread that will keep the returned stream alive (`Stream` is `!Send`).
fn build_stream(
    sample_tx: Arc<SampleSender>,
    selected_device: Option<String>,
    start_gate: Option<&CaptureStartGate>,
    held: Option<&'static MicOwnership>,
    failed: Arc<AtomicBool>,
    invalidated: Arc<AtomicBool>,
    track_terminal: bool,
) -> Result<cpal::Stream, String> {
    let host = cpal::default_host();
    let device = open_input_device(&host, selected_device.as_deref())?;
    let supported = device.default_input_config().map_err(|e| e.to_string())?;
    let input_rate = supported.sample_rate();
    let channels = supported.channels();
    let config = supported.config();
    let err_fn = stream_error_handler(failed, invalidated, track_terminal);
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => build_stream_for::<f32>(
            &device, &config, channels, input_rate, sample_tx, held, err_fn,
        ),
        cpal::SampleFormat::F64 => build_stream_for::<f64>(
            &device, &config, channels, input_rate, sample_tx, held, err_fn,
        ),
        cpal::SampleFormat::I8 => build_stream_for::<i8>(
            &device, &config, channels, input_rate, sample_tx, held, err_fn,
        ),
        cpal::SampleFormat::I16 => build_stream_for::<i16>(
            &device, &config, channels, input_rate, sample_tx, held, err_fn,
        ),
        cpal::SampleFormat::I24 => build_stream_for::<cpal::I24>(
            &device, &config, channels, input_rate, sample_tx, held, err_fn,
        ),
        cpal::SampleFormat::I32 => build_stream_for::<i32>(
            &device, &config, channels, input_rate, sample_tx, held, err_fn,
        ),
        cpal::SampleFormat::I64 => build_stream_for::<i64>(
            &device, &config, channels, input_rate, sample_tx, held, err_fn,
        ),
        cpal::SampleFormat::U8 => build_stream_for::<u8>(
            &device, &config, channels, input_rate, sample_tx, held, err_fn,
        ),
        cpal::SampleFormat::U16 => build_stream_for::<u16>(
            &device, &config, channels, input_rate, sample_tx, held, err_fn,
        ),
        cpal::SampleFormat::U32 => build_stream_for::<u32>(
            &device, &config, channels, input_rate, sample_tx, held, err_fn,
        ),
        cpal::SampleFormat::U64 => build_stream_for::<u64>(
            &device, &config, channels, input_rate, sample_tx, held, err_fn,
        ),
        other => return Err(format!("unsupported microphone sample format: {other}")),
    }?;
    if let Some(start_gate) = start_gate {
        let Some(_start_permit) = start_gate.lock_pending() else {
            return Err(CAPTURE_START_CANCELLED.to_string());
        };
        stream.play().map_err(|error| error.to_string())?;
    } else {
        stream.play().map_err(|e| e.to_string())?;
    }
    Ok(stream)
}

impl AudioSource for Mic {
    fn capture_failures(&self) -> CaptureFailures {
        CaptureFailures {
            microphone: self.capture_failed(),
            system_audio: false,
        }
    }
    fn capture_failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }
    fn take_dropped_samples(&self) -> u64 {
        self.rx.take_dropped()
    }
    fn try_recv(&mut self) -> Option<Frame> {
        self.try_recv_with_offset().map(|(frame, _)| frame)
    }
    fn try_recv_with_offset(&mut self) -> Option<(Frame, Option<u64>)> {
        match self.rx.try_recv_with_offset() {
            Ok((samples, offset)) => Some((
                Frame {
                    speaker: Speaker::Me,
                    samples,
                },
                offset,
            )),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                if !self.stop.load(Ordering::Acquire) {
                    self.failed.store(true, Ordering::Release);
                }
                None
            }
        }
    }
    fn stop(&mut self) {
        // Idempotent: signal the capture thread to drop the stream and release the device.
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Mic {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_error_releases_device_without_a_stop_request() {
        struct Stream(std::sync::mpsc::Sender<()>);
        impl Drop for Stream {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }
        let failed = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_failed = Arc::clone(&failed);
        let worker_stop = Arc::clone(&stop);
        let (closed_tx, closed_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            hold_stream(
                Stream(closed_tx),
                &worker_stop,
                &worker_failed,
                &AtomicBool::new(false),
                || Err::<Stream, String>("not reopened".into()),
            );
        });
        stream_error_handler(Arc::clone(&failed), Arc::default(), true)(cpal::Error::new(
            cpal::ErrorKind::DeviceNotAvailable,
        ));
        let closed = closed_rx.recv_timeout(std::time::Duration::from_secs(1));
        stop.store(true, Ordering::Relaxed);
        worker.join().unwrap();
        assert_eq!(
            closed,
            Ok(()),
            "a failed stream must release its device before key-up"
        );
    }

    #[test]
    fn recoverable_buffer_error_does_not_end_dictation() {
        let failed = Arc::new(AtomicBool::new(false));
        let mut report = stream_error_handler(Arc::clone(&failed), Arc::default(), true);
        report(cpal::Error::new(cpal::ErrorKind::Xrun));
        report(cpal::Error::with_message(
            cpal::ErrorKind::Other,
            "timestamp conversion failed",
        ));
        assert!(!failed.load(Ordering::Acquire));
        report(cpal::Error::new(cpal::ErrorKind::DeviceNotAvailable));
        assert!(failed.load(Ordering::Acquire));
    }

    #[test]
    fn unguarded_legacy_mic_retains_opt_out_failure_policy() {
        let failed = Arc::new(AtomicBool::new(false));
        stream_error_handler(Arc::clone(&failed), Arc::default(), false)(cpal::Error::new(
            cpal::ErrorKind::DeviceNotAvailable,
        ));
        assert!(!failed.load(Ordering::Acquire));
    }

    #[test]
    fn invalidated_stream_is_reopened_instead_of_going_silent() {
        let failed = Arc::new(AtomicBool::new(false));
        let invalidated = Arc::new(AtomicBool::new(false));
        stream_error_handler(Arc::clone(&failed), Arc::clone(&invalidated), true)(
            cpal::Error::new(cpal::ErrorKind::StreamInvalidated),
        );
        assert!(!failed.load(Ordering::Acquire));
        let stop = AtomicBool::new(false);
        let mut reopened = 0;
        hold_stream(0, &stop, &failed, &invalidated, || {
            reopened += 1;
            stop.store(true, Ordering::Relaxed);
            Ok(reopened)
        });
        assert_eq!(reopened, 1);
        assert!(!failed.load(Ordering::Acquire));
    }

    #[test]
    fn push_resampled_downmixes_and_resamples_i16_input() {
        let (tx, rx) = channel(true);
        let stereo = [16_384_i16, 16_384, 0, 0];

        push_resampled(&stereo, 2, &mut resample::Resampler::new(32_000), &tx, None);

        let samples = rx.try_recv().expect("resampled samples should be sent");
        assert_eq!(samples.len(), 1);
        assert!((samples[0] - 0.5).abs() < 0.01);
    }
}

#[cfg(test)]
mod health_tests {
    use super::*;

    #[test]
    fn channel_disconnection_is_failed_while_an_empty_channel_is_healthy() {
        let (tx, rx) = channel(true);
        let mut mic = Mic {
            rx,
            stop: Arc::new(AtomicBool::new(false)),
            failed: Arc::new(AtomicBool::new(false)),
            thread: None,
        };
        assert!(mic.try_recv().is_none());
        assert!(!mic.capture_failed());
        drop(tx);
        assert!(mic.try_recv().is_none());
        assert!(mic.capture_failures().microphone);
        mic.stop();
    }
}
