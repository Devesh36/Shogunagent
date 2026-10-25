//! Issue #780: "Delete everything" removes every user secret from the Keychain. The accounts are
//! declared across the tree (this crate, the MCP crate, the macOS shell), so this test reads the
//! sources and fails when an account is in neither `USER_SECRET_ACCOUNTS` (deleted) nor
//! `KEPT_ACCOUNTS` (kept on purpose). A new Keychain account has to be placed before it ships.

use std::path::{Path, PathBuf};

use shogun_integrations::keychain_accounts::{KEPT_ACCOUNTS, USER_SECRET_ACCOUNTS};

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target" || n == "node_modules") {
                continue;
            }
            rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// The string literal on a `const …ACCOUNT…: &str = "…";` line, or a `"<provider>-byok"` literal.
fn declared_accounts(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    for line in source.lines() {
        let trimmed = line.trim_start();
        let is_account_const = (trimmed.starts_with("const ") || trimmed.starts_with("pub const "))
            && trimmed.contains("ACCOUNT")
            && trimmed.contains(": &str = \"");
        for literal in line.split('"').skip(1).step_by(2) {
            let byok = literal.len() > "-byok".len()
                && literal.ends_with("-byok")
                && literal.chars().all(|c| c.is_ascii_lowercase() || c == '-');
            if (is_account_const && !literal.is_empty()) || byok {
                found.push(literal.to_string());
            }
        }
    }
    found
}

#[test]
fn every_keychain_account_is_deleted_or_deliberately_kept() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    rust_files(&root.join("crates"), &mut files);
    rust_files(&root.join("apps/desktop/src-tauri/src"), &mut files);
    assert!(files.len() > 50, "the source tree was not found from {}", root.display());

    let mut unplaced = Vec::new();
    let mut seen = 0;
    for file in &files {
        let Ok(source) = std::fs::read_to_string(file) else { continue };
        for account in declared_accounts(&source) {
            seen += 1;
            if !USER_SECRET_ACCOUNTS.contains(&account.as_str()) && !KEPT_ACCOUNTS.contains(&account.as_str()) {
                unplaced.push(format!("{account} ({})", file.display()));
            }
        }
    }
    assert!(seen > 10, "the account scan found almost nothing; the patterns no longer match the tree");
    unplaced.sort();
    unplaced.dedup();
    assert!(unplaced.is_empty(), "Keychain accounts neither deleted nor kept: {unplaced:#?}");
}

#[test]
fn the_account_profile_is_kept_with_the_session() {
    // "Delete everything" keeps the session, so the first name it greets with stays too.
    assert!(KEPT_ACCOUNTS.contains(&"account-session"));
    assert!(KEPT_ACCOUNTS.contains(&"account-local-profile"));
}

#[test]
fn the_deleted_and_kept_lists_do_not_overlap() {
    for account in USER_SECRET_ACCOUNTS {
        assert!(!KEPT_ACCOUNTS.contains(account), "{account} is both deleted and kept");
    }
}
