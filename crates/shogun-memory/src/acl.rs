//! Access grants on a principal's store (V101, ShogunAgent E18).
//!
//! **Scratch ACL.** An agent sees and writes its own `agent_scratch:<id>` namespace and the world.
//! Another agent's scratch is invisible to it — not in a search, not in a list, not through a GET
//! by id — unless a human or an admin of the principal granted it ([`grant_scratch`]). `write`
//! implies `read`. The check is in the data layer: [`crate::actor::ReadScope::predicate`] and
//! [`crate::actor::ReadScope::allows`] consult `scratch_acl` for the calling agent, and every
//! scratch write ([`may_write`]) is refused into a namespace the writer neither owns nor holds a
//! `write` grant on. A non-agent writer (the owner, a team member, a connector, a system job) is
//! not restricted.
//!
//! **Team grants.** [`grant_principal`] lets another principal of the same team read this
//! principal's `world` state (never scratch, never a write). The grant lives in the granting
//! principal's own file; the multi-principal sidecar checks it before it opens the file for a
//! cross-principal read.

use rusqlite::{params, Connection, OptionalExtension};

use crate::actor::{Actor, Namespace, SCRATCH_PREFIX};

/// What a scratch grant allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScratchAccess {
    Read,
    /// Read and write.
    Write,
}

impl ScratchAccess {
    pub fn as_str(self) -> &'static str {
        match self {
            ScratchAccess::Read => "read",
            ScratchAccess::Write => "write",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "read" => Some(ScratchAccess::Read),
            "write" => Some(ScratchAccess::Write),
            _ => None,
        }
    }
}

/// One scratch grant as listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScratchGrant {
    /// The agent that may access (`agent:<id>`).
    pub grantee: String,
    /// The namespace it may access (`agent_scratch:<id>`).
    pub namespace: String,
    pub access: ScratchAccess,
    pub granted_by: String,
    pub granted_at: i64,
}

/// One Team grant as listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrincipalGrant {
    pub grantee_principal: String,
    /// Always `world_read`.
    pub scope: String,
    pub granted_by: String,
    pub granted_at: i64,
}

/// The only Team scope: read the `world` namespace.
pub const WORLD_READ: &str = "world_read";

/// Why a grant was refused. Never carries a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AclError {
    #[error("the grantee must be an agent (`agent:<id>`)")]
    GranteeNotAgent,
    #[error("an agent's own scratch needs no grant")]
    OwnNamespace,
    #[error("the principal id is not a valid principal slug")]
    InvalidPrincipal,
    #[error("the access-control table could not be read or written")]
    Db,
}

impl From<rusqlite::Error> for AclError {
    fn from(_: rusqlite::Error) -> Self {
        AclError::Db
    }
}

