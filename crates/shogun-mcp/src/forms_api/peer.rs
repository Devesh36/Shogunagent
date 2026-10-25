//! The peer-credential check on the Mac app's owner-only forms socket (feature `desktop`,
//! E1-T7). Unix only: `getpeereid` on the BSD family (macOS included), `SO_PEERCRED` on Linux.

/// Check a connected IPC peer is the same effective user before sharing credentials.
pub fn same_uid(stream: &std::os::unix::net::UnixStream) -> bool {
    use std::os::fd::AsRawFd;
    // SAFETY: geteuid takes no arguments or pointers.
    let ours = unsafe { libc::geteuid() };
    #[cfg(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd"
    ))]
    {
        let mut uid = 0;
        let mut gid = 0;
        // SAFETY: live socket fd and valid uid/gid output pointers.
        unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) == 0 && uid == ours }
    }
    #[cfg(target_os = "linux")]
    {
        let mut credentials: libc::ucred = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: live socket fd, correctly sized ucred output buffer and length pointer.
        unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credentials as *mut libc::ucred).cast(),
                &mut len,
            ) == 0
                && credentials.uid == ours
        }
    }
    #[cfg(not(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "linux"
    )))]
    {
        let _ = (stream, ours);
        false
    }
}
