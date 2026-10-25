//! The names the user goes by in chat apps, learned from what the screen shows, never hard-coded.
//!
//! The Mac account and the About You profile name the user, but a chat app shows a display name or
//! handle nothing else ties to them, so their own messages read as a stranger's and pages get who
//! said what backwards. Evidence that a name is the user's (V86 `self_name_evidence`, [`note`]):
//!
//! * `dm`: in a one-to-one chat (the title names the other party), the one other message author.
//! * `echo`: the author header at or above a message the user just sent ([`echo_author`]).
//! * `review`: a name the hourly review says the window shows is the user's.
//!
//! Two distinct windows, or one sent echo, make the name an alias of the user's own `self` people
//! row ([`learn`]). A person row made for that name then folds into the self row; one the user
//! edited is left alone, and so is the name.

use rusqlite::{params, Connection, OptionalExtension};

use crate::entity_names as names;
use crate::state::StateTable;

pub const KIND_DM: &str = "dm";
pub const KIND_ECHO: &str = "echo";
pub const KIND_REVIEW: &str = "review";

/// How far back the first scan of one-to-one chats looks.
const DM_SCAN_MS: i64 = 7 * 86_400_000;
/// Chat events one pass reads; the watermark carries the rest to the next pass.
const DM_SCAN_ROWS: i64 = 1_000;
/// Lines above a sent message searched for its author header.
const ECHO_LOOKBACK: usize = 12;
const SCAN_MARK: &str = "self_names.dm_scan";
const PAGES_REFRESH_MARK: &str = "pages.speaker_refresh";

/// A display name as a chat shows it: 1 to 3 words, at most 40 characters, a letter, no clock
/// time and no punctuation a sentence or a UI label would carry.
fn plausible(name: &str) -> Option<&str> {
    let n = name.trim();
    let words = n.split_whitespace().count();
    let ok = (1..=3).contains(&words)
        && n.chars().count() <= 40
        && n.chars().any(char::is_alphabetic)
        && !n.starts_with('@')
        && !n.contains([':', ',', '"', '|'])
        && !crate::people_quality::has_clock_time(n);
    ok.then_some(n)
}

/// The other party of a one-to-one chat window, from its title: Discord "@kai - Discord" (app or
/// browser), Slack "Kai (DM) - Acme - Slack". `None` for a channel, a group, or any other window.
pub fn dm_party(title: &str) -> Option<&str> {
    let t = title.trim();
    if let Some(rest) = t.strip_prefix('@') {
        return [" - Discord", " | Discord"].iter().find_map(|sep| rest.split_once(sep)).and_then(|(p, _)| plausible(p));
    }
    if t.ends_with("Slack") {
        return t.split_once(" (DM)").and_then(|(p, _)| plausible(p));
    }
    None
}

/// The author of a captured message row: "kai Server Tag: X , text , 10:29 AM", "kai replying to
/// dana , text , 10:31 AM", "kai , text , 9:05 PM". `None` for any other line.
// ponytail: the accessibility row shape chat apps like Discord expose; a header on its own line
// above the messages (Slack's "Kai  10:29 AM") is not read, add it when its capture is verified.
pub fn message_author(line: &str) -> Option<&str> {
    let (head, rest) = line.split_once(" , ")?;
    if !crate::people_quality::has_clock_time(rest) {
        return None;
    }
    let head = head.split_once(" Server Tag:").map_or(head, |(h, _)| h);
    let head = [" replying to ", " replied to "].iter().find_map(|s| head.split_once(s)).map_or(head, |(h, _)| h);
    plausible(head)
}

/// The author header of the newest line showing `sent` in `visible`, or the nearest one above it
/// (a run of messages from one author shows the header once).
pub fn echo_author(sent: &str, visible: &str) -> Option<String> {
    let norm = |s: &str| crate::rewrite_loop::clean(s).split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    let t = norm(sent);
    if t.is_empty() {
        return None;
    }
    let lines: Vec<&str> = visible.lines().collect();
    let at = lines.iter().rposition(|l| norm(l).contains(&t) || crate::rewrite_loop::similarity(l, sent) >= 0.9)?;
    // A header-less row ("Okay , 10:30 AM") puts the message where the author would be: never
    // take the sent text, or a piece of it, as the user's name.
    lines[..=at]
        .iter()
        .rev()
        .take(ECHO_LOOKBACK)
        .find_map(|l| message_author(l))
        .filter(|author| !t.contains(&norm(author)))
        .map(str::to_string)
}

