//! The Memory API token list the two standalone binaries load at start-up (feature `desktop`).
//!
//! On a Mac it is the app's Keychain item, read **uncached** (issue #744): the desktop rewrites
//! that item on every issue or revoke, and the live re-check (`shogun_mcp::live_tokens`) must see
//! that; only a missing item means "none issued", which `memory_api_settings::token_blob_read`
//! classifies in one place. Off macOS the feature-on build takes the same env token the server
//! build takes (`SHOGUN_API_TOKEN`), so the binaries behave alike wherever there is no Keychain.

use shogun_mcp::memory_api::TokenRegistry;

/// The token registry for `shogun-api` / `shogun-mcp`: the Keychain list on a Mac, else
/// `SHOGUN_API_TOKEN`.
#[cfg(target_os = "macos")]
pub fn load_token_registry() -> Result<TokenRegistry, String> {
    use shogun_mcp::memory_api_settings::{self, TOKENS_KEYCHAIN_ACCOUNT};
    let mut tokens = TokenRegistry::new();
    let blob = memory_api_settings::load_token_blob_with_migration(
        || {
            memory_api_settings::token_blob_read(
                shogun_integrations::keychain_store::get_generic_secret_uncached(
                    TOKENS_KEYCHAIN_ACCOUNT,
                )
                .map_err(|error| error.code()),
            )
        },
        |bytes| {
            shogun_integrations::keychain_store::set_generic_secret(TOKENS_KEYCHAIN_ACCOUNT, bytes)
                .map_err(|_| "could not migrate Memory API token verifiers".to_string())
        },
    )?;
    // Each record brings its scope (issue #400); a pre-scope record reads as `full`. The app
    // connections (issue #963) come from the same item.
    tokens.register_blob(&blob)?;
    Ok(tokens)
}

#[cfg(not(target_os = "macos"))]
pub fn load_token_registry() -> Result<TokenRegistry, String> {
    let mut tokens = TokenRegistry::new();
    if let Ok(token) = std::env::var(shogun_mcp::client_config::API_TOKEN_ENV) {
        if !token.is_empty() {
            tokens.issue(token);
        }
    }
    Ok(tokens)
}
