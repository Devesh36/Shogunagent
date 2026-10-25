//! This Mac's licence device id (issue #777, FR-BIL-08).
//!
//! A licence token is bound to the `device` inside its signed payload. The id it is checked
//! against must not live in the file that carries the token (`billing.json`): copying that one
//! file to another Mac would carry the expected id with it and the paid plan would verify there
//! too.
//!
//! So the id comes from the hardware: the Mac's `IOPlatformUUID`, read from IOKit in-process
//! (no subprocess, no Keychain, so the CLI, the MCP server and the REST face can all derive it),
//! and hashed with a domain tag:
//!
//! ```text
//! device = base64url_nopad(SHA-256("shogunai-device-v1:" + UPPERCASE(IOPlatformUUID)))   // 43 chars
//! ```
//!
//! The hash keeps the id anonymous (FR-BIL-08): it cannot be turned back into the hardware UUID,
//! and the domain tag keeps it from matching any other product's hash of the same UUID. The
//! result fits the licence API's device-id validator (`^[A-Za-z0-9_-]{8,64}$`).
//!
//! Neither the UUID nor the derived id is ever logged.
//!
//! Off macOS, or on a Mac (some VMs) that exposes no usable UUID, there is no hardware id and the
//! callers fall back to the id saved in `billing.json`.

use std::sync::OnceLock;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use sha2::{Digest, Sha256};

/// Domain tag hashed in front of the platform UUID. Changing it changes every Mac's device id,
/// which the licence API counts as a new device — never edit it; add a `-v2` tag instead.
const DEVICE_ID_DOMAIN: &str = "shogunai-device-v1:";

/// Length of a derived device id: the unpadded base64url of a 32-byte SHA-256.
pub const HARDWARE_DEVICE_ID_LEN: usize = 43;

/// Derive the licence device id from an `IOPlatformUUID` string. `None` for anything that is not
/// a canonical 8-4-4-4-12 hex UUID, and for the all-zero UUID some virtual machines report
/// (every such VM would otherwise share one licence seat).
///
/// Case-insensitive: the UUID is upper-cased first, so the id does not depend on how a given
/// macOS version prints it.
pub fn device_id_from_platform_uuid(uuid: &str) -> Option<String> {
    let uuid = uuid.trim();
    if !is_uuid_shaped(uuid) || uuid.bytes().all(|c| c == b'0' || c == b'-') {
        return None;
    }
    let mut hasher = Sha256::new();
    hasher.update(DEVICE_ID_DOMAIN.as_bytes());
    hasher.update(uuid.to_ascii_uppercase().as_bytes());
    let id = URL_SAFE_NO_PAD.encode(hasher.finalize());
    debug_assert_eq!(id.len(), HARDWARE_DEVICE_ID_LEN);
    Some(id)
}

/// A canonical 8-4-4-4-12 hex UUID string. Both an `IOPlatformUUID` and the random device id a
/// pre-#777 build minted have this shape; a hardware-derived id (43 base64url characters) never
/// does, which is how [`crate::DeviceBinding`] tells a legacy token from another Mac's
/// hardware-bound one.
pub(crate) fn is_uuid_shaped(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, c)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                c == b'-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

/// This Mac's hardware-derived licence device id, or `None` when the platform exposes no usable
/// UUID (not macOS, or a VM without one). Read once per process: the UUID does not change while
/// the process runs, and every plan resolution asks.
pub fn hardware_device_id() -> Option<&'static str> {
    static ID: OnceLock<Option<String>> = OnceLock::new();
    ID.get_or_init(|| platform_uuid().as_deref().and_then(device_id_from_platform_uuid))
        .as_deref()
}

#[cfg(target_os = "macos")]
fn platform_uuid() -> Option<String> {
    mac::platform_uuid()
}