/// Record that window `window` showed `name` is the user's. Returns whether it was new.
pub fn note(conn: &Connection, name: &str, kind: &str, window: &str, now: i64) -> rusqlite::Result<bool> {
    let Some(name) = plausible(name) else { return Ok(false) };
    let n = conn.execute(
        "INSERT OR IGNORE INTO self_name_evidence (name_key, name, window_key, kind, seen_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![names::key(name), name, window, kind, now],
    )?;
    Ok(n > 0)
}

fn marker(conn: &Connection, key: &str) -> rusqlite::Result<Option<i64>> {
    conn.query_row("SELECT value FROM memory_markers WHERE key = ?1", [key], |r| r.get(0)).optional()
}

/// One-to-one chat windows captured since the last pass: each event whose rows name exactly one
/// author other than the titled party is `dm` evidence for that author. An author who is the
/// titled party of any scanned chat is never the user.
fn scan_dms(conn: &Connection, now: i64) -> rusqlite::Result<()> {
    let from = marker(conn, SCAN_MARK)?.unwrap_or(0);
    let newest: i64 = conn.query_row("SELECT COALESCE(MAX(id), 0) FROM event_log", [], |r| r.get(0))?;
    let rows: Vec<(i64, String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT id, window_title, content FROM event_log
              WHERE id > ?1 AND id <= ?2 AND ts > ?3
                AND (window_title LIKE '@%Discord%' OR window_title LIKE '%(DM)%Slack')
              ORDER BY id LIMIT ?4",
        )?;
        let it = stmt.query_map(params![from, newest, now - DM_SCAN_MS, DM_SCAN_ROWS], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        it.collect::<Result<_, _>>()?
    };
    let full = rows.len() as i64 == DM_SCAN_ROWS;
    let mark = if full { rows.last().map_or(newest, |r| r.0) } else { newest };
    let first = |k: &str| k.split(' ').next().unwrap_or_default().to_string();
    let mut parties = std::collections::HashSet::new();
    let mut found: Vec<(String, String)> = Vec::new();
    for (_, title, content) in &rows {
        let Some(party) = dm_party(title) else { continue };
        let pk = names::key(party);
        let mut authors: Vec<&str> = Vec::new();
        for a in content.lines().filter_map(message_author) {
            let ak = names::key(a);
            if ak != pk && first(&ak) != first(&pk) && !authors.iter().any(|b| names::key(b) == ak) {
                authors.push(a);
            }
        }
        if let [author] = authors[..] {
            found.push((author.to_string(), pk.clone()));
        }
        parties.insert(pk);
    }
    for (author, party) in found {
        if !parties.contains(&names::key(&author)) {
            note(conn, &author, KIND_DM, &format!("dm:{party}"), now)?;
        }
    }
    conn.execute(
        "INSERT INTO memory_markers (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = ?2",
        params![SCAN_MARK, mark],
    )?;
    Ok(())
}

/// The owner's own people row, created from `owners` (the Mac account / profile names,
/// [`crate::people_quality::owner_names`]) when there is none. `None` when no row exists and no
/// name is known yet.
pub fn ensure_self_row(conn: &Connection, owners: &[String], now: i64) -> rusqlite::Result<Option<i64>> {
    let id: Option<i64> = conn.query_row("SELECT MIN(id) FROM people WHERE person_class = 'self'", [], |r| r.get(0))?;
    if id.is_some() {
        return Ok(id);
    }
    let Some(display) = owners.iter().find(|n| n.contains(' ')).or(owners.first()).cloned() else { return Ok(None) };
    let aliases = names::union(owners.iter().cloned(), &names::key(&display));
    let aliases = (!aliases.is_empty()).then(|| serde_json::to_string(&aliases).ok()).flatten();
    conn.execute(
        "INSERT INTO people (display_name, aliases, person_class, confidence, base_confidence, created_at, updated_at, last_evidence_at)
         VALUES (?1, ?2, 'self', 0.9, 0.9, ?3, ?3, ?3)",
        params![display, aliases, now],
    )?;
    Ok(Some(conn.last_insert_rowid()))
}

/// The user's own details for chat: the self row's name, aliases, emails and handles, plus the
/// addresses their connected mailbox sends from (`mail_identities`). Facts about the user.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct OwnDetails {
    pub name: Option<String>,
    pub aliases: Vec<String>,
    pub emails: Vec<String>,
    pub handles: Vec<String>,
}

