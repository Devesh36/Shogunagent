//! Sessions, not rows — the unit the nightly consolidation lane sends (zero-base plan L1/L2,
//! `docs/select-kk-cost-zero-base-plan.md` §3). Pure string work, no I/O, no model.
//!
//! Sending every `event_log` row as its own request costs ~1,650 requests a night, each
//! re-carrying the instruction and each answered with verbatim quotes. The same window captured
//! again and again yields the same lines again and again, and the output (five times the price
//! of the input) repeats them back. This module folds the night instead:
//!
//! 1. **Pre-filter per line** ([`super::prefilter`], #286) runs first, so every
//!    rule it holds still holds.
//! 2. **Secondary sources step aside** (L0(a)): a `screen_ocr` row is dropped when its window-hour
//!    also has accessibility (`capture`) text — the OCR exists for search, the model reads the AX
//!    text. An OCR-only window is still sent. **Sources another Batch lane already reads step
//!    aside too** (L0(b), [`SessionPolicy`]): while mail triage is on, every `gmail` row is read
//!    by its Stage B request (which since L0(b) asks for commitments and open loops as well as a
//!    label), so the consolidation lane skips it; with triage off the rows stay here, or mail
//!    commitments would vanish. A runtime decision, not a change to the static source filter.
//! 3. **Group by session**: key = (`app_bundle_id` or source, normalised `window_title`,
//!    `ts / 3600 s`). Within a session, rows stay in time order and each line remembers the
//!    `event_id` it came from.
//! 4. **Dedup lines**: a line whose whitespace-/case-normalised form was already kept, or that
//!    is a ≥98 % bigram-Dice twin of one of the last few kept lines (`capture/dedup.rs`) **with
//!    the same words in it** (the same multiset of lower-cased alphanumeric words — so only
//!    whitespace, punctuation, case and word-order twins merge; `approved` → `rejected`,
//!    `Friday` → `Monday`, `1023` → `1024` never do), is dropped — the first copy stays, and
//!    inherits `changed` when the dropped copy came from a `text_delta` row.
//! 5. **Cap at [`SESSION_MAX_CHARS`]**: lines from `text_delta` rows (what a small AX revision
//!    added) are kept in preference to lines from full checkpoints when a session has to be cut.
//! 6. **One request per session**, the shared instruction in `system`, the body as numbered lines
//!    behind a `Window:` header, every line through [`shogun_redact::redact_egress`].
//!
//! **Anchors instead of quotes** (L2): the model names its support as `line` (the `[n]` number
//! of the one line that supports the item), `anchor` (the first ≤ 40 characters of that line, the
//! verification) and `anchor_len` (how many characters of that line, from its start, make up the
//! span). [`BatchSession::resolve_anchor`] takes the numbered line when its text starts with the
//! anchor, otherwise the **unique** line the anchor names (an anchor two lines share does not
//! resolve), cuts the span, and hands back the **raw** source text of that span together with the
//! line's `event_id` — so `knowledge_provenance(entity_id, event_id, quote)` and the
//! `source.contains(quote)` check in `knowledge::persist` hold as for a verbatim quote. An anchor
//! that does not resolve is dropped, never guessed: a stored quote comes from the event it is
//! attributed to, or it is not stored.
//!
//! Provenance granularity is one `event_id` per stored span, whatever the unit the model sees.

use std::collections::{HashMap, HashSet};

use shogun_memory::event_log::{
    BatchEventMeta, BatchEventText, BATCH_PRIMARY_SOURCE, BATCH_SECONDARY_SOURCES,
};

use super::prefilter::{self, BatchPrefilterStats};
use crate::capture::dedup;

/// Upper bound on the captured characters one session body carries (`docs/select-kk-cost-zero-base-plan.md`
/// §2 L1). The header line and the `[n] ` markers are not counted.
pub const SESSION_MAX_CHARS: usize = 12_000;

/// Width of the session key's time bucket.
pub const SESSION_HOUR_MS: i64 = 3_600_000;

/// How many characters of the supporting line the model copies as the anchor. The resolver is
/// lenient about a few more; the instruction asks for this.
pub const ANCHOR_MAX_CHARS: usize = 40;

/// Longest anchor the resolver will even look for. Past this the model is quoting, not anchoring.
const ANCHOR_HARD_MAX_CHARS: usize = 200;

/// Shortest anchor the `find` fallback (an anchor that starts no line) accepts. Shorter than this
/// — "I'll", "Re:", a first name — it names a phrase, not a line, and could sit in many lines.
pub const ANCHOR_FIND_MIN_CHARS: usize = 12;

/// Byte ceiling of a provenance quote — `knowledge::Candidate::valid_shape` and the stored
/// `knowledge_provenance.quote` reads both stop at 800 bytes.
pub const QUOTE_MAX_BYTES: usize = 800;

/// How many recently kept lines a new line is compared against for the near-duplicate rule.
/// Bounded so a thousand-line session stays linear.
const NEAR_DUP_LOOKBACK: usize = 8;

/// `event_log.source` of synced Gmail threads — the rows mail triage Stage B reads.
pub const MAIL_SOURCE: &str = "gmail";

/// What the night's rows are subject to besides the static source filter
/// (`BATCH_EXCLUDED_SOURCES`, a consent rule held by `event_log`): the sources whose rows another
/// Batch lane already covers tonight and the consolidation lane therefore skips (L0(b)). Decided
/// at run time from the user's settings — [`SessionPolicy::for_mail_triage`] is the one place
/// that maps the mail triage switch to a source list; `Db::consolidation_session_policy` reads
/// the switch and calls it — and passed to [`build_sessions_with_policy`]. A skipped row is
/// counted (`rows_dropped_covered`), never silently gone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionPolicy {
    /// Sources whose rows are not folded into sessions tonight.
    pub skip_sources: Vec<String>,
}

impl SessionPolicy {
    /// The policy for a night on which mail triage is on (`triage_on`): `gmail` rows are left to
    /// Stage B, which extracts their commitments and open loops into the same state tables with
    /// provenance to the same event rows. With triage off nothing else reads them, so they stay
    /// in the consolidation lane.
    pub fn for_mail_triage(triage_on: bool) -> Self {
        Self {
            skip_sources: if triage_on { vec![MAIL_SOURCE.to_string()] } else { Vec::new() },
        }
    }

    fn skips(&self, source: &str) -> bool {
        self.skip_sources.iter().any(|s| s == source)
    }
}

/// `event_log.kind` of the rows that hold only the lines a small AX revision added.
const DELTA_KIND: &str = "text_delta";

/// How much of one event's text goes to the Batch classifier. A capture checkpoint can be
/// 32 KiB of a rendered page; the promises and open loops in it are in the prose, which the
/// head of the window carries as well as the rest, and the nightly bill was linear in bytes
/// nobody needed sent (~2–5 MB/night at ~1,650 rows). Chars, not bytes, so CJK text is not cut
/// mid-character.
pub const BATCH_EXCERPT_CHARS: usize = 4_096;

/// The head of `content`, cut on a character boundary at [`BATCH_EXCERPT_CHARS`] — trailing
/// partial words are kept: the classifier reads prose, not tokens, and a cut word costs nothing.
pub fn batch_excerpt(content: &str) -> &str {
    match content.char_indices().nth(BATCH_EXCERPT_CHARS) {
        Some((cut, _)) => &content[..cut],
        None => content,
    }
}

/// What a session is keyed on. `app` is the bundle id, or the source name for integration rows
/// (`slack`, `gcal`, …) that have no app; `title` is the window title after [`session_title`]
/// ([`shogun_memory::thread::normalise_window_title`] — unread badges, dirty markers and the
/// trailing " — App" segment removed, lower-cased — then the volatile counters a title carries
/// while the window stays the same), empty when there is none; `hour` is
/// `ts.div_euclid(SESSION_HOUR_MS)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionKey {
    pub app: String,
    pub title: String,
    pub hour: i64,
}

