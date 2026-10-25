//! Who writes a row, and which namespace it belongs to (ShogunAgent E2-T4 / E2-T5, V92 / V93).
//!
//! Every write carries an [`Actor`] (`human:owner`, `agent:<id>`, `connector:<source>`,
//! `system:<job>`) and a [`Namespace`]: `world`, the shared model every reader sees, or
//! `agent_scratch:<id>`, an agent's private working rows that default reads leave out. The two
//! travel together as a [`WriteContext`]; a read carries a [`ReadScope`] saying whether scratch
//! rows are wanted.
//!
//! The repository functions take the context explicitly (`insert_as`, `insert_with_provenance_as`,
//! `update_state`, `loops::apply`, …). The faces that serve many principals and actors from one
//! process set it once per call with [`with_call_scope`], the way `shogun_mcp::source_policy`
//! sets a connection's access, and every write under that call that did not name its context
//! reads it from [`current_write`]. Outside any call scope the writer is the owner
//! ([`Actor::owner`]) and reads are world-only, so the single-principal desktop keeps working
//! unchanged.
//!
//! An empty actor is refused at this layer ([`MemoryError::EmptyActor`]), the way an empty
//! provenance is ([`MemoryError::EmptyProvenance`]): a row never records "nobody" as its writer.

use std::borrow::Cow;
use std::cell::RefCell;
use std::fmt;

use crate::MemoryError;

/// The writer every pre-V92 row and every single-principal desktop write is attributed to.
pub const OWNER_ACTOR: &str = "human:owner";
/// The shared namespace: what every reader sees by default.
pub const WORLD: &str = "world";
/// The prefix of an agent's private namespace: `agent_scratch:<id>`.
pub const SCRATCH_PREFIX: &str = "agent_scratch:";
/// Longest actor accepted (characters). Long enough for `agent:<key id>`; short enough that an
/// actor is never a place to smuggle content.
pub const MAX_ACTOR_CHARS: usize = 128;

/// A validated writer id: `<kind>:<id>`, non-empty on both sides, no whitespace or control
/// characters, at most [`MAX_ACTOR_CHARS`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Actor(String);

impl Actor {
    /// Validate a wire or configuration string. Empty (after trimming) is
    /// [`MemoryError::EmptyActor`]; any other malformed value is [`MemoryError::InvalidActor`].
    pub fn parse(raw: &str) -> Result<Self, MemoryError> {
        let actor = raw.trim();
        if actor.is_empty() {
            return Err(MemoryError::EmptyActor);
        }
        if actor.chars().count() > MAX_ACTOR_CHARS
            || actor.chars().any(|c| c.is_control() || c.is_whitespace())
        {
            return Err(MemoryError::InvalidActor);
        }
        match actor.split_once(':') {
            Some((kind, id)) if !kind.is_empty() && !id.is_empty() => Ok(Self(actor.to_string())),
            _ => Err(MemoryError::InvalidActor),
        }
    }

    /// `human:owner`: the person the store belongs to.
    pub fn owner() -> Self {
        Self(OWNER_ACTOR.to_string())
    }

    /// `agent:<id>` — an agent key or client id.
    pub fn agent(id: &str) -> Result<Self, MemoryError> {
        Self::parse(&format!("agent:{}", id.trim()))
    }

    /// `human:<id>` — a person other than the owner (a Team member).
    pub fn human(id: &str) -> Result<Self, MemoryError> {
        Self::parse(&format!("human:{}", id.trim()))
    }

    /// `connector:<source>` — a connector sync (`gmail`, `gcal`, …). Infallible: the source tags
    /// are this crate's own constants, and an odd one is folded to a valid id rather than
    /// refused, so a sync never fails for want of a name.
    pub fn connector(source: &str) -> Self {
        Self::system_like("connector", source)
    }

    /// `system:<job>` — a background job of the engine (the loop consolidator, maintenance).
    pub fn system(job: &str) -> Self {
        Self::system_like("system", job)
    }