/// The self row's name and its aliases / emails / handles JSON columns.
type SelfRow = (String, Option<String>, Option<String>, Option<String>);

pub fn own_details(conn: &Connection) -> rusqlite::Result<OwnDetails> {
    let row: Option<SelfRow> = conn
        .query_row(
            "SELECT display_name, aliases, emails, handles FROM people WHERE person_class = 'self' ORDER BY id LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let mut own = OwnDetails::default();
    if let Some((name, aliases, emails, handles)) = row {
        own.aliases = names::parse_aliases(aliases.as_deref());
        own.emails = names::parse_aliases(emails.as_deref());
        own.handles = names::parse_aliases(handles.as_deref());
        own.name = Some(name);
    }
    own.emails = with_addresses(std::mem::take(&mut own.emails), crate::mail_triage::identities(conn)?);
    Ok(own)
}

/// `stored` then each address of `more` it lacks (case-insensitive); entries without an `@` drop.
fn with_addresses(stored: Vec<String>, more: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for e in stored.into_iter().chain(more) {
        let e = e.trim();
        if e.contains('@') && !out.iter().any(|o| o.eq_ignore_ascii_case(e)) {
            out.push(e.to_string());
        }
    }
    out
}

/// Add the addresses the user's mailbox sends from to the self row's emails. Additive only; a row
/// the user edited keeps exactly the emails they gave it, and malformed stored JSON is left alone.
fn absorb_mail_identities(conn: &Connection, self_id: i64) -> rusqlite::Result<()> {
    let (raw, locked): (Option<String>, bool) = conn.query_row(
        &format!("SELECT emails, {} FROM people WHERE id = ?1", names::LOCKED_SQL),
        [self_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let malformed = raw.as_deref().is_some_and(|r| serde_json::from_str::<Vec<serde_json::Value>>(r).is_err());
    if locked || malformed {
        return Ok(());
    }
    let stored = names::parse_aliases(raw.as_deref());
    let new: Vec<String> = with_addresses(Vec::new(), crate::mail_triage::identities(conn)?)
        .into_iter()
        .filter(|a| !stored.iter().any(|s| s.trim().eq_ignore_ascii_case(a)))
        .collect();
    if new.is_empty() {
        return Ok(());
    }
    let emails: Vec<String> = stored.into_iter().chain(new).collect();
    conn.execute("UPDATE people SET emails = ?2 WHERE id = ?1", params![self_id, serde_json::to_string(&emails).ok()])?;
    Ok(())
}

/// Whether `name` is one of the names on the owner's own row.
pub fn is_self_name(conn: &Connection, name: &str) -> bool {
    let k = names::key(name);
    !k.is_empty() && self_keys(conn).is_ok_and(|keys| keys.contains(&k))
}

fn self_keys(conn: &Connection) -> rusqlite::Result<std::collections::HashSet<String>> {
    let mut stmt = conn.prepare("SELECT display_name, aliases FROM people WHERE person_class = 'self'")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))?;
    let mut keys = std::collections::HashSet::new();
    for row in rows {
        let (name, aliases) = row?;
        keys.insert(names::key(&name));
        keys.extend(names::parse_aliases(aliases.as_deref()).iter().map(|a| names::key(a)));
    }
    Ok(keys)
}

/// Names with enough evidence: two distinct windows, or one sent echo. A name that is the other
/// party of a one-to-one chat is someone the user writes to, never the user.
fn learned(conn: &Connection) -> rusqlite::Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT e.name_key, MIN(e.name) FROM self_name_evidence e
          WHERE NOT EXISTS (SELECT 1 FROM self_name_evidence d WHERE d.window_key = 'dm:' || e.name_key)
          GROUP BY e.name_key
         HAVING COUNT(DISTINCT e.window_key) >= 2 OR MAX(e.kind = 'echo') = 1",
    )?;
    let it = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    it.collect()
}

