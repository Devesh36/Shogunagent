//! Accessibility degraded mode (FR-CAP-08 / FR-OB-04; owner decision 2026-10-03, issue #778).
//!
//! Declining Accessibility, or a grant that does not survive a re-signed update, must not lock
//! the user out of the app. Setup can finish without it. Only the screen-reading features stop
//! (context capture, context actions that need screen text, anything else that needs the
//! Accessibility permission). Settings, chat, meeting notes, Morning Brief and Connections stay
//! usable, and a non-blocking banner offers the grant. When macOS later reports the permission,
//! the degraded mode ends without a restart.
//!
//! These are the decisions the desktop shell enforces, kept pure so they run on Linux CI. The
//! shell supplies the live permission state and the persisted onboarding record; nothing here
//! prompts or touches macOS.

use std::time::Duration;

/// Whether a setup write must wait for Accessibility.
///
/// `requires_choice` is true for every step past the permissions screen and for the write that
/// newly completes setup. Such a write needs either the grant or the user's explicit choice to
/// continue without it (`skipped`, which is the persisted flag or this write's own request).
/// Without one of those, the permissions screen cannot be bypassed by a stale or forged write.
pub fn setup_write_blocked(requires_choice: bool, accessibility: bool, skipped: bool) -> bool {
    requires_choice && !accessibility && !skipped
}

/// The persisted skip flag after a setup write. A grant always clears it, so a later loss of
/// trust (for example after a re-signed update) reopens the repair card instead of being treated
/// as the earlier "not now".
pub fn skipped_after_write(prev_skipped: bool, skip_requested: bool, accessibility: bool) -> bool {
    !accessibility && (prev_skipped || skip_requested)
}

/// Whether the record holds a stale skip that a grant has made obsolete. The shell persists the
/// cleared flag when this is true, outside any setup write.
pub fn skip_is_obsolete(skipped: bool, accessibility: bool) -> bool {
    skipped && accessibility
}

/// Degraded mode: setup is finished and Accessibility is not granted. The app is usable; only
/// the screen-reading features are off, and the banner offering the grant is shown.
pub fn degraded(completed: bool, accessibility: bool) -> bool {
    completed && !accessibility
}

/// Whether a finished setup reopens the focused permissions card at launch: trust was lost after
/// the user had granted it. An explicit skip is respected; the banner covers that case.
pub fn needs_permissions_repair(completed: bool, accessibility: bool, skipped: bool) -> bool {
    completed && !accessibility && !skipped
}

/// A change in Accessibility trust seen by [`TrustWatch`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrustEdge {
    Granted,
    Revoked,
}

/// Edge detector over the silent trust check, so the shell can end (or show) the degraded mode
/// without a relaunch and without polling consumers.
#[derive(Clone, Copy, Debug, Default)]
pub struct TrustWatch {
    last: Option<bool>,
}

impl TrustWatch {
    pub const fn new() -> Self {
        Self { last: None }
    }

    /// Record one sample. The first sample reports `Granted` when trusted, so a permission granted
    /// while the app was not running still clears a stale skip; an untrusted first sample reports
    /// nothing, because every consumer already reads the live state at startup.
    pub fn observe(&mut self, trusted: bool) -> Option<TrustEdge> {
        let previous = self.last.replace(trusted);
        match (previous, trusted) {
            (None | Some(false), true) => Some(TrustEdge::Granted),
            (Some(true), false) => Some(TrustEdge::Revoked),
            _ => None,
        }
    }
}

/// How long the shell waits between silent trust checks. Quick while degraded, so the banner
/// clears within moments of the user flipping the switch; slower once trusted, where the only
/// job is to notice a revocation.
pub fn poll_interval(trusted: bool) -> Duration {
    if trusted {
        Duration::from_secs(5)
    } else {
        Duration::from_secs(2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permissions_screen_cannot_be_bypassed_without_a_choice() {
        assert!(setup_write_blocked(true, false, false));
        assert!(!setup_write_blocked(true, true, false));
        assert!(!setup_write_blocked(false, false, false));
    }

    #[test]
    fn an_explicit_skip_lets_setup_finish_without_accessibility() {
        assert!(!setup_write_blocked(true, false, true));
    }

    #[test]
    fn a_grant_clears_the_skip_and_a_request_sets_it() {
        assert!(skipped_after_write(false, true, false));
        assert!(skipped_after_write(true, false, false));
        assert!(!skipped_after_write(false, false, false));
        assert!(!skipped_after_write(true, true, true));
        assert!(!skipped_after_write(true, false, true));
    }

    #[test]
    fn only_a_granted_skip_is_obsolete() {
        assert!(skip_is_obsolete(true, true));
        assert!(!skip_is_obsolete(true, false));
        assert!(!skip_is_obsolete(false, true));
    }

    #[test]
    fn degraded_mode_needs_finished_setup_and_no_grant() {
        assert!(degraded(true, false));
        assert!(!degraded(true, true));
        assert!(!degraded(false, false));
    }

    #[test]
    fn a_skip_keeps_the_repair_card_closed_but_lost_trust_reopens_it() {
        assert!(!needs_permissions_repair(true, false, true));
        assert!(needs_permissions_repair(true, false, false));
        assert!(!needs_permissions_repair(true, true, false));
        assert!(!needs_permissions_repair(false, false, false));
    }

    #[test]
    fn trust_watch_reports_edges_only() {
        let mut watch = TrustWatch::new();
        assert_eq!(watch.observe(false), None);
        assert_eq!(watch.observe(false), None);
        assert_eq!(watch.observe(true), Some(TrustEdge::Granted));
        assert_eq!(watch.observe(true), None);
        assert_eq!(watch.observe(false), Some(TrustEdge::Revoked));
    }

    #[test]
    fn trust_watch_treats_a_trusted_first_sample_as_granted() {
        let mut watch = TrustWatch::new();
        assert_eq!(watch.observe(true), Some(TrustEdge::Granted));
        assert_eq!(watch.observe(true), None);
    }

    #[test]
    fn degraded_mode_polls_faster_than_trusted() {
        assert!(poll_interval(false) < poll_interval(true));
        assert!(poll_interval(false) <= Duration::from_secs(2));
    }
}
