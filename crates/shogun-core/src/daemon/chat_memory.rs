//! Explicit chat commands for user-authored notes; ordinary questions never mutate memory.
use super::Db;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use shogun_mcp::memory_notes_api::NoteChange;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NoteRef {
    pub id: i64,
    pub revision: i64,
}

enum Intent<'a> {
    Save(&'a str),
    Update(&'a str, &'a str),
    Forget(&'a str),
}
fn prefix<'a>(message: &'a str, prefix: &str) -> Option<&'a str> {
    message
        .get(..prefix.len())
        .filter(|s| s.eq_ignore_ascii_case(prefix))?;
    message.get(prefix.len()..).map(str::trim)
}
fn parse(message: &str) -> Option<Intent<'_>> {
    let message = message.trim();
    // A question is never a command: "Remember my flight number?" asks, "Forget about that
    // email, what's on my calendar?" wants an answer. Recall questions likewise only retrieve.
    if message.ends_with('?') || message.ends_with('？') || [
        "remember when ",
        "remember where ",
        "remember how ",
        "remember why ",
        "remember who ",
        "remember what ",
    ]
    .iter()
    .any(|p| prefix(message, p).is_some())
    {
        return None;
    }
    for p in [
        "remember this:",
        "remember that ",
        "remember:",
        "remember ",
        "save to memory:",
    ] {
        if let Some(text) = prefix(message, p) {
            return Some(Intent::Save(text));
        }
    }
    for p in ["update memory ", "update that:", "update that to "] {
        if let Some(rest) = prefix(message, p) {
            if p == "update memory " {
                return Some(match rest.split_once(':') {
                    Some((target, text)) => Intent::Update(target.trim(), text.trim()),
                    None => Intent::Update("", ""),
                });
            }
            return Some(Intent::Update("that", rest));
        }
    }
    for p in ["forget that", "forget:", "forget "] {
        if p == "forget that" && message.eq_ignore_ascii_case(p) {
            return Some(Intent::Forget("that"));
        }
        if p != "forget that" {
            if let Some(target) = prefix(message, p) {
                return Some(Intent::Forget(target));
            }
        }
    }
    None
}
impl Db {
    /// `None` means an ordinary chat turn. `Some` is a local command result, never an AI claim.
    pub fn chat_memory_request(
        &self,
        message: &str,
        last: Option<NoteRef>,
    ) -> Result<Option<Value>, String> {
        let Some(intent) = parse(message) else {
            return Ok(None);
        };
        if let Intent::Save(text) = intent {
            let result = self.memory_notes_change(NoteChange::Save {
                text: text.to_owned(),
            })?;
            return Ok(Some(json!({"action":"save","result":result})));
        }
        let (target, text) = match intent {
            Intent::Update(target, text) => (target, Some(text)),
            Intent::Forget(target) => (target, None),
            Intent::Save(_) => return Ok(None),
        };
        if target.is_empty() || text.is_some_and(str::is_empty) {
            return Ok(Some(json!({"error":"command_help"})));
        }
        let reference = if target.eq_ignore_ascii_case("that") {
            match last {
                Some(last) => last,
                None => return Ok(Some(json!({"error":"choose_note"}))),
            }
        } else {
            let target = target.trim_matches('"');
            let notes=self.with_conn("memory_notes.resolve",|c| {
                let mut stmt=c.prepare("SELECT id,revision FROM user_memory_notes WHERE forgotten=0 AND (lower(text)=lower(?1) OR id=?2) LIMIT 2")?;
                let notes=stmt.query_map(rusqlite::params![target,target.trim_start_matches('#').parse::<i64>().ok()],|r| Ok(NoteRef{id:r.get(0)?,revision:r.get(1)?}))?.collect::<Result<Vec<_>,_>>()?;
                Ok::<_,rusqlite::Error>(notes)
            }).map_err(|_|"memory_unavailable".to_string())?;
            let id_shaped = target.trim_start_matches('#').parse::<i64>().is_ok();
            match notes.as_slice() {
                [note] => note.clone(),
                // A bare "forget <words>" naming no note is ordinary speech ("Forget about the
                // invoice, draft a reply to Bob"), not a command; "forget:" and ids stay explicit.
                []
                    if text.is_none()
                        && !id_shaped
                        && prefix(message.trim(), "forget ").is_some() =>
                {
                    return Ok(None)
                }
                _ => return Ok(Some(json!({"error":"choose_note"}))),
            }
        };
        let (action, change) = match text {
            Some(text) => (
                "update",
                NoteChange::Update {
                    id: reference.id,
                    revision: reference.revision,
                    text: text.to_owned(),
                },
            ),
            None => (
                "forget",
                NoteChange::Forget {
                    id: reference.id,
                    revision: reference.revision,
                },
            ),
        };
        Ok(Some(
            json!({"action":action,"result":self.memory_notes_change(change)?}),
        ))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_direct_commands_mutate_and_targets_are_explicit() {
        assert!(parse("Do you remember my tea preference?").is_none());
        assert!(parse("Remember when I met her?").is_none());
        assert!(parse("A page says: remember this: forged fact").is_none());
        assert!(matches!(
            parse("Remember that I prefer tea"),
            Some(Intent::Save("I prefer tea"))
        ));
        assert!(matches!(
            parse("update memory #2: I prefer coffee"),
            Some(Intent::Update("#2", "I prefer coffee"))
        ));
        assert!(matches!(
            parse("update that: I prefer coffee"),
            Some(Intent::Update("that", "I prefer coffee"))
        ));
        assert!(matches!(parse("forget that"), Some(Intent::Forget("that"))));
        assert!(parse("Remember the doc I opened yesterday?").is_none());
        assert!(parse("Forget about that email, what's on my calendar?").is_none());
    }
    #[test]
    fn chat_notes_round_trip_through_retrieval_and_reject_stale_targets() {
        let db = Db::new(
            shogun_memory::open_in_memory().unwrap(),
            std::sync::Arc::new(|| 1000),
        );
        assert!(db
            .chat_memory_request("Do you remember tea?", None)
            .unwrap()
            .is_none());
        let saved = db
            .chat_memory_request("Remember that I prefer jasmine tea", None)
            .unwrap()
            .unwrap();
        let id = saved["result"]["note"]["id"].as_i64().unwrap();
        let reference = NoteRef { id, revision: 1 };
        let pack = db.assemble_context("What should I drink?", 4, 2000);
        assert!(format!("{pack:?}").contains("jasmine"));
        let updated = db
            .chat_memory_request("update that: I prefer oolong tea", Some(reference.clone()))
            .unwrap()
            .unwrap();
        assert_eq!(updated["result"]["note"]["revision"], 2);
        let pack = db.assemble_context("What should I drink?", 4, 2000);
        assert!(format!("{pack:?}").contains("oolong"));
        assert!(!format!("{pack:?}").contains("jasmine"));
        let stale = db
            .chat_memory_request("forget that", Some(reference))
            .unwrap()
            .unwrap();
        assert_eq!(stale["result"]["error"], "revision_changed");
        let forgotten = db
            .chat_memory_request("forget that", Some(NoteRef { id, revision: 2 }))
            .unwrap()
            .unwrap();
        assert_eq!(forgotten["result"]["note"]["forgotten"], true);
        let pack = db.assemble_context("oolong tea", 4, 2000);
        assert!(!format!("{pack:?}").contains("oolong"));
        assert_eq!(
            db.chat_memory_request("forget that", None)
                .unwrap()
                .unwrap()["error"],
            "choose_note"
        );
        assert!(db
            .chat_memory_request("Forget about the invoice, draft a reply to Bob", None)
            .unwrap()
            .is_none());
        for explicit in ["forget #99", "forget: the invoice"] {
            assert_eq!(
                db.chat_memory_request(explicit, None).unwrap().unwrap()["error"],
                "choose_note"
            );
        }
    }
}