/// Make every learned name an alias of the self row and fold the person rows made for it into
/// that row. A name some user-edited person row carries is skipped: the user said that is someone
/// else. Returns the person rows folded in.
fn promote(conn: &mut Connection, self_id: i64, now: i64) -> rusqlite::Result<usize> {
    let mut merged = 0;
    for (key, name) in learned(conn)? {
        let matches: Vec<(i64, bool)> = {
            let mut stmt = conn.prepare(&format!(
                "SELECT id, display_name, aliases, {} FROM people WHERE COALESCE(person_class, '') <> 'self'",
                names::LOCKED_SQL
            ))?;
            let rows = stmt.query_map([], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, bool>(3)?))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (id, display, aliases, locked) = row?;
                if names::key(&display) == key || names::parse_aliases(aliases.as_deref()).iter().any(|a| names::key(a) == key) {
                    out.push((id, locked));
                }
            }
            out
        };
        if matches.iter().any(|(_, locked)| *locked) || (matches.is_empty() && self_keys(conn)?.contains(&key)) {
            continue;
        }
        // The self row keeps its own name, aliases and description through a merge; it gains
        // only the learned name.
        let (display, aliases, summary): (String, Option<String>, Option<String>) = conn.query_row(
            "SELECT display_name, aliases, relationship_summary FROM people WHERE id = ?1",
            [self_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let aliases = names::union(names::parse_aliases(aliases.as_deref()).into_iter().chain([name]), &names::key(&display));
        for (id, _) in matches {
            merged += crate::entity_dedupe::merge(conn, StateTable::People, &[self_id, id], None, now)?;
        }
        conn.execute(
            "UPDATE people SET display_name = ?2, aliases = ?3, relationship_summary = ?4, person_class = 'self', page = NULL
              WHERE id = ?1",
            params![self_id, display, serde_json::to_string(&aliases).ok(), summary],
        )?;
    }
    Ok(merged)
}

/// Once per database, after the first pass that knows the owner: every person and project page
/// is marked for a rewrite, so pages written before the user's names were known are redone under
/// the speaker rules. The per-run and daily rewrite caps still apply.
fn refresh_pages_once(conn: &Connection, now: i64) -> rusqlite::Result<bool> {
    let first = conn.execute("INSERT OR IGNORE INTO memory_markers (key, value) VALUES (?1, ?2)", params![PAGES_REFRESH_MARK, now])? > 0;
    if first {
        conn.execute("UPDATE people SET page_dirty = 1 WHERE page IS NOT NULL", [])?;
        conn.execute("UPDATE projects SET page_dirty = 1 WHERE page IS NOT NULL", [])?;
    }
    Ok(first)
}