    fn system_like(kind: &str, id: &str) -> Self {
        let mut folded: String = id
            .trim()
            .chars()
            .map(|c| if c.is_control() || c.is_whitespace() { '_' } else { c })
            .take(MAX_ACTOR_CHARS - kind.len() - 1)
            .collect();
        if folded.is_empty() {
            folded.push_str("unnamed");
        }
        Self(format!("{kind}:{folded}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The `<kind>` before the colon.
    pub fn kind(&self) -> &str {
        self.0.split_once(':').map_or(self.0.as_str(), |(kind, _)| kind)
    }

    /// The `<id>` after the colon.
    pub fn id(&self) -> &str {
        self.0.split_once(':').map_or("", |(_, id)| id)
    }

    pub fn is_owner(&self) -> bool {
        self.0 == OWNER_ACTOR
    }
}

impl fmt::Display for Actor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Which rows a write lands in and which a read sees.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Namespace {
    /// The shared model (`world`).
    World,
    /// One agent's private rows (`agent_scratch:<id>`).
    AgentScratch(String),
}

impl Namespace {
    /// `agent_scratch:<id>` for an id that passes the same rule as an actor id.
    pub fn scratch(id: &str) -> Result<Self, MemoryError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(MemoryError::EmptyActor);
        }
        if id.chars().count() > MAX_ACTOR_CHARS
            || id.chars().any(|c| c.is_control() || c.is_whitespace())
        {
            return Err(MemoryError::InvalidActor);
        }
        Ok(Self::AgentScratch(id.to_string()))
    }

    /// The scratch namespace of `actor`, keyed by its id (`agent:claude` → `agent_scratch:claude`).
    pub fn scratch_of(actor: &Actor) -> Result<Self, MemoryError> {
        Self::scratch(actor.id())
    }

    /// Parse a stored column value. `None` for a value this build does not know.
    pub fn parse(raw: &str) -> Option<Self> {
        if raw == WORLD {
            return Some(Self::World);
        }
        raw.strip_prefix(SCRATCH_PREFIX)
            .and_then(|id| Self::scratch(id).ok())
    }

    /// The column value.
    pub fn as_str(&self) -> Cow<'_, str> {
        match self {
            Self::World => Cow::Borrowed(WORLD),
            Self::AgentScratch(id) => Cow::Owned(format!("{SCRATCH_PREFIX}{id}")),
        }
    }

    pub fn is_world(&self) -> bool {
        matches!(self, Self::World)
    }
}

impl fmt::Display for Namespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_str())
    }
}

/// What a write records about itself: who, and into which namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteContext {
    pub actor: Actor,
    pub namespace: Namespace,
}

impl WriteContext {
    /// Validate `actor` and pair it with `namespace`. The repository-layer refusal of a write
    /// without a writer: an empty actor is [`MemoryError::EmptyActor`].
    pub fn new(actor: &str, namespace: Namespace) -> Result<Self, MemoryError> {
        Ok(Self { actor: Actor::parse(actor)?, namespace })
    }

    /// The owner writing into the world: the single-principal desktop default.
    pub fn owner() -> Self {
        Self { actor: Actor::owner(), namespace: Namespace::World }
    }

    /// `actor` writing into the shared model.
    pub fn world(actor: Actor) -> Self {
        Self { actor, namespace: Namespace::World }
    }

    /// `actor` writing into its own scratch namespace.
    pub fn scratch(actor: Actor) -> Result<Self, MemoryError> {
        let namespace = Namespace::scratch_of(&actor)?;
        Ok(Self { actor, namespace })
    }

    /// The `actor` column value.
    pub fn actor_sql(&self) -> &str {
        self.actor.as_str()
    }

    /// The `namespace` column value.
    pub fn namespace_sql(&self) -> String {
        self.namespace.as_str().into_owned()
    }
}

impl Default for WriteContext {
    fn default() -> Self {
        Self::owner()
    }
}

/// Which namespaces a read returns. The default is the world only: an agent's scratch rows never
/// reach another caller's context pack, search or state list unless asked for (E2-T5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReadScope {
    /// Also return rows in every `agent_scratch:*` namespace.
    pub include_scratch: bool,
}

