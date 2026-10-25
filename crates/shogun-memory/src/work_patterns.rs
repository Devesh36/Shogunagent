//! Continual learning v2 / Work Patterns (Issue #242, FR-PAT-03..07).
//!
//! Two halves:
//! - **offer_impressions** — the presentation log (FR-PAT-04). "The user ignored this" is only
//!   derivable as presented − decided, and a CTR needs a denominator. Metadata only: an action
//!   kind, a surface, a slot, a bundle id. Never content. The writer purges rows older than the
//!   30-day retention on every batch, so the table cannot grow unbounded.
//! - **work_patterns** — the numeric behavior profile (FR-PAT-03): activity / focus / rhythm /
//!   avoid rows, recomputed nightly by the distillation job (P1), shown in full by the Patterns
//!   UI (FR-PAT-06), consumed only for offer selection and ranking (FR-PAT-05).
//!
//! Nothing in this module logs row content, and none of these tables have an egress path
//! (FR-PAT-07: not even aggregates reach telemetry).

use rusqlite::{params, Connection, OptionalExtension};

/// Impressions are kept this long (FR-PAT-04). Old rows carry no decision that isn't already
/// reflected in the profile, and the log is high-volume by design.
pub const IMPRESSION_RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// The 7-day aggregation window shared by the overview and (in P1) the distillation job.
pub const WEEK_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// One offered candidate: its kind and the slot it sat in (`None` on unranked surfaces).
pub type OfferedKind<'a> = (&'a str, Option<i64>);

/// Record one surface's worth of offered candidates, and purge expired rows in the same
/// transaction — the writer is the retention enforcement point, so retention cannot be forgotten
/// by a missing maintenance hook.
pub fn record_impressions(
    conn: &Connection,
    ts_ms: i64,
    surface: &str,
    context_app: Option<&str>,
    offered: &[OfferedKind<'_>],
) -> Result<(), rusqlite::Error> {
    if offered.is_empty() {
        return Ok(());
    }
    conn.execute(
        "DELETE FROM offer_impressions WHERE ts_ms < ?1",
        [ts_ms - IMPRESSION_RETENTION_MS],
    )?;
    let mut stmt = conn.prepare(
        "INSERT INTO offer_impressions (ts_ms, action_kind, surface, rank, context_app)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for (kind, rank) in offered {
        stmt.execute(params![ts_ms, kind, surface, rank, context_app])?;
    }
    Ok(())
}

/// One activity line of the Patterns overview: offered vs decided vs adopted, last 7 days.
/// CTR is left to the presentation layer as adopted/shown — returning counts, not a ratio,
/// keeps thin evidence visible (same reasoning as [`crate::lessons::acceptance_by_kind`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityAgg {
    pub kind: String,
    pub shown_7d: i64,
    pub decided_7d: i64,
    pub adopted_7d: i64,
}

/// The live activity overview (P0's read-only Patterns card, and the CTR metrics supply):
/// impressions joined with decisions per action kind over the last 7 days, most-shown first.
/// Computed on read — `work_patterns` rows are the distilled nightly view; this is today's.
pub fn activity_overview(
    conn: &Connection,
    now_ms: i64,
) -> Result<Vec<ActivityAgg>, rusqlite::Error> {
    let since = now_ms - WEEK_MS;
    let mut by_kind: Vec<ActivityAgg> = Vec::new();
    let find = |rows: &mut Vec<ActivityAgg>, kind: &str| -> usize {
        if let Some(i) = rows.iter().position(|a| a.kind == kind) {
            i
        } else {
            rows.push(ActivityAgg {
                kind: kind.to_string(),
                shown_7d: 0,
                decided_7d: 0,
                adopted_7d: 0,
            });
            rows.len() - 1
        }
    };

    {
        let mut stmt = conn.prepare(
            "SELECT action_kind, count(*) FROM offer_impressions
             WHERE ts_ms >= ?1 GROUP BY action_kind",
        )?;
        let shown = stmt.query_map([since], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        for row in shown {
            let (kind, n) = row?;
            let i = find(&mut by_kind, &kind);
            by_kind[i].shown_7d = n;
        }
    }

    for (kind, decided, adopted) in crate::lessons::acceptance_by_kind(conn, since)? {
        let i = find(&mut by_kind, &kind);
        by_kind[i].decided_7d = decided;
        by_kind[i].adopted_7d = adopted;
    }

    by_kind.sort_by(|a, b| b.shown_7d.cmp(&a.shown_7d).then(a.kind.cmp(&b.kind)));
    Ok(by_kind)
}

/// One distilled profile row, as the Patterns UI and the API face render it.
#[derive(Debug, Clone, PartialEq)]
pub struct PatternRow {
    pub id: i64,
    pub kind: String,
    pub key: String,
    pub scope: String,
    pub scope_ref: Option<String>,
    pub count_7d: i64,
    pub count_30d: i64,
    pub trend: f64,
    pub confidence: f64,
    pub active: bool,
    pub last_seen_at: i64,
}

/// Every profile row, strongest first within each kind. The Patterns UI shows ALL of them —
/// the profile must never be a black box (FR-PAT-06).
pub fn list_patterns(conn: &Connection) -> Result<Vec<PatternRow>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT id, kind, key, scope, scope_ref, count_7d, count_30d, trend, confidence, active,
                last_seen_at
         FROM work_patterns ORDER BY kind, count_7d DESC, id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(PatternRow {
            id: r.get(0)?,
            kind: r.get(1)?,
            key: r.get(2)?,
            scope: r.get(3)?,
            // '' is the stored "no ref" (NULL would break the upsert's UNIQUE target).
            scope_ref: r.get::<_, String>(4).map(|s| (!s.is_empty()).then_some(s))?,
            count_7d: r.get(5)?,
            count_30d: r.get(6)?,
            trend: r.get(7)?,
            confidence: r.get(8)?,
            active: r.get::<_, i64>(9)? != 0,
            last_seen_at: r.get(10)?,
        })
    })?;
    rows.collect()
}

/// The user's per-row switch (FR-PAT-06): an OFF pattern stays visible but is never consumed.
/// Distillation must respect it (P1 keeps `active` across recomputes).
pub fn set_pattern_active(
    conn: &Connection,
    id: i64,
    active: bool,
    now_ms: i64,
) -> Result<bool, rusqlite::Error> {
    let n = conn.execute(
        "UPDATE work_patterns SET active = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, active as i64, now_ms],
    )?;
    Ok(n > 0)
}