impl SessionKey {
    /// The key of a row, from its grouping metadata.
    pub fn from_meta(meta: &BatchEventMeta) -> Self {
        Self {
            app: meta
                .app_bundle_id
                .clone()
                .filter(|a| !a.trim().is_empty())
                .unwrap_or_else(|| meta.source.clone()),
            title: meta.window_title.as_deref().map(session_title).unwrap_or_default(),
            hour: meta.ts.div_euclid(SESSION_HOUR_MS),
        }
    }

    /// A row the caller gave no metadata for is a session of its own — the per-row behaviour.
    fn solo(id: i64) -> Self {
        Self {
            app: format!("event:{id}"),
            title: String::new(),
            hour: 0,
        }
    }
}

/// The title half of a session key. Local to the session lane on purpose: the thread key
/// (`shogun_memory::thread`) must stay what it was for the threads already stored, while a
/// session is a grouping of one night's rows that nothing persists.
///
/// After [`shogun_memory::thread::normalise_window_title`], the counters a window title changes
/// while the window itself does not are removed anywhere in the title: a count in parentheses
/// (`Inbox (12)`, `(1,234)`), a percentage (`Export 43%`), a clock or timer (`12:03`, `1:02:15`).
/// Whitespace is collapsed. Other digits (an issue number, `Q3`, an invoice id) are kept: they
/// name the window.
pub fn session_title(title: &str) -> String {
    let base = shogun_memory::thread::normalise_window_title(title);
    let chars: Vec<char> = base.chars().collect();
    let mut out = String::with_capacity(base.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '(' {
            // "(123)" / "(1,234)": a badge count.
            let mut j = i + 1;
            while j < chars.len() && (chars[j].is_ascii_digit() || chars[j] == ',') {
                j += 1;
            }
            if j > i + 1 && chars[i + 1].is_ascii_digit() && chars.get(j) == Some(&')') {
                i = j + 1;
                continue;
            }
        }
        if chars[i].is_ascii_digit() {
            let mut j = i;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            // "43%": a progress percentage.
            if chars.get(j) == Some(&'%') {
                i = j + 1;
                continue;
            }
            // "12:03" / "1:02:15": a clock or timer — one or two digits, then one or two ":dd".
            if j - i <= 2 && chars.get(j) == Some(&':') {
                let mut k = j;
                let mut groups = 0;
                while groups < 2
                    && chars.get(k) == Some(&':')
                    && chars.get(k + 1).is_some_and(char::is_ascii_digit)
                    && chars.get(k + 2).is_some_and(char::is_ascii_digit)
                {
                    k += 3;
                    groups += 1;
                }
                if groups > 0 && !chars.get(k).is_some_and(char::is_ascii_digit) {
                    i = k;
                    continue;
                }
            }
            out.extend(&chars[i..j]);
            i = j;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The Batch `custom_id` of a session: `s` + 16 hex digits of the key's xxhash64. A function of
/// the key only, so a night resumed after a restart (or after an explicit deletion emptied part of
/// a session) rebuilds the same ids; `[A-Za-z0-9_-]{1,64}` as the provider and relay require.
pub fn session_custom_id(key: &SessionKey) -> String {
    use std::hash::Hasher;
    let mut h = twox_hash::XxHash64::with_seed(0);
    h.write(key.app.as_bytes());
    h.write_u8(0);
    h.write(key.title.as_bytes());
    h.write_u8(0);
    h.write_i64(key.hour);
    format!("s{:016x}", h.finish())
}

/// One line of a session body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionLine {
    /// The `event_log` row the line came from — the provenance of anything anchored on it.
    pub event_id: i64,
    /// Exactly what the model reads for this line (whitespace-normalised, egress-redacted),
    /// without the `[n] ` marker.
    pub sent: String,
    /// The line as it stands in the event's stored content (trimmed), so a resolved span is a
    /// substring of `event_log.content` and `knowledge::persist`'s `source.contains(quote)` holds.
    pub raw: String,
    /// From a `text_delta` row: the text a small revision added, preferred under the cap.
    pub changed: bool,
}

/// One Batch request of the consolidation lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchSession {
    pub custom_id: String,
    pub key: SessionKey,
    /// The `Window:` header the body opens with (app and title, redacted).
    pub header: String,
    pub lines: Vec<SessionLine>,
}

/// A resolved anchor: the raw source span and the event it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSpan {
    pub event_id: i64,
    pub quote: String,
    /// The 1-based number of the line the span was cut from (the `[n]` the model read). A caller
    /// that passed a `line` hint compares it to this to count a hint the anchor check rejected.
    pub line: usize,
}

/// How a window's anchors fared at parse time (L2 telemetry). Counts only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AnchorStats {
    /// Items whose anchor named a line and whose span was recovered from the raw source.
    #[serde(default)]
    pub resolved: usize,
    /// Items dropped because their anchor did not resolve (never attributed by guess): missing,
    /// paraphrased, shared by two lines, pointing at the header, through redacted text.
    #[serde(default)]
    pub unresolved: usize,
    /// Resolved items whose `line` named a line that does not start with the anchor — the number
    /// was wrong, the text search found the one line that is. A model-quality signal, not a loss.
    #[serde(default)]
    pub line_mismatch: usize,
}

impl BatchSession {
    /// Distinct event ids the session's lines came from, in first-seen (time) order.
    pub fn event_ids(&self) -> Vec<i64> {
        let mut seen = HashSet::new();
        self.lines
            .iter()
            .map(|l| l.event_id)
            .filter(|id| seen.insert(*id))
            .collect()
    }

    /// Captured characters in the body (what the cap and `chars_sent` count).
    pub fn chars(&self) -> usize {
        self.lines.iter().map(|l| l.sent.chars().count()).sum()
    }

    /// The body the model reads: the header, then every line numbered from 1.
    pub fn text(&self) -> String {
        let mut out = String::with_capacity(self.header.len() + 16 + self.lines.iter().map(|l| l.sent.len() + 8).sum::<usize>());
        out.push_str("Window: ");
        out.push_str(&self.header);
        for line in self.numbered_lines() {
            out.push('\n');
            out.push_str(&line);
        }
        out
    }

    /// The numbered lines alone (`[n] text`, one per line, from 1) — what a lane with its own
    /// header (mail triage Stage B) puts after it. The `[n]` here is the `line` the resolver reads.
    pub fn body(&self) -> String {
        self.numbered_lines().collect::<Vec<_>>().join("\n")
    }

