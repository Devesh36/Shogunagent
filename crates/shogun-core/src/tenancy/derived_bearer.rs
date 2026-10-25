//! Derived sidecar bearers (E34, threat model R4): the hosted gateway (`apps/api/src/mcp-proxy.ts`,
//! `derivedSidecarBearer`) presents `sgw1.<principal_id>.<key_id>.<mac>` to the sidecar, where
//! `mac` is base64url (no padding) of HMAC-SHA256(`SHOGUN_SIDECAR_BEARER_SECRET`,
//! `"shogun/sidecar-bearer/v1\n" + principal_id + "\n" + key_id`).
//!
//! [`DerivedBearers`] verifies such a bearer in constant time and answers the principal the token
//! names and the actor `agent:<key_id>`. Nothing is listed per principal: with the shared secret
//! set, the sidecar needs no `SHOGUN_PRINCIPAL_TOKENS`, so no principal bearer sits in clear in
//! its environment. [`ChainedResolver`] puts a listed-token map in front of it (a listed token
//! wins), which is how `shogun-api --multi` accepts both.

use base64::Engine as _;
use hmac::{Hmac, Mac};
use sha2::Sha256;

use super::{PrincipalId, PrincipalResolver, ResolvedPrincipal, TenancyError};
use crate::llm::Secret;

/// The token's version prefix.
pub const PREFIX: &str = "sgw1";
/// The HMAC context line, the same constant as the gateway's `DERIVED_CONTEXT`.
pub const CONTEXT: &str = "shogun/sidecar-bearer/v1";
/// The shortest secret accepted (characters), as the gateway requires.
pub const MIN_SECRET_CHARS: usize = 32;
/// The longest key id that travels in a token (`[A-Za-z0-9_-]{1,64}`, as the gateway checks).
const MAX_KEY_ID_CHARS: usize = 64;

/// Verifies `sgw1` bearers against the shared secret.
#[derive(Debug, Clone)]
pub struct DerivedBearers {
    secret: Secret,
}

impl DerivedBearers {
    /// `SHOGUN_SIDECAR_BEARER_SECRET`: the same value the gateway is given.
    pub const ENV_VAR: &'static str = "SHOGUN_SIDECAR_BEARER_SECRET";

    /// A verifier over `secret` (at least [`MIN_SECRET_CHARS`] characters).
    pub fn new(secret: Secret) -> Result<Self, TenancyError> {
        if secret.expose().chars().count() < MIN_SECRET_CHARS {
            return Err(TenancyError::InvalidTokenMap(
                "SHOGUN_SIDECAR_BEARER_SECRET must be at least 32 characters",
            ));
        }
        Ok(Self { secret })
    }

    /// The verifier configured in the environment: `None` when the variable is unset or empty.
    pub fn from_env() -> Result<Option<Self>, TenancyError> {
        match std::env::var(Self::ENV_VAR) {
            Ok(value) if !value.is_empty() => Self::new(Secret::new(value)).map(Some),
            _ => Ok(None),
        }
    }

    fn keyed(&self, principal: &str, key_id: &str) -> Option<Hmac<Sha256>> {
        let mut mac = Hmac::<Sha256>::new_from_slice(self.secret.expose().as_bytes()).ok()?;
        mac.update(format!("{CONTEXT}\n{principal}\n{key_id}").as_bytes());
        Some(mac)
    }

    /// The bearer the gateway derives for `principal` and `key_id` (tests and operator tooling;
    /// the sidecar itself only verifies). `None` when an id cannot travel inside the token.
    pub fn derive(&self, principal: &PrincipalId, key_id: &str) -> Option<String> {
        if !valid_key_id(key_id) {
            return None;
        }
        let tag = self.keyed(principal.as_str(), key_id)?.finalize().into_bytes();
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(tag);
        Some(format!("{PREFIX}.{}.{key_id}.{encoded}", principal.as_str()))
    }
}

