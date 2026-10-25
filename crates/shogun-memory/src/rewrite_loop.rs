//! Rewrite learning loop (#1066): what each inline rewrite turned into (kept / edited / reverted /
//! sent), short style notes mined from that, and the prompt-side reads (length prior, few-shot,
//! active notes). Text lives only in the encrypted store and is redacted before every write; no
//! function here logs.
//!
//! This deliberately REVERSES the V35 rule that generated text is never a positive signal: owner
//! OK 2026-10-12 (#1066). A rewrite the user kept or sent is evidence of how they want to sound.

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

const MAX_TEXT_CHARS: usize = 4000;
const RETENTION_MS: i64 = 90 * 24 * 60 * 60 * 1000;
/// A later rewrite in the same field within this window supersedes an unresolved earlier one.
const SUPERSEDE_MS: i64 = 5 * 60 * 1000;
/// At or above this similarity to the output the text counts as unchanged.
pub const KEPT_MIN: f64 = 0.97;
/// Below [`KEPT_MIN`] but at or above this, the text is still derived from the output.
pub const EDITED_MIN: f64 = 0.45;
/// Only an unparseable model answer counts; a failed call is retried without spending one.
pub const MAX_MINE_ATTEMPTS: i64 = 10;

// pure: similarity, diff, classifier

fn words(s: &str) -> Vec<String> {
    s.split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase())
        .filter(|w| !w.is_empty())
        .collect()
}

/// Word-level similarity in 0..=1 (1 - edit distance / longer length), ignoring case, punctuation
/// and whitespace. Long inputs are compared on their first 600 words.
pub fn similarity(a: &str, b: &str) -> f64 {
    let (wa, wb) = (words(a), words(b));
    let (wa, wb) = (&wa[..wa.len().min(600)], &wb[..wb.len().min(600)]);
    if wa.is_empty() && wb.is_empty() {
        return 1.0;
    }
    if wa.is_empty() || wb.is_empty() {
        return 0.0;
    }
    let mut prev: Vec<usize> = (0..=wb.len()).collect();
    for (i, x) in wa.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, y) in wb.iter().enumerate() {
            let cost = usize::from(x != y);
            cur.push((prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    1.0 - prev[wb.len()] as f64 / wa.len().max(wb.len()) as f64
}

fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// The changed middle between two texts as `- old | + new` (common word prefix/suffix removed).
pub fn compact_diff(from: &str, to: &str) -> String {
    let (a, b) = (from.split_whitespace().collect::<Vec<_>>(), to.split_whitespace().collect::<Vec<_>>());
    let pre = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let suf = a[pre..]
        .iter()
        .rev()
        .zip(b[pre..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (old, new) = (&a[pre..a.len() - suf], &b[pre..b.len() - suf]);
    format!("- {} | + {}", clip(&old.join(" "), 160), clip(&new.join(" "), 160))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Kept,
    Edited,
    Reverted,
    Unseen,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Kept => "kept",
            Verdict::Edited => "edited",
            Verdict::Reverted => "reverted",
            Verdict::Unseen => "unseen",
        }
    }
}

/// Field text without zero-width / hidden format characters (Discord's empty composer reads as
/// U+FEFF). Same set as the inline empty-field check (`shogun_redact::is_hidden`).
pub fn clean(s: &str) -> String {
    shogun_redact::strip_hidden(s).text.into_owned()
}

/// How many times `text` appears in the conversation text (exact after normalising, else lines
/// that are >= 0.9 similar). A send is a NEW occurrence after the field emptied, not the field
/// emptying (select-all + delete empties it too) and not an identical earlier message.
pub fn echo_count(text: &str, visible: &str) -> usize {
    let norm = |s: &str| clean(s).split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    let t = norm(text);
    if t.is_empty() {
        return 0;
    }
    // Both measures every time: a baseline taken in exact mode (the field's own copy matches) and
    // a post-delete count that fell back to near-duplicate lines would read as a new echo.
    let exact = norm(visible).matches(&t).count();
    let near = visible.lines().filter(|l| similarity(&norm(l), &t) >= 0.9).count();
    exact.max(near)
}

/// What the field holds now, relative to the rewrite. `None` verdict = unrelated text, no change.
pub fn classify(input: &str, output: &str, observed: &str) -> Option<(Verdict, f64)> {
    let sim_out = similarity(observed, output);
    if sim_out >= KEPT_MIN {
        return Some((Verdict::Kept, sim_out));
    }
    let sim_in = similarity(observed, input);
    if !input.trim().is_empty() && sim_in >= KEPT_MIN && sim_in > sim_out {
        return Some((Verdict::Reverted, sim_in));
    }
    if sim_out >= EDITED_MIN {
        return Some((Verdict::Edited, sim_out));
    }
    None
}

/// One step of the post-insert watch. Fed field reads by the device layer; pure so the send and
/// revert rules are testable.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// The field now reads this (verdict from [`classify`]).
    Observed { verdict: Verdict, score: f64, text: String },
    /// The field emptied right after it held `text`: sent or deleted. The caller waits for the
    /// echo in the conversation ([`echo_count`]) and decides with [`ending`].
    Emptied { text: String },
}

/// How an emptied field ended, judged after waiting for the echo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    /// The text newly appeared in the conversation.
    Sent,
    /// The field is still there and empty, the conversation was readable, nothing appeared.
    Deleted,
    /// Could not tell (a compose that closes on send, an app switch, a vanished field or
    /// window): keep the last verdict and store the text as observed, never as sent.
    Unknown,
}

/// `sent_key`: Return / Cmd-Return went to the app just before the field emptied or its compose
/// closed (Goldfish's send commit); select-all + delete never has one. `readable`: the
/// conversation could be read, with the app still in front, at the end of the wait. `baselined`:
/// the count before was taken while the field still held the text; without it any copy in the
/// window counts as the echo (Goldfish's draft echo), and only a window with no copy at all
/// proves a delete. `field_now`: the field's text at the end, `None` = gone.
pub fn ending(sent_key: bool, echoed: bool, readable: bool, baselined: bool, remaining: usize, field_now: Option<&str>) -> Ending {
    if sent_key || echoed || (!baselined && remaining > 0) {
        return Ending::Sent;
    }
    let still_empty = field_now.is_some_and(|t| clean(t).trim().is_empty());
    if readable && still_empty && (baselined || remaining == 0) {
        Ending::Deleted
    } else {
        Ending::Unknown
    }
}