    fn numbered_lines(&self) -> impl Iterator<Item = String> + '_ {
        self.lines.iter().enumerate().map(|(n, line)| format!("[{}] {}", n + 1, line.sent))
    }

    /// Resolve an item's `anchor` / `anchor_len` / `line` against this session's lines.
    ///
    /// 1. The anchor is cleaned: a copied `[n] ` marker is stripped, whitespace trimmed. Empty or
    ///    absurdly long anchors do not resolve.
    /// 2. The supporting line is chosen, never guessed ([`Self::locate`]):
    ///    - `line` (1-based, the `[n]` marker the model read) names a line whose text **starts
    ///      with** the anchor → that line. A `line` whose text does not start with the anchor is
    ///      ignored (the caller counts it from [`ResolvedSpan::line`]);
    ///    - otherwise the lines that start with the anchor: exactly one → that line; several →
    ///      the one whose length is `anchor_len`, and only when exactly one is; otherwise the
    ///      anchor is **ambiguous** and does not resolve (two lines sharing a 40-character prefix
    ///      — the same promise on Friday and on Monday, two events — must not be merged);
    ///    - no line starts with it → the lines that **contain** it (`find`), accepted only when
    ///      the anchor is at least [`ANCHOR_FIND_MIN_CHARS`] long and exactly one line contains it.
    /// 3. The span is `anchor_len` characters of that line from the anchor's position (never
    ///    fewer than the anchor itself, never past the line's end). When `must_contain` (an
    ///    entity's name and context) does not fit in the span but fits in the rest of the line,
    ///    the span widens to the line's end. The span is clamped to [`QUOTE_MAX_BYTES`].
    /// 4. The quote is that span **as it stands in the raw line** — the sent line is the raw line
    ///    with whitespace collapsed and secrets masked, so the span is looked up in the raw text;
    ///    if it is not there verbatim but the whole line was sent unchanged (no redaction, only
    ///    whitespace), the whole raw line stands in. A span that touched redacted text, or that
    ///    cannot be found in the raw line, does not resolve: provenance never widens to text the
    ///    model did not see, and never carries a value the egress masked.
    pub fn resolve_anchor(
        &self,
        anchor: &str,
        anchor_len: Option<usize>,
        line: Option<usize>,
        must_contain: &[&str],
    ) -> Option<ResolvedSpan> {
        let anchor = clean_anchor(anchor)?;
        let anchor_chars = anchor.chars().count();
        let (index, pos) = self.locate(&anchor, anchor_len, line)?;
        let line = &self.lines[index];
        let tail = &line.sent[pos..];
        let want = anchor_len.unwrap_or(usize::MAX).max(anchor_chars);
        let mut span = head_chars(tail, want);
        if !contains_all(span, must_contain) {
            if contains_all(tail, must_contain) {
                span = tail;
            } else {
                return None;
            }
        }
        let span = head_within_bytes(span, QUOTE_MAX_BYTES);
        if span.len() < anchor.len() || !contains_all(span, must_contain) {
            return None;
        }
        let quote = if line.raw.contains(span) {
            span.to_string()
        } else if prefilter::normalize_line(&line.raw) == line.sent
            && line.raw.len() <= QUOTE_MAX_BYTES
            && contains_all(&line.raw, must_contain)
        {
            line.raw.clone()
        } else {
            return None;
        };
        Some(ResolvedSpan {
            event_id: line.event_id,
            quote,
            line: index + 1,
        })
    }

    /// Step 2 of [`Self::resolve_anchor`]: the one line a cleaned anchor names, as
    /// `(line index, byte offset of the anchor in that line)`. `None` when no line or more than
    /// one line could be meant.
    fn locate(&self, anchor: &str, anchor_len: Option<usize>, line: Option<usize>) -> Option<(usize, usize)> {
        // The numbered line, when its text confirms the number.
        if let Some(index) = line.and_then(|n| n.checked_sub(1)) {
            if self.lines.get(index).is_some_and(|l| l.sent.starts_with(anchor)) {
                return Some((index, 0));
            }
        }
        // The lines that start with the anchor — one, or one of the stated length.
        let starts: Vec<usize> = self
            .lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.sent.starts_with(anchor))
            .map(|(i, _)| i)
            .collect();
        match starts.as_slice() {
            [] => {}
            [only] => return Some((*only, 0)),
            several => {
                let want = anchor_len?;
                let mut of_length = several
                    .iter()
                    .copied()
                    .filter(|i| self.lines[*i].sent.chars().count() == want);
                return match (of_length.next(), of_length.next()) {
                    (Some(index), None) => Some((index, 0)),
                    _ => None,
                };
            }
        }
        // A mid-line anchor: long enough to be a phrase from one line, and in exactly one line.
        if anchor.chars().count() < ANCHOR_FIND_MIN_CHARS {
            return None;
        }
        let mut found = self
            .lines
            .iter()
            .enumerate()
            .filter_map(|(i, l)| l.sent.find(anchor).map(|p| (i, p)));
        match (found.next(), found.next()) {
            (Some(hit), None) => Some(hit),
            _ => None,
        }
    }
}

/// Strip a copied `[n] ` marker and surrounding whitespace; refuse empty and over-long anchors.
fn clean_anchor(anchor: &str) -> Option<String> {
    let mut s = anchor.trim();
    if let Some(rest) = s.strip_prefix('[') {
        if let Some((digits, tail)) = rest.split_once(']') {
            if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
                s = tail.trim_start();
            }
        }
    }
    let s = s.trim();
    if s.is_empty() || s.chars().count() > ANCHOR_HARD_MAX_CHARS {
        return None;
    }
    Some(s.to_string())
}

fn contains_all(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().all(|n| haystack.contains(n))
}

/// The first `n` chars of `s`.
fn head_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((cut, _)) => &s[..cut],
        None => s,
    }
}

/// The head of `text` in at most `max_bytes` bytes, cut on a character boundary.
fn head_within_bytes(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut cut = max_bytes;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    &text[..cut]
}

/// A pre-filtered row with its lines mapped back to the raw text, before grouping.
struct Row {
    index: usize,
    source: String,
    lines: Vec<SessionLine>,
}

/// Fold a window of cloud-eligible rows into sessions with no runtime source policy (every
/// cloud-eligible row is a candidate). [`build_sessions_with_policy`] with
/// [`SessionPolicy::default`]: the shape the eval harness and the unit tests use, and what the
/// lane sends when mail triage is off.
pub fn build_sessions(
    events: &[BatchEventText],
    meta: &[BatchEventMeta],
) -> (Vec<BatchSession>, BatchPrefilterStats) {
    build_sessions_with_policy(events, meta, &SessionPolicy::default())
}

/// Fold a window of cloud-eligible rows into sessions. `meta` is matched to `events` by id; a row
/// without metadata is sent as a session of its own. Rows whose source `policy` skips (L0(b)) are
/// set aside before the pre-filter runs — they are not sent, so they must not shape its
/// cross-row rules either — and counted in `rows_dropped_covered`. Returns the sessions in time
/// order and the night's counts (the pre-filter's, plus the session counters; `kept` is the
/// number of sessions; `input` is every row handed in, skipped ones included).
pub fn build_sessions_with_policy(
    events: &[BatchEventText],
    meta: &[BatchEventMeta],
    policy: &SessionPolicy,
) -> (Vec<BatchSession>, BatchPrefilterStats) {
    let meta_by_id: HashMap<i64, &BatchEventMeta> = meta.iter().map(|m| (m.id, m)).collect();
    // The rows the policy leaves to this lane, as indices into `events`.
    let eligible: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| !meta_by_id.get(&e.id).is_some_and(|m| policy.skips(&m.source)))
        .map(|(i, _)| i)
        .collect();
    let excerpts: Vec<&str> = eligible.iter().map(|&i| batch_excerpt(&events[i].content)).collect();
    let (kept, mut stats) = prefilter::prefilter_texts(&excerpts);
    stats.input = events.len();
    stats.rows_dropped_covered = events.len() - eligible.len();

    // Group the surviving rows by session key, first-seen order (rows arrive in time order).
    let mut order: Vec<SessionKey> = Vec::new();
    let mut groups: HashMap<SessionKey, (Vec<Row>, Option<&BatchEventMeta>)> = HashMap::new();
    for survivor in kept {
        let Some(&index) = eligible.get(survivor.index) else {
            continue;
        };
        let Some(event) = events.get(index) else {
            continue;
        };
        let meta = meta_by_id.get(&event.id).copied();
        let key = meta.map_or_else(|| SessionKey::solo(event.id), SessionKey::from_meta);
        let changed = meta.is_some_and(|m| m.kind == DELTA_KIND);
        let lines = map_lines(event.id, excerpts[survivor.index], &survivor.text, changed);
        stats.session_lines_in += lines.len();
        let row = Row {
            index,
            source: meta.map(|m| m.source.clone()).unwrap_or_default(),
            lines,
        };
        let entry = groups.entry(key.clone()).or_insert_with(|| {
            order.push(key);
            (Vec::new(), meta)
        });
        entry.0.push(row);
    }

    let mut sessions = Vec::with_capacity(order.len());
    for key in order {
        let Some((mut rows, first_meta)) = groups.remove(&key) else {
            continue;
        };
        // L0(a): secondary sources only stand in where the primary text is absent.
        if rows.iter().any(|r| r.source == BATCH_PRIMARY_SOURCE) {
            let before = rows.len();
            rows.retain(|r| !BATCH_SECONDARY_SOURCES.contains(&r.source.as_str()));
            stats.rows_dropped_shadowed += before - rows.len();
        }
        rows.sort_by_key(|r| r.index);
        let rows_in = rows.len();
        let lines: Vec<SessionLine> = rows.into_iter().flat_map(|r| r.lines).collect();
        let (lines, deduped) = dedup_lines(lines);
        stats.session_lines_deduped += deduped;
        let (lines, capped) = cap_lines(lines, SESSION_MAX_CHARS);
        stats.session_lines_capped += capped;
        if lines.is_empty() {
            continue;
        }
        let header = header_for(&key, first_meta);
        let session = BatchSession {
            custom_id: session_custom_id(&key),
            key,
            header,
            lines,
        };
        stats.rows_in_sessions += rows_in;
        stats.chars_sent += session.chars();
        sessions.push(session);
    }
    stats.sessions = sessions.len();
    stats.kept = sessions.len();
    (sessions, stats)
}