/// Remove one profile row entirely ("that's not me"). The next distillation may re-derive it
/// from fresh behavior — deletion erases the row, not the underlying events; a pattern the user
/// wants silenced durably is `set_pattern_active(false)`.
pub fn delete_pattern(conn: &Connection, id: i64) -> Result<bool, rusqlite::Error> {
    let n = conn.execute("DELETE FROM work_patterns WHERE id = ?1", [id])?;
    Ok(n > 0)
}

// time bands (rhythm)

/// The closed band vocabulary (FR-PAT-03). A band label is `scope_ref` on rhythm rows.
pub const BAND_NAMES: &[&str] = &["early", "morning", "afternoon", "evening", "night"];

/// Local hour → band. Total over 0..=23; anything outside the day bands is night.
pub fn band_of(local_hour: u32) -> &'static str {
    match local_hour {
        5..=8 => "early",
        9..=11 => "morning",
        12..=16 => "afternoon",
        17..=20 => "evening",
        _ => "night",
    }
}

// distillation (nightly)

/// Adoptions per week an activity needs before it becomes a row (design §4-1).
pub const MIN_ACTIVITY_7D: i64 = 5;
/// A rhythm row exists only when one band holds at least this share of the activity (§4-3):
/// "does it morning AND night" is not a rhythm.
pub const RHYTHM_MIN_SHARE: f64 = 0.5;
/// Focus rows kept per distillation (§4-2).
pub const FOCUS_TOP_K: usize = 5;
/// A title-derived focus label needs at least this share of the week's titled dwell (§4-2).
pub const FOCUS_MIN_DWELL_SHARE: f64 = 0.05;
/// Consecutive un-adopted impressions of one (kind × app) before it is off-target (§4-4).
pub const AVOID_MIN_IMPRESSIONS: i64 = 5;
/// The window avoid evidence is read from, and the consumption-side cooldown: a suppression
/// whose evidence is older than this lapses, so the candidate gets one more chance (FR-PAT-05b).
pub const AVOID_WINDOW_MS: i64 = 14 * 24 * 60 * 60 * 1000;

const DAY_MS: i64 = 24 * 60 * 60 * 1000;
const MONTH_MS: i64 = 30 * DAY_MS;

fn adoption_kinds_sql() -> String {
    crate::lessons::ALL_FEEDBACK_KINDS
        .iter()
        .copied()
        .filter(|k| k.is_adoption())
        .map(|k| format!("'{}'", k.as_str()))
        .collect::<Vec<_>>()
        .join(", ")
}

fn local_hour(ts_ms: i64, gmt_offset_secs: i32) -> u32 {
    let local_secs = ts_ms.div_euclid(1000) + i64::from(gmt_offset_secs);
    (local_secs.rem_euclid(86_400) / 3_600) as u32
}

fn local_day(ts_ms: i64, gmt_offset_secs: i32) -> i64 {
    (ts_ms.div_euclid(1000) + i64::from(gmt_offset_secs)).div_euclid(86_400)
}

/// One row the current distillation derived.
struct Derived {
    kind: &'static str,
    key: String,
    scope: &'static str,
    scope_ref: Option<String>,
    count_7d: i64,
    count_30d: i64,
    last_seen_at: i64,
}

/// Trend against the 30-day baseline: (this week − expected week) / expected week, clamped to
/// [-1, 1]. Deterministic from the two counts — no history table needed for "up 2× this week".
fn trend_of(count_7d: i64, count_30d: i64) -> f64 {
    let expected = (count_30d as f64 / 4.29).max(1.0);
    ((count_7d as f64 - expected) / expected).clamp(-1.0, 1.0)
}

/// Split a window title into candidate focus labels: segments between common title separators,
/// 3..=40 chars, not purely numeric/URL-ish. "ShogunAI — daemon.rs" yields both halves.
fn title_labels(title: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in title.split([
        '\u{2014}', // —
        '\u{2013}', // –
        '|', '·', ':',
    ]) {
        for piece in raw.split(" - ") {
            let label = piece.trim();
            let n = label.chars().count();
            if !(3..=40).contains(&n) {
                continue;
            }
            if label.chars().all(|c| c.is_ascii_digit() || c.is_ascii_punctuation()) {
                continue;
            }
            if label.contains("://") {
                continue;
            }
            if !out.iter().any(|have: &String| have.eq_ignore_ascii_case(label)) {
                out.push(label.to_string());
            }
        }
    }
    out
}