/// SQL string literal for a validated identifier (actor ids carry no whitespace or control
/// characters; a quote is doubled).
pub(crate) fn sql_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Grant `grantee` (an agent) `access` to `namespace` (another agent's scratch). Replaces an
/// earlier grant of the same pair.
pub fn grant_scratch(
    conn: &Connection,
    grantee: &Actor,
    namespace: &Namespace,
    access: ScratchAccess,
    granted_by: &Actor,
    now: i64,
) -> Result<(), AclError> {
    if grantee.kind() != "agent" {
        return Err(AclError::GranteeNotAgent);
    }
    let Namespace::AgentScratch(owner) = namespace else {
        return Err(AclError::OwnNamespace);
    };
    if owner == grantee.id() {
        return Err(AclError::OwnNamespace);
    }
    conn.execute(
        "INSERT INTO scratch_acl (grantee, namespace, access, granted_by, granted_at) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (grantee, namespace) DO UPDATE SET access = excluded.access,
             granted_by = excluded.granted_by, granted_at = excluded.granted_at",
        params![grantee.as_str(), namespace.as_str(), access.as_str(), granted_by.as_str(), now],
    )?;
    Ok(())
}

/// Remove `grantee`'s grant on `namespace`. `true` when one existed.
pub fn revoke_scratch(conn: &Connection, grantee: &Actor, namespace: &Namespace) -> Result<bool, AclError> {
    let n = conn.execute(
        "DELETE FROM scratch_acl WHERE grantee = ?1 AND namespace = ?2",
        params![grantee.as_str(), namespace.as_str()],
    )?;
    Ok(n > 0)
}

/// Every scratch grant, by grantee then namespace.
pub fn list_scratch(conn: &Connection) -> Result<Vec<ScratchGrant>, AclError> {
    let mut stmt = conn.prepare(
        "SELECT grantee, namespace, access, granted_by, granted_at FROM scratch_acl ORDER BY grantee, namespace",
    )?;
    let rows = stmt
        .query_map([], |r| {
            let access: String = r.get(2)?;
            Ok(ScratchGrant {
                grantee: r.get(0)?,
                namespace: r.get(1)?,
                access: ScratchAccess::parse(&access).unwrap_or(ScratchAccess::Read),
                granted_by: r.get(3)?,
                granted_at: r.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Whether `actor` is restricted by the scratch ACL: agents are; the owner, team members,
/// connectors and system jobs are not.
pub fn is_restricted(actor: &Actor) -> bool {
    actor.kind() == "agent"
}

/// The SQL predicate (on the `namespace` column, behind `prefix`) of the rows `actor` may read
/// when scratch is asked for: the world, its own scratch, and every namespace it holds a grant
/// on. The grant lookup is a subquery on the same connection, so a grant or revoke applies to the
/// very next statement.
pub fn readable_predicate(prefix: &str, actor: &Actor) -> String {
    let own = Namespace::scratch_of(actor)
        .map(|ns| ns.as_str().into_owned())
        .unwrap_or_default();
    format!(
        "({prefix}namespace = 'world' OR {prefix}namespace = {own} OR {prefix}namespace IN \
         (SELECT namespace FROM scratch_acl WHERE grantee = {grantee}))",
        own = sql_literal(&own),
        grantee = sql_literal(actor.as_str()),
    )
}

fn grant_of(conn: &Connection, actor: &Actor, namespace: &str) -> Result<Option<ScratchAccess>, AclError> {
    let access: Option<String> = conn
        .query_row(
            "SELECT access FROM scratch_acl WHERE grantee = ?1 AND namespace = ?2",
            params![actor.as_str(), namespace],
            |r| r.get(0),
        )
        .optional()?;
    Ok(access.as_deref().and_then(ScratchAccess::parse))
}

fn owns(actor: &Actor, namespace: &str) -> bool {
    namespace == crate::actor::WORLD
        || namespace.strip_prefix(SCRATCH_PREFIX).is_some_and(|id| id == actor.id())
}

/// Whether `actor` may read rows in `namespace`.
pub fn may_read(conn: &Connection, actor: &Actor, namespace: &str) -> Result<bool, AclError> {
    if !is_restricted(actor) || owns(actor, namespace) {
        return Ok(true);
    }
    Ok(grant_of(conn, actor, namespace)?.is_some())
}

/// Whether `actor` may write rows into `namespace`.
pub fn may_write(conn: &Connection, actor: &Actor, namespace: &str) -> Result<bool, AclError> {
    if !is_restricted(actor) || owns(actor, namespace) {
        return Ok(true);
    }
    Ok(grant_of(conn, actor, namespace)? == Some(ScratchAccess::Write))
}

fn valid_principal(raw: &str) -> Result<&str, AclError> {
    let p = raw.trim();
    let ok = !p.is_empty()
        && p.len() <= 64
        && p.bytes().next().is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && p.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
    if ok {
        Ok(p)
    } else {
        Err(AclError::InvalidPrincipal)
    }
}

/// Let `grantee_principal` read this principal's world state (Team).
pub fn grant_principal(conn: &Connection, grantee_principal: &str, granted_by: &Actor, now: i64) -> Result<(), AclError> {
    let grantee = valid_principal(grantee_principal)?;
    conn.execute(
        "INSERT INTO principal_grants (grantee_principal, scope, granted_by, granted_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (grantee_principal, scope) DO UPDATE SET granted_by = excluded.granted_by,
             granted_at = excluded.granted_at",
        params![grantee, WORLD_READ, granted_by.as_str(), now],
    )?;
    Ok(())
}

/// Withdraw `grantee_principal`'s read access. `true` when a grant existed.
pub fn revoke_principal(conn: &Connection, grantee_principal: &str) -> Result<bool, AclError> {
    let grantee = valid_principal(grantee_principal)?;
    let n = conn.execute(
        "DELETE FROM principal_grants WHERE grantee_principal = ?1 AND scope = ?2",
        params![grantee, WORLD_READ],
    )?;
    Ok(n > 0)
}

/// Every Team grant of this principal.
pub fn list_principal(conn: &Connection) -> Result<Vec<PrincipalGrant>, AclError> {
    let mut stmt = conn.prepare(
        "SELECT grantee_principal, scope, granted_by, granted_at FROM principal_grants ORDER BY grantee_principal",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(PrincipalGrant { grantee_principal: r.get(0)?, scope: r.get(1)?, granted_by: r.get(2)?, granted_at: r.get(3)? })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Whether `grantee_principal` may read this principal's world state.
pub fn principal_may_read(conn: &Connection, grantee_principal: &str) -> Result<bool, AclError> {
    let Ok(grantee) = valid_principal(grantee_principal) else {
        return Ok(false);
    };
    let found: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM principal_grants WHERE grantee_principal = ?1 AND scope = ?2",
            params![grantee, WORLD_READ],
            |r| r.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(id: &str) -> Actor {
        Actor::agent(id).unwrap()
    }

    #[test]
    fn scratch_grants_read_write_and_revoke() {
        let conn = crate::open_in_memory().unwrap();
        let a = agent("a");
        let b_ns = Namespace::scratch("b").unwrap();
        assert!(!may_read(&conn, &a, "agent_scratch:b").unwrap());
        assert!(may_read(&conn, &a, "agent_scratch:a").unwrap());
        assert!(may_read(&conn, &a, "world").unwrap());
        assert!(may_read(&conn, &Actor::owner(), "agent_scratch:b").unwrap());
        grant_scratch(&conn, &a, &b_ns, ScratchAccess::Read, &Actor::owner(), 1).unwrap();
        assert!(may_read(&conn, &a, "agent_scratch:b").unwrap());
        assert!(!may_write(&conn, &a, "agent_scratch:b").unwrap());
        grant_scratch(&conn, &a, &b_ns, ScratchAccess::Write, &Actor::owner(), 2).unwrap();
        assert!(may_write(&conn, &a, "agent_scratch:b").unwrap());
        assert_eq!(list_scratch(&conn).unwrap().len(), 1);
        assert!(revoke_scratch(&conn, &a, &b_ns).unwrap());
        assert!(!revoke_scratch(&conn, &a, &b_ns).unwrap());
        assert!(!may_read(&conn, &a, "agent_scratch:b").unwrap());
        assert_eq!(
            grant_scratch(&conn, &a, &Namespace::scratch("a").unwrap(), ScratchAccess::Read, &Actor::owner(), 3),
            Err(AclError::OwnNamespace)
        );
        assert_eq!(
            grant_scratch(&conn, &Actor::owner(), &b_ns, ScratchAccess::Read, &Actor::owner(), 3),
            Err(AclError::GranteeNotAgent)
        );
    }

    #[test]
    fn readable_predicate_follows_grants_on_the_next_statement() {
        let conn = crate::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TEMP TABLE t (namespace TEXT);
             INSERT INTO t VALUES ('world'), ('agent_scratch:a'), ('agent_scratch:b'), ('agent_scratch:c');",
        )
        .unwrap();
        let a = agent("a");
        let count = |conn: &Connection| -> i64 {
            conn.query_row(&format!("SELECT count(*) FROM t WHERE {}", readable_predicate("t.", &a)), [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(count(&conn), 2);
        grant_scratch(&conn, &a, &Namespace::scratch("b").unwrap(), ScratchAccess::Read, &Actor::owner(), 1).unwrap();
        assert_eq!(count(&conn), 3);
        revoke_scratch(&conn, &a, &Namespace::scratch("b").unwrap()).unwrap();
        assert_eq!(count(&conn), 2);
        // A quote in an id cannot break out of the literal.
        let odd = Actor::parse("agent:o'x").unwrap();
        let sql = format!("SELECT count(*) FROM t WHERE {}", readable_predicate("", &odd));
        let n: i64 = conn.query_row(&sql, [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn team_grants_are_world_read_only_and_revocable() {
        let conn = crate::open_in_memory().unwrap();
        assert!(!principal_may_read(&conn, "bob").unwrap());
        grant_principal(&conn, "bob", &Actor::owner(), 5).unwrap();
        assert!(principal_may_read(&conn, "bob").unwrap());
        assert_eq!(list_principal(&conn).unwrap()[0].scope, WORLD_READ);
        assert!(revoke_principal(&conn, "bob").unwrap());
        assert!(!principal_may_read(&conn, "bob").unwrap());
        assert_eq!(grant_principal(&conn, "Bob", &Actor::owner(), 5), Err(AclError::InvalidPrincipal));
        assert!(!principal_may_read(&conn, "../x").unwrap());
    }
}