impl ReadScope {
    pub const WORLD_ONLY: Self = Self { include_scratch: false };
    pub const WITH_SCRATCH: Self = Self { include_scratch: true };

    /// A SQL predicate on the `namespace` column of the table `prefix` names (`""` for the
    /// statement's only table, `"e."` for an alias).
    ///
    /// With scratch included, an agent writer of the current call (E18 scratch ACL) reads the
    /// world, its own scratch and the namespaces it holds a grant on ([`crate::acl`]); anyone
    /// else (the owner, a team member, a system job, no call scope) reads every namespace.
    pub fn predicate(self, prefix: &str) -> String {
        if !self.include_scratch {
            return format!("{prefix}namespace = '{WORLD}'");
        }
        match restricted_reader() {
            Some(actor) => crate::acl::readable_predicate(prefix, &actor),
            None => "1 = 1".to_string(),
        }
    }

    /// Whether every row is in scope (scratch included, and the call's writer is not an agent
    /// restricted by the scratch ACL). Callers use it to skip the scope filter entirely.
    pub fn unrestricted(self) -> bool {
        self.include_scratch && restricted_reader().is_none()
    }

    /// Whether a row whose `namespace` column holds `namespace` is in scope, without consulting
    /// grants: for a restricted agent only the world and its own scratch. The SQL
    /// [`Self::predicate`] is the full check.
    pub fn allows(self, namespace: &str) -> bool {
        if namespace == WORLD {
            return true;
        }
        if !self.include_scratch {
            return false;
        }
        match restricted_reader() {
            Some(actor) => namespace.strip_prefix(SCRATCH_PREFIX).is_some_and(|id| id == actor.id()),
            None => true,
        }
    }
}

/// The current call's writer when the scratch ACL restricts what it reads: an agent.
pub fn restricted_reader() -> Option<Actor> {
    let actor = current_write().actor;
    crate::acl::is_restricted(&actor).then_some(actor)
}

/// The context of the call a thread is serving: its writer and its read scope.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CallScope {
    pub write: WriteContext,
    pub read: ReadScope,
}

thread_local! {
    static CURRENT: RefCell<Option<CallScope>> = const { RefCell::new(None) };
}

/// Restores the previous scope when dropped, so a panic inside the call cannot leave a later
/// call on this thread writing as someone else.
struct Restore(Option<CallScope>);

impl Drop for Restore {
    fn drop(&mut self) {
        let previous = self.0.take();
        CURRENT.with(|cell| *cell.borrow_mut() = previous);
    }
}

/// Run `f` (one tool call) as `scope`. Nested calls replace the scope for their duration.
pub fn with_call_scope<T>(scope: CallScope, f: impl FnOnce() -> T) -> T {
    let previous = CURRENT.with(|cell| cell.borrow_mut().replace(scope));
    let _restore = Restore(previous);
    f()
}

/// Run `f` with the read scope `read`, keeping the current writer.
pub fn with_read_scope<T>(read: ReadScope, f: impl FnOnce() -> T) -> T {
    let write = current_write();
    with_call_scope(CallScope { write, read }, f)
}

/// The writer of the call this thread is serving: the owner unless a face set one.
pub fn current_write() -> WriteContext {
    CURRENT.with(|cell| cell.borrow().as_ref().map(|s| s.write.clone()).unwrap_or_default())
}

