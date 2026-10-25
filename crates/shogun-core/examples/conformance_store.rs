//! `conformance_store <path>`: create a fresh, migrated, encrypted memory store at `<path>` with the
//! key in `SHOGUN_DB_KEY` (64 hex characters), as the desktop app would leave it, and exit.
//!
//! `shogun-api` never creates or migrates a store (#800), so the MCP conformance suite
//! (`packages/conformance`, E36a) uses this to stand up a throwaway sidecar on a temp directory:
//! `scripts/run-local.mjs` there builds this example next to the binary and calls it once. The
//! store is empty; nothing is seeded. Refuses to touch a path that already exists.

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let Some(path) = std::env::args_os().nth(1).map(PathBuf::from) else {
        eprintln!("conformance_store: usage: conformance_store <path-to-new-memory.db>");
        return ExitCode::from(2);
    };
    if path.exists() {
        eprintln!("conformance_store: refusing to overwrite an existing path");
        return ExitCode::from(2);
    }
    let Some(key) = std::env::var("SHOGUN_DB_KEY")
        .ok()
        .and_then(|hex| shogun_memory::DbKey::from_hex(hex.trim()))
    else {
        eprintln!("conformance_store: SHOGUN_DB_KEY must be 64 hex characters");
        return ExitCode::from(2);
    };
    match shogun_memory::open_encrypted(&path, &key) {
        Ok(connection) => {
            drop(connection);
            println!("conformance_store: created a store at schema {}", shogun_memory::LATEST_SCHEMA_VERSION);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("conformance_store: {e}");
            ExitCode::FAILURE
        }
    }
}
