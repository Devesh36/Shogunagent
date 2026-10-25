//! Call-audio metadata and a human-confirmed retry request, shared by all API faces.
//! No API operation grants macOS permission or starts capture directly.
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub updated_at_ms: i64,
    pub state: String,
    pub status: String,
    pub session_id: Option<i64>,
    pub generation: u64,
    pub offer_id: u64,
    pub preflight_pending: bool,
    pub paused: bool,
    pub retry_requested: bool,
}

pub fn snapshot_path() -> Option<std::path::PathBuf> {
    crate::plan_source::app_data_file(
        "SHOGUN_MEETING_AUDIO_STATUS_JSON",
        "meeting_system_audio.json",
    )
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or_default()
}

fn read_at(path: &Path, now: i64) -> Option<Snapshot> {
    let snapshot: Snapshot = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    let age = now.checked_sub(snapshot.updated_at_ms)?;
    (0..=5_000).contains(&age).then_some(snapshot)
}

pub fn get() -> String {
    snapshot_path()
        .and_then(|path| read_at(&path, now_ms()))
        .and_then(|snapshot| serde_json::to_string(&snapshot).ok())
        .unwrap_or_else(|| serde_json::json!({"available": false, "status": "unknown"}).to_string())
}

/// A single desktop driver publishes this small metadata snapshot outside LANE.
pub fn publish(snapshot: &Snapshot) -> Result<(), String> {
    let path = snapshot_path().ok_or("meeting call audio unavailable")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|_| "meeting call audio unavailable")?;
    }
    let temporary = path.with_extension(format!("json.tmp.{}", std::process::id()));
    let bytes = serde_json::to_vec(snapshot).map_err(|_| "meeting call audio unavailable")?;
    std::fs::write(&temporary, bytes).map_err(|_| "meeting call audio unavailable")?;
    if std::fs::rename(&temporary, &path).is_err() {
        let _ = std::fs::remove_file(temporary);
        return Err("meeting call audio unavailable".into());
    }
    Ok(())
}

fn retry_for(snapshot: &Snapshot, now: i64) -> Result<serde_json::Value, String> {
    if snapshot.state != "recording"
        || snapshot.paused
        || matches!(snapshot.status.as_str(), "in_person" | "unsupported")
    {
        return Err("No online meeting to retry.".into());
    }
    let session_id = snapshot
        .session_id
        .filter(|id| *id > 0)
        .ok_or("No online meeting to retry.")?;
    Ok(
        serde_json::json!({"session_id": session_id, "generation": snapshot.generation,
        "expires_at_ms": now.saturating_add(10_000)}),
    )
}

pub fn request_retry() -> Result<String, String> {
    let now = now_ms();
    let snapshot = snapshot_path()
        .and_then(|path| read_at(&path, now))
        .ok_or("meeting call audio unavailable")?;
    let request = retry_for(&snapshot, now)?;
    crate::meeting_microphone_api::update(|settings| {
        settings
            .as_object_mut()
            .ok_or("meeting settings unavailable")?
            .insert("system_audio_retry_request".into(), request.clone());
        Ok(())
    })?;
    Ok(serde_json::json!({"status": "pending_user_confirmation",
        "session_id": snapshot.session_id, "expires_at_ms": request["expires_at_ms"]})
    .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn snapshot() -> Snapshot {
        Snapshot {
            updated_at_ms: 1_000,
            state: "recording".into(),
            status: "microphone_only".into(),
            session_id: Some(7),
            generation: 2,
            offer_id: 1,
            preflight_pending: false,
            paused: false,
            retry_requested: false,
        }
    }
    #[test]
    fn retry_requires_a_live_online_unpaused_session() {
        let mut s = snapshot();
        assert_eq!(retry_for(&s, 1_000).unwrap()["generation"], 2);
        s.paused = true;
        assert!(retry_for(&s, 1_000).is_err());
        s.paused = false;
        s.status = "unsupported".into();
        assert!(retry_for(&s, 1_000).is_err());
        s.status = "in_person".into();
        assert!(retry_for(&s, 1_000).is_err());
        s.status = "active".into();
        s.session_id = None;
        assert!(retry_for(&s, 1_000).is_err());
    }
    #[test]
    fn an_old_snapshot_cannot_claim_live_permission_health() {
        let path = std::env::temp_dir().join(format!(
            "meeting-audio-status-test-{}.json",
            std::process::id()
        ));
        std::fs::write(&path, serde_json::to_vec(&snapshot()).unwrap()).unwrap();
        assert!(read_at(&path, 6_000).is_some());
        assert!(read_at(&path, 6_001).is_none());
        assert!(read_at(&path, 999).is_none());
        let _ = std::fs::remove_file(path);
    }
}
