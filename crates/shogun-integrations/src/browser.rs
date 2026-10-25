//! The Mac app's browser launcher (features `live` + `desktop`): `open <url>` on macOS. Off macOS a
//! desktop build reports the same failure the server build does, so the caller shows the URL.

#[cfg(target_os = "macos")]
pub(crate) fn open(url: &str) -> Result<(), String> {
    let status = std::process::Command::new("open")
        .arg(url)
        .status()
        .map_err(|e| format!("open failed: {e}"))?;
    // `open` exits nonzero when no browser takes the URL; waiting for a redirect that can never
    // come would hold "Connecting…" for the whole flow timeout.
    outcome(status.success())
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn open(_url: &str) -> Result<(), String> {
    Err("browser-open is only wired for macOS".to_string())
}

/// A nonzero exit from the browser opener is a failure, not a five-minute wait.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn outcome(succeeded: bool) -> Result<(), String> {
    if succeeded {
        Ok(())
    } else {
        Err("open exited with a failure status".to_string())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_failed_browser_open_fails_fast() {
        assert!(super::outcome(true).is_ok());
        assert!(super::outcome(false).is_err());
    }
}