fn valid_key_id(key_id: &str) -> bool {
    !key_id.is_empty()
        && key_id.len() <= MAX_KEY_ID_CHARS
        && key_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

impl PrincipalResolver for DerivedBearers {
    fn resolve(&self, bearer: &str) -> Option<ResolvedPrincipal> {
        let mut parts = bearer.split('.');
        let (Some(PREFIX), Some(principal), Some(key_id), Some(mac), None) =
            (parts.next(), parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return None;
        };
        let principal = PrincipalId::parse(principal).ok()?;
        if !valid_key_id(key_id) {
            return None;
        }
        // 32 bytes encode to exactly 43 characters; anything else cannot match.
        if mac.len() != 43 {
            return None;
        }
        let tag = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(mac).ok()?;
        // `verify_slice` compares in constant time.
        self.keyed(principal.as_str(), key_id)?.verify_slice(&tag).ok()?;
        let actor = shogun_memory::actor::Actor::agent(key_id).ok()?;
        Some(ResolvedPrincipal { principal, actor: actor.as_str().to_string() })
    }

    /// No token is listed: every derived bearer verifies on its own.
    fn tokens(&self) -> Vec<String> {
        Vec::new()
    }

    fn verifies_unlisted(&self) -> bool {
        true
    }
}

/// A listed-token map first, then the derived bearers: a listed token wins.
pub struct ChainedResolver<A, B> {
    pub listed: A,
    pub derived: Option<B>,
}

impl<A: PrincipalResolver, B: PrincipalResolver> PrincipalResolver for ChainedResolver<A, B> {
    fn resolve(&self, bearer: &str) -> Option<ResolvedPrincipal> {
        self.listed
            .resolve(bearer)
            .or_else(|| self.derived.as_ref().and_then(|derived| derived.resolve(bearer)))
    }

    fn tokens(&self) -> Vec<String> {
        self.listed.tokens()
    }

    fn verifies_unlisted(&self) -> bool {
        self.listed.verifies_unlisted() || self.derived.as_ref().is_some_and(PrincipalResolver::verifies_unlisted)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::tenancy::TokenPrincipalMap;

    /// The shared vector (E34): computed by the gateway's own `derivedSidecarBearer`
    /// (apps/api/src/mcp-proxy.ts) with node, and asserted on both sides
    /// (apps/api/test/mcp-relay.test.ts asserts the same string).
    const VECTOR_SECRET: &str = "conformance-shared-sidecar-secret-0123456789";
    const VECTOR_PRINCIPAL: &str = "prn_alice-01";
    const VECTOR_KEY: &str = "key_AbC-123_x";
    const VECTOR_BEARER: &str = "sgw1.prn_alice-01.key_AbC-123_x.Ozj4qWUTuoA1pltKOvZ9a5Eiu7rS-0ehsArUCWsSzHc";

    fn verifier() -> DerivedBearers {
        DerivedBearers::new(Secret::new(VECTOR_SECRET)).unwrap()
    }

    #[test]
    fn matches_the_gateway_byte_for_byte() {
        let principal = PrincipalId::parse(VECTOR_PRINCIPAL).unwrap();
        assert_eq!(verifier().derive(&principal, VECTOR_KEY).as_deref(), Some(VECTOR_BEARER));
        let resolved = verifier().resolve(VECTOR_BEARER).expect("the gateway's bearer verifies");
        assert_eq!(resolved.principal.as_str(), VECTOR_PRINCIPAL);
        assert_eq!(resolved.actor, format!("agent:{VECTOR_KEY}"));
    }

    #[test]
    fn refuses_anything_the_secret_did_not_sign() {
        let v = verifier();
        let good = VECTOR_BEARER.to_string();
        let mac = good.rsplit('.').next().unwrap().to_string();
        let mut flipped = mac.clone().into_bytes();
        flipped[0] = if flipped[0] == b'A' { b'B' } else { b'A' };
        let flipped = String::from_utf8(flipped).unwrap();
        for bad in [
            format!("sgw1.prn_bob.{VECTOR_KEY}.{mac}"),             // another principal, same mac
            format!("sgw1.{VECTOR_PRINCIPAL}.key_other.{mac}"),     // another key, same mac
            format!("sgw1.{VECTOR_PRINCIPAL}.{VECTOR_KEY}.{flipped}"),
            format!("sgw2.{VECTOR_PRINCIPAL}.{VECTOR_KEY}.{mac}"),
            format!("sgw1.{VECTOR_PRINCIPAL}.{VECTOR_KEY}.{mac}.x"),
            format!("sgw1.{VECTOR_PRINCIPAL}.{VECTOR_KEY}.{mac}="),
            format!("sgw1.Alice.{VECTOR_KEY}.{mac}"),
            format!("sgw1.{VECTOR_PRINCIPAL}.key id.{mac}"),
            "sgw1...".to_string(),
            String::new(),
        ] {
            assert!(v.resolve(&bad).is_none(), "{bad}");
        }
        let other = DerivedBearers::new(Secret::new("another-secret-of-at-least-32-characters!")).unwrap();
        assert!(other.resolve(&good).is_none(), "another secret's bearer");
        assert!(v.tokens().is_empty());
        assert!(v.verifies_unlisted());
    }

    #[test]
    fn a_short_secret_is_refused() {
        assert!(DerivedBearers::new(Secret::new("too-short")).is_err());
        let rendered = format!("{:?}", verifier());
        assert!(!rendered.contains(VECTOR_SECRET), "the secret never renders: {rendered}");
    }

    #[test]
    fn a_listed_token_wins_and_derived_bearers_follow() {
        let listed = TokenPrincipalMap::parse("bob:tok-bob-0001:claude-code").unwrap();
        let chain = ChainedResolver { listed, derived: Some(verifier()) };
        assert_eq!(chain.resolve("tok-bob-0001").unwrap().principal.as_str(), "bob");
        assert_eq!(chain.resolve(VECTOR_BEARER).unwrap().principal.as_str(), VECTOR_PRINCIPAL);
        assert_eq!(chain.tokens(), vec!["tok-bob-0001".to_string()]);
        assert!(chain.verifies_unlisted());
        let listed_only: ChainedResolver<TokenPrincipalMap, DerivedBearers> =
            ChainedResolver { listed: TokenPrincipalMap::default(), derived: None };
        assert!(!listed_only.verifies_unlisted());
        assert!(listed_only.resolve(VECTOR_BEARER).is_none());
    }
}