/// The maintenance pass: read new one-to-one chats, add the mailbox's own addresses to the self
/// row, promote names with enough evidence, fold the person rows made for them into the self row,
/// then (once) mark every page for a rewrite. Nothing is promoted while the owner has no row and
/// no known name (`owners`, see [`ensure_self_row`]). Returns the person rows folded in.
pub fn learn(conn: &mut Connection, owners: &[String], now: i64) -> rusqlite::Result<usize> {
    scan_dms(conn, now)?;
    let Some(self_id) = ensure_self_row(conn, owners, now)? else { return Ok(0) };
    absorb_mail_identities(conn, self_id)?;
    let merged = promote(conn, self_id, now)?;
    refresh_pages_once(conn, now)?;
    Ok(merged)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const H: i64 = 3_600_000;

    fn event(conn: &Connection, ts: i64, title: &str, content: &str) -> i64 {
        crate::event_log::insert(
            conn,
            &crate::event_log::NewEvent {
                ts,
                source: "capture",
                kind: "text",
                app_bundle_id: Some("app.chat"),
                window_title: Some(title),
                content,
                content_hash: &format!("h{ts}{title}{content}"),
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            },
        )
        .unwrap()
    }

    fn ada() -> Vec<String> {
        vec!["Ada Example".to_string(), "adaex".to_string()]
    }

    fn person(conn: &Connection, name: &str, locked: bool) -> i64 {
        conn.execute(
            "INSERT INTO people (display_name, confidence, base_confidence, created_at, updated_at, last_evidence_at, user_pinned, page)
             VALUES (?1, 0.6, 0.6, 1, 1, 1, ?2, '{}')",
            params![name, i64::from(locked)],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn rows(conn: &Connection) -> Vec<(String, Option<String>)> {
        let mut stmt = conn.prepare("SELECT display_name, person_class FROM people ORDER BY id").unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(Result::unwrap).collect()
    }

    fn self_aliases(conn: &Connection) -> Vec<String> {
        let raw: Option<String> = conn.query_row("SELECT aliases FROM people WHERE person_class = 'self'", [], |r| r.get(0)).unwrap();
        names::parse_aliases(raw.as_deref())
    }

    #[test]
    fn titles_and_message_rows() {
        assert_eq!(dm_party("@dana - Discord"), Some("dana"));
        assert_eq!(dm_party("@Dana Reyes | Discord"), Some("Dana Reyes"));
        assert_eq!(dm_party("Dana (DM) - Acme - Slack"), Some("Dana"));
        assert_eq!(dm_party("#general | Acme - Discord"), None);
        assert_eq!(dm_party("Friends - Discord"), None);
        assert_eq!(message_author("kai Server Tag: CREW , ship it today , 10:29 AM"), Some("kai"));
        assert_eq!(message_author("dana replying to kai , sure , Yesterday at 9:05 PM"), Some("dana"));
        assert_eq!(message_author("dana , sure , 9:05 PM"), Some("dana"));
        assert_eq!(message_author("let us meet , at 10:30 , ok"), Some("let us meet"), "shape only: windows and the threshold filter");
        assert_eq!(message_author("we said this, then that , no time"), None);
        assert_eq!(message_author("Yesterday at 10:33 AM , hi , 10:33 AM"), None);
    }

    #[test]
    fn a_dm_title_finds_the_users_header_and_two_chats_make_it_an_alias() {
        let mut conn = crate::open_in_memory().unwrap();
        let now = 10 * H;
        event(&conn, now - H, "@dana - Discord", "dana , hi , 9:00 AM\nkai Server Tag: CREW , hey , 9:01 AM\nkai replying to dana , ok , 9:02 AM");
        learn(&mut conn, &ada(), now).unwrap();
        assert!(!self_aliases(&conn).contains(&"kai".to_string()), "one chat is not enough");
        assert_eq!(rows(&conn)[0], ("Ada Example".to_string(), Some("self".to_string())), "self row made from the account name");

        // A group-like window (two other authors) teaches nothing; a second one-to-one chat does.
        event(&conn, now, "@lee - Discord", "lee , yo , 9:00 AM\nkai , yo , 9:01 AM\nmo , hey , 9:02 AM");
        learn(&mut conn, &ada(), now + 1).unwrap();
        assert!(!self_aliases(&conn).contains(&"kai".to_string()));
        event(&conn, now + 2, "@lee - Discord", "lee , yo , 9:00 AM\nkai , sure , 9:01 AM");
        learn(&mut conn, &ada(), now + 3).unwrap();
        assert!(self_aliases(&conn).contains(&"kai".to_string()));
        assert!(is_self_name(&conn, "Kai"));
        assert!(!self_aliases(&conn).contains(&"dana".to_string()), "the titled party is never the user");
    }

    #[test]
    fn a_sent_echo_names_the_header_above_it_and_is_enough_alone() {
        let visible = "dana , hello there , 9:00 AM\nkai , first line , 9:01 AM\nsecond message from me\nfield";
        assert_eq!(echo_author("Second message from me", visible).as_deref(), Some("kai"), "nearest header above");
        assert_eq!(echo_author("first line", visible).as_deref(), Some("kai"), "the row's own header");
        assert_eq!(echo_author("never shown", visible), None);
        // A header-less row puts the message where the author would be: it is never a name.
        assert_eq!(echo_author("Sounds good", "Sounds good , 10:30 AM"), None);

        let mut conn = crate::open_in_memory().unwrap();
        note(&conn, "kai", KIND_ECHO, "echo:1", 1).unwrap();
        learn(&mut conn, &ada(), 2).unwrap();
        assert!(self_aliases(&conn).contains(&"kai".to_string()));
    }

    #[test]
    fn one_review_window_is_not_enough_and_a_dm_party_never_counts() {
        let mut conn = crate::open_in_memory().unwrap();
        note(&conn, "kai", KIND_REVIEW, "review:S1", 1).unwrap();
        assert!(!note(&conn, "kai", KIND_REVIEW, "review:S1", 2).unwrap(), "same window counts once");
        learn(&mut conn, &ada(), 3).unwrap();
        assert!(!is_self_name(&conn, "kai"));
        note(&conn, "kai", KIND_REVIEW, "review:S2", 4).unwrap();
        // Someone the user DMs is not the user, whatever a review said.
        note(&conn, "dana", KIND_REVIEW, "review:S1", 4).unwrap();
        note(&conn, "dana", KIND_REVIEW, "review:S2", 4).unwrap();
        note(&conn, "kai", KIND_DM, "dm:dana", 4).unwrap();
        learn(&mut conn, &ada(), 5).unwrap();
        assert!(is_self_name(&conn, "kai"));
        assert!(!is_self_name(&conn, "dana"));
    }

    #[test]
    fn a_learned_name_folds_its_person_row_into_self_unless_the_user_edited_it() {
        let mut conn = crate::open_in_memory().unwrap();
        let kai = person(&conn, "Kai", false);
        let e = event(&conn, 1, "@dana - Discord", "x");
        conn.execute("INSERT INTO state_provenance (state_table, state_id, event_id, weight) VALUES ('people', ?1, ?2, 1.0)", params![kai, e]).unwrap();
        let mo = person(&conn, "Mo", true);
        for (n, w) in [("kai", "echo:1"), ("mo", "echo:2")] {
            note(&conn, n, KIND_ECHO, w, 1).unwrap();
        }
        assert_eq!(learn(&mut conn, &ada(), 10).unwrap(), 1);
        let (id, class, page): (i64, String, Option<String>) = conn
            .query_row("SELECT id, person_class, page FROM people WHERE display_name = 'Ada Example'", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap();
        assert_eq!((class.as_str(), page), ("self", None), "the self row survives, no page");
        let linked: i64 = conn.query_row("SELECT COUNT(*) FROM state_provenance WHERE state_table = 'people' AND state_id = ?1", [id], |r| r.get(0)).unwrap();
        assert_eq!(linked, 1, "provenance moved to the self row");
        assert!(rows(&conn).iter().all(|(n, _)| n != "Kai"));
        assert!(rows(&conn).iter().any(|(n, _)| n == "Mo"), "a user-edited row is left alone");
        assert!(!is_self_name(&conn, "mo"));
        let _ = mo;
    }

    #[test]
    fn the_mailbox_addresses_join_the_self_row_unless_the_user_edited_it() {
        let mut conn = crate::open_in_memory().unwrap();
        crate::mail_triage::add_identity(&conn, "Ada@Example.com", 1).unwrap();
        learn(&mut conn, &ada(), 2).unwrap();
        let emails = |c: &Connection| -> Option<String> {
            c.query_row("SELECT emails FROM people WHERE person_class = 'self'", [], |r| r.get(0)).unwrap()
        };
        assert_eq!(emails(&conn).as_deref(), Some(r#"["ada@example.com"]"#));
        let own = own_details(&conn).unwrap();
        assert_eq!(own.name.as_deref(), Some("Ada Example"));
        assert_eq!(own.emails, vec!["ada@example.com".to_string()]);
        assert_eq!(own.aliases, vec!["adaex".to_string()]);

        // Additive: what the row already has stays first, a second address is appended once.
        conn.execute("UPDATE people SET emails = '[\"ada@work.test\"]' WHERE person_class = 'self'", []).unwrap();
        crate::mail_triage::add_identity(&conn, "ada@example.com", 3).unwrap();
        learn(&mut conn, &ada(), 4).unwrap();
        learn(&mut conn, &ada(), 5).unwrap();
        assert_eq!(emails(&conn).as_deref(), Some(r#"["ada@work.test","ada@example.com"]"#));

        // A row the user edited keeps exactly their emails; chat still sees the mailbox address.
        conn.execute("UPDATE people SET emails = '[]', user_pinned = 1 WHERE person_class = 'self'", []).unwrap();
        learn(&mut conn, &ada(), 6).unwrap();
        assert_eq!(emails(&conn).as_deref(), Some("[]"));
        assert_eq!(own_details(&conn).unwrap().emails, vec!["ada@example.com".to_string()]);
    }

    #[test]
    fn own_details_without_a_self_row_still_carry_the_mailbox_address() {
        let conn = crate::open_in_memory().unwrap();
        assert_eq!(own_details(&conn).unwrap(), OwnDetails::default());
        crate::mail_triage::add_identity(&conn, "ada@example.com", 1).unwrap();
        let own = own_details(&conn).unwrap();
        assert_eq!((own.name, own.emails), (None, vec!["ada@example.com".to_string()]));
    }

    #[test]
    fn pages_are_marked_for_a_rewrite_once() {
        let mut conn = crate::open_in_memory().unwrap();
        let dana = person(&conn, "Dana", false);
        let dirty = |c: &Connection| -> i64 { c.query_row("SELECT page_dirty FROM people WHERE id = ?1", [dana], |r| r.get(0)).unwrap() };
        learn(&mut conn, &[], 1).unwrap();
        assert_eq!(dirty(&conn), 0, "not before the owner is known");
        learn(&mut conn, &ada(), 2).unwrap();
        assert_eq!(dirty(&conn), 1);
        conn.execute("UPDATE people SET page_dirty = 0", []).unwrap();
        learn(&mut conn, &ada(), 3).unwrap();
        assert_eq!(dirty(&conn), 0, "only once");
    }
}
