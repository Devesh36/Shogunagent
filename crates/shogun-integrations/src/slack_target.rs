//! Proposal-time Slack addressing. Exact matches only; no sends or HTTP here.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use shogun_agents::permission::is_slack_conversation_id;
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedTarget {
    pub id: String,
    pub label: String,
}

#[derive(Clone, Debug)]
pub struct Candidate {
    pub target: ResolvedTarget,
    pub names: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetKind {
    Channel,
    User,
}

/// Owned by one desktop session, invalidated on connector authorization changes.
#[derive(Default)]
pub struct Resolver {
    cache: HashMap<String, ResolvedTarget>,
}

impl Resolver {
    pub fn resolve(
        &mut self,
        input: &str,
        mut lookup: impl FnMut(TargetKind, &str) -> Result<Vec<Candidate>, String>,
    ) -> Result<ResolvedTarget, String> {
        let input = input.trim();
        if is_slack_conversation_id(input) {
            return Ok(ResolvedTarget {
                id: input.into(),
                label: input.into(),
            });
        }
        if input.is_empty() || input.len() > 256 || input.chars().any(char::is_control) {
            return Err(json!({"code":"slack_target_invalid"}).to_string());
        }
        let kind = if input.starts_with('@') || input.contains('@') {
            TargetKind::User
        } else {
            TargetKind::Channel
        };
        let name = input.trim_start_matches(['#', '@']);
        if name.is_empty() {
            return Err(json!({"code":"slack_target_invalid"}).to_string());
        }
        let key = format!("{kind:?}:{}", name.to_lowercase());
        if let Some(hit) = self.cache.get(&key) {
            return Ok(hit.clone());
        }
        let mut matches = lookup(kind, name)?
            .into_iter()
            .filter(|c| {
                is_slack_conversation_id(&c.target.id)
                    && c.names
                        .iter()
                        .any(|n| n.to_lowercase() == name.to_lowercase())
            })
            .map(|c| c.target)
            .collect::<Vec<_>>();
        matches.sort_by(|a, b| a.id.cmp(&b.id));
        matches.dedup_by(|a, b| a.id == b.id);
        match matches.as_slice() {
            [target] => {
                // Bound session memory without ever caching misses or ambiguous results.
                if self.cache.len() >= 256 {
                    self.cache.clear();
                }
                self.cache.insert(key, target.clone());
                Ok(target.clone())
            }
            [] => Err(json!({"code":"slack_target_not_found","target":input}).to_string()),
            _ => Err(
                json!({"code":"slack_target_ambiguous","target":input,"matches":matches})
                    .to_string(),
            ),
        }
    }
}

/// Exhaust bounded search pages before deciding uniqueness. The caller owns read gates and traces.
pub fn lookup(
    kind: TargetKind,
    query: &str,
    mut call: impl FnMut(&str, Value) -> Result<Value, String>,
) -> Result<Vec<Candidate>, String> {
    let tool = if kind == TargetKind::Channel {
        "slack_search_channels"
    } else {
        "slack_search_users"
    };
    let unavailable = || json!({"code":"slack_target_lookup_unavailable"}).to_string();
    let mut cursor = None;
    let mut seen = std::collections::HashSet::new();
    let mut all = Vec::new();
    for _ in 0..10 {
        let mut arguments = json!({"query":query,"limit":20});
        if let Some(cursor) = &cursor {
            arguments["cursor"] = json!(cursor);
        }
        let result = call(tool, arguments)?;
        let payload = crate::result::payload(&result).map_err(|_| unavailable())?;
        let data = payload.get("data").unwrap_or(&payload);
        let next = data
            .pointer("/response_metadata/next_cursor")
            .or_else(|| data.get("next_cursor"))
            .or_else(|| data.get("cursor"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let mut page = data.clone();
        if let Some(object) = page.as_object_mut() {
            object.remove("response_metadata");
            object.remove("next_cursor");
            object.remove("cursor");
            object.remove("total");
            object.remove("has_more");
        }
        let rows = candidates(&page, kind)?;
        let count = rows.len();
        all.extend(rows);
        match next {
            Some(next) if seen.insert(next.to_string()) => cursor = Some(next.to_string()),
            Some(_) => return Err(unavailable()),
            None => {
                if data.get("has_more").and_then(Value::as_bool) == Some(true)
                    || data
                        .get("total")
                        .and_then(Value::as_u64)
                        .is_some_and(|n| n > all.len() as u64)
                    || count >= 20
                {
                    return Err(unavailable());
                }
                return Ok(all);
            }
        }
    }
    Err(unavailable())
}

/// Parse structured official-MCP search results. Refuse incomplete pages rather than falsely
/// accepting one of several matches. Natural-language responses need a provider adapter update.
pub fn candidates(payload: &Value, kind: TargetKind) -> Result<Vec<Candidate>, String> {
    let unavailable = || json!({"code":"slack_target_lookup_unavailable"}).to_string();
    let data = payload.get("data").unwrap_or(payload);
    let rows = data
        .as_array()
        .or_else(|| {
            [
                if kind == TargetKind::User {
                    "users"
                } else {
                    "channels"
                },
                "results",
                "matches",
            ]
            .iter()
            .find_map(|key| data.get(key).and_then(Value::as_array))
        })
        .ok_or_else(unavailable)?;
    if rows.len() >= 100
        || data.get("has_more").and_then(Value::as_bool) == Some(true)
        || data
            .pointer("/response_metadata/next_cursor")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
        || data
            .get("next_cursor")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
        || data
            .get("total")
            .and_then(Value::as_u64)
            .is_some_and(|n| n > rows.len() as u64)
    {
        return Err(unavailable());
    }
    rows.iter()
        .filter(|row| {
            row.get("deleted").and_then(Value::as_bool) != Some(true)
                && row.get("is_archived").and_then(Value::as_bool) != Some(true)
        })
        .map(|row| {
            let text = |key: &str| row.get(key).and_then(Value::as_str);
            let id = text("id")
                .or_else(|| text("user_id"))
                .or_else(|| text("channel_id"))
                .ok_or_else(unavailable)?;
            if !is_slack_conversation_id(id)
                || (kind == TargetKind::User && !id.starts_with(['U', 'W']))
                || (kind == TargetKind::Channel && !id.starts_with(['C', 'G', 'D']))
            {
                return Err(unavailable());
            }
            let mut names = Vec::new();
            for value in [
                row.get("name"),
                row.get("real_name"),
                row.get("email"),
                row.get("display_name"),
                row.pointer("/profile/display_name"),
                row.pointer("/profile/real_name"),
                row.pointer("/profile/email"),
            ] {
                if let Some(name) = value.and_then(Value::as_str).filter(|s| !s.is_empty()) {
                    names.push(name.into());
                }
            }
            let name = if kind == TargetKind::Channel {
                text("name")
            } else {
                row.pointer("/profile/display_name")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .or_else(|| text("display_name"))
                    .or_else(|| text("real_name"))
                    .or_else(|| text("name"))
            }
            .filter(|s| !s.is_empty())
            .ok_or_else(unavailable)?;
            let workspace = text("team_name")
                .or_else(|| text("workspace_name"))
                .or_else(|| row.pointer("/team/name").and_then(Value::as_str))
                .or_else(|| data.pointer("/team/name").and_then(Value::as_str))
                .or_else(|| text("team_id"));
            let prefix = if kind == TargetKind::Channel {
                '#'
            } else {
                '@'
            };
            let mut label = format!("{prefix}{name}");
            if let Some(workspace) = workspace {
                label.push_str(&format!(" · {workspace}"));
            }
            if kind == TargetKind::User {
                if let Some(email) = row
                    .pointer("/profile/email")
                    .and_then(Value::as_str)
                    .or_else(|| text("email"))
                {
                    label.push_str(&format!(" · {email}"));
                }
            }
            if label.len() > 512 || label.chars().any(char::is_control) {
                return Err(unavailable());
            }
            Ok(Candidate {
                target: ResolvedTarget {
                    id: id.into(),
                    label,
                },
                names,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn candidate(id: &str, name: &str) -> Candidate {
        Candidate {
            target: ResolvedTarget {
                id: id.into(),
                label: format!("#{name} · Acme"),
            },
            names: vec![name.into()],
        }
    }
    #[test]
    fn exact_case_insensitive_cache_ambiguity_missing_and_ids() {
        let mut resolver = Resolver::default();
        let hit = resolver
            .resolve("#Design", |kind, query| {
                assert_eq!(kind, TargetKind::Channel);
                assert_eq!(query, "Design");
                Ok(vec![
                    candidate("C0123ABCD", "design"),
                    candidate("C0123ABCE", "design-team"),
                ])
            })
            .expect("test");
        assert_eq!(hit.id, "C0123ABCD");
        assert_eq!(hit.label, "#design · Acme");
        assert_eq!(
            resolver.resolve("design", |_, _| panic!("cached")).expect("test"),
            hit
        );
        for id in ["C0123ABCD", "U0123ABCD", "D0123ABCD"] {
            assert_eq!(
                resolver.resolve(id, |_, _| panic!("raw ID")).expect("test").id,
                id
            );
        }
        assert!(resolver
            .resolve("missing", |_, _| Ok(vec![]))
            .unwrap_err()
            .contains("slack_target_not_found"));
        let ambiguous = resolver
            .resolve("@Dana", |kind, _| {
                assert_eq!(kind, TargetKind::User);
                Ok(vec![
                    candidate("U0123ABCD", "dana"),
                    candidate("U0123ABCE", "Dana"),
                ])
            })
            .unwrap_err();
        assert!(ambiguous.contains("slack_target_ambiguous") && ambiguous.contains("U0123ABCE"));
        assert!(resolver
            .resolve("dana@example.com", |_, _| Ok(vec![candidate(
                "U0123ABCD",
                "dana@example.com"
            )]))
            .is_ok());
    }
    #[test]
    fn pagination_collects_duplicates_before_resolution() {
        let mut calls = 0;
        let result = Resolver::default().resolve("#design", |kind, query| {
            lookup(kind, query, |tool, args| {
                calls += 1;
                assert_eq!(tool, "slack_search_channels");
                let page = if calls == 1 {
                    assert!(args.get("cursor").is_none());
                    json!({"channels":[{"id":"C0123ABCD","name":"design"}],"next_cursor":"page2"})
                } else {
                    assert_eq!(args["cursor"], "page2");
                    json!({"channels":[{"id":"C0123ABCE","name":"design"}]})
                };
                Ok(json!({"structuredContent":page}))
            })
        });
        assert_eq!(calls, 2);
        assert!(result.unwrap_err().contains("slack_target_ambiguous"));
    }

    #[test]
    fn parses_profiles_and_refuses_incomplete_results() {
        let value = json!({"users":[{"id":"U0123ABCD","name":"dana","team_name":"Acme","profile":{"display_name":"Dana","email":"dana@example.com"}}]});
        let rows = candidates(&value, TargetKind::User).expect("test");
        assert_eq!(rows[0].target.label, "@Dana · Acme · dana@example.com");
        assert!(candidates(&json!({"channels":[],"has_more":true}), TargetKind::Channel).is_err());
    }
}