#[cfg(not(target_os = "macos"))]
fn platform_uuid() -> Option<String> {
    None
}

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::CStr;
    use std::os::raw::c_char;

    use core_foundation_sys::base::{
        kCFAllocatorDefault, CFAllocatorRef, CFGetTypeID, CFIndex, CFRelease, CFTypeRef,
    };
    use core_foundation_sys::dictionary::{CFDictionaryRef, CFMutableDictionaryRef};
    use core_foundation_sys::string::{
        kCFStringEncodingUTF8, CFStringCreateWithCString, CFStringGetCString, CFStringGetTypeID,
        CFStringRef,
    };

    /// `io_object_t` / `io_service_t` / `io_registry_entry_t` are all `mach_port_t` (`u32`).
    type IoObject = u32;

    /// `kIOMainPortDefault` (formerly `kIOMasterPortDefault`) is `MACH_PORT_NULL`.
    const IO_MAIN_PORT_DEFAULT: u32 = 0;

    /// `IO_OBJECT_NULL`.
    const IO_OBJECT_NULL: IoObject = 0;

    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        /// Returns a +1 matching dictionary (or NULL).
        fn IOServiceMatching(name: *const c_char) -> CFMutableDictionaryRef;
        /// Consumes one reference to `matching`, whether or not a service is found.
        fn IOServiceGetMatchingService(main_port: u32, matching: CFDictionaryRef) -> IoObject;
        /// Returns a +1 CF object (or NULL).
        fn IORegistryEntryCreateCFProperty(
            entry: IoObject,
            key: CFStringRef,
            allocator: CFAllocatorRef,
            options: u32,
        ) -> CFTypeRef;
        fn IOObjectRelease(object: IoObject) -> i32;
    }

    /// `IOPlatformUUID` of `IOPlatformExpertDevice`, as IOKit prints it. Every object created
    /// here is released on every path; nothing is cached or logged.
    pub(super) fn platform_uuid() -> Option<String> {
        // SAFETY: plain IOKit / CoreFoundation calls with C-string literals. Each
        // returned object is checked for NULL before use and released exactly once:
        // `matching` is consumed by IOServiceGetMatchingService, `service` by IOObjectRelease,
        // `key` and `value` by CFRelease. The CFString is only read after its type is checked,
        // into a fixed buffer whose length is passed alongside it.
        unsafe {
            let matching = IOServiceMatching(c"IOPlatformExpertDevice".as_ptr());
            if matching.is_null() {
                return None;
            }
            let service =
                IOServiceGetMatchingService(IO_MAIN_PORT_DEFAULT, matching as CFDictionaryRef);
            if service == IO_OBJECT_NULL {
                return None;
            }
            let key = CFStringCreateWithCString(
                kCFAllocatorDefault,
                c"IOPlatformUUID".as_ptr(),
                kCFStringEncodingUTF8,
            );
            if key.is_null() {
                IOObjectRelease(service);
                return None;
            }
            let value = IORegistryEntryCreateCFProperty(service, key, kCFAllocatorDefault, 0);
            CFRelease(key as CFTypeRef);
            IOObjectRelease(service);
            if value.is_null() {
                return None;
            }
            let mut out = None;
            if CFGetTypeID(value) == CFStringGetTypeID() {
                // A UUID is 36 ASCII characters; 64 leaves room and NUL.
                let mut buf: [c_char; 64] = [0; 64];
                let len = CFIndex::try_from(buf.len()).unwrap_or(0);
                if CFStringGetCString(
                    value as CFStringRef,
                    buf.as_mut_ptr(),
                    len,
                    kCFStringEncodingUTF8,
                ) != 0
                {
                    out = CStr::from_ptr(buf.as_ptr()).to_str().ok().map(str::to_owned);
                }
            }
            CFRelease(value);
            out
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// The server's validator (`apps/website/src/lib/license.ts` `isValidDeviceId`).
    fn server_accepts(id: &str) -> bool {
        (8..=64).contains(&id.len())
            && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    }

    #[test]
    fn derivation_is_stable_anonymous_and_fits_the_server_validator() {
        let uuid = "11111111-2222-3333-4444-555555555555";
        let id = device_id_from_platform_uuid(uuid).expect("a valid UUID derives an id");
        // Pinned: a change here re-keys every Mac's licence seat.
        assert_eq!(id, "4H9iyD3A6qHY2K_QmvBt6BY9XVGThDWcDpTCyvV_iHM");
        assert_eq!(id.len(), HARDWARE_DEVICE_ID_LEN);
        assert!(server_accepts(&id));
        assert!(!id.contains(uuid), "the UUID does not survive hashing");
        assert_eq!(device_id_from_platform_uuid(uuid), Some(id), "same input, same id");
    }

    #[test]
    fn derivation_ignores_case_and_surrounding_space() {
        let upper = device_id_from_platform_uuid("AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE");
        assert_eq!(upper.as_deref(), Some("YLBItJt5pOm21XXUG5u3Yp_iuTj6PjOY-uvskMDp9Ig"));
        assert_eq!(device_id_from_platform_uuid("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"), upper);
        assert_eq!(device_id_from_platform_uuid(" AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE\n"), upper);
        assert_ne!(
            device_id_from_platform_uuid("11111111-2222-3333-4444-555555555555"),
            upper,
            "different Macs, different ids"
        );
    }

    #[test]
    fn rejects_the_all_zero_and_malformed_uuids() {
        for bad in [
            "00000000-0000-0000-0000-000000000000",
            "",
            "not-a-uuid",
            "11111111222233334444555555555555",      // no hyphens
            "11111111-2222-3333-4444-55555555555",    // short
            "11111111-2222-3333-4444-5555555555555",  // long
            "1111111G-2222-3333-4444-555555555555",   // non-hex
            "11111111_2222-3333-4444-555555555555",   // wrong separator
            "11111111-2222-3333-4444-555555555555-0", // trailing junk
        ] {
            assert_eq!(device_id_from_platform_uuid(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_derived_id_is_never_uuid_shaped() {
        let id = device_id_from_platform_uuid("11111111-2222-3333-4444-555555555555").unwrap();
        assert!(!is_uuid_shaped(&id));
        assert!(is_uuid_shaped("0b7f3c1e-5a2d-4c6b-9e8f-1a2b3c4d5e6f"), "a pre-#777 random id");
    }

    #[test]
    fn no_hardware_id_off_macos() {
        if !cfg!(target_os = "macos") {
            assert_eq!(hardware_device_id(), None);
        } else if let Some(id) = hardware_device_id() {
            assert_eq!(id.len(), HARDWARE_DEVICE_ID_LEN);
            assert!(server_accepts(id));
        }
    }
}
