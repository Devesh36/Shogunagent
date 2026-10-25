//! Bearer token → principal + actor (E2-T2). The trait is the seam `apps/api`'s agent keys plug
//! into in E3; [`TokenPrincipalMap`] is the sidecar's own in-memory stand-in, loaded from
//! `SHOGUN_PRINCIPAL_TOKENS="principal:token[:agent_id],principal:token[:agent_id]"`.

use std::collections::HashMap;

use super::{PrincipalId, TenancyError};

/// Whose store a request reaches, and the writer its rows record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPrincipal {
    pub principal: PrincipalId,
    /// `agent:<id>` — a valid [`shogun_memory::actor::Actor`].
    pub actor: String,
}

/// Resolve a bearer token to a principal and an actor.
pub trait PrincipalResolver: Send + Sync {
    /// `None` for a token no principal owns.
    fn resolve(&self, bearer: &str) -> Option<ResolvedPrincipal>;

    /// Every token this resolver accepts, for the server's token registry (auth happens there
    /// first, so the registry and the resolver must agree).
    fn tokens(&self) -> Vec<String>;

    /// Whether [`Self::resolve`] also accepts bearers [`Self::tokens`] does not list (the derived
    /// `sgw1` bearers of [`super::DerivedBearers`]): the server's token registry then asks the
    /// resolver about an unlisted bearer instead of refusing it.
    fn verifies_unlisted(&self) -> bool {
        false
    }
}

/// A fixed map of tokens to principals.
#[derive(Debug, Default, Clone)]
pub struct TokenPrincipalMap {
    entries: HashMap<String, ResolvedPrincipal>,
}

impl TokenPrincipalMap {
    pub const ENV_VAR: &'static str = "SHOGUN_PRINCIPAL_TOKENS";

    /// Parse `principal:token[:agent_id],…`. The actor is `agent:<agent_id>`, or
    /// `agent:<principal>` when no agent id is given. Whitespace around entries is ignored; a
    /// token must be at least 8 characters without whitespace; two entries may not share a token.
    pub fn parse(spec: &str) -> Result<Self, TenancyError> {
        let mut map = Self::default();
        for entry in spec.split(',').map(str::trim).filter(|e| !e.is_empty()) {
            let mut parts = entry.splitn(3, ':');
            let principal = parts.next().unwrap_or_default();
            let token = parts.next().ok_or(TenancyError::InvalidTokenMap("entry is not principal:token"))?;
            let agent = parts.next();
            let principal = PrincipalId::parse(principal)?;
            let actor = match agent {
                Some(agent) => shogun_memory::actor::Actor::agent(agent),
                None => shogun_memory::actor::Actor::agent(principal.as_str()),
            }
            .map_err(|_| TenancyError::InvalidTokenMap("agent id is not a valid actor id"))?;
            map.insert(principal, token, actor.as_str())?;
        }
        Ok(map)
    }

    /// [`Self::parse`] of `SHOGUN_PRINCIPAL_TOKENS`; an unset variable is an empty map.
    pub fn from_env() -> Result<Self, TenancyError> {
        match std::env::var(Self::ENV_VAR) {
            Ok(spec) => Self::parse(&spec),
            Err(_) => Ok(Self::default()),
        }
    }

    /// Add one token. `actor` must be a valid actor id (`agent:<id>`).
    pub fn insert(&mut self, principal: PrincipalId, token: &str, actor: &str) -> Result<(), TenancyError> {
        if token.chars().count() < 8 || token.chars().any(char::is_whitespace) {
            return Err(TenancyError::InvalidTokenMap("token must be at least 8 characters without whitespace"));
        }
        shogun_memory::actor::Actor::parse(actor)
            .map_err(|_| TenancyError::InvalidTokenMap("actor is not `<kind>:<id>`"))?;
        if self.entries.contains_key(token) {
            return Err(TenancyError::InvalidTokenMap("two entries share a token"));
        }
        self.entries.insert(token.to_string(), ResolvedPrincipal { principal, actor: actor.to_string() });
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl PrincipalResolver for TokenPrincipalMap {
    fn resolve(&self, bearer: &str) -> Option<ResolvedPrincipal> {
        self.entries.get(bearer).cloned()
    }

    fn tokens(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_entries_and_derives_the_actor() {
        let map = TokenPrincipalMap::parse(" alice:tok-alice-1 , bob:tok-bob-0001:claude-code ,").unwrap();
        assert_eq!(map.len(), 2);
        let alice = map.resolve("tok-alice-1").unwrap();
        assert_eq!(alice.principal.as_str(), "alice");
        assert_eq!(alice.actor, "agent:alice");
        let bob = map.resolve("tok-bob-0001").unwrap();
        assert_eq!(bob.principal.as_str(), "bob");
        assert_eq!(bob.actor, "agent:claude-code");
        assert!(map.resolve("tok-nobody-1").is_none());
        let mut tokens = map.tokens();
        tokens.sort();
        assert_eq!(tokens, vec!["tok-alice-1", "tok-bob-0001"]);
        assert!(TokenPrincipalMap::parse("").unwrap().is_empty());
    }

    #[test]
    fn refuses_bad_entries() {
        for bad in ["alice", "Alice:tok-alice-1", "alice:short", "alice:tok alice 1", "alice:tok-alice-1,bob:tok-alice-1"] {
            assert!(TokenPrincipalMap::parse(bad).is_err(), "{bad}");
        }
    }
}
