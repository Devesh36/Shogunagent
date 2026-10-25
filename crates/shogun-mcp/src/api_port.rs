//! Where a running `shogun-api` is listening (FR-API-01, issue #776).
//!
//! The REST server moves to an ephemeral port when its configured one is taken, so the port a
//! client should dial is only known after the bind. The server records it in a small file next to
//! the memory database, readable only by this user, and `shogun` reads it before falling back to
//! the default. A client that guessed 7464 would otherwise hand its bearer token, and whatever it
//! was writing, to whichever process took that port first.
//!
//! A file left behind by a server that has stopped still names its last port. Dialling it then
//! fails as "could not reach the daemon" unless something has since bound that exact port.

use std::io::Write;
use std::path::{Path, PathBuf};

/// The file name: a sibling of `memory.db`, like `memory_api.json`.
pub const PORT_FILE: &str = "api.port";

/// The port file for a server whose database is `db_path`.
pub fn port_file_path(db_path: &Path) -> PathBuf {
    db_path
        .parent()
        .map(|dir| dir.join(PORT_FILE))
        .unwrap_or_else(|| PathBuf::from(PORT_FILE))
}

/// Record `port`. It is written to a temporary sibling and renamed over the old file, so a reader
/// never sees half a number; on Unix only this user can read it.
pub fn write(path: &Path, port: u16) -> std::io::Result<()> {
    let tmp = path.with_extension("port.tmp");
    let _ = std::fs::remove_file(&tmp);
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(port.to_string().as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// The recorded port, or `None` when the file is missing, unreadable, or not a port.
pub fn read(path: &Path) -> Option<u16> {
    std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("shogun-api-port-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn the_port_file_sits_next_to_the_database() {
        assert_eq!(
            port_file_path(Path::new("/data/com.syogun.shogunai/memory.db")),
            Path::new("/data/com.syogun.shogunai").join(PORT_FILE)
        );
        assert_eq!(port_file_path(Path::new("memory.db")), PathBuf::from("").join(PORT_FILE));
    }

    #[test]
    fn a_written_port_reads_back_and_a_later_one_replaces_it() {
        let dir = scratch("roundtrip");
        let path = dir.join(PORT_FILE);
        assert_eq!(read(&path), None, "no server has run yet");
        write(&path, 53_447).expect("write");
        assert_eq!(read(&path), Some(53_447));
        write(&path, 7_464).expect("rewrite");
        assert_eq!(read(&path), Some(7_464));
        assert!(!path.with_extension("port.tmp").exists(), "no temporary file is left behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn anything_that_is_not_a_port_is_ignored() {
        let dir = scratch("garbage");
        let path = dir.join(PORT_FILE);
        for bad in ["", "0", "70000", "port", "-1"] {
            std::fs::write(&path, bad).expect("seed");
            assert_eq!(read(&path), None, "{bad:?}");
        }
        std::fs::write(&path, " 8123\n").expect("seed");
        assert_eq!(read(&path), Some(8_123), "surrounding whitespace is fine");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn only_this_user_can_read_it() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("mode");
        let path = dir.join(PORT_FILE);
        write(&path, 9_000).expect("write");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
