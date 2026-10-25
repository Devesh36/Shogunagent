//! What `shogun` says when no REST server answers (issue #762).
//!
//! The installed app ships `shogun-api` beside `shogun` (`ShogunAI.app/Contents/MacOS`), so the
//! hint names that file and the database it has to be pointed at. A build from the repository
//! without one beside it gets the command that builds and runs one.

use std::path::{Path, PathBuf};

/// The REST server's file name, in the app bundle and in a cargo target directory alike.
pub const API_BINARY: &str = "shogun-api";

/// The `shogun-api` beside the `shogun` at `exe`, whether or not it exists.
pub fn api_beside(exe: &Path) -> PathBuf {
    exe.with_file_name(API_BINARY)
}

/// The line printed after "could not reach the daemon". `bundled` is the `shogun-api` beside this
/// CLI when there is one; `db` is SHOGUN's own memory.db when its folder is known.
pub fn start_api(bundled: Option<&Path>, db: Option<&Path>) -> String {
    let Some(api) = bundled else {
        return "(is `shogun-api` running?  start it with: cargo run -p shogun-core --features daemon-server --bin shogun-api)".to_string();
    };
    let db = db.map_or_else(
        || "<ShogunAI's memory.db>".to_string(),
        |db| db.display().to_string(),
    );
    format!(
        "(is `shogun-api` running?  start it in another Terminal window with: SHOGUN_DB_PATH=\"{db}\" \"{}\")",
        api.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_server_is_looked_for_beside_the_cli() {
        assert_eq!(
            api_beside(Path::new(
                "/Applications/ShogunAI.app/Contents/MacOS/shogun"
            )),
            PathBuf::from("/Applications/ShogunAI.app/Contents/MacOS/shogun-api")
        );
    }

    #[test]
    fn an_installed_app_is_told_how_to_start_its_own_server() {
        let hint = start_api(
            Some(Path::new(
                "/Applications/ShogunAI.app/Contents/MacOS/shogun-api",
            )),
            Some(Path::new(
                "/Users/me/Library/Application Support/com.syogun.shogunai/memory.db",
            )),
        );
        // Both paths quoted: "Application Support" has a space.
        assert!(
            hint.contains(r#"SHOGUN_DB_PATH="/Users/me/Library/Application Support/com.syogun.shogunai/memory.db" "/Applications/ShogunAI.app/Contents/MacOS/shogun-api""#),
            "{hint}"
        );
        assert!(!hint.contains("cargo"), "{hint}");
    }

    #[test]
    fn a_source_build_without_a_server_beside_it_gets_the_cargo_command() {
        let hint = start_api(None, None);
        assert!(
            hint.contains("cargo run -p shogun-core --features daemon-server --bin shogun-api"),
            "{hint}"
        );
    }

    #[test]
    fn an_unknown_data_folder_still_names_the_database() {
        let hint = start_api(Some(Path::new("/opt/shogun/shogun-api")), None);
        assert!(
            hint.contains("SHOGUN_DB_PATH=\"<ShogunAI's memory.db>\""),
            "{hint}"
        );
    }
}
