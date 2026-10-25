//! When a Keychain read may fall back to the pre-unification service name (issue #744).
//!
//! [`crate::keychain_store`] stores every secret under one service and still reads a few legacy
//! items saved under an older one. The fallback must only answer "the current item does not
//! exist". Any other failure (the user clicked Deny, the Keychain is locked, interaction is not
//! allowed in a background launch) is the real answer and must reach the caller unchanged:
//! reporting it as the legacy lookup's "not found" would make callers that treat "not found" as
//! "nothing issued yet" open up (the Memory API token list reads as empty, i.e. process-trusted
//! full scope).
//!
//! Pure, so it is tested on every platform; the macOS store only supplies the two lookups.

/// `errSecItemNotFound`: the only status that means "no such item".
pub const ITEM_NOT_FOUND: i32 = -25300;

/// Delete both service entries. A missing entry is already deleted; other failures must surface.
pub fn delete_current_and_legacy(
    current: impl FnOnce() -> Result<(), i32>,
    legacy: impl FnOnce() -> Result<(), i32>,
) -> Result<(), i32> {
    // Evaluate both before inspecting either result, so one denied delete cannot skip the other.
    for result in [current(), legacy()] {
        match result {
            Ok(()) | Err(ITEM_NOT_FOUND) => {}
            Err(status) => return Err(status),
        }
    }
    Ok(())
}

/// Read from the current service, and from the legacy service only when the current one reports
/// [`ITEM_NOT_FOUND`]. `Err` carries the `OSStatus` the caller should see: the current service's
/// status for any failure other than not-found, otherwise the legacy lookup's status.
pub fn read_with_legacy_fallback<T>(
    current: impl FnOnce() -> Result<T, i32>,
    legacy: impl FnOnce() -> Result<T, i32>,
) -> Result<Found<T>, i32> {
    match current() {
        Ok(value) => Ok(Found::Current(value)),
        Err(ITEM_NOT_FOUND) => legacy().map(Found::Legacy),
        Err(status) => Err(status),
    }
}

/// Where a secret was found. A legacy hit is migrated by the caller when it may write.
#[derive(Debug, PartialEq, Eq)]
pub enum Found<T> {
    Current(T),
    Legacy(T),
}

impl<T> Found<T> {
    pub fn into_inner(self) -> T {
        match self {
            Found::Current(value) | Found::Legacy(value) => value,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // errSecUserCanceled / errSecInteractionNotAllowed / errSecAuthFailed.
    const USER_CANCELED: i32 = -128;
    const INTERACTION_NOT_ALLOWED: i32 = -25308;
    const AUTH_FAILED: i32 = -25293;

    #[test]
    fn deleting_absent_entries_succeeds() {
        for current in [Ok(()), Err(ITEM_NOT_FOUND)] {
            for legacy in [Ok(()), Err(ITEM_NOT_FOUND)] {
                assert_eq!(delete_current_and_legacy(|| current, || legacy), Ok(()));
            }
        }
    }

    #[test]
    fn deletion_reports_a_failure_in_either_service() {
        for status in [USER_CANCELED, INTERACTION_NOT_ALLOWED, AUTH_FAILED] {
            assert_eq!(
                delete_current_and_legacy(|| Ok(()), || Err(status)),
                Err(status)
            );
            assert_eq!(
                delete_current_and_legacy(|| Err(status), || Ok(())),
                Err(status)
            );
        }
    }

    #[test]
    fn deletion_attempts_both_services_even_when_one_fails() {
        let called = std::cell::Cell::new(0);
        let result = delete_current_and_legacy(
            || {
                called.set(called.get() + 1);
                Err(AUTH_FAILED)
            },
            || {
                called.set(called.get() + 1);
                Ok(())
            },
        );
        assert_eq!(called.get(), 2);
        assert_eq!(result, Err(AUTH_FAILED));
    }

    #[test]
    fn a_current_hit_never_touches_the_legacy_service() {
        let found = read_with_legacy_fallback(|| Ok::<_, i32>(1), || panic!("legacy read"));
        assert_eq!(found, Ok(Found::Current(1)));
    }

    #[test]
    fn only_not_found_falls_back_to_the_legacy_service() {
        assert_eq!(
            read_with_legacy_fallback(|| Err(ITEM_NOT_FOUND), || Ok::<_, i32>(2)),
            Ok(Found::Legacy(2))
        );
        assert_eq!(
            read_with_legacy_fallback::<u8>(|| Err(ITEM_NOT_FOUND), || Err(ITEM_NOT_FOUND)),
            Err(ITEM_NOT_FOUND),
            "absent under both names is still plain not-found"
        );
    }

    #[test]
    fn a_denied_or_locked_read_is_never_reported_as_not_found() {
        for status in [USER_CANCELED, INTERACTION_NOT_ALLOWED, AUTH_FAILED, -1] {
            let got = read_with_legacy_fallback::<u8>(
                || Err(status),
                || panic!("legacy lookup would turn {status} into not-found"),
            );
            assert_eq!(got, Err(status), "status {status} must reach the caller");
        }
    }

    #[test]
    fn a_legacy_failure_other_than_not_found_is_reported_too() {
        assert_eq!(
            read_with_legacy_fallback::<u8>(|| Err(ITEM_NOT_FOUND), || Err(USER_CANCELED)),
            Err(USER_CANCELED)
        );
    }

    #[test]
    fn into_inner_returns_the_value_from_either_service() {
        assert_eq!(Found::Current(3).into_inner(), 3);
        assert_eq!(Found::Legacy(4).into_inner(), 4);
    }
}
