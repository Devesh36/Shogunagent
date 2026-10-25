//! Copying a newly issued REST / script token (issue #979).
//!
//! The token is shown once in Settings, and copying it is the one moment it sits somewhere other
//! apps can read. The desktop writes it to the general pasteboard itself (not the webview), so it
//! can mark it the way clipboard-history apps and Universal Clipboard understand
//! (<http://nspasteboard.org>):
//!
//! - [`CONCEALED_TYPE`]: a history app that honours it does not show (or keeps no copy of) the
//!   value.
//! - [`TRANSIENT_TYPE`]: the value is short-lived; a history app does not record it at all.
//! - The write is made with `NSPasteboardContentsCurrentHostOnly`, so Universal Clipboard does not
//!   carry it to the user's other devices.
//!
//! After [`CLEAR_AFTER`] the desktop clears the pasteboard, but only if it still holds this copy:
//! [`should_clear`] compares the pasteboard's change count with the one the write produced, so
//! anything the user copied in the meantime is left alone.
//!
//! This module is the platform-free part (the marker list, the delay, the decision, the shape a
//! copyable value must have), tested on Linux. The AppKit calls are in the desktop
//! (`memory_api_settings::mac::memory_api_copy_token`).

use std::time::Duration;

/// nspasteboard.org: "this value is a secret; do not show it". Written with an empty value.
pub const CONCEALED_TYPE: &str = "org.nspasteboard.ConcealedType";

/// nspasteboard.org: "this value is short-lived; do not record it". Written with an empty value.
pub const TRANSIENT_TYPE: &str = "org.nspasteboard.TransientType";

/// The marker types written next to the token's plain text, in this order.
pub const MARKER_TYPES: [&str; 2] = [CONCEALED_TYPE, TRANSIENT_TYPE];

/// The plain-text type the token itself is written as.
pub const TEXT_TYPE: &str = "public.utf8-plain-text";

/// How long a copied token stays on the pasteboard.
pub const CLEAR_AFTER: Duration = Duration::from_secs(60);

/// Whether the timer may clear the pasteboard: only when nothing was copied since the token was
/// written (`written` is the change count right after the write, `now` the current one). Any other
/// count means the pasteboard holds something newer that is not ours to remove.
pub fn should_clear(written: isize, now: isize) -> bool {
    written == now
}

/// The prefix every issued Memory API token starts with.
pub const TOKEN_PREFIX: &str = "shogun_";

/// Whether `text` has the shape of a token Settings issues (`shogun_` and lowercase hex). The
/// copy command refuses anything else, so it cannot be used to put arbitrary text on the
/// pasteboard under the "secret" markers.
pub fn is_issued_token(text: &str) -> bool {
    text.strip_prefix(TOKEN_PREFIX).is_some_and(|hex| {
        (32..=128).contains(&hex.len())
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_are_the_nspasteboard_org_types() {
        assert_eq!(
            MARKER_TYPES,
            [
                "org.nspasteboard.ConcealedType",
                "org.nspasteboard.TransientType"
            ]
        );
        assert_eq!(TEXT_TYPE, "public.utf8-plain-text");
    }

    #[test]
    fn clears_after_sixty_seconds() {
        assert_eq!(CLEAR_AFTER, Duration::from_secs(60));
    }

    #[test]
    fn clears_only_when_the_pasteboard_still_holds_the_token() {
        assert!(should_clear(41, 41));
        // The user (or another app) copied something since: leave it.
        assert!(!should_clear(41, 42));
        assert!(!should_clear(41, 50));
        // A pasteboard server restart can reset the count; anything but an exact match is not ours.
        assert!(!should_clear(41, 0));
    }

    #[test]
    fn copies_only_token_shaped_text() {
        let minted = format!("shogun_{}", "0a".repeat(24));
        assert!(is_issued_token(&minted));
        assert!(is_issued_token(&format!("shogun_{}", "f".repeat(32))));
        for text in [
            "",
            "shogun_",
            "shogun_0123",
            "hello",
            "shogun_PASTE_ISSUED_TOKEN",
            &format!("shogun_{}", "A".repeat(48)),
            &format!(" shogun_{}", "a".repeat(48)),
            &format!("shogun_{} ", "a".repeat(48)),
            &format!("shogun_{}", "a".repeat(129)),
            &format!("other_{}", "a".repeat(48)),
        ] {
            assert!(
                !is_issued_token(text),
                "{text:?} must not be copyable as a token"
            );
        }
    }
}