/// Map the pre-filter's surviving lines (whitespace-normalised, in order) back to the raw lines
/// of the excerpt they came from, and apply egress redaction to what will be sent.
fn map_lines(event_id: i64, excerpt: &str, cleaned: &str, changed: bool) -> Vec<SessionLine> {
    let mut raw_lines = excerpt.lines();
    let mut out = Vec::new();
    for sent in cleaned.lines() {
        // Advance through the raw lines until the one this cleaned line was made from.
        let raw = raw_lines
            .by_ref()
            .find(|raw| prefilter::normalize_line(raw) == sent)
            .map(str::trim)
            // Unreachable by construction (the cleaned text is a subset of the normalised raw
            // lines); if it ever happened the line would still be verifiable only against
            // itself, and `knowledge::persist` drops what the source does not contain.
            .unwrap_or(sent);
        out.push(SessionLine {
            event_id,
            sent: shogun_redact::redact_egress(sent).into_owned(),
            raw: raw.to_string(),
            changed,
        });
    }
    out
}

/// The lines of one event's text as a session would carry them, **without** the pre-filter's
/// chrome rules: every non-blank line, whitespace-normalised and egress-redacted in `sent`, the
/// trimmed raw line in `raw`, all attributed to `event_id`. For a lane whose text is one message
/// body rather than a night of screen captures (mail triage Stage B reads a thread excerpt this
/// way), so its anchors resolve through [`BatchSession::resolve_anchor`] under exactly the same
/// contract: the quote is a substring of the event's stored content or the item is dropped.
pub fn lines_from_text(event_id: i64, text: &str) -> Vec<SessionLine> {
    text.lines()
        .filter_map(|raw| {
            let sent = prefilter::normalize_line(raw);
            (!sent.is_empty()).then(|| SessionLine {
                event_id,
                sent: shogun_redact::redact_egress(&sent).into_owned(),
                raw: raw.trim().to_string(),
                changed: false,
            })
        })
        .collect()
}

/// Drop exact (normalised) and near-duplicate lines, keeping the first copy. A kept line whose
/// later copy came from a `text_delta` row takes the copy's `changed` flag, so the cap still
/// prefers it. Returns the kept lines and how many were dropped.
fn dedup_lines(lines: Vec<SessionLine>) -> (Vec<SessionLine>, usize) {
    let mut seen: HashMap<String, usize> = HashMap::with_capacity(lines.len());
    let mut kept: Vec<SessionLine> = Vec::with_capacity(lines.len());
    let mut dropped = 0;
    for line in lines {
        let key = dedup::normalize(&line.sent);
        if key.is_empty() {
            dropped += 1;
            continue;
        }
        if let Some(&first) = seen.get(&key) {
            kept[first].changed |= line.changed;
            dropped += 1;
            continue;
        }
        let len = key.chars().count();
        // Near duplicate: a ≥98 % bigram twin of a recent line *with the same digits and the
        // same words* — so only whitespace, punctuation, case and word-order twins merge. The
        // digit guard keeps enumerated lines apart ("Invoice 1023: $400" / "Invoice 1024: $400",
        // two dates, two times); the word guard keeps a one-word edit of a long line apart
        // ("approved" → "rejected", "yes" → "no", "Friday" → "Monday" in a 300-character line
        // scores ≥ 0.98 on bigrams and is a different fact).
        let digits: String = line.sent.chars().filter(char::is_ascii_digit).collect();
        let words = word_multiset(&line.sent);
        let twin = kept
            .iter()
            .enumerate()
            .rev()
            .take(NEAR_DUP_LOOKBACK)
            .filter(|(_, k)| {
                let other = k.sent.chars().count();
                other.abs_diff(len) <= 2 + len / 20
                    && k.sent.chars().filter(char::is_ascii_digit).eq(digits.chars())
            })
            .find(|(_, k)| {
                dedup::similarity(&k.sent, &line.sent) >= dedup::NEAR_DUP_THRESHOLD
                    && word_multiset(&k.sent) == words
            })
            .map(|(i, _)| i);
        if let Some(first) = twin {
            kept[first].changed |= line.changed;
            dropped += 1;
            continue;
        }
        seen.insert(key, kept.len());
        kept.push(line);
    }
    (kept, dropped)
}

/// The lower-cased alphanumeric words of a line, sorted: what two lines must share to be
/// near-duplicates. Splits on everything that is not a letter or digit, so punctuation and
/// spacing differences vanish and any word change shows.
fn word_multiset(line: &str) -> Vec<String> {
    let mut words: Vec<String> = line
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect();
    words.sort_unstable();
    words
}

/// Keep the session under `max_chars`: lines from changed (delta) rows first, then checkpoint
/// lines, each class in time order; the result keeps the original order. Returns the kept lines
/// and how many were cut.
fn cap_lines(lines: Vec<SessionLine>, max_chars: usize) -> (Vec<SessionLine>, usize) {
    let sizes: Vec<usize> = lines.iter().map(|l| l.sent.chars().count()).collect();
    if sizes.iter().sum::<usize>() <= max_chars {
        return (lines, 0);
    }
    let mut budget = max_chars;
    let mut keep = vec![false; lines.len()];
    for changed_pass in [true, false] {
        for (i, line) in lines.iter().enumerate() {
            if line.changed != changed_pass || keep[i] {
                continue;
            }
            if sizes[i] <= budget {
                keep[i] = true;
                budget -= sizes[i];
            }
        }
    }
    let total = lines.len();
    let kept: Vec<SessionLine> = lines
        .into_iter()
        .zip(keep)
        .filter_map(|(line, keep)| keep.then_some(line))
        .collect();
    let cut = total - kept.len();
    (kept, cut)
}

