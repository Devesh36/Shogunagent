//! Bulk import / export of the voice dictionary (docs/meeting-gap-analysis-2026-09.md C2).
//!
//! A pasted spreadsheet selection, a CSV file or a Markdown table becomes user-confirmed terms
//! in one go, with a per-row report the UI can preview before committing. Parsing is pure and
//! tolerant about the container; the term validation itself is the same `NewVoiceTerm` path
//! every single-term write uses, so nothing an import can express escapes the usual checks.
//!
//! Matching on import is by canonical spelling within the same scope (case-insensitive after
//! NFKC): a re-import of an edited sheet updates rows rather than duplicating them.
//!
//! No automatic learning is involved: every imported row is something the user chose to paste
//! (`provenance = 'user'`, the only variant the schema admits).

use rusqlite::Connection;
use serde::Serialize;
use unicode_normalization::UnicodeNormalization;

use crate::voice_terms::{self, NewVoiceTerm, VoiceTerm, VoiceTermScope, MAX_VOICE_TERMS};

/// Header names understood in the first row, lowercased. Anything else makes the first row a
/// data row (positional columns: canonical, aliases, locale, scope, scope_ref, priority, enabled).
const CANONICAL_HEADERS: &[&str] = &[
    "canonical",
    "term",
    "word",
    "name",
    "用語",
    "単語",
    "正式名称",
];
const ALIASES_HEADERS: &[&str] = &["aliases", "alias", "variants", "読み", "別名", "揺れ"];
const LOCALE_HEADERS: &[&str] = &["locale", "language", "lang", "言語"];
const SCOPE_HEADERS: &[&str] = &["scope", "スコープ"];
const SCOPE_REF_HEADERS: &[&str] = &["scope_ref", "scope ref", "app", "bundle", "対象"];
const PRIORITY_HEADERS: &[&str] = &["priority", "優先度"];
const ENABLED_HEADERS: &[&str] = &["enabled", "on", "有効"];

/// Aliases inside one cell are separated by `;` (never `,` — that is the CSV delimiter — and
/// never `|` — that is the Markdown table delimiter).
const ALIAS_SEPARATOR: char = ';';

/// One row that parsed into a term, with the 1-based line it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedRow {
    pub line: usize,
    pub term: NewVoiceTerm,
}

/// What the text parsed into: rows ready to import, and the lines that were not terms (with a
/// reason each), in order. Blank lines and Markdown separator rows are silently skipped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedImport {
    pub rows: Vec<ParsedRow>,
    pub skipped: Vec<(usize, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Csv,
    Tsv,
    Markdown,
}

/// Detect the container by looking at the first non-blank line: a leading `|` is a Markdown
/// table, a tab is TSV, everything else is CSV (quoted fields allowed).
fn detect(text: &str) -> Format {
    let first = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    if first.starts_with('|') {
        Format::Markdown
    } else if first.contains('\t') {
        Format::Tsv
    } else {
        Format::Csv
    }
}

/// Split one CSV line into fields (RFC 4180 quoting: `"a,b"`, `""` for a literal quote).
fn split_csv(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if in_quotes => {
                if chars.peek() == Some(&'"') {
                    field.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            }
            '"' if field.trim().is_empty() => {
                field.clear();
                in_quotes = true;
            }
            ',' if !in_quotes => {
                fields.push(std::mem::take(&mut field));
            }
            _ => field.push(c),
        }
    }
    fields.push(field);
    fields.into_iter().map(|f| f.trim().to_string()).collect()
}

fn split_tsv(line: &str) -> Vec<String> {
    line.split('\t').map(|f| f.trim().to_string()).collect()
}

/// `| a | b |` → `["a", "b"]`; a separator row (`|---|---|`) → `None`.
fn split_markdown(line: &str) -> Option<Vec<String>> {
    let inner = line.trim().trim_start_matches('|').trim_end_matches('|');
    let cells: Vec<String> = inner.split('|').map(|c| c.trim().to_string()).collect();
    if cells
        .iter()
        .all(|c| !c.is_empty() && c.chars().all(|ch| matches!(ch, '-' | ':' | ' ')))
    {
        return None;
    }
    Some(cells)
}

#[derive(Debug, Clone, Copy, Default)]
struct Columns {
    canonical: Option<usize>,
    aliases: Option<usize>,
    locale: Option<usize>,
    scope: Option<usize>,
    scope_ref: Option<usize>,
    priority: Option<usize>,
    enabled: Option<usize>,
}

impl Columns {
    const POSITIONAL: Columns = Columns {
        canonical: Some(0),
        aliases: Some(1),
        locale: Some(2),
        scope: Some(3),
        scope_ref: Some(4),
        priority: Some(5),
        enabled: Some(6),
    };