#[derive(Debug, Default)]
pub struct Tracker {
    last_text: Option<String>,
    last_verdict: Option<Verdict>,
}

impl Tracker {
    /// Seeded with the inserted output, so a field that empties before the first read (Enter
    /// right after the rewrite) is a send of that output verbatim.
    pub fn seeded(output: &str) -> Self {
        Self { last_text: Some(output.to_string()), last_verdict: None }
    }

    /// `observed`: the field's current text, `None` when the read failed (no event).
    pub fn step(&mut self, input: &str, output: &str, observed: Option<&str>) -> Option<Event> {
        let observed = clean(observed?);
        let observed = observed.as_str();
        if observed.trim().is_empty() {
            let text = self.last_text.take()?;
            // Emptied while the output was in place: a send, whatever it had become by then.
            return Some(Event::Emptied { text });
        }
        let changed = self.last_text.as_deref() != Some(observed);
        // Remember what the field says even when it drifted too far to classify, so a later
        // empty read reports the real sent text.
        self.last_text = Some(observed.to_string());
        let (verdict, score) = classify(input, output, observed)?;
        if changed || self.last_verdict != Some(verdict) {
            self.last_verdict = Some(verdict);
            return Some(Event::Observed { verdict, score, text: observed.to_string() });
        }
        None
    }
}

fn store(text: &str) -> String {
    crate::redact::redact(&clip(text.trim(), MAX_TEXT_CHARS)).into_owned()
}

#[derive(Debug, Clone)]
pub struct NewOutcome<'a> {
    pub app: &'a str,
    pub partner: Option<&'a str>,
    /// The user's typed instruction ("make it shorter") when the field held one, else "".
    pub instruction: &'a str,
    pub input: &'a str,
    pub output: &'a str,
    /// Notes that were in the prompt for this output.
    pub note_ids: &'a [i64],
}

/// Record a delivered rewrite. `None` for an empty app/output.
pub fn record_outcome(conn: &mut Connection, o: &NewOutcome<'_>, now: i64) -> rusqlite::Result<Option<i64>> {
    if o.app.is_empty() || o.app.len() > 256 || o.output.trim().is_empty() {
        return Ok(None);
    }
    let tx = conn.transaction()?;
    tx.execute(
        "UPDATE rewrite_outcomes SET superseded_at = ?3
          WHERE app = ?1 AND partner IS ?2 AND superseded_at IS NULL AND verdict = 'unseen' AND created_at >= ?4",
        params![o.app, o.partner, now, now - SUPERSEDE_MS],
    )?;
    tx.execute(
        "INSERT INTO rewrite_outcomes(app, partner, instruction, input_digest, input_len, input_text, output_text, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            o.app,
            o.partner,
            store(o.instruction),
            crate::event_log::content_hash(o.input.trim()),
            o.input.trim().chars().count() as i64,
            store(o.input),
            store(o.output),
            now
        ],
    )?;
    let id = tx.last_insert_rowid();
    for note in o.note_ids {
        tx.execute(
            "INSERT OR IGNORE INTO rewrite_note_impressions(outcome_id, note_id) SELECT ?1, id FROM rewrite_style_notes WHERE id = ?2",
            params![id, note],
        )?;
        tx.execute("UPDATE rewrite_style_notes SET impressions = impressions + 1 WHERE id = ?1", [note])?;
    }
    tx.execute("DELETE FROM rewrite_outcomes WHERE created_at < ?1", [now - RETENTION_MS])?;
    tx.commit()?;
    Ok(Some(id))
}

fn resolve_impressions(conn: &Connection, id: i64, verdict: Verdict) -> rusqlite::Result<()> {
    let col = match verdict {
        Verdict::Kept => "confirmations",
        Verdict::Edited => "rejections",
        _ => return Ok(()),
    };
    conn.execute(
        &format!(
            "UPDATE rewrite_style_notes SET {col} = {col} + 1
              WHERE id IN (SELECT note_id FROM rewrite_note_impressions WHERE outcome_id = ?1 AND resolved = 0)"
        ),
        [id],
    )?;
    conn.execute("UPDATE rewrite_note_impressions SET resolved = 1 WHERE outcome_id = ?1", [id])?;
    Ok(())
}

/// Store what the watch saw. A later `kept` upgrades `reverted` (the user undid, then sent it).
pub fn apply_observation(conn: &Connection, id: i64, verdict: Verdict, score: f64, text: &str, now: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE rewrite_outcomes SET verdict = ?2, score = ?3, checked_at = ?4, observed_text = ?5 WHERE id = ?1",
        params![id, verdict.as_str(), score, now, store(text)],
    )?;
    resolve_impressions(conn, id, verdict)
}