/// The `Window:` header: app (or source) and the title as captured, egress-redacted.
fn header_for(key: &SessionKey, meta: Option<&BatchEventMeta>) -> String {
    let title = meta
        .and_then(|m| m.window_title.as_deref())
        .map(str::trim)
        .filter(|t| !t.is_empty());
    let header = match title {
        Some(title) => format!("{} | {title}", key.app),
        None => key.app.clone(),
    };
    let header = prefilter::normalize_line(&header);
    shogun_redact::redact_egress(&header).into_owned()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    const HOUR: i64 = SESSION_HOUR_MS;

    fn ev(id: i64, content: &str) -> BatchEventText {
        BatchEventText { id, content: content.to_string() }
    }

    fn meta(id: i64, ts: i64, source: &str, kind: &str, app: Option<&str>, title: Option<&str>) -> BatchEventMeta {
        BatchEventMeta {
            id,
            ts,
            source: source.to_string(),
            kind: kind.to_string(),
            app_bundle_id: app.map(str::to_string),
            window_title: title.map(str::to_string),
        }
    }

    fn capture(id: i64, ts: i64, app: &str, title: &str) -> BatchEventMeta {
        meta(id, ts, "capture", "text", Some(app), Some(title))
    }

    #[test]
    fn rows_group_by_app_window_and_hour() {
        // Two windows in the same hour, and the same window across two hours: three sessions.
        let events = vec![
            ev(1, "Alice: I'll send the deck on Friday."),
            ev(2, "Bob asked for the Q3 numbers today."),
            ev(3, "Carol: the review is booked for Monday."),
            ev(4, "Dana: still waiting on legal for the contract."),
        ];
        let m = vec![
            capture(1, 10 * HOUR + 1_000, "com.mail", "Re: deck — Mail"),
            capture(2, 10 * HOUR + 2_000, "com.mail", "Re: deck — Mail"),
            capture(3, 10 * HOUR + 3_000, "com.slack", "#launch"),
            capture(4, 11 * HOUR + 500, "com.mail", "(2) Re: deck — Mail"),
        ];
        let (sessions, stats) = build_sessions(&events, &m);
        assert_eq!(sessions.len(), 3, "{sessions:#?}");
        assert_eq!(sessions[0].event_ids(), vec![1, 2]);
        assert_eq!(sessions[0].key, SessionKey { app: "com.mail".into(), title: "re: deck".into(), hour: 10 });
        assert_eq!(sessions[1].event_ids(), vec![3]);
        assert_eq!(sessions[2].event_ids(), vec![4]);
        // The badge and the trailing app segment do not split the window; the hour does.
        assert_eq!(sessions[2].key.title, "re: deck");
        assert_ne!(sessions[0].custom_id, sessions[2].custom_id);
        assert_eq!(stats.sessions, 3);
        assert_eq!(stats.kept, 3);
        assert_eq!(stats.input, 4);
        assert_eq!(stats.rows_in_sessions, 4);
        // The ids are a function of the key alone.
        assert_eq!(sessions[0].custom_id, session_custom_id(&sessions[0].key));
        assert!(sessions[0].custom_id.len() == 17 && sessions[0].custom_id.starts_with('s'));
        assert!(sessions[0].custom_id.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn integration_rows_without_an_app_group_by_source_and_title() {
        let events = vec![ev(1, "Tamsin: I'll get the plan to you by Thursday."), ev(2, "Mara: perfect, thanks.")];
        let m = vec![
            meta(1, HOUR, "slack", "message", None, Some("#ops-sync")),
            meta(2, HOUR + 60_000, "slack", "message", None, Some("#ops-sync")),
        ];
        let (sessions, _) = build_sessions(&events, &m);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].key.app, "slack");
        assert_eq!(sessions[0].header, "slack | #ops-sync");
        assert!(sessions[0].text().starts_with("Window: slack | #ops-sync\n[1] Tamsin"));
    }

    #[test]
    fn a_row_without_metadata_is_its_own_session() {
        let events = vec![ev(7, "hello there"), ev(9, "world again")];
        let (sessions, stats) = build_sessions(&events, &[]);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].event_ids(), vec![7]);
        assert_eq!(sessions[1].event_ids(), vec![9]);
        assert_eq!(stats.sessions, 2);
    }

    #[test]
    fn repeated_checkpoint_lines_collapse_and_a_changed_line_survives() {
        let promise = "Alice: I'll send the final version of the deck to the whole team on Friday.";
        let page = format!("Project inbox\n{promise}\nBob: thanks, looking forward to it.");
        let events = vec![
            ev(1, &page),
            ev(2, promise),                                                        // delta repeating a line
            ev(3, &format!("{page}\nCarol: can we move the review to Monday?")),  // re-read + one new line
            ev(4, &promise.replace(' ', "   ")),                                   // whitespace twin
            ev(5, &promise.replace("Friday.", "Friday!")),                         // one-char edit → near duplicate
        ];
        let m = (1..=5)
            .map(|id| capture(id, HOUR + id * 1_000, "com.mail", "Inbox"))
            .collect::<Vec<_>>();
        let (sessions, stats) = build_sessions(&events, &m);
        assert_eq!(sessions.len(), 1);
        let sent: Vec<&str> = sessions[0].lines.iter().map(|l| l.sent.as_str()).collect();
        assert_eq!(
            sent,
            vec![
                "Project inbox",
                promise,
                "Bob: thanks, looking forward to it.",
                "Carol: can we move the review to Monday?",
            ],
            "{sent:?}"
        );
        // Every kept line knows its event; the new line came from event 3.
        assert_eq!(sessions[0].lines[3].event_id, 3);
        assert!(stats.session_lines_deduped >= 1, "{stats:?}");
        assert_eq!(stats.session_lines_capped, 0);
    }

    #[test]
    fn the_cap_prefers_changed_lines_over_checkpoint_lines() {
        // Four ~3,500-char checkpoints (each under the per-event excerpt) and 2,300 chars of
        // deltas: over the cap together, so the deltas are kept whole and the checkpoints are
        // cut to fit.
        let mut events = Vec::new();
        let mut m = Vec::new();
        for c in 0..4i64 {
            let checkpoint: String = (0..38)
                .map(|i| format!("Checkpoint {c} paragraph {i:03}: the quarterly numbers were discussed at length in this meeting."))
                .collect::<Vec<_>>()
                .join("\n");
            events.push(ev(c + 1, &checkpoint));
            m.push(capture(c + 1, HOUR + c * 1_000, "com.notes", "Q3"));
        }
        let deltas: Vec<String> = (0..30)
            .map(|i| format!("Delta line {i:03}: I'll follow up with Dana on item {i} before Friday, as agreed."))
            .collect();
        for (i, d) in deltas.iter().enumerate() {
            let id = i as i64 + 10;
            events.push(ev(id, d));
            m.push(meta(id, HOUR + id * 1_000, "capture", "text_delta", Some("com.notes"), Some("Q3")));
        }
        let (sessions, stats) = build_sessions(&events, &m);
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert!(s.chars() <= SESSION_MAX_CHARS, "{}", s.chars());
        let delta_lines = s.lines.iter().filter(|l| l.changed).count();
        assert_eq!(delta_lines, 30, "every changed line is kept");
        assert!(stats.session_lines_capped > 0);
        assert!(s.lines.iter().filter(|l| !l.changed).count() < 4 * 38, "the checkpoints were cut");
        // Order is the original time order: checkpoint lines (event 1) first, then the deltas.
        let ids: Vec<i64> = s.lines.iter().map(|l| l.event_id).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted);
    }

    #[test]
    fn an_exact_span_is_recovered_with_its_event_id() {
        let events = vec![
            ev(1, "Tamsin Ekberg: I'll get the rollout plan to you by Thursday.\nMara: Perfect, thanks."),
            ev(2, "Juno Farrant: I'll get the test report to you by Tuesday."),
        ];
        let m = vec![
            meta(1, HOUR, "slack", "message", None, Some("#ops-sync")),
            meta(2, HOUR + 1_000, "slack", "message", None, Some("#ops-sync")),
        ];
        let (sessions, _) = build_sessions(&events, &m);
        let s = &sessions[0];
        let hit = s.resolve_anchor("Juno Farrant: I'll get the test report t", Some(57), None, &["Juno Farrant"]).unwrap();
        assert_eq!(hit.event_id, 2);
        assert_eq!(hit.quote, "Juno Farrant: I'll get the test report to you by Tuesday.");
        // A copied marker and a shorter anchor still resolve; the span is at least the anchor.
        let hit = s.resolve_anchor("[1] Tamsin Ekberg", Some(13), None, &[]).unwrap();
        assert_eq!((hit.event_id, hit.quote.as_str()), (1, "Tamsin Ekberg"));
        // A mid-line anchor is found by `find`, and a span cut short of the entity widens to the
        // line's end.
        let hit = s.resolve_anchor("get the rollout plan", Some(5), None, &["Thursday"]).unwrap();
        assert_eq!(hit.quote, "get the rollout plan to you by Thursday.");
        // No anchor_len: the rest of the line.
        let hit = s.resolve_anchor("Mara:", None, None, &[]).unwrap();
        assert_eq!((hit.event_id, hit.quote.as_str()), (1, "Mara: Perfect, thanks."));
        // The event's stored content contains every quote (what knowledge::persist checks).
        for (anchor, ev_id) in [("Juno Farrant", 2), ("Tamsin Ekberg", 1)] {
            let hit = s.resolve_anchor(anchor, None, None, &[]).unwrap();
            assert!(events[ev_id as usize - 1].content.contains(&hit.quote));
        }
    }

    #[test]
    fn an_unresolvable_anchor_is_dropped_not_guessed() {
        let events = vec![ev(1, "Alice: I'll send the deck on Friday.")];
        let m = vec![capture(1, HOUR, "com.mail", "Re: deck")];
        let (sessions, _) = build_sessions(&events, &m);
        let s = &sessions[0];
        assert!(s.resolve_anchor("Alice: I will send the deck", Some(30), None, &[]).is_none(), "a paraphrase");
        assert!(s.resolve_anchor("", Some(3), None, &[]).is_none());
        assert!(s.resolve_anchor("Window: com.mail", None, None, &[]).is_none(), "the header is not a line");
        assert!(s.resolve_anchor("Alice", Some(5), None, &["Bob"]).is_none(), "the entity is not in the line");
        let long = "x".repeat(ANCHOR_HARD_MAX_CHARS + 1);
        assert!(s.resolve_anchor(&long, None, None, &[]).is_none());
    }

    #[test]
    fn a_span_through_redacted_text_does_not_resolve_and_the_raw_line_never_leaks() {
        let content = "Alice wrote: settle the invoice with card 4111 1111 1111 1111 before Friday and confirm the delivery date";
        let events = vec![ev(1, content)];
        let m = vec![capture(1, HOUR, "com.mail", "Invoice")];
        let (sessions, _) = build_sessions(&events, &m);
        let s = &sessions[0];
        let text = s.text();
        assert!(!text.contains("4111"), "{text}");
        assert!(text.contains("[redacted]"), "{text}");
        // The whole (redacted) line as the model saw it cannot be mapped to a raw span.
        assert!(s.resolve_anchor("Alice wrote: settle the invoice with card", None, None, &[]).is_none());
        // A span that stops before the masked value is a real source span.
        let hit = s.resolve_anchor("Alice wrote: settle the invoice with card", Some(41), None, &[]).unwrap();
        assert_eq!(hit.quote, "Alice wrote: settle the invoice with card");
        assert!(content.contains(&hit.quote));
        // Only `raw` (never sent, only compared against) holds the value.
        assert!(s.lines[0].raw.contains("4111"));
        assert!(!s.lines[0].sent.contains("4111"));
    }

    #[test]
    fn whitespace_only_differences_fall_back_to_the_raw_line() {
        let content = "Alice:   I'll send   the deck on Friday.";
        let events = vec![ev(1, content)];
        let m = vec![capture(1, HOUR, "com.mail", "Re: deck")];
        let (sessions, _) = build_sessions(&events, &m);
        let s = &sessions[0];
        assert_eq!(s.lines[0].sent, "Alice: I'll send the deck on Friday.");
        let hit = s.resolve_anchor("Alice: I'll send the deck", None, None, &[]).unwrap();
        assert_eq!(hit.quote, content, "the raw line stands in for a span the collapse changed");
        // A sub-span that exists verbatim in the raw line is preferred.
        let hit = s.resolve_anchor("the deck on Friday.", None, None, &[]).unwrap();
        assert_eq!(hit.quote, "the deck on Friday.");
    }

    #[test]
    fn ocr_is_dropped_when_the_window_has_ax_text_and_sent_when_it_does_not() {
        let events = vec![
            ev(1, "AX: Alice: I'll send the deck on Friday."),
            ev(2, "OCR: Alice: I'll send the deck on Friday."),
            ev(3, "OCR only: the design review moved to Monday."),
        ];
        let m = vec![
            capture(1, HOUR, "com.mail", "Re: deck"),
            meta(2, HOUR + 10_000, "screen_ocr", "text", Some("com.mail"), Some("Re: deck")),
            meta(3, HOUR + 20_000, "screen_ocr", "text", Some("com.figma"), Some("Review board")),
        ];
        let (sessions, stats) = build_sessions(&events, &m);
        assert_eq!(sessions.len(), 2, "{sessions:#?}");
        assert_eq!(sessions[0].event_ids(), vec![1], "the OCR twin of an AX window is not sent");
        assert_eq!(sessions[1].event_ids(), vec![3], "an OCR-only window is sent");
        assert_eq!(stats.rows_dropped_shadowed, 1);
        assert_eq!(stats.rows_in_sessions, 2);
        // In a later hour with no AX row, the same window's OCR is sent.
        let events = vec![ev(4, "OCR: Alice: I'll send the deck on Friday.")];
        let m = vec![meta(4, 2 * HOUR, "screen_ocr", "text", Some("com.mail"), Some("Re: deck"))];
        let (sessions, stats) = build_sessions(&events, &m);
        assert_eq!(sessions.len(), 1);
        assert_eq!(stats.rows_dropped_shadowed, 0);
    }

    #[test]
    fn stats_count_rows_lines_sessions_and_chars_without_text() {
        let secret = "TOP-SECRET-CHUNK-ALPHA";
        let events = vec![ev(1, &format!("{secret} one")), ev(2, &format!("{secret} one")), ev(3, "   ")];
        let m = vec![
            capture(1, HOUR, "com.a", "T"),
            capture(2, HOUR + 1, "com.a", "T"),
            capture(3, HOUR + 2, "com.a", "T"),
        ];
        let (sessions, stats) = build_sessions(&events, &m);
        assert_eq!(sessions.len(), 1);
        assert_eq!(stats.input, 3);
        assert_eq!(stats.dropped_empty, 1);
        assert_eq!(stats.dropped_duplicate, 1);
        assert_eq!(stats.rows_in_sessions, 1);
        assert_eq!(stats.sessions, 1);
        assert_eq!(stats.chars_sent, sessions[0].chars());
        assert_eq!(stats.session_lines_in, 1);
        let dumped = format!("{stats:?}");
        assert!(!dumped.contains(secret));
        assert!(!dumped.contains("com.a"));
    }

    #[test]
    fn the_cap_and_excerpt_bound_every_session() {
        let line = "約束します。金曜までに見積書を田中さんに送ります。";
        let big: String = (0..2_000).map(|i| format!("{i} {line}")).collect::<Vec<_>>().join("\n");
        let events = vec![ev(1, &big)];
        let m = vec![capture(1, HOUR, "com.a", "T")];
        let (sessions, _) = build_sessions(&events, &m);
        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].chars() <= SESSION_MAX_CHARS);
        assert!(sessions[0].chars() <= BATCH_EXCERPT_CHARS);
        assert_eq!(batch_excerpt("short").len(), 5);
        assert_eq!(batch_excerpt(&big).chars().count(), BATCH_EXCERPT_CHARS);
    }

    #[test]
    fn an_anchor_two_lines_share_resolves_by_line_number_and_never_by_guess() {
        // Friday (event 101) and Monday (event 102): the same 40-character prefix, two promises,
        // two events. The second must be attributed to 102 — or to nothing.
        let friday = "Alice: I'll send the final version of the deck on Friday.";
        let monday = "Alice: I'll send the final version of the deck on Monday.";
        let events = vec![ev(101, friday), ev(102, monday)];
        let m = vec![capture(101, HOUR, "com.mail", "Re: deck"), capture(102, HOUR + 60_000, "com.mail", "Re: deck")];
        let (sessions, _) = build_sessions(&events, &m);
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.lines.len(), 2, "different words: not twins");
        let anchor: String = monday.chars().take(ANCHOR_MAX_CHARS).collect();
        assert!(friday.starts_with(&anchor), "the test needs a shared prefix");

        // With `line`, the numbered line is confirmed by the anchor and taken.
        let hit = s.resolve_anchor(&anchor, Some(57), Some(2), &[]).unwrap();
        assert_eq!((hit.event_id, hit.line, hit.quote.as_str()), (102, 2, monday));
        let hit = s.resolve_anchor(&anchor, Some(57), Some(1), &[]).unwrap();
        assert_eq!((hit.event_id, hit.line, hit.quote.as_str()), (101, 1, friday));
        // Without it, two lines of the same length start with the anchor: ambiguous → dropped,
        // never the first one.
        assert!(s.resolve_anchor(&anchor, Some(57), None, &[]).is_none());
        assert!(s.resolve_anchor(&anchor, None, None, &[]).is_none());
        // A `line` that points past the body or at a line that does not start with the anchor is
        // ignored, and the same ambiguity rule applies.
        assert!(s.resolve_anchor(&anchor, Some(57), Some(3), &[]).is_none());
        assert!(s.resolve_anchor(&anchor, Some(57), Some(0), &[]).is_none());

        // When the two lines differ in length, `anchor_len` tells them apart.
        let monday_long = "Alice: I'll send the final version of the deck on Monday, after the review.";
        let events = vec![ev(101, friday), ev(102, monday_long)];
        let (sessions, _) = build_sessions(&events, &m);
        let s = &sessions[0];
        let hit = s.resolve_anchor(&anchor, Some(monday_long.chars().count()), None, &[]).unwrap();
        assert_eq!((hit.event_id, hit.quote.as_str()), (102, monday_long));
        let hit = s.resolve_anchor(&anchor, Some(friday.chars().count()), None, &[]).unwrap();
        assert_eq!((hit.event_id, hit.quote.as_str()), (101, friday));
        assert!(s.resolve_anchor(&anchor, Some(10), None, &[]).is_none(), "a length neither line has");
    }

    #[test]
    fn a_wrong_line_number_is_overruled_by_the_anchor_and_reported() {
        let events = vec![
            ev(1, "Tamsin Ekberg: I'll get the rollout plan to you by Thursday.\nMara: Perfect, thanks."),
            ev(2, "Juno Farrant: I'll get the test report to you by Tuesday."),
        ];
        let m = vec![
            meta(1, HOUR, "slack", "message", None, Some("#ops-sync")),
            meta(2, HOUR + 1_000, "slack", "message", None, Some("#ops-sync")),
        ];
        let (sessions, _) = build_sessions(&events, &m);
        let s = &sessions[0];
        // `line: 1` does not start with the anchor; the unique line that does is line 3.
        let hit = s.resolve_anchor("Juno Farrant: I'll get the test report t", Some(57), Some(1), &[]).unwrap();
        assert_eq!((hit.event_id, hit.line), (2, 3));
        // The right number is confirmed, and reported as itself.
        let hit = s.resolve_anchor("Mara:", None, Some(2), &[]).unwrap();
        assert_eq!((hit.event_id, hit.line, hit.quote.as_str()), (1, 2, "Mara: Perfect, thanks."));
    }

    #[test]
    fn a_short_or_shared_mid_line_anchor_does_not_resolve() {
        let events = vec![
            ev(1, "Tamsin Ekberg: I'll get the rollout plan to you by Thursday."),
            ev(2, "Juno Farrant: I'll get the test report to you by Tuesday."),
        ];
        let m = vec![capture(1, HOUR, "com.slack", "#ops"), capture(2, HOUR + 1_000, "com.slack", "#ops")];
        let (sessions, _) = build_sessions(&events, &m);
        let s = &sessions[0];
        // "I'll" starts no line and could be in any: unresolved, with or without a length.
        assert!(s.resolve_anchor("I'll", None, None, &[]).is_none());
        assert!(s.resolve_anchor("I'll", Some(4), None, &[]).is_none());
        // Long enough, but in both lines: unresolved.
        assert!(s.resolve_anchor("I'll get the", None, None, &[]).is_none());
        assert!(s.resolve_anchor("to you by", None, None, &[]).is_none(), "9 chars, in both lines");
        // Long enough and in exactly one line: resolved to that line.
        let hit = s.resolve_anchor("get the test report", None, None, &[]).unwrap();
        assert_eq!((hit.event_id, hit.line, hit.quote.as_str()), (2, 2, "get the test report to you by Tuesday."));
        // A `line` hint does not rescue a mid-line anchor: the numbered line must start with it.
        assert!(s.resolve_anchor("I'll", None, Some(1), &[]).is_none());
    }

    #[test]
    fn a_one_word_edit_of_a_long_line_is_not_a_duplicate_but_a_spacing_twin_is() {
        let filler = "The committee reviewed the proposal in detail across the whole afternoon session, weighing the budget, the staffing plan, the dependency on the platform team, the launch window and the support rota before reaching a position on it, and the chair asked that the decision be recorded in the minutes together with the reasoning so that the outcome is";
        let approved = format!("{filler} approved for the next quarter and nobody has to ask again.");
        let rejected = format!("{filler} rejected for the next quarter and nobody has to ask again.");
        assert!(approved.chars().count() >= 370, "{}", approved.chars().count());
        assert!(dedup::similarity(&approved, &rejected) >= dedup::NEAR_DUP_THRESHOLD, "the pair must be a bigram twin for the test to mean anything");
        let spaced = approved.replace(", ", ",   ").replace(". ", ".  ");
        // Case and one punctuation mark: a bigram twin with the same words.
        let repunctuated = approved.replace("again.", "again!").to_uppercase();
        assert!(dedup::similarity(&approved, &repunctuated) >= dedup::NEAR_DUP_THRESHOLD);
        let events = vec![ev(1, &approved), ev(2, &rejected), ev(3, &spaced), ev(4, &repunctuated)];
        let m = vec![
            capture(1, HOUR, "com.notes", "Minutes"),
            capture(2, HOUR + 1_000, "com.notes", "Minutes"),
            capture(3, HOUR + 2_000, "com.notes", "Minutes"),
            meta(4, HOUR + 3_000, "capture", "text_delta", Some("com.notes"), Some("Minutes")),
        ];
        let (sessions, stats) = build_sessions(&events, &m);
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        let ids: Vec<i64> = s.lines.iter().map(|l| l.event_id).collect();
        assert_eq!(ids, vec![1, 2], "approved and rejected both stay; the spacing and punctuation twins fold");
        // The whitespace twin is an exact duplicate *item* the per-item pre-filter already drops;
        // the punctuation/case twin reaches the session and folds there.
        assert_eq!((stats.dropped_duplicate, stats.session_lines_deduped), (1, 1), "{stats:?}");
        // The kept first copy inherits `changed` from the dropped delta twin, so the cap keeps it.
        assert!(s.lines[0].changed);
        assert!(!s.lines[1].changed);
        // Same shape, one word swapped: yes/no and Friday/Monday are different facts too.
        for (a, b) in [("yes", "no"), ("Friday", "Monday")] {
            let la = format!("{filler} {a}, as discussed with everyone at length today.");
            let lb = format!("{filler} {b}, as discussed with everyone at length today.");
            let events = vec![ev(1, &la), ev(2, &lb)];
            let (sessions, _) = build_sessions(&events, &m[..2]);
            assert_eq!(sessions[0].lines.len(), 2, "{a}/{b} merged");
        }
    }

    #[test]
    fn an_exact_delta_copy_of_a_checkpoint_line_marks_the_kept_line_changed() {
        // A delta row that repeats a checkpoint line and adds one. (A delta that repeats only
        // what the previous item showed never reaches the session: the per-item pre-filter drops
        // it first.)
        let promise = "Alice: I'll send the final version of the deck to the whole team on Friday.";
        let events = vec![
            ev(1, &format!("Project inbox\n{promise}")),
            ev(2, &format!("{promise}\nCarol: can we move the review to Monday?")),
        ];
        let m = vec![
            capture(1, HOUR, "com.mail", "Inbox"),
            meta(2, HOUR + 1_000, "capture", "text_delta", Some("com.mail"), Some("Inbox")),
        ];
        let (sessions, stats) = build_sessions(&events, &m);
        let s = &sessions[0];
        assert_eq!(s.lines.len(), 3, "{:?}", s.lines.iter().map(|l| &l.sent).collect::<Vec<_>>());
        assert_eq!(stats.session_lines_deduped, 1);
        assert_eq!(s.lines[1].event_id, 1, "the first copy is kept");
        assert!(s.lines[1].changed, "and carries the delta's priority");
        assert!(!s.lines[0].changed);
        assert!(s.lines[2].changed);
    }

    #[test]
    fn volatile_counters_in_a_title_do_not_split_a_session() {
        let same = |a: &str, b: &str| assert_eq!(session_title(a), session_title(b), "{a:?} vs {b:?}");
        same("Inbox (12) - me@x.com - Gmail", "Inbox (13) - me@x.com - Gmail");
        assert_eq!(session_title("Inbox (12) - me@x.com - Gmail"), "inbox - me@x.com");
        same("Export 43%", "Export 44%");
        assert_eq!(session_title("Export 43%"), "export");
        same("(1,204) Inbox (3) — Mail", "(1,310) Inbox (4) — Mail");
        same("Zoom Meeting 12:03", "Zoom Meeting 12:04");
        same("Recording 1:02:15 — QuickTime", "Recording 1:02:16 — QuickTime");
        same("Building… 7% — Xcode", "Building… 99% — Xcode");
        // Digits that name the window stay.
        assert_eq!(session_title("Invoice 1024 — Numbers"), "invoice 1024");
        assert_eq!(session_title("Q3 planning"), "q3 planning");
        assert_eq!(session_title("Issue #302 (draft)"), "issue #302 (draft)");
        assert_eq!(session_title("Room 12:3"), "room 12:3", "not a clock");
        // Through the key.
        let a = SessionKey::from_meta(&capture(1, HOUR, "com.google.Chrome", "Inbox (12) - me@x.com - Gmail"));
        let b = SessionKey::from_meta(&capture(2, HOUR + 5_000, "com.google.Chrome", "Inbox (13) - me@x.com - Gmail"));
        assert_eq!(a, b);
        assert_eq!(session_custom_id(&a), session_custom_id(&b));
    }

    /// L0(b): with mail triage on, `gmail` rows are Stage B's and leave the consolidation lane —
    /// counted, not silently gone; with triage off they stay, so mail commitments are still
    /// extracted somewhere. The decision is the policy's, not the static source filter's.
    #[test]
    fn the_mail_source_leaves_the_lane_only_while_triage_covers_it() {
        let events = vec![
            ev(1, "Alice: I'll send the deck on Friday."),
            ev(2, "From: Oren Vale\nI will send you the signed contract tomorrow."),
            ev(3, "Carol: the review is booked for Monday."),
        ];
        let m = vec![
            capture(1, HOUR + 1_000, "com.mail", "Re: deck — Mail"),
            meta(2, HOUR + 2_000, MAIL_SOURCE, "email", None, Some("Contract")),
            capture(3, HOUR + 3_000, "com.mail", "Re: deck — Mail"),
        ];
        let on = SessionPolicy::for_mail_triage(true);
        assert_eq!(on.skip_sources, vec![MAIL_SOURCE.to_string()]);
        let (sessions, stats) = build_sessions_with_policy(&events, &m, &on);
        assert_eq!(sessions.len(), 1, "the gmail session is gone");
        assert_eq!(sessions[0].event_ids(), vec![1, 3]);
        assert_eq!(stats.input, 3);
        assert_eq!(stats.rows_dropped_covered, 1);
        assert_eq!(stats.rows_in_sessions, 2);
        assert_eq!(stats.rows_in_sessions + stats.dropped() + stats.rows_dropped_shadowed + stats.rows_dropped_covered, stats.input);

        let off = SessionPolicy::for_mail_triage(false);
        assert_eq!(off, SessionPolicy::default());
        let (sessions, stats) = build_sessions_with_policy(&events, &m, &off);
        assert_eq!(sessions.len(), 2);
        assert!(sessions.iter().any(|s| s.key.app == MAIL_SOURCE && s.event_ids() == vec![2]));
        assert_eq!(stats.rows_dropped_covered, 0);
        // The plain builder is the triage-off shape.
        assert_eq!(build_sessions(&events, &m), (sessions, stats));
    }

    /// Lines built from one text (the Stage B excerpt) resolve under the session contract: the
    /// quote is raw text of the event, redaction never reaches it, `[n]` is 1-based.
    #[test]
    fn lines_from_text_carry_the_event_and_resolve_like_a_session() {
        let lines = lines_from_text(42, "Hi there,\n\n  I'll  send the signed  contract by Friday.  \nCard 4111 1111 1111 1111 attached.\n");
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().all(|l| l.event_id == 42 && !l.changed));
        assert_eq!(lines[1].sent, "I'll send the signed contract by Friday.");
        assert_eq!(lines[1].raw, "I'll  send the signed  contract by Friday.");
        assert!(!lines[2].sent.contains("4111"), "{}", lines[2].sent);
        let session = BatchSession {
            custom_id: "mt-test".into(),
            key: SessionKey { app: MAIL_SOURCE.into(), title: String::new(), hour: 0 },
            header: String::new(),
            lines,
        };
        assert!(session.body().starts_with("[1] Hi there,\n[2] I'll send"));
        assert!(!session.body().contains("Window:"));
        let hit = session.resolve_anchor("I'll send the signed contract by Friday.", Some(41), Some(2), &[]).unwrap();
        assert_eq!((hit.event_id, hit.line), (42, 2));
        assert_eq!(hit.quote, "I'll  send the signed  contract by Friday.", "the raw line, whitespace and all");
        // A span through redacted text does not resolve.
        assert!(session.resolve_anchor("Card [redacted] attached.", Some(30), Some(3), &[]).is_none());
    }
}
