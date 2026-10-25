//! Teams (ShogunAgent E18): a tenant names a group of its principals. Being on a team is what
//! makes a cross-principal grant usable — a principal can grant read access to its `world` state
//! only to a teammate, and the grant itself lives in the granting principal's own file
//! (`principal_grants`, V101). Files never mix (CLAUDE.md invariant 6).
//!
//! The registry is one JSON file beside the principal directories (`<root>/teams.json`; a
//! principal id cannot contain `.`, so it never collides with one). It holds only ids: tenant,
//! team and member principal ids.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{json, Value};

use super::{PrincipalId, TenancyError};

/// The registry file name under the multi-principal root.
pub const TEAMS_FILE: &str = "teams.json";
/// Most members one team holds.
pub const MAX_MEMBERS: usize = 256;

/// One team.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Team {
    pub tenant: String,
    pub team: String,
    pub members: Vec<PrincipalId>,
}

/// The teams of every tenant this sidecar serves.
#[derive(Debug)]
pub struct TeamRegistry {
    path: Option<PathBuf>,
    teams: Mutex<BTreeMap<(String, String), Team>>,
}

fn slug(raw: &str) -> Result<String, TenancyError> {
    PrincipalId::parse(raw.trim()).map(|p| p.as_str().to_string())
}

impl TeamRegistry {
    /// An empty registry that is not persisted (tests).
    pub fn in_memory() -> Self {
        Self { path: None, teams: Mutex::new(BTreeMap::new()) }
    }

    /// The registry under `root`, loaded from `<root>/teams.json` when it exists. An unreadable
    /// file is an error rather than an empty registry: losing teams silently would only narrow
    /// access, but a corrupt file should be seen.
    pub fn load(root: &Path) -> Result<Self, TenancyError> {
        let path = root.join(TEAMS_FILE);
        let registry = Self { path: Some(path.clone()), teams: Mutex::new(BTreeMap::new()) };
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(registry),
            Err(e) => return Err(TenancyError::Io(e)),
        };
        let value: Value = serde_json::from_str(&raw).map_err(|_| TenancyError::Unavailable("teams.json is not valid JSON".into()))?;
        for team in value["teams"].as_array().cloned().unwrap_or_default() {
            let members: Vec<String> =
                team["members"].as_array().cloned().unwrap_or_default().iter().filter_map(|m| m.as_str().map(str::to_string)).collect();
            registry.set_in_memory(team["tenant"].as_str().unwrap_or(""), team["team"].as_str().unwrap_or(""), &members)?;
        }
        Ok(registry)
    }

    fn set_in_memory(&self, tenant: &str, team: &str, members: &[String]) -> Result<Team, TenancyError> {
        let tenant = slug(tenant)?;
        let team_id = slug(team)?;
        if members.len() > MAX_MEMBERS {
            return Err(TenancyError::Unavailable(format!("a team holds at most {MAX_MEMBERS} members")));
        }
        let mut ids = members.iter().map(|m| PrincipalId::parse(m.trim())).collect::<Result<Vec<_>, _>>()?;
        ids.sort();
        ids.dedup();
        let entry = Team { tenant: tenant.clone(), team: team_id.clone(), members: ids };
        let mut teams = self.teams.lock().map_err(|_| TenancyError::Unavailable("team registry lock".into()))?;
        if entry.members.is_empty() {
            teams.remove(&(tenant, team_id));
        } else {
            teams.insert((tenant, team_id), entry.clone());
        }
        Ok(entry)
    }

    /// Define (or replace) a team; an empty member list removes it. Persisted atomically.
    pub fn set(&self, tenant: &str, team: &str, members: &[String]) -> Result<Team, TenancyError> {
        let entry = self.set_in_memory(tenant, team, members)?;
        self.save()?;
        Ok(entry)
    }

    fn save(&self) -> Result<(), TenancyError> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let body = self.to_json().to_string();
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Whether `a` and `b` share a team (and are different principals).
    pub fn teammates(&self, a: &PrincipalId, b: &PrincipalId) -> bool {
        if a == b {
            return false;
        }
        self.teams
            .lock()
            .map(|teams| teams.values().any(|t| t.members.contains(a) && t.members.contains(b)))
            .unwrap_or(false)
    }

    /// Every team, as `{"teams": [{"tenant", "team", "members"}]}`.
    pub fn to_json(&self) -> Value {
        let teams: Vec<Value> = self
            .teams
            .lock()
            .map(|teams| {
                teams
                    .values()
                    .map(|t| json!({ "tenant": t.tenant, "team": t.team, "members": t.members.iter().map(PrincipalId::as_str).collect::<Vec<_>>() }))
                    .collect()
            })
            .unwrap_or_default();
        json!({ "teams": teams })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn p(id: &str) -> PrincipalId {
        PrincipalId::parse(id).unwrap()
    }

    #[test]
    fn teams_persist_and_answer_membership() {
        let dir = std::env::temp_dir().join(format!("shogun_teams_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let reg = TeamRegistry::load(&dir).unwrap();
        reg.set("acme", "ops", &["alice".into(), "bob".into()]).unwrap();
        assert!(reg.teammates(&p("alice"), &p("bob")));
        assert!(!reg.teammates(&p("alice"), &p("carol")));
        assert!(!reg.teammates(&p("alice"), &p("alice")));
        let again = TeamRegistry::load(&dir).unwrap();
        assert!(again.teammates(&p("bob"), &p("alice")));
        assert!(reg.set("acme", "ops", &["Alice".into()]).is_err());
        again.set("acme", "ops", &[]).unwrap();
        assert!(!TeamRegistry::load(&dir).unwrap().teammates(&p("alice"), &p("bob")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