/// The read scope of the call this thread is serving: world-only unless a face set one.
pub fn current_read() -> ReadScope {
    CURRENT.with(|cell| cell.borrow().as_ref().map_or(ReadScope::WORLD_ONLY, |s| s.read))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_actor_is_refused_as_empty_and_malformed_as_invalid() {
        assert!(matches!(Actor::parse(""), Err(MemoryError::EmptyActor)));
        assert!(matches!(Actor::parse("   "), Err(MemoryError::EmptyActor)));
        assert!(matches!(WriteContext::new("", Namespace::World), Err(MemoryError::EmptyActor)));
        assert!(matches!(Actor::parse("owner"), Err(MemoryError::InvalidActor)));
        assert!(matches!(Actor::parse(":x"), Err(MemoryError::InvalidActor)));
        assert!(matches!(Actor::parse("agent:"), Err(MemoryError::InvalidActor)));
        assert!(matches!(Actor::parse("agent:a b"), Err(MemoryError::InvalidActor)));
        assert!(matches!(Actor::parse("agent:a\nb"), Err(MemoryError::InvalidActor)));
        let long = format!("agent:{}", "x".repeat(MAX_ACTOR_CHARS));
        assert!(matches!(Actor::parse(&long), Err(MemoryError::InvalidActor)));
        let actor = Actor::parse(" agent:claude-code ").unwrap();
        assert_eq!(actor.as_str(), "agent:claude-code");
        assert_eq!(actor.kind(), "agent");
        assert_eq!(actor.id(), "claude-code");
        assert!(Actor::owner().is_owner());
        assert_eq!(Actor::connector("gmail").as_str(), "connector:gmail");
        assert_eq!(Actor::system("loop consolidator").as_str(), "system:loop_consolidator");
        assert_eq!(Actor::connector("  ").as_str(), "connector:unnamed");
    }

    #[test]
    fn namespaces_round_trip_and_scratch_follows_the_actor_id() {
        assert_eq!(Namespace::World.as_str(), "world");
        let scratch = Namespace::scratch_of(&Actor::agent("codex").unwrap()).unwrap();
        assert_eq!(scratch.as_str(), "agent_scratch:codex");
        assert_eq!(Namespace::parse("world"), Some(Namespace::World));
        assert_eq!(Namespace::parse("agent_scratch:codex"), Some(scratch));
        assert_eq!(Namespace::parse("agent_scratch:"), None);
        assert_eq!(Namespace::parse("tenant_x"), None);
        assert!(matches!(Namespace::scratch(""), Err(MemoryError::EmptyActor)));
    }

    #[test]
    fn read_scope_defaults_to_world_only() {
        assert_eq!(ReadScope::default(), ReadScope::WORLD_ONLY);
        assert_eq!(ReadScope::WORLD_ONLY.predicate("e."), "e.namespace = 'world'");
        assert_eq!(ReadScope::WITH_SCRATCH.predicate(""), "1 = 1");
        assert!(ReadScope::WORLD_ONLY.allows("world"));
        assert!(!ReadScope::WORLD_ONLY.allows("agent_scratch:codex"));
        assert!(ReadScope::WITH_SCRATCH.allows("agent_scratch:codex"));
    }

    #[test]
    fn call_scope_is_per_thread_and_restored_after_the_call() {
        assert_eq!(current_write(), WriteContext::owner());
        assert_eq!(current_read(), ReadScope::WORLD_ONLY);
        let agent = WriteContext::scratch(Actor::agent("codex").unwrap()).unwrap();
        let seen = with_call_scope(
            CallScope { write: agent.clone(), read: ReadScope::WITH_SCRATCH },
            || {
                let inner = with_read_scope(ReadScope::WORLD_ONLY, || (current_write(), current_read()));
                assert_eq!(inner, (agent.clone(), ReadScope::WORLD_ONLY));
                (current_write(), current_read())
            },
        );
        assert_eq!(seen, (agent, ReadScope::WITH_SCRATCH));
        assert_eq!(current_write(), WriteContext::owner());
        assert_eq!(current_read(), ReadScope::WORLD_ONLY);
        // Another thread never sees this thread's scope.
        let other = std::thread::spawn(current_write).join().unwrap();
        assert_eq!(other, WriteContext::owner());
    }

    #[test]
    fn a_panicking_call_restores_the_previous_scope() {
        let agent = WriteContext::world(Actor::agent("codex").unwrap());
        let result = std::panic::catch_unwind(|| {
            with_call_scope(CallScope { write: agent, read: ReadScope::WITH_SCRATCH }, || {
                panic!("inside the call");
            })
        });
        assert!(result.is_err());
        assert_eq!(current_write(), WriteContext::owner());
        assert_eq!(current_read(), ReadScope::WORLD_ONLY);
    }
}
