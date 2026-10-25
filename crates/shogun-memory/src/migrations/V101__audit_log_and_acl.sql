-- Rollback: docs/migrations/V101-rollback.md
-- V101 (ShogunAgent E18): the principal's audit log and its access-control grants.
--
-- `audit_log`: one row per write and per approval decision on this principal's store. Who
-- (`actor`, V92 vocabulary), for which principal (`principal`, NULL on a single-principal store),
-- through which tool or route (`route`), at which tier (`level`: L1 / L2 / L3 / admin), what came
-- of it (`outcome`), when (`ts`, unix ms) and under which request (`request_id`). Never a body, an
-- argument value or a row's text: the columns are names, tiers and outcomes only.
CREATE TABLE audit_log (
    id         INTEGER PRIMARY KEY,
    ts         INTEGER NOT NULL,
    actor      TEXT    NOT NULL,
    principal  TEXT,
    route      TEXT    NOT NULL,
    level      TEXT    NOT NULL,
    outcome    TEXT    NOT NULL,
    request_id TEXT    NOT NULL
) STRICT;
CREATE INDEX audit_log_ts ON audit_log (ts, id);

-- `scratch_acl`: which agent may read (or read and write) another agent's
-- `agent_scratch:<id>` namespace. Granted by a human or an admin of the principal; an agent's own
-- namespace never needs a row. `write` implies `read`.
CREATE TABLE scratch_acl (
    grantee    TEXT    NOT NULL,
    namespace  TEXT    NOT NULL CHECK (namespace LIKE 'agent_scratch:%'),
    access     TEXT    NOT NULL CHECK (access IN ('read', 'write')),
    granted_by TEXT    NOT NULL,
    granted_at INTEGER NOT NULL,
    PRIMARY KEY (grantee, namespace)
) STRICT;

-- `principal_grants` (Team): another principal of the same team that may read this principal's
-- `world` state. Never scratch, never a write. Stored in the granting principal's own file.
CREATE TABLE principal_grants (
    grantee_principal TEXT    NOT NULL,
    scope             TEXT    NOT NULL CHECK (scope = 'world_read'),
    granted_by        TEXT    NOT NULL,
    granted_at        INTEGER NOT NULL,
    PRIMARY KEY (grantee_principal, scope)
) STRICT;
