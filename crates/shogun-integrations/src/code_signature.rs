//! The macOS side of the launcher check (issue #979): the processes above `shogun-mcp`, nearest
//! first, each tested against a code requirement. The decision is
//! [`shogun_mcp::launcher_check::judge`], tested on Linux; this file only reads the process tree
//! and asks Security.framework.
//!
//! Each process is looked up by pid (`SecCodeCopyGuestWithAttributes` with `kSecGuestAttributePid`)
//! and tested with `SecCodeCheckValidity`: first against [`APPLE_REQUIREMENT`] (macOS's own
//! programs, looked through), then against the expected team's requirement. Any failure to read a
//! process reads as [`Ancestor::Unreadable`], and a signature that does not satisfy the
//! requirement for any reason (another team, unsigned, no longer valid) as [`Ancestor::Other`]:
//! both refuse.
//!
//! Known limit: a pid is looked up after it was read, so a process that exits in between and whose
//! pid is reused is judged as the new one. Closing that needs the parent's audit token, which a
//! child cannot get from its parent today.
//!
//! Called only for a connection that has an expected signer
//! ([`shogun_mcp::launcher_check::EXPECTED_SIGNERS`]); with none listed it never runs.

use std::str::FromStr;

use security_framework::os::macos::code_signing::{
    Flags, GuestAttributes, SecCode, SecRequirement,
};
use shogun_mcp::launcher_check::{Ancestor, APPLE_REQUIREMENT};

/// The parent of `pid`, or `None` at the top of the tree (launchd's parent) or when it cannot be
/// read.
pub fn parent_pid(pid: i32) -> Option<i32> {
    let size = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    // SAFETY: `info` is a zeroed `proc_bsdinfo` of exactly `size` bytes, and PROC_PIDTBSDINFO
    // writes at most that many.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    // SAFETY: zero-initialised plain integers, filled in full by the call above.
    let info = unsafe { info.assume_init() };
    i32::try_from(info.pbi_ppid)
        .ok()
        .filter(|parent| *parent > 0)
}

/// How the process `pid` is signed, against `requirement` (the expected team's).
pub fn classify(pid: i32, requirement: &str) -> Ancestor {
    let (Ok(apple), Ok(expected)) = (
        SecRequirement::from_str(APPLE_REQUIREMENT),
        SecRequirement::from_str(requirement),
    ) else {
        return Ancestor::Unreadable;
    };
    let mut attributes = GuestAttributes::new();
    attributes.set_pid(pid);
    let Ok(code) = SecCode::copy_guest_with_attribues(None, &attributes, Flags::NONE) else {
        return Ancestor::Unreadable;
    };
    if code.check_validity(Flags::NONE, &apple).is_ok() {
        Ancestor::Apple
    } else if code.check_validity(Flags::NONE, &expected).is_ok() {
        Ancestor::Expected
    } else {
        Ancestor::Other
    }
}

/// The processes above this one, nearest first, in the form
/// [`shogun_mcp::launcher_check::judge`] asks for them.
pub fn ancestors() -> impl FnMut(&str) -> Option<Ancestor> {
    let mut next = i32::try_from(std::os::unix::process::parent_id())
        .ok()
        .filter(|pid| *pid > 0);
    move |requirement| {
        let pid = next?;
        next = parent_pid(pid);
        Some(classify(pid, requirement))
    }
}
