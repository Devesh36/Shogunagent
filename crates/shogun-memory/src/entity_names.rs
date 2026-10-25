//! The names a person or project goes by.
//!
//! A `people` / `projects` row has one display name and an accumulating `aliases` list (first
//! name, handle, nickname, a misspelling). A new mention is matched, normalised
//! ([`crate::memory_identity::entity_name_key`]), against the name OR any alias, so a case or
//! wording variant never becomes a second row; each match adds what it learned back here.

use std::collections::HashSet;

use rusqlite::{params, Connection, OptionalExtension};

use crate::state::StateTable;

/// Aliases kept per row. A handful covers a first name, a handle, a nickname and two misspellings.
pub const MAX_ALIASES: usize = 12;
const MAX_NAME_BYTES: usize = 80;

/// Normalised form of a name; see [`crate::memory_identity::entity_name_key`].
pub fn key(name: &str) -> String {
    crate::memory_identity::entity_name_key(name)
}

/// The strings of a stored JSON array. Malformed JSON and non-string entries read as absent.
pub fn parse_aliases(raw: Option<&str>) -> Vec<String> {
    raw.and_then(|r| serde_json::from_str::<Vec<serde_json::Value>>(r).ok())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

/// `names` without blanks, control characters, over-long entries or duplicates (by [`key`]), and
/// without the name whose key is `except_key`. First spelling wins; capped at [`MAX_ALIASES`].
pub fn union<I: IntoIterator<Item = String>>(names: I, except_key: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for name in names {
        let name = name.trim();
        if name.is_empty() || name.len() > MAX_NAME_BYTES || name.chars().any(char::is_control) {
            continue;
        }
        let k = key(name);
        if k.is_empty() || k == except_key || !seen.insert(k) {
            continue;
        }
        out.push(name.to_string());
        if out.len() >= MAX_ALIASES {
            break;
        }
    }
    out
}

/// Han, kana and Hangul: scripts whose names are short and written without spaces.
fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF)
}

/// Shortest name part that counts on its own: 2 characters for CJK ("山田"), 3 otherwise ("Al" is noise).
pub(crate) fn min_part_chars(s: &str) -> usize {
    if s.chars().any(is_cjk) {
        2
    } else {
        3
    }
}

/// `needle` occurs in `hay` as whole words. CJK is written without spaces, so there it is plain
/// containment.
pub(crate) fn contains_word(hay: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    if needle.chars().any(is_cjk) {
        return hay.contains(needle);
    }
    hay.match_indices(needle).any(|(i, _)| {
        let before = hay[..i].chars().next_back();
        let after = hay[i + needle.len()..].chars().next();
        !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
    })
}

/// Two sets of [`key`]s name one entity lexically: a shared key, a shared word, or one word a
/// prefix of the other ("toru" / "torutesu", "山田" / "山田太郎").
pub(crate) fn related(a: &HashSet<String>, b: &HashSet<String>) -> bool {
    if a.iter().any(|k| b.contains(k)) {
        return true;
    }
    let parts = |keys: &HashSet<String>| -> Vec<String> {
        keys.iter()
            .flat_map(|k| k.split(' ').map(str::to_string).collect::<Vec<_>>())
            .filter(|t| t.chars().count() >= min_part_chars(t))
            .collect()
    };
    let (ta, tb) = (parts(a), parts(b));
    ta.iter().any(|x| tb.iter().any(|y| x == y || x.starts_with(y.as_str()) || y.starts_with(x.as_str())))
}

fn words(s: &str) -> usize {
    s.split_whitespace().count()
}

/// "mikel", "TORU TANO": no capital/lowercase distinction left to lose.
fn flat_case(s: &str) -> bool {
    s.chars().any(char::is_alphabetic) && (s == s.to_lowercase() || s == s.to_uppercase())
}

