//! [`PrincipalGrants`]: one principal's connector grants on any [`TokenStore`], seen as the
//! per-service [`GrantStore`] the refresh path uses. This is how connector sync is routed through
//! the store (E5-T5): `ConnectorRuntime::restore_from_store` and
//! `ManagedTokenProvider` take a `GrantStore`, and this adapter is that for a file-sealed, hosted
//! or Keychain store alike.
//!
//! Reads the raw grant (`.expose(`) to parse it — allowlisted in
//! `scripts/check-secret-exposure.py`; nothing here logs or returns it beyond the parsed
//! [`TokenSet`] the refresh logic already handles.

use shogun_mcp::scope::Service;

use super::{CredentialSlot, Secret, TokenStore, TokenStoreError};
use crate::oauth::TokenSet;
use crate::token::{self, GrantStore, TokenLoadError};

/// `principal`'s grants in `store`.
pub struct PrincipalGrants<S> {
    store: S,
    principal: String,
}

impl<S: TokenStore> PrincipalGrants<S> {
    pub fn new(store: S, principal: impl Into<String>) -> Self {
        Self { store, principal: principal.into() }
    }

    /// The single-user deployment's grants ([`super::LOCAL_PRINCIPAL`]).
    pub fn local(store: S) -> Self {
        Self::new(store, super::LOCAL_PRINCIPAL)
    }

    pub fn principal(&self) -> &str {
        &self.principal
    }
}

fn load_error(error: TokenStoreError) -> TokenLoadError {
    match error {
        TokenStoreError::Locked => TokenLoadError::KeychainLocked,
        _ => TokenLoadError::Unavailable,
    }
}

impl<S: TokenStore> GrantStore for PrincipalGrants<S> {
    fn load(&self, service: Service) -> Result<Option<TokenSet>, TokenLoadError> {
        let Some(secret) = self
            .store
            .get(&self.principal, CredentialSlot::Connector(service))
            .map_err(load_error)?
        else {
            return Ok(None);
        };
        token::deserialize(secret.expose()).map(Some).map_err(|_| TokenLoadError::Unavailable)
    }

    fn save(&self, service: Service, tokens: &TokenSet) -> Result<(), String> {
        let blob = Secret::new(token::serialize(tokens)?);
        self.store
            .put(&self.principal, CredentialSlot::Connector(service), &blob)
            .map_err(|error| format!("{error} for {}", service.source_str()))
    }

    fn delete(&self, service: Service) -> Result<(), String> {
        self.store
            .delete(&self.principal, CredentialSlot::Connector(service))
            .map_err(|error| format!("{error} for {}", service.source_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token_store::MemorySecretStore;

    fn grant(access: &str) -> TokenSet {
        TokenSet {
            access_token: access.into(),
            refresh_token: Some("refresh".into()),
            expires_at_ms: 1_000,
            account_label: None,
        }
    }

    #[test]
    fn grants_round_trip_per_principal() {
        let store = MemorySecretStore::new();
        let alice = PrincipalGrants::new(&store, "alice");
        let bob = PrincipalGrants::new(&store, "bob");
        alice.save(Service::Slack, &grant("a-access")).unwrap();
        assert_eq!(alice.load(Service::Slack).unwrap(), Some(grant("a-access")));
        assert_eq!(bob.load(Service::Slack).unwrap(), None, "principals never share a grant");
        assert_eq!(alice.load(Service::Gmail).unwrap(), None);
        alice.delete(Service::Slack).unwrap();
        assert_eq!(alice.load(Service::Slack).unwrap(), None);
    }

    /// Connector sync routed through the trait: the refresh path (`ManagedTokenProvider`, what the
    /// live MCP client and `ConnectorRuntime` sync use) runs unchanged over a file-sealed store,
    /// and the refreshed grant lands back sealed for that principal only.
    #[test]
    fn managed_refresh_runs_over_a_sealed_principal_store() {
        use crate::oauth::{AuthConfig, TokenExchange};
        use crate::rpc::TokenProvider;
        use crate::token::ManagedTokenProvider;
        use crate::token_store::{FileSealedTokenStore, SealKey};

        struct Exchange;
        impl TokenExchange for Exchange {
            fn post_form(&self, _endpoint: &str, form: &[(String, String)]) -> Result<String, String> {
                assert!(form.iter().any(|(k, v)| k == "refresh_token" && v == "refresh"));
                Ok(r#"{"access_token":"fresh-access","expires_in":3600}"#.into())
            }
        }

        let dir = std::env::temp_dir().join(format!("shogun_grants_refresh_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = FileSealedTokenStore::new(&dir, |p: &str| SealKey::derive(&[4; 32], p));
        PrincipalGrants::new(&store, "alice").save(Service::GoogleCalendar, &grant("stale")).unwrap();

        let provider = ManagedTokenProvider::new(
            AuthConfig::google("cid", Some("secret".into()), "http://127.0.0.1:0/callback"),
            Exchange,
            PrincipalGrants::new(&store, "alice"),
        )
        .with_clock(|| 10_000_000);
        assert_eq!(provider.access_token(Service::GoogleCalendar).unwrap(), "fresh-access");

        let saved = PrincipalGrants::new(&store, "alice").load(Service::GoogleCalendar).unwrap().unwrap();
        assert_eq!(saved.access_token, "fresh-access");
        assert_eq!(saved.refresh_token.as_deref(), Some("refresh"), "the refresh token is carried forward");
        assert!(PrincipalGrants::new(&store, "bob").load(Service::GoogleCalendar).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_value_is_unavailable_not_absent() {
        let store = MemorySecretStore::new();
        store
            .put("alice", CredentialSlot::Connector(Service::Gmail), &Secret::new("not json"))
            .unwrap();
        let grants = PrincipalGrants::new(&store, "alice");
        assert_eq!(grants.load(Service::Gmail), Err(TokenLoadError::Unavailable));
    }
}
