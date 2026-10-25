//! Fetch a pinned **static model asset** over the traced egress (feature `net`).
//!
//! FR-TR-03 requires that the single raw HTTP client lives in shogun-core, the one traced egress —
//! no other crate may reach for reqwest directly. The whisper turbo weights the desktop fetches on
//! first use are a static asset (not user data), but the *download itself* is still an HTTP egress,
//! so it belongs here rather than in the desktop shell.
//!
//! **Invariant 2 does not apply:** this is a static model asset, not user audio. Writing it to disk
//! is fine. What we must not do is hold the whole file in RAM — so the stream is copied straight to
//! the temp file, hashed on the way through, verified, and only then atomically moved into place.
//!
//! **Degradation is the rule.** Any failure (no network, a truncated download, a hash mismatch)
//! returns an `Err`; the caller deletes the temp file and falls back to its bundled default. Nothing
//! here panics.

use std::io::Read;
use std::path::Path;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::llm::traceability::{Route, TraceRecord, TraceabilitySink};

/// The ledger purpose of a model download.
pub const TRACE_PURPOSE: &str = "model_download";

/// Per-operation stall bound (issue #267): connect and each read/write must complete within this,
/// so a dead or stalled endpoint fails instead of blocking the caller forever. It is NOT a total
/// download deadline — a healthy multi-GB transfer keeps making progress and never trips it.
const STALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Stream `url` into `tmp`, verify its SHA256 against `expected_sha256` (lowercase hex; `None` skips
/// verification with a loud warning), then atomically rename `tmp` → `dest`. Returns the number of
/// bytes written on success. On any error the caller is responsible for removing `tmp`.
///
/// `tmp` and `dest` must live on the same filesystem (put `tmp` in the same directory as `dest`) so
/// the final rename is atomic and a crash mid-download never leaves a half-written asset at `dest`.
///
/// A stalled endpoint errors after [`STALL_TIMEOUT`] rather than hanging the caller (fallback to
/// the bundled default depends on this returning at all).
///
/// The request is recorded in the traceability ledger before it is sent (issue #831): nothing of
/// the user's goes with it, but it is still a request this Mac makes, so it appears on the
/// Traceability screen as a `web` row (`model_download`, host, URL).
pub fn download_verified(
    url: &str,
    tmp: &Path,
    dest: &Path,
    expected_sha256: Option<&str>,
    trace: &dyn TraceabilitySink,
) -> Result<u64, String> {
    download_verified_with_timeout(url, tmp, dest, expected_sha256, STALL_TIMEOUT, trace)
}

/// [`download_verified`] with the stall bound injectable, so the timeout path is testable in
/// milliseconds instead of half a minute. Private on purpose: production callers get the default.
fn download_verified_with_timeout(
    url: &str,
    tmp: &Path,
    dest: &Path,
    expected_sha256: Option<&str>,
    stall_timeout: Duration,
    trace: &dyn TraceabilitySink,
) -> Result<u64, String> {
    let client = reqwest::blocking::Client::builder()
        // `timeout` on the blocking client bounds connect and each blocking read/write operation,
        // not the whole transfer — the streaming copy below reads 64 KiB at a time, so a live
        // download of any size is fine while a stall errors out.
        .timeout(stall_timeout)
        .connect_timeout(stall_timeout)
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    trace.record(
        TraceRecord::for_chunk(Route::Web, TRACE_PURPOSE, crate::web_fetch::host_of(url), url, false)
            .with_url(url.to_string()),
    );
    let resp = client
        .get(url)
        .send()
        .map_err(|e| format!("request: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("unexpected status {}", resp.status()));
    }

    // Stream response → temp file while hashing on the way through, so the file is never held in
    // RAM in full and we do not re-read it from disk to hash it.
    let mut file = std::fs::File::create(tmp).map_err(|e| format!("create temp: {e}"))?;
    let mut hasher = Sha256::new();
    let mut reader = resp;
    let mut buf = [0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = reader.read(&mut buf).map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        use std::io::Write;
        file.write_all(&buf[..n]).map_err(|e| format!("write: {e}"))?;
        total += n as u64;
    }
    file.sync_all().map_err(|e| format!("sync: {e}"))?;
    drop(file);

    let digest = hasher.finalize();
    let got = hex_lower(&digest);
    match expected_sha256 {
        Some(expected) => {
            if !got.eq_ignore_ascii_case(expected) {
                return Err(format!("sha256 mismatch: got {got}"));
            }
        }
        None => {
            crate::elog!("[model_asset] WARNING: SHA256 not pinned; skipping verification (got {got})");
        }
    }

    std::fs::rename(tmp, dest).map_err(|e| format!("rename into place: {e}"))?;
    Ok(total)
}

/// Lowercase hex of a byte slice. Small and dependency-free (avoids pulling in `hex`).
fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
        s.push(char::from_digit((b & 0xf) as u32, 16).unwrap_or('0'));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_lower_encodes_bytes() {
        assert_eq!(hex_lower(&[0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
    }

    /// Issue #267: a stalled endpoint (accepts, never responds) must fail within the injected
    /// bound instead of blocking the caller forever, and must not create the destination file.
    #[test]
    fn stalled_server_times_out_without_creating_dest() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        // Hold the accepted socket open without ever writing a response; the thread outlives the
        // client's timeout and is left to exit with the process.
        std::thread::spawn(move || {
            let conn = listener.accept();
            std::thread::sleep(Duration::from_secs(2));
            drop(conn);
        });

        let dir =
            std::env::temp_dir().join(format!("shogun-model-asset-stall-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let tmp = dir.join("asset.part");
        let dest = dir.join("asset.bin");

        let sink = crate::llm::traceability::RecordingSink::new();
        let started = std::time::Instant::now();
        let result = download_verified_with_timeout(
            &format!("http://{addr}/model.bin"),
            &tmp,
            &dest,
            None,
            Duration::from_millis(250),
            &sink,
        );

        assert!(result.is_err(), "a stalled endpoint must error, not hang");
        // The request was on the ledger before it left (issue #831), failed or not.
        let rows = sink.records();
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].route, rows[0].purpose.as_str()), (Route::Web, TRACE_PURPOSE));
        assert_eq!(rows[0].url.as_deref(), Some(format!("http://{addr}/model.bin").as_str()));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the failure must arrive within the bound, not at some transport default"
        );
        assert!(!dest.exists(), "no destination file may appear on failure");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
