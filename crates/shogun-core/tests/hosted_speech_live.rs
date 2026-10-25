//! Explicit live smoke: no personal provider key; synthetic PCM stays in memory.
//! Run only with SHOGUN_ASR_SMOKE_BILLING pointing at this Mac's signed licence file.
#![cfg(all(feature = "desktop", feature = "net", target_os = "macos"))]
use shogun_core::audio::asr::{
    deepgram::{DeepgramConfig, DeepgramLive, LiveMode},
    hosted,
};
use shogun_core::llm::traceability::RecordingSink;
use std::{sync::Arc, time::Duration};

#[test]
#[ignore = "explicit operator live test; uses company speech quota"]
fn company_speech_transcribes_and_refreshes_without_a_personal_key() {
    let path = std::env::var("SHOGUN_ASR_SMOKE_BILLING").expect("explicit licence file required");
    let billing: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path).expect("read licence")).expect("licence JSON");
    let bearer = billing["token"].as_str().expect("signed token").to_owned();
    let trace = Arc::new(RecordingSink::default());
    let sink = trace.clone();
    hosted::configure(Arc::new(move || Ok((bearer.clone(), sink.clone()))));
    let generated = std::process::Command::new("/usr/bin/swift")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../scripts/speech-smoke.swift"
        ))
        .output()
        .expect("synthesize test speech in memory");
    assert!(generated.status.success());
    let pcm: Vec<f32> = generated
        .stdout
        .chunks_exact(2)
        .map(|b| f32::from(i16::from_le_bytes([b[0], b[1]])) / 32768.0)
        .collect();
    assert!(pcm.len() > 16000);
    let config = DeepgramConfig::default();
    let (mut first, mut second) = hosted::pair();
    let mut voice = DeepgramLive::connect(&config, first.as_mut(), LiveMode::Voice, trace.clone())
        .expect("hosted voice");
    let meeting = shogun_core::audio::asr::deepgram::meeting_live::MeetingLive::start(
        &config.clone().with_diarize(true),
        second.as_mut(),
        trace.clone(),
    )
    .expect("hosted meeting");
    for chunk in pcm.chunks(1600) {
        voice.push_pcm(chunk).expect("voice PCM");
        meeting.push_pcm(chunk);
        std::thread::sleep(Duration::from_millis(100));
    }
    let mut meeting_text = String::new();
    let status = meeting.finish(|event| {
        if event.result.is_final {
            meeting_text.push_str(&event.result.text);
        }
    });
    assert!(!status.auth_rejected && !status.quota_exhausted);
    assert_eq!(status.dropped_samples, 0);
    for text in [voice.finish().expect("voice finalize"), meeting_text] {
        assert!(
            text.contains("会議") && text.contains("資料"),
            "expected synthetic words absent"
        );
    }
    let lease = first.hosted_session().expect("session").expect("hosted");
    let deadline = lease.deadline;
    std::thread::sleep(Duration::from_secs(65));
    let reconnected =
        DeepgramLive::connect(&config, first.as_mut(), LiveMode::Voice, trace.clone())
            .expect("expired grant refreshed");
    reconnected.discard();
    assert_eq!(lease.deadline, deadline);
    assert!(trace
        .records()
        .iter()
        .any(|r| r.purpose == "speech_session_refresh"));
    // Discard is intentionally asynchronous. Wait for the socket worker to release
    // its reference so the last drop (and synchronous finish request) is on this thread.
    for _ in 0..100 {
        if Arc::strong_count(&lease) == 2 {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        Arc::strong_count(&lease),
        2,
        "cancelled worker retained its lease"
    );
    drop(lease);
    drop(first);
    drop(second);
    assert!(trace
        .records()
        .iter()
        .any(|r| r.purpose == "speech_session_finish"));
}