    /// A header row is one whose cells name the canonical column; the other columns are then
    /// wherever the header puts them (missing ones simply are not imported).
    fn from_header(cells: &[String]) -> Option<Columns> {
        let lower: Vec<String> = cells.iter().map(|c| c.trim().to_lowercase()).collect();
        let find = |names: &[&str]| lower.iter().position(|c| names.contains(&c.as_str()));
        let canonical = find(CANONICAL_HEADERS)?;
        Some(Columns {
            canonical: Some(canonical),
            aliases: find(ALIASES_HEADERS),
            locale: find(LOCALE_HEADERS),
            scope: find(SCOPE_HEADERS),
            scope_ref: find(SCOPE_REF_HEADERS),
            priority: find(PRIORITY_HEADERS),
            enabled: find(ENABLED_HEADERS),
        })
    }
}

fn cell(cells: &[String], index: Option<usize>) -> Option<&str> {
    index
        .and_then(|i| cells.get(i))
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
}

fn parse_scope(value: &str) -> Result<VoiceTermScope, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "global" => Ok(VoiceTermScope::Global),
        "bundle" | "app" => Ok(VoiceTermScope::Bundle),
        "surface" => Ok(VoiceTermScope::Surface),
        other => Err(format!(
            "unknown scope \"{other}\" (global / bundle / surface)"
        )),
    }
}

fn parse_enabled(value: &str) -> Result<bool, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "1" | "true" | "yes" | "on" | "有効" => Ok(true),
        "0" | "false" | "no" | "off" | "無効" => Ok(false),
        other => Err(format!("unknown enabled value \"{other}\"")),
    }
}

