//! macOS Keychain token store (invariant 7: secrets never in a file/DB/log).
//!
//! One generic-password entry per service holding the serialized [`crate::oauth::TokenSet`] (access +
//! refresh + expiry), keyed `"<source>-tokenset"` (e.g. `gmail-tokenset`) under the SHOGUN Keychain
//! service ([`keychain_store::SERVICE`]). Read + written only by [`crate::token::TokenManager`].
//!
//! macOS-only (`security-framework`) and — unlike the network transport — NOT behind the `live`
//! feature: it has no network dependency, so it type-checks for the Apple target without a TLS/C
//! toolchain.
#![cfg(target_os = "macos")]
use shogun_mcp::scope::Service;

use crate::keychain_store;
use crate::oauth::TokenSet;
use crate::token::{self, GrantStore, TokenLoadError};

/// A Keychain-backed [`GrantStore`] and [`crate::token_store::TokenStore`].
pub struct KeychainTokenStore;

impl KeychainTokenStore {
    /// `keychain_service` is ignored — all secrets use [`keychain_store::SERVICE`].
    pub fn new(_keychain_service: impl Into<String>) -> Self {
        Self
    }

    fn account(service: Service) -> String {
        format!("{}-tokenset", service.source_str())
    }
}

impl GrantStore for KeychainTokenStore {
    fn load(&self, service: Service) -> Result<Option<TokenSet>, TokenLoadError> {
        let account = Self::account(service);
        let bytes = match keychain_store::get_generic_secret_read_only(&account) {
            Ok(bytes) => bytes,
            Err(error) => {
                return match TokenLoadError::from_keychain_status(error.code()) {
                    None => Ok(None),
                    Some(error) => Err(error),
                };
            }
        };
        let blob = String::from_utf8(bytes).map_err(|_| TokenLoadError::Unavailable)?;
        token::deserialize(&blob).map(Some).map_err(|_| TokenLoadError::Unavailable)
    }

    fn save(&self, service: Service, tokens: &TokenSet) -> Result<(), String> {
        let account = Self::account(service);
        let blob = token::serialize(tokens)?;
        keychain_store::set_generic_secret(&account, blob.as_bytes())
            .map_err(|_| format!("keychain write failed for {}", service.source_str()))
    }

    fn delete(&self, service: Service) -> Result<(), String> {
        let account = Self::account(service);
        keychain_store::delete_generic_secret(&account)
            .map_err(|_| format!("keychain delete failed for {}", service.source_str()))
    }
}

/// The principal × slot store (E5-T5) over the same Keychain items. A Mac holds one person's
/// grants, so only [`crate::token_store::LOCAL_PRINCIPAL`] is served; any other principal is
/// refused rather than mapped onto this Mac's items. Connector slots use the same
/// `<source>-tokenset` accounts as [`GrantStore`], so both views read one grant.
impl crate::token_store::TokenStore for KeychainTokenStore {
    fn get(
        &self,
        principal: &str,
        slot: crate::token_store::CredentialSlot,
    ) -> Result<Option<crate::token_store::Secret>, crate::token_store::TokenStoreError> {
        use crate::token_store::{Secret, TokenStoreError};
        local_only(principal)?;
        let bytes = match keychain_store::get_generic_secret_read_only(&slot.account()) {
            Ok(bytes) => bytes,
            Err(error) => {
                return match TokenLoadError::from_keychain_status(error.code()) {
                    None => Ok(None),
                    Some(TokenLoadError::KeychainLocked) => Err(TokenStoreError::Locked),
                    Some(TokenLoadError::Unavailable) => Err(TokenStoreError::Unavailable),
                };
            }
        };
        String::from_utf8(bytes).map(|value| Some(Secret::new(value))).map_err(|_| TokenStoreError::Unavailable)
    }

    fn put(
        &self,
        principal: &str,
        slot: crate::token_store::CredentialSlot,
        value: &crate::token_store::Secret,
    ) -> Result<(), crate::token_store::TokenStoreError> {
        local_only(principal)?;
        keychain_store::set_generic_secret(&slot.account(), value.expose().as_bytes())
            .map_err(|_| crate::token_store::TokenStoreError::Write)
    }

    fn delete(
        &self,
        principal: &str,
        slot: crate::token_store::CredentialSlot,
    ) -> Result<(), crate::token_store::TokenStoreError> {
        local_only(principal)?;
        keychain_store::delete_generic_secret(&slot.account())
            .map_err(|_| crate::token_store::TokenStoreError::Write)
    }
}

fn local_only(principal: &str) -> Result<(), crate::token_store::TokenStoreError> {
    if principal == crate::token_store::LOCAL_PRINCIPAL {
        Ok(())
    } else {
        Err(crate::token_store::TokenStoreError::InvalidPrincipal)
    }
}