/// Store the text that was sent. The verdict follows what was sent, not an earlier revert.
pub fn apply_sent(conn: &Connection, id: i64, sent: &str, now: i64) -> rusqlite::Result<()> {
    let Some((output, input)) = conn
        .query_row("SELECT output_text, input_text FROM rewrite_outcomes WHERE id = ?1", [id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .optional()?
    else {
        return Ok(());
    };
    let sim = similarity(sent, &output);
    let diff = if sim >= KEPT_MIN { String::new() } else { compact_diff(&output, sent) };
    conn.execute(
        "UPDATE rewrite_outcomes SET sent_text = ?2, sent_at = ?3, sent_similarity = ?4, sent_diff = ?5 WHERE id = ?1",
        params![id, store(sent), now, sim, store(&diff)],
    )?;
    // What was sent is the final word. Text that is neither the output nor the original is still
    // the user's revision of it (the watch only reports a send for a field derived from the output).
    let (verdict, score) = classify(&input, &output, sent).unwrap_or((Verdict::Edited, sim));
    if verdict != Verdict::Reverted {
        conn.execute(
            "UPDATE rewrite_outcomes SET verdict = ?2, score = ?3 WHERE id = ?1",
            params![id, verdict.as_str(), score],
        )?;
        resolve_impressions(conn, id, verdict)?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LengthPrior {
    pub samples: usize,
    pub median_words: usize,
    pub p75_words: usize,
    /// "partner" or "surface".
    pub scope: &'static str,
}

fn final_text_of(sent: Option<String>, observed: Option<String>, verdict: &str, output: String) -> Option<String> {
    sent.or(observed).or_else(|| (verdict == "kept").then_some(output)).filter(|t| !t.trim().is_empty())
}

/// What the user actually ended up sending/keeping, newest first (their own length, not the model's).
fn recent_finals(conn: &Connection, app: &str, partner: Option<&str>, limit: i64) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT sent_text, observed_text, verdict, output_text FROM rewrite_outcomes
          WHERE app = ?1 AND partner IS ?2 AND verdict IN ('kept','edited') ORDER BY created_at DESC, id DESC LIMIT ?3",
    )?;
    let rows = stmt
        .query_map(params![app, partner, limit], |r| {
            Ok(final_text_of(r.get(0)?, r.get(1)?, &r.get::<_, String>(2)?, r.get(3)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows.into_iter().flatten().collect())
}

/// Per-partner signals (length, notes, examples, history) only outrank the surface once the user
/// has this many kept/edited finals to that partner. Below it the surface's voice wins: thin
/// history is noisy and some old outcomes were mis-recorded as kept.
pub const PARTNER_MIN_FINALS: i64 = 5;

fn partner_established(conn: &Connection, app: &str, partner: Option<&str>) -> rusqlite::Result<bool> {
    let Some(p) = partner else { return Ok(false) };
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM rewrite_outcomes
          WHERE app = ?1 AND partner = ?2 AND superseded_at IS NULL AND verdict IN ('kept','edited')",
        params![app, p],
        |r| r.get(0),
    )?;
    Ok(n >= PARTNER_MIN_FINALS)
}

pub fn length_prior(conn: &Connection, app: &str, partner: Option<&str>) -> rusqlite::Result<Option<LengthPrior>> {
    for (p, scope) in [(partner, "partner"), (None, "surface")] {
        if p.is_none() && scope == "partner" {
            continue;
        }
        let finals = if scope == "surface" {
            // Whole surface, every partner.
            let mut stmt = conn.prepare(
                "SELECT sent_text, observed_text, verdict, output_text FROM rewrite_outcomes
                  WHERE app = ?1 AND verdict IN ('kept','edited') ORDER BY created_at DESC, id DESC LIMIT 30",
            )?;
            let rows = stmt
                .query_map([app], |r| Ok(final_text_of(r.get(0)?, r.get(1)?, &r.get::<_, String>(2)?, r.get(3)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows.into_iter().flatten().collect()
        } else {
            recent_finals(conn, app, p, 30)?
        };
        let mut lens: Vec<usize> = finals.iter().map(|t| t.split_whitespace().count()).collect();
        let min = if scope == "partner" { PARTNER_MIN_FINALS as usize } else { 3 };
        if lens.len() >= min {
            lens.sort_unstable();
            return Ok(Some(LengthPrior {
                samples: lens.len(),
                median_words: lens[lens.len() / 2],
                p75_words: lens[(lens.len() * 3 / 4).min(lens.len() - 1)],
                scope,
            }));
        }
    }
    Ok(None)
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PartnerFacts {
    pub rewrites: i64,
    pub kept: i64,
    pub edited: i64,
}

/// How the user's past rewrites to this partner went. `None` until the history is established.
pub fn partner_facts(conn: &Connection, app: &str, partner: &str) -> rusqlite::Result<Option<PartnerFacts>> {
    let f = conn.query_row(
        "SELECT count(*), coalesce(sum(verdict='kept'),0), coalesce(sum(verdict='edited'),0)
           FROM rewrite_outcomes WHERE app = ?1 AND partner = ?2 AND superseded_at IS NULL",
        params![app, partner],
        |r| Ok(PartnerFacts { rewrites: r.get(0)?, kept: r.get(1)?, edited: r.get(2)? }),
    )?;
    Ok((f.kept + f.edited >= PARTNER_MIN_FINALS).then_some(f))
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PersonFacts {
    pub name: String,
    pub relationship: String,
    pub confidence: f64,
}

/// An optional extra: the `people` row behind a partner key (an `email:` address, or a `thread:`
/// title that equals a known display name). Memory is often empty here, so callers never depend on it.
pub fn person_facts(conn: &Connection, partner: &str) -> rusqlite::Result<Option<PersonFacts>> {
    let row = |r: &rusqlite::Row<'_>| {
        Ok(PersonFacts { name: r.get(0)?, relationship: r.get::<_, Option<String>>(1)?.unwrap_or_default(), confidence: r.get(2)? })
    };
    if let Some(addr) = partner.strip_prefix("email:") {
        let like = format!("%\"{}\"%", addr.replace(['%', '_', '"'], ""));
        return conn
            .query_row(
                "SELECT display_name, relationship_summary, confidence FROM people WHERE lower(emails) LIKE ?1 ORDER BY confidence DESC LIMIT 1",
                [like],
                row,
            )
            .optional();
    }
    if let Some(title) = partner.strip_prefix("thread:") {
        return conn
            .query_row(
                "SELECT display_name, relationship_summary, confidence FROM people WHERE lower(display_name) = ?1 ORDER BY confidence DESC LIMIT 1",
                [title.to_lowercase()],
                row,
            )
            .optional();
    }
    Ok(None)
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Shot {
    pub input: String,
    pub final_text: String,
    /// `revised` (the user's own revision of an edited rewrite), `kept`, or `rejected`.
    pub kind: &'static str,
}

/// Up to `limit` examples for this surface/partner: the user's revisions first, then kept outputs;
/// `rejected` ones (reverted rewrites) are returned as negatives, at most one.
pub fn few_shot(conn: &Connection, app: &str, partner: Option<&str>, limit: usize) -> rusqlite::Result<Vec<Shot>> {
    let mut stmt = conn.prepare(
        "SELECT input_text, output_text, verdict, sent_text, observed_text, partner IS ?2, sent_similarity FROM rewrite_outcomes
          WHERE app = ?1 AND verdict IN ('kept','edited','reverted') AND input_len > 0 AND superseded_at IS NULL
            AND length(input_text) BETWEEN 8 AND 600 AND length(output_text) <= 800
          ORDER BY (?3 AND partner IS ?2) DESC, created_at DESC, id DESC LIMIT 60",
    )?;
    let boost = partner_established(conn, app, partner)?;
    let rows = stmt
        .query_map(params![app, partner, boost], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let (mut revised, mut kept, mut neg) = (vec![], vec![], vec![]);
    for (input, output, verdict, sent, observed) in rows {
        let shot = |final_text: String, kind| Shot { input: input.clone(), final_text, kind };
        match verdict.as_str() {
            "edited" => {
                if let Some(t) = sent.or(observed).filter(|t| t.len() <= 800) {
                    revised.push(shot(t, "revised"));
                }
            }
            // What the user actually sent is their own voice; the model output is the fallback.
            "kept" => kept.push(shot(sent.filter(|t| t.len() <= 800).unwrap_or(output), "kept")),
            _ => neg.push(shot(output, "rejected")),
        }
    }
    let mut out: Vec<Shot> = revised.into_iter().chain(kept).take(limit.saturating_sub(1).max(1)).collect();
    if let Some(n) = neg.into_iter().next() {
        out.push(n);
    }
    out.truncate(limit);
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Note {
    pub id: i64,
    pub scope: String,
    pub app: String,
    pub partner: String,
    pub kind: String,
    pub text: String,
    pub impressions: i64,
    pub confirmations: i64,
    pub rejections: i64,
    pub created_at: i64,
}

fn note_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Note> {
    Ok(Note {
        id: r.get(0)?,
        scope: r.get(1)?,
        app: r.get(2)?,
        partner: r.get(3)?,
        kind: r.get(4)?,
        text: r.get(5)?,
        impressions: r.get(6)?,
        confirmations: r.get(7)?,
        rejections: r.get(8)?,
        created_at: r.get(9)?,
    })
}

const NOTE_COLS: &str = "id, scope, app, partner, kind, text, impressions, confirmations, rejections, created_at";

/// Every note the user can see in Settings (active ones, newest first).
pub fn list_notes(conn: &Connection) -> rusqlite::Result<Vec<Note>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {NOTE_COLS} FROM rewrite_style_notes WHERE user_deleted = 0 AND invalidated_at IS NULL ORDER BY created_at DESC, id DESC LIMIT 200"
    ))?;
    let rows = stmt.query_map([], note_row)?.collect();
    rows
}

/// Notes for a rewrite: stated before learned, partner-scoped (once established) before surface before global, then
/// by net confirmations. At most `limit`. A note is never deleted for being rejected (Goldfish keeps
/// a note at 12 confirmations / 2 rejections); one that is clearly rejected (3+ rejections and more
/// than twice its confirmations) just sits out of the prompt until it is confirmed again.
pub fn active_notes(conn: &Connection, app: &str, partner: Option<&str>, limit: usize) -> rusqlite::Result<Vec<Note>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {NOTE_COLS} FROM rewrite_style_notes
          WHERE user_deleted = 0 AND invalidated_at IS NULL AND rejections < 2 * confirmations + 3
            AND ((scope = 'global') OR (scope = 'surface' AND app = ?1) OR (scope = 'partner' AND app = ?1 AND partner = ?2))
          ORDER BY (kind = 'stated') DESC, (?4 AND scope = 'partner') DESC, (scope = 'surface') DESC,
                   (confirmations - rejections) DESC, id DESC LIMIT ?3"
    ))?;
    let boost = partner_established(conn, app, partner)?;
    let rows = stmt.query_map(params![app, partner.unwrap_or("\u{0}"), limit as i64, boost], note_row)?.collect();
    rows
}

/// Add a note unless an identical live (or user-deleted) one exists. Returns the new id.
#[allow(clippy::too_many_arguments)]
pub fn add_note(
    conn: &Connection,
    scope: &str,
    app: &str,
    partner: &str,
    kind: &str,
    text: &str,
    origin: &[i64],
    now: i64,
) -> rusqlite::Result<Option<i64>> {
    let text = store(text);
    if text.is_empty() || text.chars().count() > 240 || !matches!(scope, "surface" | "partner" | "global") {
        return Ok(None);
    }
    let dup: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM rewrite_style_notes WHERE lower(text) = lower(?1) AND scope = ?2 AND app = ?3 AND partner = ?4
                       AND (invalidated_at IS NULL OR user_deleted = 1))",
        params![text, scope, app, partner],
        |r| r.get(0),
    )?;
    if dup {
        return Ok(None);
    }
    conn.execute(
        "INSERT INTO rewrite_style_notes(scope, app, partner, kind, text, origin_outcomes, created_at) VALUES (?1,?2,?3,?4,?5,?6,?7)",
        params![scope, app, partner, kind, text, serde_json::to_string(origin).unwrap_or_default(), now],
    )?;
    Ok(Some(conn.last_insert_rowid()))
}

/// The user's delete: the row stays (hidden) so mining does not relearn the same note.
pub fn delete_note(conn: &Connection, id: i64) -> rusqlite::Result<bool> {
    Ok(conn.execute("UPDATE rewrite_style_notes SET user_deleted = 1 WHERE id = ?1 AND user_deleted = 0", [id])? > 0)
}

/// The user's confirm / reject of a note; counts like a kept / edited outcome would. Delete is
/// the user's way to drop a note; rejections only rank it down (see [`active_notes`]).
pub fn rate_note(conn: &Connection, id: i64, confirm: bool) -> rusqlite::Result<bool> {
    let col = if confirm { "confirmations" } else { "rejections" };
    let n = conn.execute(&format!("UPDATE rewrite_style_notes SET {col} = {col} + 1 WHERE id = ?1 AND user_deleted = 0 AND invalidated_at IS NULL"), [id])?;
    Ok(n > 0)
}

/// Mining found a note that is already on file: one more confirmation (Goldfish's corroboration).
// ponytail: reuses `confirmations` (V78 is frozen); a separate corroborations column only if the UI must tell them apart.
pub fn corroborate_note(conn: &Connection, scope: &str, app: &str, partner: &str, text: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE rewrite_style_notes SET confirmations = confirmations + 1
          WHERE lower(text) = lower(?1) AND scope = ?2 AND app = ?3 AND partner = ?4 AND invalidated_at IS NULL AND user_deleted = 0",
        params![store(text), scope, app, partner],
    )?;
    Ok(())
}

/// Mining marked an older note as replaced.
pub fn supersede_note(conn: &Connection, id: i64, now: i64) -> rusqlite::Result<()> {
    conn.execute("UPDATE rewrite_style_notes SET invalidated_at = ?2 WHERE id = ?1 AND invalidated_at IS NULL AND user_deleted = 0", params![id, now])?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MiningRow {
    pub id: i64,
    pub app: String,
    pub partner: Option<String>,
    pub instruction: String,
    pub ai_output: String,
    pub user_final: String,
    /// Kept exactly as written (whitespace aside): evidence of what the user accepts, not an edit.
    pub accepted: bool,
}

/// Outcomes nobody mined yet: edited ones, sends that differ from the output, and kept ones —
/// with a punctuation / greeting / sign-off change (word similarity ignores those) or verbatim
/// (`accepted`, what the user takes as written; Goldfish learns from kept outputs too). Reverted
/// and unseen outcomes are never mined.
pub fn unmined(conn: &Connection, cap: usize) -> rusqlite::Result<Vec<MiningRow>> {
    // Whitespace never counts as a change: web editors read line breaks back differently.
    let squash = |x: &str| format!("replace(replace(replace(replace({x}, char(10), ''), char(13), ''), char(9), ''), ' ', '')");
    let fin = "coalesce(nullif(trim(coalesce(sent_text, observed_text)), ''), output_text)";
    let mut stmt = conn.prepare(&format!(
        "SELECT id, app, partner, instruction, output_text, {fin}, verdict = 'kept' AND {} = {} FROM rewrite_outcomes
          WHERE mined_at IS NULL AND mine_attempts < ?1 AND superseded_at IS NULL
            AND ((verdict = 'edited' AND nullif(trim(coalesce(sent_text, observed_text)), '') IS NOT NULL)
                 OR (sent_similarity IS NOT NULL AND sent_similarity < ?4)
                 OR (verdict = 'kept' AND coalesce(sent_similarity, score, 1.0) >= ?2))
          ORDER BY created_at DESC LIMIT ?3",
        squash(fin),
        squash("output_text"),
    ))?;
    let rows = stmt
        .query_map(params![MAX_MINE_ATTEMPTS, EDITED_MIN, cap as i64, KEPT_MIN], |r| {
            Ok(MiningRow {
                id: r.get(0)?,
                app: r.get(1)?,
                partner: r.get(2)?,
                instruction: r.get(3)?,
                ai_output: r.get(4)?,
                user_final: r.get(5)?,
                accepted: r.get(6)?,
            })
        })?
        .collect();
    rows
}

/// Outcomes not mined yet, of any verdict (for the per-pass log line).
pub fn unmined_backlog(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("SELECT count(*) FROM rewrite_outcomes WHERE mined_at IS NULL AND superseded_at IS NULL", [], |r| r.get(0))
}

pub fn mark_mined(conn: &Connection, ids: &[i64], now: i64) -> rusqlite::Result<()> {
    for id in ids {
        conn.execute("UPDATE rewrite_outcomes SET mined_at = ?2 WHERE id = ?1", params![id, now])?;
    }
    Ok(())
}

pub fn bump_mine_attempts(conn: &Connection, ids: &[i64]) -> rusqlite::Result<()> {
    for id in ids {
        conn.execute("UPDATE rewrite_outcomes SET mine_attempts = mine_attempts + 1 WHERE id = ?1", [id])?;
    }
    Ok(())
}

fn normalize_instruction(s: &str) -> String {
    s.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ").trim_matches(|c: char| !c.is_alphanumeric()).to_string()
}

/// Words that make a typed instruction a standing rule rather than a one-off ("from now on sign
/// off with Best", "never use emoji"), the cue Goldfish keys its stated rules on.
/// Only time-anchored cues: Goldfish's looser ones ("remember to", "stop") go through a model
/// classifier there, and without one "remember to mention the deadline" would become a rule.
const STANDING_CUES: &[&str] = &["always", "never", "from now on", "next time", "going forward", "in the future", "every time"];

fn is_standing(norm: &str) -> bool {
    let words: Vec<&str> = norm.split_whitespace().map(|w| w.trim_matches(|c: char| !c.is_alphanumeric())).collect();
    let padded = format!(" {} ", words.join(" "));
    STANDING_CUES.iter().any(|cue| padded.contains(&format!(" {cue} ")))
}

/// Turn the user's own typed instructions into `stated` surface notes: at once when it reads as a
/// standing rule ([`STANDING_CUES`], the note is the instruction itself), else once typed 3+ times
/// or on 2+ days (last 30 days). A rewrite re-run within minutes (superseded) is a retry, not a
/// repeat. Returns the number of notes created. Needs no model call.
pub fn promote_repeated_instructions(conn: &Connection, now: i64) -> rusqlite::Result<usize> {
    const DAY_MS: i64 = 24 * 60 * 60 * 1000;
    let mut stmt = conn.prepare(
        "SELECT app, instruction, created_at FROM rewrite_outcomes
          WHERE instruction != '' AND length(instruction) <= 120 AND created_at >= ?1 AND superseded_at IS NULL",
    )?;
    let rows = stmt
        .query_map([now - 30 * DAY_MS], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    type Seen = (usize, std::collections::BTreeSet<i64>, String);
    let mut counts: std::collections::BTreeMap<(String, String), Seen> = Default::default();
    for (app, ins, at) in rows {
        let e = counts.entry((app, normalize_instruction(&ins))).or_insert((0, Default::default(), ins.clone()));
        e.0 += 1;
        e.1.insert(at.div_euclid(DAY_MS));
    }
    let mut made = 0;
    for ((app, norm), (n, days, original)) in counts {
        let text = if is_standing(&norm) {
            original.trim().to_string()
        } else if n >= 3 || days.len() >= 2 {
            format!("The user has asked for this repeatedly here: {}", original.trim())
        } else {
            continue;
        };
        if !norm.is_empty() && add_note(conn, "surface", &app, "", "stated", &text, &[], now)?.is_some() {
            made += 1;
        }
    }
    Ok(made)
}

/// Every `thread:` partner key in use, for re-keying after the title normalisation changed.
pub fn thread_partner_keys(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT partner FROM rewrite_outcomes WHERE partner LIKE 'thread:%'
         UNION SELECT partner FROM rewrite_style_notes WHERE partner LIKE 'thread:%'",
    )?;
    let rows = stmt.query_map([], |r| r.get(0))?.collect();
    rows
}

/// Move outcomes and notes from an old partner key to its current form.
pub fn rename_partner(conn: &Connection, old: &str, new: &str) -> rusqlite::Result<()> {
    conn.execute("UPDATE rewrite_outcomes SET partner = ?2 WHERE partner = ?1", params![old, new])?;
    conn.execute("UPDATE rewrite_style_notes SET partner = ?2 WHERE partner = ?1", params![old, new])?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Summary {
    pub total: i64,
    pub kept: i64,
    pub edited: i64,
    pub reverted: i64,
    pub unseen: i64,
    pub sent: i64,
    pub notes: i64,
}

pub fn summary(conn: &Connection) -> rusqlite::Result<Summary> {
    conn.query_row(
        "SELECT count(*), coalesce(sum(verdict='kept'),0), coalesce(sum(verdict='edited'),0), coalesce(sum(verdict='reverted'),0),
                coalesce(sum(verdict='unseen'),0), coalesce(sum(sent_text IS NOT NULL),0),
                (SELECT count(*) FROM rewrite_style_notes WHERE user_deleted = 0 AND invalidated_at IS NULL)
           FROM rewrite_outcomes WHERE superseded_at IS NULL",
        [],
        |r| {
            Ok(Summary {
                total: r.get(0)?,
                kept: r.get(1)?,
                edited: r.get(2)?,
                reverted: r.get(3)?,
                unseen: r.get(4)?,
                sent: r.get(5)?,
                notes: r.get(6)?,
            })
        },
    )
}

/// "Clear learned writing" also clears this loop.
pub fn clear_all(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM rewrite_note_impressions", [])?;
    conn.execute("DELETE FROM rewrite_style_notes", [])?;
    conn.execute("DELETE FROM rewrite_outcomes", [])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out<'a>(app: &'a str, partner: Option<&'a str>, input: &'a str, output: &'a str, notes: &'a [i64]) -> NewOutcome<'a> {
        NewOutcome { app, partner, instruction: "", input, output, note_ids: notes }
    }

    #[test]
    fn classifier_thresholds() {
        let (i, o) = ("hey can u send the file", "Hey, could you send me the file?");
        assert_eq!(classify(i, o, "Hey could you send me the file").unwrap().0, Verdict::Kept);
        assert_eq!(classify(i, o, "hey can u send the file").unwrap().0, Verdict::Reverted);
        assert_eq!(classify(i, o, "Hey, could you send me the file today please?").unwrap().0, Verdict::Edited);
        assert!(classify(i, o, "completely unrelated words here now").is_none());
        assert!(similarity("a b c", "a b c") > 0.99 && similarity("", "x") == 0.0);
    }

    #[test]
    fn tracker_detects_edit_then_send_and_ignores_cleared_without_text() {
        let (i, o) = ("hey can u send the file", "Hey, could you send me the file?");
        let mut t = Tracker::default();
        assert!(matches!(t.step(i, o, Some(o)), Some(Event::Observed { verdict: Verdict::Kept, .. })));
        assert_eq!(t.step(i, o, Some(o)), None);
        assert!(matches!(t.step(i, o, Some("Hey, could you send me the file asap?")), Some(Event::Observed { verdict: Verdict::Edited, .. })));
        assert_eq!(t.step(i, o, None), None);
        assert_eq!(t.step(i, o, Some("")), Some(Event::Emptied { text: "Hey, could you send me the file asap?".into() }));
        assert_eq!(Tracker::default().step(i, o, Some("")), None);
    }

    #[test]
    fn bom_only_field_is_empty_and_echo_confirms_a_send() {
        let (i, o) = ("hey can u send the file", "Hey, could you send me the file?");
        assert_eq!(clean("\u{FEFF}\u{200B}"), "");
        assert_eq!(Tracker::seeded(o).step(i, o, Some("\u{FEFF}")), Some(Event::Emptied { text: o.into() }));
        // sent: the text appears in the thread; deleted: it does not.
        assert_eq!(echo_count(o, "Anna\nok\nhey, could you send me the file?\n"), 1);
        assert_eq!(echo_count(o, "Anna\nok\nsee you at 5"), 0);
        // same text already earlier in the thread + delete: count never rises -> reverted.
        let before = "me\nHey, could you send me the file?\nAnna\nno";
        assert_eq!(echo_count(o, before), 1);
        // a real send adds a second occurrence.
        let after = "me\nHey, could you send me the file?\nAnna\nno\nHey, could you send me the file?";
        assert!(echo_count(o, after) > echo_count(o, before));
        // a near-duplicate earlier line (not an exact match): deleting the field must not look
        // like a new echo. Baseline = count with the field's copy, minus that copy.
        let thread = "Anna\nhey could you send me the file\n";
        let with_field = format!("{thread}{o}");
        assert!(echo_count(o, thread) < echo_count(o, &with_field));
    }

    #[test]
    fn only_a_present_empty_field_without_echo_or_return_is_a_delete() {
        assert_eq!(ending(false, true, false, false, 9, None), Ending::Sent);
        assert_eq!(ending(false, false, true, true, 1, Some("\u{FEFF}")), Ending::Deleted);
        // Return just before the field emptied: sent, echo or not, field there or compose closed
        assert_eq!(ending(true, false, true, true, 1, Some("")), Ending::Sent);
        assert_eq!(ending(true, false, false, false, 0, None), Ending::Sent);
        // compose window closed without Return (button click) / app switched away
        assert_eq!(ending(false, false, true, true, 0, None), Ending::Unknown);
        assert_eq!(ending(false, false, false, true, 0, Some("")), Ending::Unknown);
        // field refilled (undo, new text)
        assert_eq!(ending(false, false, true, true, 0, Some("hi")), Ending::Unknown);
        // no baseline: any copy in the window is the echo
        assert_eq!(ending(false, false, true, false, 1, Some("")), Ending::Sent);
        assert_eq!(ending(false, false, true, false, 0, Some("")), Ending::Deleted);
    }

    #[test]
    fn emptied_before_first_poll_is_sent_verbatim_and_drift_keeps_last_text() {
        let (i, o) = ("hey can u send the file", "Hey, could you send me the file?");
        assert_eq!(Tracker::seeded(o).step(i, o, Some("")), Some(Event::Emptied { text: o.into() }));
        let mut t = Tracker::seeded(o);
        assert_eq!(t.step(i, o, Some("totally different words now")), None);
        assert_eq!(t.step(i, o, Some("")), Some(Event::Emptied { text: "totally different words now".into() }));
    }

    #[test]
    fn outcome_lifecycle_notes_and_sent_upgrade() {
        let mut conn = crate::open_in_memory().unwrap();
        let note = add_note(&conn, "surface", "app.chat", "", "learned", "Keep it casual.", &[], 1).unwrap().unwrap();
        let id = record_outcome(&mut conn, &out("app.chat", Some("thread:a"), "hey can u send the file", "Hey, could you send me the file?", &[note]), 10).unwrap().unwrap();
        // reverted first, then sent as the output: the send upgrades it.
        apply_observation(&conn, id, Verdict::Reverted, 0.99, "hey can u send the file", 11).unwrap();
        apply_sent(&conn, id, "Hey, could you send me the file?", 12).unwrap();
        let v: String = conn.query_row("SELECT verdict FROM rewrite_outcomes WHERE id=?1", [id], |r| r.get(0)).unwrap();
        assert_eq!(v, "kept");
        let n = list_notes(&conn).unwrap();
        assert_eq!((n[0].impressions, n[0].confirmations, n[0].rejections), (1, 1, 0));
        // an edited send is a rejection and is mined.
        let id2 = record_outcome(&mut conn, &out("app.chat", Some("thread:a"), "ok thanks see u", "Okay, thank you. See you then.", &[note]), 20).unwrap().unwrap();
        apply_sent(&conn, id2, "ok thx see u!", 21).unwrap();
        let changed = |conn: &Connection| unmined(conn, 20).unwrap().into_iter().filter(|r| !r.accepted).count();
        assert_eq!(changed(&conn), 1);
        mark_mined(&conn, &[id2], 30).unwrap();
        assert_eq!(changed(&conn), 0);
        // 1 confirmation, 1 rejection (the edited send): rejections alone never delete a note;
        // a clearly rejected one (5 vs 1) leaves the prompt but stays listed, and comes back
        // once confirmed again.
        for _ in 0..3 {
            rate_note(&conn, note, false).unwrap();
        }
        assert_eq!(active_notes(&conn, "app.chat", None, 6).unwrap().len(), 1);
        rate_note(&conn, note, false).unwrap();
        assert!(active_notes(&conn, "app.chat", None, 6).unwrap().is_empty());
        assert_eq!(list_notes(&conn).unwrap().len(), 1);
        rate_note(&conn, note, true).unwrap();
        assert_eq!(active_notes(&conn, "app.chat", None, 6).unwrap().len(), 1);
        // deleted notes are not relearned.
        let n2 = add_note(&conn, "surface", "app.chat", "", "learned", "No emoji.", &[], 50).unwrap().unwrap();
        delete_note(&conn, n2).unwrap();
        assert!(add_note(&conn, "surface", "app.chat", "", "learned", "no emoji.", &[], 51).unwrap().is_none());
    }

    #[test]
    fn kept_and_sent_finals_are_mined_verbatim_as_accepted_reverts_never() {
        let mut conn = crate::open_in_memory().unwrap();
        let (i, o) = ("hey can u send the file", "Hey, could you send me the file?");
        let mut rec = |at| record_outcome(&mut conn, &out("a", None, i, o, &[]), at).unwrap().unwrap();
        let (verbatim, punct, sent_punct, reverted) = (rec(1), rec(10 * 60_000), rec(20 * 60_000), rec(30 * 60_000));
        // the same text with its line breaks / spacing read back differently is not a change
        apply_observation(&conn, verbatim, Verdict::Kept, 1.0, "Hey,  could you\nsend me the file?", 2).unwrap();
        // word similarity 1.0, but the user dropped the comma and the question mark
        apply_observation(&conn, punct, Verdict::Kept, 1.0, "Hey could you send me the file", 3).unwrap();
        apply_sent(&conn, sent_punct, "hey, could you send me the file", 4).unwrap();
        apply_observation(&conn, reverted, Verdict::Reverted, 0.99, i, 5).unwrap();
        let mut rows: Vec<(i64, bool)> = unmined(&conn, 20).unwrap().iter().map(|r| (r.id, r.accepted)).collect();
        rows.sort_unstable();
        assert_eq!(rows, vec![(verbatim, true), (punct, false), (sent_punct, false)]);
    }

    #[test]
    fn a_note_mined_again_is_corroborated_not_duplicated() {
        let conn = crate::open_in_memory().unwrap();
        add_note(&conn, "surface", "a", "", "learned", "Keep it casual.", &[], 1).unwrap().unwrap();
        assert!(add_note(&conn, "surface", "a", "", "learned", "keep it casual.", &[], 4).unwrap().is_none());
        corroborate_note(&conn, "surface", "a", "", "keep it casual.").unwrap();
        let n = list_notes(&conn).unwrap();
        assert_eq!((n.len(), n[0].confirmations), (1, 1));
    }

    #[test]
    fn a_standing_instruction_is_a_stated_note_the_first_time() {
        let mut conn = crate::open_in_memory().unwrap();
        let o = NewOutcome { app: "m", partner: None, instruction: "From now on, never use emoji!", input: "x", output: "Hi.", note_ids: &[] };
        record_outcome(&mut conn, &o, 0).unwrap();
        let once = NewOutcome { instruction: "make it formal", ..o.clone() };
        record_outcome(&mut conn, &once, 10 * 60_000).unwrap();
        assert_eq!(promote_repeated_instructions(&conn, 20 * 60_000).unwrap(), 1);
        let n = active_notes(&conn, "m", None, 6).unwrap();
        assert_eq!((n.len(), n[0].kind.as_str(), n[0].text.as_str()), (1, "stated", "From now on, never use emoji!"));
        assert!(is_standing("always, sign off with best") && !is_standing("make it shorter") && !is_standing("nevermind the date"));
        assert!(!is_standing("remember to mention the deadline"));
    }

    #[test]
    fn length_prior_few_shot_and_empty_db() {
        let mut conn = crate::open_in_memory().unwrap();
        assert!(length_prior(&conn, "a", None).unwrap().is_none());
        assert!(few_shot(&conn, "a", None, 3).unwrap().is_empty());
        for (k, (i, o)) in [("hey you free", "Hey, are you free later today?"), ("ok sure thing", "Okay, sure thing."), ("thx a lot", "Thanks a lot for this."), ("lol no way", "Ha, no way.")].iter().enumerate() {
            let id = record_outcome(&mut conn, &out("a", Some("p"), i, o, &[]), 100 + k as i64 * 1000 * 60 * 10).unwrap().unwrap();
            apply_observation(&conn, id, if k == 1 { Verdict::Edited } else { Verdict::Kept }, 0.9, if k == 1 { "Okay sure!" } else { o }, 5).unwrap();
        }
        let p = length_prior(&conn, "a", Some("p")).unwrap().unwrap();
        assert_eq!((p.samples, p.scope), (4, "surface"));
        let shots = few_shot(&conn, "a", Some("p"), 3).unwrap();
        assert_eq!(shots[0].kind, "revised");
        assert_eq!(shots[0].final_text, "Okay sure!");
        assert!(shots.len() <= 3);
    }

    #[test]
    fn thin_partner_history_does_not_outrank_the_surface() {
        let mut conn = crate::open_in_memory().unwrap();
        let mut at = 100;
        let mut add = |conn: &mut Connection, partner: Option<&str>, input: &str| {
            at += 1000 * 60 * 10;
            let id = record_outcome(conn, &out("a", partner, input, "Sounds good, thanks!", &[]), at).unwrap().unwrap();
            apply_observation(conn, id, Verdict::Kept, 0.9, "Sounds good, thanks!", at).unwrap();
        };
        for k in 0..4 {
            add(&mut conn, Some("p"), &format!("partner draft {k}"));
        }
        add(&mut conn, None, "newest surface draft");
        add_note(&conn, "surface", "a", "", "learned", "Surface note.", &[], 1).unwrap();
        add_note(&conn, "partner", "a", "p", "learned", "Partner note.", &[], 2).unwrap();
        // 4 finals: surface voice wins everywhere, no history line
        assert_eq!(length_prior(&conn, "a", Some("p")).unwrap().unwrap().scope, "surface");
        assert!(partner_facts(&conn, "a", "p").unwrap().is_none());
        assert_eq!(few_shot(&conn, "a", Some("p"), 3).unwrap()[0].input, "newest surface draft");
        assert_eq!(active_notes(&conn, "a", Some("p"), 6).unwrap()[0].text, "Surface note.");
        // the 5th final establishes the partner
        add(&mut conn, Some("p"), "partner draft 4");
        add(&mut conn, None, "even newer surface draft");
        assert_eq!(length_prior(&conn, "a", Some("p")).unwrap().unwrap().scope, "partner");
        assert!(partner_facts(&conn, "a", "p").unwrap().is_some());
        assert_eq!(few_shot(&conn, "a", Some("p"), 3).unwrap()[0].input, "partner draft 4");
        assert_eq!(active_notes(&conn, "a", Some("p"), 6).unwrap()[0].text, "Partner note.");
    }

    #[test]
    fn repeated_instruction_becomes_a_stated_note_and_redaction_applies() {
        let mut conn = crate::open_in_memory().unwrap();
        let o = NewOutcome { app: "m", partner: None, instruction: "Make it shorter", input: "make it shorter", output: "key sk-ant-abc123456789012345678901234 ok", note_ids: &[] };
        let day = 24 * 60 * 60 * 1000;
        // a retry within minutes supersedes the first: not a repeat
        record_outcome(&mut conn, &o, 0).unwrap();
        record_outcome(&mut conn, &o, 60_000).unwrap();
        // twice on one day is not enough either
        record_outcome(&mut conn, &o, 60 * 60 * 1000).unwrap();
        assert_eq!(promote_repeated_instructions(&conn, day).unwrap(), 0);
        // a second day is
        record_outcome(&mut conn, &o, day + 1).unwrap();
        assert_eq!(promote_repeated_instructions(&conn, day + 2).unwrap(), 1);
        assert_eq!(promote_repeated_instructions(&conn, day + 2).unwrap(), 0);
        let stored: String = conn.query_row("SELECT output_text FROM rewrite_outcomes LIMIT 1", [], |r| r.get(0)).unwrap();
        assert!(!stored.contains("sk-ant-"));
        assert_eq!(summary(&conn).unwrap().notes, 1);
    }

    #[test]
    fn old_thread_keys_are_renamed_with_their_notes() {
        let mut conn = crate::open_in_memory().unwrap();
        let old = "thread:(2) honmaru ai - google chrome – anant";
        record_outcome(&mut conn, &out("a", Some(old), "hi", "Hi there.", &[]), 1).unwrap();
        add_note(&conn, "partner", "a", old, "learned", "Short.", &[], 1).unwrap();
        assert_eq!(thread_partner_keys(&conn).unwrap(), vec![old.to_string()]);
        rename_partner(&conn, old, "thread:honmaru ai").unwrap();
        assert_eq!(thread_partner_keys(&conn).unwrap(), vec!["thread:honmaru ai".to_string()]);
        assert_eq!(active_notes(&conn, "a", Some("thread:honmaru ai"), 6).unwrap().len(), 1);
    }
}