fn row_to_term(cells: &[String], columns: &Columns) -> Result<NewVoiceTerm, String> {
    let canonical = cell(cells, columns.canonical)
        .ok_or("empty term")?
        .to_string();
    let aliases: Vec<String> = cell(cells, columns.aliases)
        .map(|a| {
            a.split(ALIAS_SEPARATOR)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let scope = parse_scope(cell(cells, columns.scope).unwrap_or(""))?;
    let priority = match cell(cells, columns.priority) {
        None => None,
        Some(raw) => Some(
            raw.parse::<i32>()
                .map_err(|_| format!("priority \"{raw}\" is not a number"))?,
        ),
    };
    let enabled = parse_enabled(cell(cells, columns.enabled).unwrap_or(""))?;
    Ok(NewVoiceTerm {
        canonical,
        aliases,
        locale: cell(cells, columns.locale).map(str::to_string),
        scope: Some(scope),
        scope_ref: cell(cells, columns.scope_ref).map(str::to_string),
        priority,
        enabled,
    })
}

/// Parse pasted text into terms. Pure; nothing is validated against the database here beyond
/// the row shape — the store's own validation runs at import and reports per row.
pub fn parse_table(text: &str) -> ParsedImport {
    let format = detect(text);
    let mut out = ParsedImport::default();
    let mut columns: Option<Columns> = None;
    for (index, raw) in text.lines().enumerate() {
        let line = index + 1;
        if raw.trim().is_empty() {
            continue;
        }
        let cells = match format {
            Format::Csv => split_csv(raw),
            Format::Tsv => split_tsv(raw),
            Format::Markdown => match split_markdown(raw) {
                Some(cells) => cells,
                None => continue,
            },
        };
        if cells.iter().all(|c| c.is_empty()) {
            continue;
        }
        let cols = match columns {
            Some(cols) => cols,
            None => {
                let detected = Columns::from_header(&cells);
                columns = Some(detected.unwrap_or(Columns::POSITIONAL));
                if detected.is_some() {
                    continue;
                }
                Columns::POSITIONAL
            }
        };
        match row_to_term(&cells, &cols) {
            Ok(term) => out.rows.push(ParsedRow { line, term }),
            Err(reason) => out.skipped.push((line, reason)),
        }
    }
    out
}

/// The outcome of one import: counts plus the rows that did not land, with the reason each.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ImportReport {
    pub created: usize,
    pub updated: usize,
    /// `(line, reason)` — parse failures and store refusals alike.
    pub skipped: Vec<(usize, String)>,
}

fn match_key(canonical: &str) -> String {
    canonical
        .trim()
        .nfkc()
        .flat_map(char::to_lowercase)
        .collect()
}

/// Import parsed rows: a row whose canonical spelling already exists in the same scope updates
/// that term (aliases, locale, priority, enabled replaced), any other row creates one. Each row is
/// its own transaction, so one bad row skips itself and the rest still land; the report says
/// which. Stops creating (but keeps updating) once the store's term cap would be exceeded.
pub fn import_voice_terms(
    conn: &mut Connection,
    parsed: &ParsedImport,
    now_ms: i64,
) -> Result<ImportReport, rusqlite::Error> {
    let mut report = ImportReport {
        skipped: parsed.skipped.clone(),
        ..Default::default()
    };
    let existing = voice_terms::list_voice_terms(conn)?;
    let mut by_key: Vec<(String, VoiceTermScope, Option<String>, i64)> = existing
        .iter()
        .map(|t| (match_key(&t.canonical), t.scope, t.scope_ref.clone(), t.id))
        .collect();
    let mut count = existing.len();
    for row in &parsed.rows {
        let key = match_key(&row.term.canonical);
        let scope = row.term.scope.unwrap_or(VoiceTermScope::Global);
        let scope_ref = row
            .term
            .scope_ref
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let hit = by_key
            .iter()
            .find(|(k, s, r, _)| *k == key && *s == scope && *r == scope_ref)
            .map(|(_, _, _, id)| *id);
        let outcome = match hit {
            Some(id) => {
                voice_terms::update_voice_term(conn, id, &row.term, now_ms).map(|updated| {
                    if updated.is_some() {
                        report.updated += 1;
                    }
                })
            }
            None => {
                if count >= MAX_VOICE_TERMS {
                    report
                        .skipped
                        .push((row.line, "voice dictionary capacity reached".to_string()));
                    continue;
                }
                voice_terms::create_voice_term(conn, &row.term, now_ms).map(|created| {
                    count += 1;
                    report.created += 1;
                    by_key.push((
                        key.clone(),
                        created.scope,
                        created.scope_ref.clone(),
                        created.id,
                    ));
                })
            }
        };
        if let Err(error) = outcome {
            report.skipped.push((row.line, store_reason(&error)));
        }
    }
    report.skipped.sort_by_key(|(line, _)| *line);
    Ok(report)
}

/// The store's validation message without the rusqlite wrapping — the part a person can act on.
fn store_reason(error: &rusqlite::Error) -> String {
    match error {
        rusqlite::Error::InvalidParameterName(message) => message.clone(),
        other => other.to_string(),
    }
}

/// The dictionary as CSV, in the same columns [`parse_table`] reads back — a round trip is the
/// whole point (share a team vocabulary, edit it in a spreadsheet, re-import). Quoting is RFC
/// 4180; aliases are `;`-joined in one cell.
pub fn export_csv(terms: &[VoiceTerm]) -> String {
    fn quote(value: &str) -> String {
        if value.contains([',', '"', '\n', '\r']) {
            format!("\"{}\"", value.replace('"', "\"\""))
        } else {
            value.to_string()
        }
    }
    let mut out = String::from("canonical,aliases,locale,scope,scope_ref,priority,enabled\n");
    for term in terms {
        // The canonical spelling is stored as its own alias; the sheet lists only the others.
        let aliases: Vec<&str> = term
            .aliases
            .iter()
            .map(String::as_str)
            .filter(|a| match_key(a) != match_key(&term.canonical))
            .collect();
        out.push_str(&format!(
            "{},{},{},{},{},{},{}\n",
            quote(&term.canonical),
            quote(&aliases.join(&ALIAS_SEPARATOR.to_string())),
            quote(term.locale.as_deref().unwrap_or("")),
            term.scope.as_str(),
            quote(term.scope_ref.as_deref().unwrap_or("")),
            term.priority,
            if term.enabled { "true" } else { "false" },
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_with_a_header_maps_columns_by_name() {
        let text =
            "term,aliases,locale\nSelect KK,select kay kay;select k k,en\n\"Acme, Inc.\",acme,\n";
        let parsed = parse_table(text);
        assert!(parsed.skipped.is_empty(), "{:?}", parsed.skipped);
        assert_eq!(parsed.rows.len(), 2);
        assert_eq!(parsed.rows[0].line, 2);
        assert_eq!(parsed.rows[0].term.canonical, "Select KK");
        assert_eq!(
            parsed.rows[0].term.aliases,
            vec!["select kay kay", "select k k"]
        );
        assert_eq!(parsed.rows[0].term.locale.as_deref(), Some("en"));
        assert_eq!(
            parsed.rows[1].term.canonical, "Acme, Inc.",
            "quoted comma survives"
        );
        assert!(parsed.rows[1].term.aliases == vec!["acme"]);
    }

    #[test]
    fn a_headerless_sheet_is_positional_and_a_tab_paste_is_tsv() {
        let text = "Kamon\tkay mon;ka mon\t\tsurface\tmeeting\t5\tyes\nDeepgram\n";
        let parsed = parse_table(text);
        assert_eq!(parsed.rows.len(), 2);
        let first = &parsed.rows[0].term;
        assert_eq!(first.canonical, "Kamon");
        assert_eq!(first.aliases, vec!["kay mon", "ka mon"]);
        assert_eq!(first.scope, Some(VoiceTermScope::Surface));
        assert_eq!(first.scope_ref.as_deref(), Some("meeting"));
        assert_eq!(first.priority, Some(5));
        assert!(first.enabled);
        assert_eq!(parsed.rows[1].term.canonical, "Deepgram");
        assert!(parsed.rows[1].term.aliases.is_empty());
    }

    #[test]
    fn a_markdown_table_skips_its_separator_row() {
        let text =
            "| 用語 | 読み |\n|---|---|\n| 田中 | たなか |\n|  |  |\n| Nova-3 | nova three |\n";
        let parsed = parse_table(text);
        assert_eq!(parsed.rows.len(), 2);
        assert_eq!(parsed.rows[0].term.canonical, "田中");
        assert_eq!(parsed.rows[0].term.aliases, vec!["たなか"]);
        assert_eq!(parsed.rows[1].line, 5);
    }

    #[test]
    fn bad_rows_are_reported_with_their_line_and_the_rest_still_parse() {
        let text =
            "canonical,scope,priority\nGood,global,1\n,global,1\nOdd,planet,1\nSlow,global,fast\n";
        let parsed = parse_table(text);
        assert_eq!(parsed.rows.len(), 1);
        assert_eq!(parsed.skipped.len(), 3);
        assert_eq!(parsed.skipped[0].0, 3);
        assert!(parsed.skipped[1].1.contains("unknown scope"));
        assert!(parsed.skipped[2].1.contains("not a number"));
    }

    #[test]
    fn import_creates_then_updates_on_reimport_and_reports_store_refusals() {
        let mut conn = crate::open_in_memory().unwrap();
        let first = parse_table("term,aliases\nSelect KK,select kay kay\nKamon,ka mon\n");
        let report = import_voice_terms(&mut conn, &first, 1_000).unwrap();
        assert_eq!((report.created, report.updated), (2, 0));
        assert!(report.skipped.is_empty());

        // The same sheet, edited: one alias changed, one new term, one row the store refuses
        // (a bundle term needs its app) — counted, not fatal.
        let second = parse_table(
            "term,aliases,scope\nselect kk,select kay kay;select k k,global\nNew One,,global\nBroken,,bundle\n",
        );
        let report = import_voice_terms(&mut conn, &second, 2_000).unwrap();
        assert_eq!((report.created, report.updated), (1, 1));
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].0, 4);
        assert!(
            report.skipped[0].1.contains("scope_ref"),
            "{}",
            report.skipped[0].1
        );

        let terms = voice_terms::list_voice_terms(&conn).unwrap();
        assert_eq!(terms.len(), 3, "re-import updated rather than duplicated");
        let select = terms.iter().find(|t| t.canonical == "select kk").unwrap();
        assert!(select.aliases.iter().any(|a| a == "select k k"));
    }

    #[test]
    fn export_round_trips_through_parse() {
        let mut conn = crate::open_in_memory().unwrap();
        let parsed = parse_table(
            "term,aliases,locale,scope,scope_ref,priority,enabled\n\"Acme, Inc.\",acme;ak me,en,bundle,us.zoom.xos,3,true\nKamon,,,global,,0,false\n",
        );
        import_voice_terms(&mut conn, &parsed, 1_000).unwrap();
        let csv = export_csv(&voice_terms::list_voice_terms(&conn).unwrap());
        assert!(csv.starts_with("canonical,aliases,locale,scope,scope_ref,priority,enabled\n"));
        assert!(
            csv.contains("\"Acme, Inc.\",acme;ak me,en,bundle,us.zoom.xos,3,true"),
            "{csv}"
        );
        assert!(csv.contains("Kamon,,,global,,0,false"), "{csv}");
        let back = parse_table(&csv);
        assert!(back.skipped.is_empty());
        assert_eq!(back.rows.len(), 2);
        let acme = back
            .rows
            .iter()
            .find(|r| r.term.canonical == "Acme, Inc.")
            .unwrap();
        assert_eq!(acme.term.aliases, vec!["acme", "ak me"]);
        assert_eq!(acme.term.scope_ref.as_deref(), Some("us.zoom.xos"));
        assert!(
            !back
                .rows
                .iter()
                .find(|r| r.term.canonical == "Kamon")
                .unwrap()
                .term
                .enabled
        );
    }
}