/// A display name, not a handle, an email, a title line ("X · Landing page") or OCR noise.
fn plausible_name(s: &str) -> bool {
    (1..=4).contains(&words(s))
        && !s.contains('@')
        && !s.chars().any(|c| {
            c.is_ascii_digit()
                || c.is_control()
                || matches!(c, '(' | ')' | '|' | '/' | '\u{b7}' | '\u{2022}' | '\u{2013}' | '\u{2014}' | ':')
        })
}

/// Should `incoming` replace `current` as the display name of the same **person**? Only when it
/// extends the current name ("Toru" → "Toru Tano": every current word kept, more words added), or
/// is a proper-cased spelling of the same name ("Mikel" over "mikel"). A different, merely longer
/// name never wins. Projects are never renamed automatically: their new names only become aliases
/// ([`absorb`], `entity_dedupe::merge`). Only ever called for two names already known to be the
/// same entity.
pub fn better_name(current: &str, incoming: &str) -> bool {
    if !plausible_name(incoming) {
        return false;
    }
    let (c, i) = (key(current), key(incoming));
    better_case(current, incoming) || (c != i && words(incoming) > words(current) && contains_word(&i, &c))
}

/// The same name, proper-cased where the current spelling is flat ("ShogunAI" over "shogunai").
/// Not a rename: the only display change a project ever takes automatically.
pub fn better_case(current: &str, incoming: &str) -> bool {
    key(current) == key(incoming) && flat_case(current) && !flat_case(incoming)
}

/// The best display name among spellings of one entity, starting from `first`.
pub fn fullest<'a>(first: &'a str, others: impl IntoIterator<Item = &'a str>) -> String {
    let mut best = first;
    for name in others {
        if better_name(best, name) {
            best = name;
        }
    }
    best.trim().to_string()
}

fn cols(table: StateTable) -> Option<(&'static str, &'static str)> {
    match table {
        StateTable::People => Some(("people", "display_name")),
        StateTable::Projects => Some(("projects", "name")),
        _ => None,
    }
}

/// Is the row the user's own wording? A pinned, edited or re-worded row keeps its name.
pub(crate) const LOCKED_SQL: &str = "(user_pinned != 0 OR user_edited_at IS NOT NULL OR wording_edited != 0)";