/// The nightly recompute (FR-PAT-03): derive activity / rhythm / focus / avoid rows from the
/// last 30 days and REPLACE the numeric profile with them. Never accumulates:
/// - a derived row upserts (counts, trend, confidence, last_seen_at) and **preserves `active`**
///   and `created_at` — the user's OFF switch survives every recompute;
/// - an avoid row that no longer qualifies (an adoption happened, or the ignoring stopped) is
///   deleted — suppression lifts the moment the evidence does (§4-4 "1採択で即削除");
/// - an activity/focus/rhythm row no longer derived is deleted when active (the profile tracks
///   current behavior) but KEPT with zeroed counts when the user switched it off (their decision
///   outlives the behavior).
///
/// Returns how many rows the profile now holds.
pub fn distill(
    conn: &mut Connection,
    now_ms: i64,
    gmt_offset_secs: i32,
) -> Result<usize, rusqlite::Error> {
    let tx = conn.savepoint()?;
    let week_ago = now_ms - WEEK_MS;
    let month_ago = now_ms - MONTH_MS;
    let mut derived: Vec<Derived> = Vec::new();

    // activity + rhythm: adoption events by kind (and by local time band)
    {
        let adoptions = adoption_kinds_sql();
        let mut stmt = tx.prepare(&format!(
            "SELECT action_kind, ts_ms FROM feedback_events
             WHERE ts_ms >= ?1 AND action_kind IS NOT NULL AND kind IN ({adoptions})"
        ))?;
        let rows: Vec<(String, i64)> = stmt
            .query_map([month_ago], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;

        use std::collections::BTreeMap;
        /// count_7d, count_30d, last_ts, band → count_7d.
        type ActAgg = (i64, i64, i64, std::collections::BTreeMap<&'static str, i64>);
        let mut acts: BTreeMap<String, ActAgg> = BTreeMap::new();
        for (kind, ts) in rows {
            let entry = acts.entry(kind).or_insert((0, 0, 0, BTreeMap::new()));
            entry.1 += 1;
            entry.2 = entry.2.max(ts);
            if ts >= week_ago {
                entry.0 += 1;
                *entry.3.entry(band_of(local_hour(ts, gmt_offset_secs))).or_insert(0) += 1;
            }
        }
        for (kind, (c7, c30, last, bands)) in acts {
            if c7 < MIN_ACTIVITY_7D {
                continue;
            }
            if let Some((band, band_count)) = bands.iter().max_by_key(|(_, n)| **n) {
                // A rhythm exists only when one band dominates (§4-3).
                if *band_count >= MIN_ACTIVITY_7D
                    && (*band_count as f64) / (c7 as f64) >= RHYTHM_MIN_SHARE
                {
                    derived.push(Derived {
                        kind: "rhythm",
                        key: kind.clone(),
                        scope: "band",
                        scope_ref: Some((*band).to_string()),
                        count_7d: *band_count,
                        count_30d: c30,
                        last_seen_at: last,
                    });
                }
            }
            derived.push(Derived {
                kind: "activity",
                key: kind,
                scope: "global",
                scope_ref: None,
                count_7d: c7,
                count_30d: c30,
                last_seen_at: last,
            });
        }
    }

    // focus: projects (evidence-linked events) > threads > window-title labels
    {
        use std::collections::BTreeMap;
        // lowercase label → (display, priority, count_7d, count_30d, last_ts)
        let mut focus: BTreeMap<String, (String, u8, i64, i64, i64)> = BTreeMap::new();
        let add = |focus: &mut BTreeMap<String, (String, u8, i64, i64, i64)>,
                       display: &str,
                       priority: u8,
                       c7: i64,
                       c30: i64,
                       last: i64| {
            let key = display.to_lowercase();
            let entry = focus
                .entry(key)
                .or_insert_with(|| (display.to_string(), priority, 0, 0, 0));
            if priority > entry.1 {
                entry.0 = display.to_string();
                entry.1 = priority;
            }
            entry.2 += c7;
            entry.3 += c30;
            entry.4 = entry.4.max(last);
        };

        let mut stmt = tx.prepare(
            "SELECT p.name,
                    sum(CASE WHEN e.ts >= ?2 THEN 1 ELSE 0 END),
                    count(*),
                    max(e.ts)
             FROM projects p
             JOIN state_provenance sp ON sp.state_table = 'projects' AND sp.state_id = p.id
             JOIN event_log e ON e.id = sp.event_id
             WHERE e.ts >= ?1 GROUP BY p.id",
        )?;
        let projects: Vec<(String, i64, i64, i64)> = stmt
            .query_map([month_ago, week_ago], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<Result<_, _>>()?;
        for (name, c7, c30, last) in projects {
            if c7 > 0 {
                add(&mut focus, &name, 2, c7, c30, last);
            }
        }

        let mut stmt = tx.prepare(
            "SELECT t.title,
                    sum(CASE WHEN e.ts >= ?2 THEN 1 ELSE 0 END),
                    count(*),
                    max(e.ts)
             FROM threads t
             JOIN event_log e ON e.thread_key = t.thread_key
             WHERE t.title IS NOT NULL AND e.ts >= ?1 GROUP BY t.id",
        )?;
        let threads: Vec<(String, i64, i64, i64)> = stmt
            .query_map([month_ago, week_ago], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<Result<_, _>>()?;
        for (title, c7, c30, last) in threads {
            if c7 > 0 {
                add(&mut focus, &title, 1, c7, c30, last);
            }
        }

        // Titles: repeated labels weighted by dwell. A label must recur across days and carry a
        // real share of the week's titled attention — one long read of an article is not a focus.
        let mut stmt = tx.prepare(
            "SELECT window_title, dwell_ms, ts FROM event_log
             WHERE ts >= ?1 AND window_title IS NOT NULL AND window_title != ''",
        )?;
        let titled: Vec<(String, i64, i64)> = stmt
            .query_map([month_ago], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<_, _>>()?;
        /// display, dwell_7d, captures_7d, captures_30d, day set, last_ts — keyed by lowercase label.
        type LabelAgg = (String, i64, i64, i64, std::collections::BTreeSet<i64>, i64);
        let mut labels: BTreeMap<String, LabelAgg> = BTreeMap::new();
        let mut total_dwell_7d: i64 = 0;
        for (title, dwell, ts) in &titled {
            if *ts >= week_ago {
                total_dwell_7d += dwell.max(&0);
            }
            for label in title_labels(title) {
                let entry = labels
                    .entry(label.to_lowercase())
                    .or_insert_with(|| (label, 0, 0, 0, std::collections::BTreeSet::new(), 0));
                entry.3 += 1;
                entry.5 = entry.5.max(*ts);
                if *ts >= week_ago {
                    entry.1 += dwell.max(&0);
                    entry.2 += 1;
                    entry.4.insert(local_day(*ts, gmt_offset_secs));
                }
            }
        }
        for (_, (display, dwell_7d, c7, c30, days, last)) in labels {
            if c7 == 0 || days.len() < 2 || total_dwell_7d == 0 {
                continue;
            }
            if (dwell_7d as f64) / (total_dwell_7d as f64) < FOCUS_MIN_DWELL_SHARE {
                continue;
            }
            add(&mut focus, &display, 0, c7, c30, last);
        }

        let mut ranked: Vec<(String, u8, i64, i64, i64)> = focus.into_values().collect();
        ranked.sort_by(|a, b| b.2.cmp(&a.2).then(b.1.cmp(&a.1)).then(a.0.cmp(&b.0)));
        for (display, _, c7, c30, last) in ranked.into_iter().take(FOCUS_TOP_K) {
            if c7 < MIN_ACTIVITY_7D {
                continue;
            }
            derived.push(Derived {
                kind: "focus",
                key: display,
                scope: "global",
                scope_ref: None,
                count_7d: c7,
                count_30d: c30,
                last_seen_at: last,
            });
        }
    }

    // avoid: (kind × app) offered AVOID_MIN_IMPRESSIONS times with zero adoptions
    {
        let avoid_since = now_ms - AVOID_WINDOW_MS;
        let mut stmt = tx.prepare(
            "SELECT action_kind, context_app, count(*), max(ts_ms) FROM offer_impressions
             WHERE ts_ms >= ?1 AND context_app IS NOT NULL
             GROUP BY action_kind, context_app HAVING count(*) >= ?2",
        )?;
        let pairs: Vec<(String, String, i64, i64)> = stmt
            .query_map(params![avoid_since, AVOID_MIN_IMPRESSIONS], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<Result<_, _>>()?;
        let adoptions = adoption_kinds_sql();
        for (kind, app, shown, last) in pairs {
            // The window the "consecutive" reading needs: since the AVOID_MIN_IMPRESSIONS-th
            // most recent impression of this pair.
            let since_nth: i64 = tx.query_row(
                "SELECT ts_ms FROM offer_impressions
                 WHERE action_kind = ?1 AND context_app = ?2
                 ORDER BY ts_ms DESC LIMIT 1 OFFSET ?3",
                params![kind, app, AVOID_MIN_IMPRESSIONS - 1],
                |r| r.get(0),
            )?;
            let adopted: i64 = tx.query_row(
                &format!(
                    "SELECT count(*) FROM feedback_events
                     WHERE action_kind = ?1 AND ts_ms >= ?2 AND kind IN ({adoptions})"
                ),
                params![kind, since_nth],
                |r| r.get(0),
            )?;
            if adopted > 0 {
                continue; // one adoption clears the pair (§4-4)
            }
            derived.push(Derived {
                kind: "avoid",
                key: kind,
                scope: "app",
                scope_ref: Some(app),
                count_7d: shown,
                count_30d: shown,
                last_seen_at: last,
            });
        }
    }

    // write: upsert derived rows (preserving active), then reconcile the rest
    for d in &derived {
        let confidence = crate::recompute::corroborated_confidence(d.count_7d as f64);
        let trend = trend_of(d.count_7d, d.count_30d);
        tx.execute(
            "INSERT INTO work_patterns
               (kind, key, scope, scope_ref, count_7d, count_30d, trend, confidence, active,
                created_at, updated_at, last_seen_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1, ?9, ?9, ?10)
             ON CONFLICT (kind, key, scope, scope_ref) DO UPDATE SET
               count_7d = ?5, count_30d = ?6, trend = ?7, confidence = ?8,
               updated_at = ?9, last_seen_at = ?10",
            params![
                d.kind,
                d.key,
                d.scope,
                d.scope_ref.clone().unwrap_or_default(),
                d.count_7d,
                d.count_30d,
                trend,
                confidence,
                now_ms,
                d.last_seen_at,
            ],
        )?;
    }
    // Not derived this run: suppressions lift entirely; live behavior rows vanish with the
    // behavior; the user's OFF rows are kept (zeroed) — their decision outlives the numbers.
    tx.execute(
        "DELETE FROM work_patterns
         WHERE updated_at != ?1 AND (kind = 'avoid' OR active = 1)",
        [now_ms],
    )?;
    tx.execute(
        "UPDATE work_patterns SET count_7d = 0, count_30d = 0, trend = 0.0, updated_at = ?1
         WHERE updated_at != ?1",
        [now_ms],
    )?;

    let total: usize =
        tx.query_row("SELECT count(*) FROM work_patterns", [], |r| r.get::<_, i64>(0))? as usize;
    tx.commit()?;
    Ok(total)
}

// promotion (P2, §4-5)

/// Weekly adoptions before an activity pattern is stable enough to become a one-sentence
/// lesson (MIN_RULE_OCCURRENCES × 5, design §4-5).
pub const PROMOTE_MIN_COUNT_7D: i64 = 15;
/// Evidence rows linked per promoted lesson (bounded — provenance, not an archive).
const PROMOTE_MAX_EVIDENCE: usize = 20;

/// The structural merge key for a promoted pattern lesson (`lessons.scope_ref`). Promotion merges
/// on this key, not on instruction text, so a Batch-lane rewording (P3) survives the nightly
/// re-promotion instead of spawning a duplicate. Global-scope injection filters use a `None`
/// scope_ref, which is a wildcard — so the key never hides the lesson from injection.
pub fn pattern_scope_ref(key: &str) -> String {
    format!("pattern:{key}")
}

/// What one promotion pass did to one pattern — the supply for the P3 wording pass.
#[derive(Debug, Clone, PartialEq)]
pub struct PromotedPattern {
    pub lesson_id: i64,
    /// The activity's `Action::kind()` key (snake_case).
    pub key: String,
    /// The dominant time band, when a live rhythm row agrees.
    pub band: Option<String>,
    /// True while the stored instruction is still the deterministic template — the one moment a
    /// Batch-lane phrase may replace it. False once phrased, so tonight's pass never overwrites
    /// last night's wording.
    pub needs_phrasing: bool,
}

/// The generation-facing sentence for a stable activity pattern. Deliberately count-free so the
/// same pattern re-promotes to the SAME instruction every night and merges (corroboration)
/// instead of accreting near-duplicates.
fn pattern_instruction(kind: &str, band: Option<&str>) -> String {
    let pretty = kind.replace('_', " ");
    match band {
        Some(band) => format!(
            "The user frequently does {pretty}, mostly in the {band}; anticipate it in suggestions and drafts."
        ),
        None => format!(
            "The user frequently does {pretty}; anticipate it in suggestions and drafts."
        ),
    }
}

/// Promote stable activity patterns to `lessons` rows (`kind='pattern'`), riding the existing
/// injection / Learned-UI / decay machinery (§5c). Only feedback-evidenced patterns promote —
/// a lesson without provenance cannot exist (repository discipline), so focus rows (event-log
/// evidence, no feedback ids) stay numeric-only for now.
///
/// Merge identity is the structural [`pattern_scope_ref`]: when a lesson for the key already
/// exists, tonight's candidate reuses its STORED instruction, so `upsert_lesson`'s text match
/// always succeeds — whether the row still carries the template or a Batch-lane phrase (P3).
///
/// Returns one [`PromotedPattern`] per upserted lesson.
pub fn promote_patterns_to_lessons(
    conn: &mut Connection,
    now_ms: i64,
) -> Result<Vec<PromotedPattern>, crate::MemoryError> {
    struct Promotable {
        key: String,
        band: Option<String>,
    }
    let promotable: Vec<Promotable> = {
        let mut stmt = conn.prepare(
            "SELECT a.key,
                    (SELECT r.scope_ref FROM work_patterns r
                      WHERE r.kind = 'rhythm' AND r.key = a.key AND r.active = 1 AND r.count_7d > 0
                      LIMIT 1)
             FROM work_patterns a
             WHERE a.kind = 'activity' AND a.active = 1 AND a.count_7d >= ?1",
        )?;
        let rows = stmt.query_map([PROMOTE_MIN_COUNT_7D], |r| {
            Ok(Promotable { key: r.get(0)?, band: r.get(1)? })
        })?;
        rows.collect::<Result<_, _>>()?
    };

    let adoptions = adoption_kinds_sql();
    let mut promoted = Vec::new();
    for p in promotable {
        let evidence: Vec<i64> = {
            let mut stmt = conn.prepare(&format!(
                "SELECT id FROM feedback_events
                 WHERE action_kind = ?1 AND ts_ms >= ?2 AND kind IN ({adoptions})
                 ORDER BY id DESC LIMIT ?3"
            ))?;
            let mut ids: Vec<i64> = stmt
                .query_map(params![p.key, now_ms - WEEK_MS, PROMOTE_MAX_EVIDENCE as i64], |r| {
                    r.get(0)
                })?
                .collect::<Result<_, _>>()?;
            ids.sort_unstable();
            ids
        };
        if evidence.is_empty() {
            continue; // no provenance, no lesson — never invented
        }
        let scope_ref = pattern_scope_ref(&p.key);
        let template = pattern_instruction(&p.key, p.band.as_deref());
        // Reuse the stored wording when the lesson already exists (see the fn doc). A stored
        // instruction that differs from tonight's template is a Batch phrase — keep it.
        let stored: Option<String> = conn
            .query_row(
                "SELECT instruction FROM lessons WHERE scope = 'global' AND scope_ref = ?1",
                [&scope_ref],
                |r| r.get(0),
            )
            .optional()?;
        let instruction = stored.clone().unwrap_or_else(|| template.clone());
        let needs_phrasing = instruction == template;
        let candidate = crate::lessons::LessonCandidate {
            kind: crate::lessons::LessonKind::Pattern,
            scope: crate::lessons::LessonScope::Global,
            scope_ref: Some(scope_ref),
            instruction,
            evidence: evidence.clone(),
        };
        let lesson_id = crate::lessons::upsert_lesson(conn, &candidate, &evidence, now_ms)?;
        promoted.push(PromotedPattern { lesson_id, key: p.key, band: p.band, needs_phrasing });
    }
    Ok(promoted)
}

// noticed moments (§5e)

/// Weekly count before a pattern is worth a noticed moment. Higher than the row threshold on
/// purpose: a notice interrupts, so it must be about something unmistakable.
pub const NOTICE_MIN_COUNT_7D: i64 = 10;

/// The one pattern eligible for a noticed moment: never noticed before, live, strong, and not an
/// avoid row (suppression is silent by design — announcing "I stopped suggesting X" invites
/// arguing with the ranking). Strongest first; one at a time.
pub fn pending_notice(conn: &Connection) -> Result<Option<PatternRow>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT id, kind, key, scope, scope_ref, count_7d, count_30d, trend, confidence, active,
                last_seen_at
         FROM work_patterns
         WHERE noticed_at IS NULL AND active = 1 AND kind != 'avoid' AND count_7d >= ?1
         ORDER BY count_7d DESC, id LIMIT 1",
    )?;
    let row = stmt
        .query_map([NOTICE_MIN_COUNT_7D], |r| {
            Ok(PatternRow {
                id: r.get(0)?,
                kind: r.get(1)?,
                key: r.get(2)?,
                scope: r.get(3)?,
                scope_ref: r.get::<_, String>(4).map(|s| (!s.is_empty()).then_some(s))?,
                count_7d: r.get(5)?,
                count_30d: r.get(6)?,
                trend: r.get(7)?,
                confidence: r.get(8)?,
                active: r.get::<_, i64>(9)? != 0,
                last_seen_at: r.get(10)?,
            })
        })?
        .next()
        .transpose()?;
    Ok(row)
}

/// Stamp a notice as shown: one notice per pattern, ever (§5e "気づき1件=提示1回").
pub fn mark_noticed(conn: &Connection, id: i64, now_ms: i64) -> Result<bool, rusqlite::Error> {
    let n = conn.execute(
        "UPDATE work_patterns SET noticed_at = ?2 WHERE id = ?1",
        params![id, now_ms],
    )?;
    Ok(n > 0)
}

/// The freshly-noticed pattern, if its stamp is at or after `since_ms` and it is still active —
/// what the Notch banner shows (with Undo in place) on the morning the Brief carried the notice.
/// An undone pattern (active = 0) drops out immediately.
pub fn recent_notice(
    conn: &Connection,
    since_ms: i64,
) -> Result<Option<PatternRow>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT id, kind, key, scope, scope_ref, count_7d, count_30d, trend, confidence, active,
                last_seen_at
         FROM work_patterns
         WHERE noticed_at >= ?1 AND active = 1 AND kind != 'avoid'
         ORDER BY noticed_at DESC, id LIMIT 1",
    )?;
    let row = stmt
        .query_map([since_ms], |r| {
            Ok(PatternRow {
                id: r.get(0)?,
                kind: r.get(1)?,
                key: r.get(2)?,
                scope: r.get(3)?,
                scope_ref: r.get::<_, String>(4).map(|s| (!s.is_empty()).then_some(s))?,
                count_7d: r.get(5)?,
                count_30d: r.get(6)?,
                trend: r.get(7)?,
                confidence: r.get(8)?,
                active: r.get::<_, i64>(9)? != 0,
                last_seen_at: r.get(10)?,
            })
        })?
        .next()
        .transpose()?;
    Ok(row)
}

/// What the ranking priors need from the profile (FR-PAT-05). Only `active` rows supply
/// anything — OFF is OFF.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PriorsSupply {
    /// Activity kinds and their weekly counts (habit prior).
    pub habit: Vec<(String, i64)>,
    /// Kinds whose rhythm band matches the current local hour (time-of-day prior).
    pub in_band: Vec<String>,
    /// Kinds suppressed for the given context app. Cooldown applied here: evidence older than
    /// [`AVOID_WINDOW_MS`] no longer suppresses, so the candidate gets one more chance.
    pub avoid: Vec<String>,
}

/// The profile as the candidate ranker consumes it.
pub fn priors_supply(
    conn: &Connection,
    now_ms: i64,
    local_hour_now: u32,
    context_app: Option<&str>,
) -> Result<PriorsSupply, rusqlite::Error> {
    let mut supply = PriorsSupply::default();
    {
        let mut stmt = conn.prepare(
            "SELECT key, count_7d FROM work_patterns
             WHERE kind = 'activity' AND active = 1 AND count_7d > 0",
        )?;
        supply.habit = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
    }
    {
        let mut stmt = conn.prepare(
            "SELECT key FROM work_patterns
             WHERE kind = 'rhythm' AND active = 1 AND count_7d > 0 AND scope_ref = ?1",
        )?;
        supply.in_band = stmt
            .query_map([band_of(local_hour_now)], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
    }
    if let Some(app) = context_app {
        let mut stmt = conn.prepare(
            "SELECT key FROM work_patterns
             WHERE kind = 'avoid' AND active = 1 AND scope_ref = ?1 AND last_seen_at >= ?2",
        )?;
        supply.avoid = stmt
            .query_map(params![app, now_ms - AVOID_WINDOW_MS], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
    }
    Ok(supply)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lessons::{record_feedback, FeedbackKind, LessonScope, NewFeedback, Surface};

    fn feedback(conn: &Connection, ts_ms: i64, kind: FeedbackKind, action_kind: &str) {
        record_feedback(
            conn,
            kind,
            LessonScope::Global,
            &NewFeedback {
                ts_ms,
                action_kind: Some(action_kind),
                scope_ref: None,
                before_text: None,
                after_text: None,
                surface: Some(Surface::Notch),
                rank: Some(0),
                context_app: None,
                latency_ms: None,
            },
        )
        .unwrap();
    }

    #[test]
    fn impressions_are_recorded_and_expired_rows_purged_on_write() {
        let conn = crate::open_in_memory().unwrap();
        let old = 1_000;
        record_impressions(&conn, old, "notch", Some("us.zoom.xos"), &[("open_app", Some(0))])
            .unwrap();
        let now = old + IMPRESSION_RETENTION_MS + 1;
        record_impressions(
            &conn,
            now,
            "notch",
            None,
            &[("local_search", Some(0)), ("open_app", Some(1))],
        )
        .unwrap();
        let n: i64 = conn
            .query_row("SELECT count(*) FROM offer_impressions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2, "the expired impression is gone, the fresh batch remains");
        let oldest: i64 = conn
            .query_row("SELECT min(ts_ms) FROM offer_impressions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(oldest, now);
    }

    #[test]
    fn the_overview_joins_shown_decided_and_adopted_per_kind() {
        let conn = crate::open_in_memory().unwrap();
        let now = 10 * 24 * 60 * 60 * 1000;
        // Shown 3× this week, adopted once, rejected once — CTR has a real denominator.
        for i in 0..3 {
            record_impressions(&conn, now - 1_000 - i, "notch", None, &[("send_email", Some(0))])
                .unwrap();
        }
        feedback(&conn, now - 500, FeedbackKind::ApproveUnchanged, "send_email");
        feedback(&conn, now - 400, FeedbackKind::Reject, "send_email");
        // Decided outside the window: not counted.
        feedback(&conn, now - WEEK_MS - 1, FeedbackKind::ApproveUnchanged, "send_email");

        let overview = activity_overview(&conn, now).unwrap();
        let email = overview.iter().find(|a| a.kind == "send_email").expect("row");
        assert_eq!(email.shown_7d, 3);
        assert_eq!(email.decided_7d, 2);
        assert_eq!(email.adopted_7d, 1);
    }

    #[test]
    fn a_kind_decided_but_never_shown_still_appears() {
        // The approval queue records decisions for proposals enqueued elsewhere; the overview
        // must not hide them just because no impression row exists.
        let conn = crate::open_in_memory().unwrap();
        let now = 10 * 24 * 60 * 60 * 1000;
        feedback(&conn, now - 500, FeedbackKind::ApproveUnchanged, "post_message");
        let overview = activity_overview(&conn, now).unwrap();
        let row = overview.iter().find(|a| a.kind == "post_message").expect("row");
        assert_eq!((row.shown_7d, row.decided_7d, row.adopted_7d), (0, 1, 1));
    }

    fn insert_pattern(conn: &Connection, kind: &str, key: &str, count_7d: i64) -> i64 {
        conn.execute(
            "INSERT INTO work_patterns
               (kind, key, scope, scope_ref, count_7d, count_30d, trend, confidence, active,
                created_at, updated_at, last_seen_at)
             VALUES (?1, ?2, 'global', '', ?3, ?3, 0.0, 0.6, 1, 1, 1, 1)",
            params![kind, key, count_7d],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn patterns_list_toggle_and_delete() {
        let conn = crate::open_in_memory().unwrap();
        let a = insert_pattern(&conn, "activity", "translate_ja_en", 31);
        let b = insert_pattern(&conn, "activity", "send_email", 4);
        insert_pattern(&conn, "focus", "ShogunAI", 62);

        let rows = list_patterns(&conn).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].key, "translate_ja_en", "strongest first within kind");
        assert_eq!(rows[1].key, "send_email");
        assert!(rows.iter().all(|r| r.active));

        assert!(set_pattern_active(&conn, b, false, 9).unwrap());
        let rows = list_patterns(&conn).unwrap();
        assert!(!rows.iter().find(|r| r.id == b).unwrap().active);
        assert!(rows.iter().find(|r| r.id == a).unwrap().active);

        assert!(delete_pattern(&conn, a).unwrap());
        assert!(!delete_pattern(&conn, a).unwrap(), "second delete is a no-op");
        assert_eq!(list_patterns(&conn).unwrap().len(), 2);
    }

    const DAY: i64 = 24 * 60 * 60 * 1000;

    fn titled_event(conn: &Connection, ts: i64, title: &str, dwell: i64) {
        let hash = format!("{title}-{ts}");
        crate::event_log::insert(
            conn,
            &crate::event_log::NewEvent {
                ts,
                source: "capture",
                kind: "ax_text",
                app_bundle_id: Some("com.apple.Safari"),
                window_title: Some(title),
                content: "…",
                content_hash: &hash,
                dwell_ms: dwell,
                display_id: None,
                window_bounds: None,
            },
        )
        .unwrap();
    }

    #[test]
    fn distill_derives_activity_rhythm_and_respects_thresholds() {
        let mut conn = crate::open_in_memory().unwrap();
        let now = 60 * DAY;
        // 6 morning adoptions of translate (band-dominant), 2 of send_email (below threshold).
        for i in 0..6 {
            // 10:00 local (UTC offset 0) on distinct days.
            let ts = now - i * DAY - (24 - 10) * 60 * 60 * 1000;
            feedback(&conn, ts, FeedbackKind::ApproveUnchanged, "translate_ja_en");
        }
        for i in 0..2 {
            feedback(&conn, now - i * DAY - 1000, FeedbackKind::ApproveUnchanged, "send_email");
        }
        distill(&mut conn, now, 0).unwrap();
        let rows = list_patterns(&conn).unwrap();
        let activity = rows.iter().find(|r| r.kind == "activity").expect("activity row");
        assert_eq!(activity.key, "translate_ja_en");
        assert_eq!(activity.count_7d, 6);
        assert!(activity.confidence <= 0.75, "offered, never asserted");
        assert!(
            !rows.iter().any(|r| r.key == "send_email"),
            "below MIN_ACTIVITY_7D is not a pattern"
        );
        let rhythm = rows.iter().find(|r| r.kind == "rhythm").expect("rhythm row");
        assert_eq!(rhythm.key, "translate_ja_en");
        assert_eq!(rhythm.scope_ref.as_deref(), Some("morning"));
    }

    #[test]
    fn a_spread_out_activity_has_no_rhythm() {
        let mut conn = crate::open_in_memory().unwrap();
        let now = 60 * DAY;
        // 6 adoptions alternating morning / night: no band reaches the 50% share… (3/6 = 50%
        // exactly qualifies, so use three bands).
        for (i, hour) in [10, 22, 14, 10, 22, 14].iter().enumerate() {
            let ts = now - (i as i64 + 1) * DAY + hour * 60 * 60 * 1000 - now.rem_euclid(DAY);
            feedback(&conn, ts, FeedbackKind::ApproveUnchanged, "translate_ja_en");
        }
        distill(&mut conn, now, 0).unwrap();
        let rows = list_patterns(&conn).unwrap();
        assert!(rows.iter().any(|r| r.kind == "activity"));
        assert!(
            !rows.iter().any(|r| r.kind == "rhythm"),
            "2/6 per band must not read as a rhythm: {rows:?}"
        );
    }

    #[test]
    fn distill_derives_focus_from_recurring_titles() {
        let mut conn = crate::open_in_memory().unwrap();
        let now = 60 * DAY;
        // "ShogunAI" recurs across 3 days with most of the dwell; a one-day article does not.
        for day in 1..=3 {
            for i in 0..3 {
                titled_event(&conn, now - day * DAY + i * 1000, "ShogunAI — daemon.rs", 60_000);
            }
        }
        titled_event(&conn, now - DAY, "Some long article", 5_000);
        distill(&mut conn, now, 0).unwrap();
        let rows = list_patterns(&conn).unwrap();
        let focus: Vec<&PatternRow> = rows.iter().filter(|r| r.kind == "focus").collect();
        assert!(focus.iter().any(|r| r.key == "ShogunAI"), "{focus:?}");
        assert!(!focus.iter().any(|r| r.key == "Some long article"), "one day is not a focus");
    }

    #[test]
    fn avoid_rows_come_from_ignored_offers_and_clear_on_adoption() {
        let mut conn = crate::open_in_memory().unwrap();
        let now = 60 * DAY;
        for i in 0..5 {
            record_impressions(
                &conn,
                now - (i + 1) * 60_000,
                "notch",
                Some("com.apple.dt.Xcode"),
                &[("open_app", Some(0))],
            )
            .unwrap();
        }
        distill(&mut conn, now, 0).unwrap();
        let rows = list_patterns(&conn).unwrap();
        let avoid = rows.iter().find(|r| r.kind == "avoid").expect("avoid row");
        assert_eq!(avoid.key, "open_app");
        assert_eq!(avoid.scope_ref.as_deref(), Some("com.apple.dt.Xcode"));

        // One adoption clears the pair on the next distillation (§4-4).
        feedback(&conn, now - 1_000, FeedbackKind::ApproveUnchanged, "open_app");
        distill(&mut conn, now + 1, 0).unwrap();
        assert!(
            !list_patterns(&conn).unwrap().iter().any(|r| r.kind == "avoid"),
            "an adoption lifts the suppression"
        );
    }

    #[test]
    fn recompute_preserves_the_off_switch_and_drops_stale_live_rows() {
        let mut conn = crate::open_in_memory().unwrap();
        let now = 60 * DAY;
        for i in 0..6 {
            feedback(&conn, now - i * DAY - 1000, FeedbackKind::ApproveUnchanged, "translate_ja_en");
        }
        distill(&mut conn, now, 0).unwrap();
        let id = list_patterns(&conn).unwrap().iter().find(|r| r.kind == "activity").unwrap().id;
        set_pattern_active(&conn, id, false, now).unwrap();

        // Re-distill with the same data: the OFF switch survives the recompute.
        distill(&mut conn, now + 1, 0).unwrap();
        let row = list_patterns(&conn)
            .unwrap()
            .into_iter()
            .find(|r| r.kind == "activity")
            .expect("row survives");
        assert!(!row.active, "the user's OFF outlives the recompute");

        // A month later the behavior is gone: the OFF row is kept (zeroed), and would-be live
        // rows for vanished behavior disappear entirely.
        let later = now + 35 * DAY;
        distill(&mut conn, later, 0).unwrap();
        let rows = list_patterns(&conn).unwrap();
        let kept = rows.iter().find(|r| r.kind == "activity").expect("OFF row kept");
        assert!(!kept.active);
        assert_eq!(kept.count_7d, 0);
    }

    #[test]
    fn priors_supply_reads_only_active_rows_and_applies_the_avoid_cooldown() {
        let conn = crate::open_in_memory().unwrap();
        let now = 60 * DAY;
        conn.execute(
            "INSERT INTO work_patterns (kind, key, scope, scope_ref, count_7d, count_30d, trend, confidence, active, created_at, updated_at, last_seen_at)
             VALUES ('activity', 'translate_ja_en', 'global', '', 12, 40, 0.2, 0.7, 1, 1, 1, 1),
                    ('activity', 'send_email', 'global', '', 8, 20, 0.0, 0.6, 0, 1, 1, 1),
                    ('rhythm', 'translate_ja_en', 'band', 'morning', 9, 30, 0.1, 0.6, 1, 1, 1, 1),
                    ('avoid', 'open_app', 'app', 'com.apple.dt.Xcode', 5, 5, 0.0, 0.6, 1, 1, 1, ?1),
                    ('avoid', 'reveal_file', 'app', 'com.apple.dt.Xcode', 5, 5, 0.0, 0.6, 1, 1, 1, ?2)",
            params![now - 1000, now - AVOID_WINDOW_MS - 1],
        )
        .unwrap();

        let supply = priors_supply(&conn, now, 10, Some("com.apple.dt.Xcode")).unwrap();
        assert_eq!(supply.habit, vec![("translate_ja_en".to_string(), 12)], "OFF rows excluded");
        assert_eq!(supply.in_band, vec!["translate_ja_en"], "10:00 is the morning band");
        assert_eq!(supply.avoid, vec!["open_app"], "stale avoid evidence lapses (cooldown)");

        let elsewhere = priors_supply(&conn, now, 22, Some("com.apple.Safari")).unwrap();
        assert!(elsewhere.in_band.is_empty(), "22:00 is night");
        assert!(elsewhere.avoid.is_empty(), "avoid is context-scoped");
    }

    #[test]
    fn stable_activity_patterns_promote_to_pattern_lessons_and_merge_across_nights() {
        let mut conn = crate::open_in_memory().unwrap();
        let now = 60 * DAY;
        for i in 0..16 {
            feedback(&conn, now - i * 3_600_000, FeedbackKind::ApproveUnchanged, "send_email");
        }
        distill(&mut conn, now, 0).unwrap();
        let promoted = promote_patterns_to_lessons(&mut conn, now).unwrap();
        assert_eq!(promoted.len(), 1);
        assert!(promoted[0].needs_phrasing, "a fresh lesson carries the template");
        let lessons = crate::lessons::active_lessons(&conn, &[], 50).unwrap();
        let pattern = lessons
            .iter()
            .find(|l| l.kind == crate::lessons::LessonKind::Pattern)
            .expect("pattern lesson");
        assert!(pattern.instruction.contains("send email"), "{}", pattern.instruction);
        assert!(
            !pattern.instruction.contains("16"),
            "count-free instruction so nightly re-promotion merges: {}",
            pattern.instruction
        );

        // The next night re-promotes the same pattern: same instruction, ONE lesson (merged).
        promote_patterns_to_lessons(&mut conn, now + DAY).unwrap();
        let lessons = crate::lessons::active_lessons(&conn, &[], 50).unwrap();
        assert_eq!(
            lessons
                .iter()
                .filter(|l| l.kind == crate::lessons::LessonKind::Pattern)
                .count(),
            1,
            "re-promotion merges, never accretes"
        );
    }

    #[test]
    fn a_below_threshold_activity_never_promotes() {
        let mut conn = crate::open_in_memory().unwrap();
        let now = 60 * DAY;
        for i in 0..6 {
            feedback(&conn, now - i * DAY - 1000, FeedbackKind::ApproveUnchanged, "send_email");
        }
        distill(&mut conn, now, 0).unwrap();
        assert!(promote_patterns_to_lessons(&mut conn, now).unwrap().is_empty());
    }

    #[test]
    fn a_batch_phrase_survives_the_nightly_re_promotion() {
        let mut conn = crate::open_in_memory().unwrap();
        let now = 60 * DAY;
        for i in 0..16 {
            feedback(&conn, now - i * 3_600_000, FeedbackKind::ApproveUnchanged, "send_email");
        }
        distill(&mut conn, now, 0).unwrap();
        let promoted = promote_patterns_to_lessons(&mut conn, now).unwrap();
        let id = promoted[0].lesson_id;

        let phrase = "You draft and send a lot of email; lean into that when suggesting actions.";
        assert!(crate::lessons::reword_lesson(&conn, id, phrase, now).unwrap());

        // Next night: same pattern promotes again. The structural scope_ref merge keeps ONE
        // lesson, the phrase is not clobbered back to the template, and the pass reports the
        // lesson as already phrased.
        let again = promote_patterns_to_lessons(&mut conn, now + DAY).unwrap();
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].lesson_id, id, "merged into the same lesson");
        assert!(!again[0].needs_phrasing, "already phrased — never re-worded nightly");
        let lessons = crate::lessons::active_lessons(&conn, &[], 50).unwrap();
        let pattern: Vec<_> = lessons
            .iter()
            .filter(|l| l.kind == crate::lessons::LessonKind::Pattern)
            .collect();
        assert_eq!(pattern.len(), 1);
        assert_eq!(pattern[0].instruction, phrase);
        assert_eq!(pattern[0].scope_ref.as_deref(), Some("pattern:send_email"));
    }

    #[test]
    fn reword_refuses_empty_and_runaway_text() {
        let mut conn = crate::open_in_memory().unwrap();
        let now = 60 * DAY;
        for i in 0..16 {
            feedback(&conn, now - i * 3_600_000, FeedbackKind::ApproveUnchanged, "send_email");
        }
        distill(&mut conn, now, 0).unwrap();
        let promoted = promote_patterns_to_lessons(&mut conn, now).unwrap();
        let id = promoted[0].lesson_id;
        let before = crate::lessons::active_lessons(&conn, &[], 50).unwrap()[0].instruction.clone();

        assert!(!crate::lessons::reword_lesson(&conn, id, "   ", now).unwrap());
        let runaway = "x".repeat(crate::lessons::REWORD_MAX_CHARS + 1);
        assert!(!crate::lessons::reword_lesson(&conn, id, &runaway, now).unwrap());
        let after = crate::lessons::active_lessons(&conn, &[], 50).unwrap()[0].instruction.clone();
        assert_eq!(before, after, "a refused reword keeps the honest template");
    }

    #[test]
    fn a_notice_is_taken_once_and_undo_removes_it_from_the_banner() {
        let mut conn = crate::open_in_memory().unwrap();
        let now = 60 * DAY;
        for i in 0..12 {
            feedback(&conn, now - i * 3_600_000, FeedbackKind::ApproveUnchanged, "send_email");
        }
        distill(&mut conn, now, 0).unwrap();

        let pending = pending_notice(&conn).unwrap().expect("strong new pattern is noticeable");
        assert_eq!(pending.key, "send_email");
        assert!(mark_noticed(&conn, pending.id, now).unwrap());
        assert!(pending_notice(&conn).unwrap().is_none(), "one notice per pattern, ever");

        // The banner sees it while fresh…
        assert!(recent_notice(&conn, now - 1000).unwrap().is_some());
        // …not after the window…
        assert!(recent_notice(&conn, now + 1).unwrap().is_none());
        // …and Undo (the OFF switch) removes it immediately.
        set_pattern_active(&conn, pending.id, false, now).unwrap();
        assert!(recent_notice(&conn, now - 1000).unwrap().is_none());

        // A recompute does not resurrect the notice: noticed_at survives the upsert.
        distill(&mut conn, now + 1, 0).unwrap();
        assert!(pending_notice(&conn).unwrap().is_none());
    }

    #[test]
    fn the_schema_rejects_an_invented_pattern_kind() {
        let conn = crate::open_in_memory().unwrap();
        let err = conn.execute(
            "INSERT INTO work_patterns
               (kind, key, scope, scope_ref, count_7d, count_30d, trend, confidence, active,
                created_at, updated_at, last_seen_at)
             VALUES ('vibe', 'x', 'global', '', 1, 1, 0.0, 0.5, 1, 1, 1, 1)",
            [],
        );
        assert!(err.is_err(), "CHECK must hold the vocabulary closed");
    }
}
