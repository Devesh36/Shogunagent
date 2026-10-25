//! The standalone faces must read the Memory API token list from the Keychain without the
//! in-process cache (issue #744).
//!
//! The desktop rewrites that item on every issue or revoke, and `shogun-mcp` / `shogun-api`
//! re-check every call against it (`shogun_mcp::live_tokens`). A cached read would keep the list
//! from its first read, so a revoked token would go on working until the client restarted.
//! The read itself is macOS-only code that no Linux test reaches: since E1-T7 it lives once, in
//! `shogun_core::desktop::tokens` (feature `desktop`), and both binaries call it. This pins the
//! call site in that module and in both binaries, and that a failed read is classified by the
//! shared `memory_api_settings::token_blob_read` rather than by hand.

const FACES: [(&str, &str); 2] = [
    ("shogun_mcp.rs", include_str!("../src/bin/shogun_mcp.rs")),
    ("shogun_api.rs", include_str!("../src/bin/shogun_api.rs")),
];

const KEYCHAIN_READ: (&str, &str) =
    ("desktop/tokens.rs", include_str!("../src/desktop/tokens.rs"));

/// The source with all whitespace removed, so formatting cannot hide or fake a call.
fn squeezed(source: &str) -> String {
    source.chars().filter(|c| !c.is_whitespace()).collect()
}

#[test]
fn the_shared_keychain_read_is_uncached_and_classified_in_one_place() {
    let (name, source) = KEYCHAIN_READ;
    let code = squeezed(source);
    assert!(
        code.contains("keychain_store::get_generic_secret_uncached(TOKENS_KEYCHAIN_ACCOUNT"),
        "{name}: the token list must be read with get_generic_secret_uncached"
    );
    assert!(
        !code.contains("keychain_store::get_generic_secret(TOKENS_KEYCHAIN_ACCOUNT"),
        "{name}: the cached read would keep a revoked token alive"
    );
    assert!(
        code.contains("memory_api_settings::token_blob_read("),
        "{name}: only a missing item may mean \"no token issued\""
    );
}

#[test]
fn both_faces_take_the_shared_read_and_reuse_its_failure_reason() {
    for (name, source) in FACES {
        let code = squeezed(source);
        assert!(
            code.contains("shogun_core::desktop::tokens::load_token_registry()"),
            "{name}: the Keychain token list is read through the one shared site"
        );
        assert!(
            !code.contains("keychain_store::"),
            "{name}: no binary reads the Keychain on its own"
        );
        assert!(
            code.contains(".last_error()"),
            "{name}: the start-up refusal reuses the failed read's reason instead of reading again"
        );
    }
}