/// Record what a matched mention taught us about row `id`: the name as written and the model's
/// `aliases` become aliases, and the fuller proper-cased name becomes the display name unless the
/// user locked it. An alias is only believed if it appears in the source text (`source_key` is
/// [`key`] of it) or is a part of the mention's own name, so the model cannot invent one.
pub fn absorb(
    conn: &Connection,
    table: StateTable,
    id: i64,
    name: &str,
    aliases: &[String],
    source_key: &str,
    now: i64,
) -> Result<(), rusqlite::Error> {
    let Some((t, name_col)) = cols(table) else { return Ok(()) };
    let row: Option<(String, Option<String>, bool)> = conn
        .query_row(
            &format!("SELECT {name_col}, aliases, {LOCKED_SQL} FROM {t} WHERE id = ?1"),
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((display, raw, locked)) = row else { return Ok(()) };
    let name_key = key(name);
    let believed = |alias: &str| {
        let k = key(alias);
        k.chars().count() >= min_part_chars(&k) && (contains_word(source_key, &k) || name_key.split(' ').any(|part| part == k))
    };
    // A project keeps its name (only its case may improve); a person's grows only by extension
    // (first name → full name).
    let better = if table == StateTable::People { better_name(&display, name) } else { better_case(&display, name) };
    let new_display = if !locked && better { name.trim().to_string() } else { display.clone() };
    let mut all = parse_aliases(raw.as_deref());
    all.push(display.clone());
    all.extend(aliases.iter().filter(|a| believed(a)).cloned());
    all.push(name.to_string());
    let merged = union(all, &key(&new_display));
    let json = if merged.is_empty() { None } else { serde_json::to_string(&merged).ok() };
    if new_display == display && json == raw {
        return Ok(());
    }
    conn.execute(
        &format!("UPDATE {t} SET {name_col} = ?2, aliases = ?3 WHERE id = ?1"),
        params![id, new_display, json],
    )?;
    if new_display != display {
        let source = if table == StateTable::People {
            crate::search_documents::SourceRef::Person(id)
        } else {
            crate::search_documents::SourceRef::Project(id)
        };
        let _ = crate::search_documents::project_source_after_write(conn, &source, now);
    }
    Ok(())
}

/// One-time repair (idempotent) for projects an earlier build renamed to a longer title line: a
/// name "X · Y" (or "X | Y", "X — Y", "X – Y") whose head X is, spaces aside, one of the row's own
/// aliases goes back to that alias, the first such in alias order (the oldest spelling kept), and
/// the title line stays as an alias. Rows the user touched are left alone. Returns rows repaired.
pub fn restore_project_names(conn: &Connection, now: i64) -> Result<usize, rusqlite::Error> {
    const SEPARATORS: [&str; 4] = [" \u{b7} ", " | ", " \u{2014} ", " \u{2013} "];
    let compact = |s: &str| key(s).replace(' ', "");
    let rows: Vec<(i64, String, Option<String>)> = {
        let mut stmt = conn.prepare(&format!("SELECT id, name, aliases FROM projects WHERE aliases IS NOT NULL AND NOT {LOCKED_SQL}"))?;
        let it = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        it.collect::<Result<_, _>>()?
    };
    let mut repaired = 0;
    for (id, name, raw) in rows {
        let Some(head) = SEPARATORS.iter().filter_map(|sep| name.split_once(sep)).map(|(head, _)| head).min_by_key(|h| h.len())
        else {
            continue;
        };
        let head = compact(head);
        let aliases = parse_aliases(raw.as_deref());
        let Some(original) = aliases.iter().find(|a| !head.is_empty() && compact(a) == head).cloned() else { continue };
        let merged = union(aliases.into_iter().chain(std::iter::once(name)), &key(&original));
        conn.execute(
            "UPDATE projects SET name = ?2, aliases = ?3 WHERE id = ?1",
            params![id, original, serde_json::to_string(&merged).ok()],
        )?;
        let _ = crate::search_documents::project_source_after_write(conn, &crate::search_documents::SourceRef::Project(id), now);
        repaired += 1;
    }
    Ok(repaired)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn keys_ignore_case_spacing_accents_and_width() {
        assert_eq!(key("Mikel"), key(" mikel "));
        assert_eq!(key("Toru  Tano"), key("toru tano"));
        assert_eq!(key("Élodie Martin"), key("elodie MARTIN"));
        assert_eq!(key("ＡＢＣ"), key("abc"), "full-width letters");
        assert_ne!(key("が"), key("か"), "kana voicing is not an accent");
        assert_ne!(key("Mikel"), key("Mike"));
    }

    #[test]
    fn union_dedupes_by_key_and_drops_the_display_name() {
        let out = union(["Toru", "toru", "TORUTESU", "Toru Tano", "", "x\u{7}y"].map(String::from), &key("Toru Tano"));
        assert_eq!(out, ["Toru", "TORUTESU"]);
    }

    #[test]
    fn the_fuller_proper_cased_name_wins() {
        assert!(better_name("toru", "Toru Tano"));
        assert!(better_name("mikel", "Mikel"));
        assert!(!better_name("Toru Tano", "toru"));
        assert!(!better_name("Mikel", "mikel"));
        assert!(!better_name("Toru", "toru@example.com"), "an email is not a name");
        assert!(!better_name("Toru", "Toru Tano (CEO)"), "a title line is not a name");
        assert!(!better_name("山田 太郎", "山田 太郎"));
        assert_eq!(fullest("toru", ["Torutesu", "toru tano", "Toru Tano"]), "Toru Tano");
    }

    #[test]
    fn a_person_is_renamed_only_by_extension_and_a_project_never() {
        assert!(better_name("Toru", "Toru Tano"), "first name → full name");
        assert!(!better_name("Toru Tano", "Dana Reyes Lopez"), "a longer, different name is not an extension");
        assert!(!better_name("Toru", "Torutesu Tano"), "a word must be kept whole");
        assert!(!better_name("Toru", "Toru \u{b7} Slack"), "a title line is not a name");
        let conn = crate::open_in_memory().unwrap();
        conn.execute(
            "INSERT INTO projects(name,status,confidence,base_confidence,created_at,updated_at,last_evidence_at)
             VALUES('Shogun AI','active',0.6,0.6,1,1,1)",
            [],
        )
        .unwrap();
        let id = conn.last_insert_rowid();
        absorb(&conn, StateTable::Projects, id, "Shogun AI Landing page", &[], &key("Shogun AI Landing page"), 5).unwrap();
        let (name, aliases): (String, Option<String>) =
            conn.query_row("SELECT name, aliases FROM projects WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!(name, "Shogun AI", "a project keeps its name");
        assert_eq!(parse_aliases(aliases.as_deref()), ["Shogun AI Landing page"], "the new name becomes an alias");
        // An earlier build's rename to a title line is put back once, from the row's own aliases.
        conn.execute(
            "UPDATE projects SET name = 'ShogunAI \u{b7} Landing page', aliases = '[\"Shogun AI\",\"ShogunAI\",\"SHOGUN\"]' WHERE id = ?1",
            [id],
        )
        .unwrap();
        assert_eq!(restore_project_names(&conn, 6).unwrap(), 1);
        let (name, aliases): (String, Option<String>) =
            conn.query_row("SELECT name, aliases FROM projects WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!(name, "Shogun AI");
        assert_eq!(parse_aliases(aliases.as_deref()), ["ShogunAI", "SHOGUN", "ShogunAI \u{b7} Landing page"]);
        assert_eq!(restore_project_names(&conn, 7).unwrap(), 0, "idempotent");
    }

    fn person(conn: &Connection, name: &str, aliases: Option<&str>, locked: bool) -> i64 {
        conn.execute(
            "INSERT INTO people(display_name,aliases,confidence,base_confidence,created_at,updated_at,last_evidence_at,user_pinned)
             VALUES(?1,?2,0.6,0.6,1,1,1,?3)",
            params![name, aliases, i64::from(locked)],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn read(conn: &Connection, id: i64) -> (String, Vec<String>) {
        conn.query_row("SELECT display_name, aliases FROM people WHERE id = ?1", [id], |r| {
            Ok((r.get::<_, String>(0)?, parse_aliases(r.get::<_, Option<String>>(1)?.as_deref())))
        })
        .unwrap()
    }

    #[test]
    fn absorb_accumulates_aliases_the_text_supports_and_promotes_the_fuller_name() {
        let conn = crate::open_in_memory().unwrap();
        let id = person(&conn, "toru", None, false);
        let source = key("Toru Tano (Torutesu on GitHub) will review");
        let aliases = vec!["Torutesu".to_string(), "Mr Invented".to_string(), "Toru".to_string()];
        absorb(&conn, StateTable::People, id, "Toru Tano", &aliases, &source, 5).unwrap();
        let (name, got) = read(&conn, id);
        assert_eq!(name, "Toru Tano");
        assert_eq!(got, ["toru", "Torutesu"], "old name kept as alias; an invented alias is not believed");
        // Idempotent: the same mention again changes nothing.
        absorb(&conn, StateTable::People, id, "Toru Tano", &aliases, &source, 6).unwrap();
        assert_eq!(read(&conn, id), (name, got));
    }

    #[test]
    fn a_locked_row_keeps_its_name_but_still_learns_aliases() {
        let conn = crate::open_in_memory().unwrap();
        let id = person(&conn, "toru", None, true);
        absorb(&conn, StateTable::People, id, "Toru Tano", &[], &key("Toru Tano"), 5).unwrap();
        let (name, got) = read(&conn, id);
        assert_eq!(name, "toru");
        assert_eq!(got, ["Toru Tano"]);
    }
}
