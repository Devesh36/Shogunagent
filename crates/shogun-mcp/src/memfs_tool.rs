//! `memory.fs` (plan E6-T2 / E6-T3): Anthropic's `memory_20250818` file operations over the
//! `/memories` virtual filesystem (`shogun_core::memfs`), on every face (MCP `tools/call`, REST
//! `POST /v1/memory/fs`, CLI `shogun fs <json>`).
//!
//! The six commands and their wire shape are the memory tool's:
//!
//! | command | arguments | effect |
//! |---|---|---|
//! | `view` | `path`, optional `view_range: [start, end]`, `offset` | a directory listing (2 levels, sizes) or the file with 6-wide line numbers; a text over 16,000 characters is cut with a marker naming the next `offset` |
//! | `create` | `path`, `file_text` | create or overwrite |
//! | `str_replace` | `path`, `old_str`, `new_str` (optional, empty deletes) | `old_str` must occur exactly once |
//! | `insert` | `path`, `insert_line`, `insert_text` | after line `insert_line` (0 = top) |
//! | `delete` | `path` | a file, or a directory recursively; the root is protected |
//! | `rename` | `old_path`, `new_path` | never overwrites; the root is protected |
//!
//! What a write does depends on where it lands. Under `/memories/scratch/<your agent id>/` it
//! edits your own file directly (V100, your scratch namespace). Anywhere else the tree is a
//! projection of the world model: a `create` / `str_replace` / `insert` there becomes one
//! `memory.ingest` event (title = the path) that the review turns into state or not, and the file
//! itself is **not** edited; `delete` and `rename` there are refused. Another agent's scratch
//! directory is invisible.
//!
//! This module is pure: parsing, path rules, the text edits and the renderers. The store side
//! (projection, scratch rows, the ingest) is [`crate::backend::MemoryBackend::memory_fs`].

use serde_json::{json, Value};

use crate::memory_api::Tool;

/// The mount point.
pub const ROOT: &str = "/memories";
/// Characters one `view` returns before the truncation marker (the memory tool's 16K rule).
pub const VIEW_MAX_CHARS: usize = 16_000;
/// Longest path accepted.
pub const MAX_PATH_CHARS: usize = 512;
/// Longest text argument accepted (`file_text`, `old_str`, `new_str`, `insert_text`).
pub const MAX_TEXT_CHARS: usize = 64_000;

/// One parsed command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsCommand {
    View { path: String, view_range: Option<(i64, i64)>, offset: usize },
    Create { path: String, file_text: String },
    StrReplace { path: String, old_str: String, new_str: String },
    Insert { path: String, insert_line: usize, insert_text: String },
    Delete { path: String },
    Rename { old_path: String, new_path: String },
}

impl FsCommand {
    pub fn name(&self) -> &'static str {
        match self {
            FsCommand::View { .. } => "view",
            FsCommand::Create { .. } => "create",
            FsCommand::StrReplace { .. } => "str_replace",
            FsCommand::Insert { .. } => "insert",
            FsCommand::Delete { .. } => "delete",
            FsCommand::Rename { .. } => "rename",
        }
    }

    /// Whether the command can change anything.
    pub fn is_write(&self) -> bool {
        !matches!(self, FsCommand::View { .. })
    }
}

/// What the store answered: the text the memory tool returns, whether it is an error in the
/// tool's contract (a missing path, a duplicate `old_str`, …), and the events a world write
/// recorded.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FsReply {
    pub output: String,
    pub is_error: bool,
    pub event_ids: Vec<i64>,
}

impl FsReply {
    pub fn ok(output: impl Into<String>) -> Self {
        Self { output: output.into(), is_error: false, event_ids: Vec::new() }
    }

    pub fn error(output: impl Into<String>) -> Self {
        Self { output: output.into(), is_error: true, event_ids: Vec::new() }
    }
}

/// Normalise a `/memories` path: must be the root or under it, `/`-separated, no empty, `.` or
/// `..` segment, no backslash or control character. A trailing slash is dropped.
pub fn normalize_path(raw: &str) -> Result<String, String> {
    let path = raw.trim();
    if path.chars().count() > MAX_PATH_CHARS || path.chars().any(|c| c.is_control() || c == '\\') {
        return Err(format!("Error: invalid path {path}. Paths are at most {MAX_PATH_CHARS} characters, without control characters."));
    }
    let path = path.trim_end_matches('/');
    if path == ROOT {
        return Ok(ROOT.to_string());
    }
    let Some(rest) = path.strip_prefix(&format!("{ROOT}/")) else {
        return Err(format!("Error: the path {raw} is outside {ROOT}. Every memory path starts with {ROOT}."));
    };
    if rest.split('/').any(|segment| segment.is_empty() || segment == "." || segment == "..") {
        return Err(format!("Error: invalid path {raw}: no empty, '.' or '..' segments."));
    }
    Ok(format!("{ROOT}/{rest}"))
}

fn string_arg(args: &Value, key: &str, required: bool) -> Result<Option<String>, String> {
    match args.get(key) {
        None | Some(Value::Null) if required => Err(format!("invalid_request: {key} is required for this command.")),
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => {
            if text.chars().count() > MAX_TEXT_CHARS {
                return Err(format!("invalid_request: {key} is at most {MAX_TEXT_CHARS} characters."));
            }
            Ok(Some(text.clone()))
        }
        Some(_) => Err(format!("invalid_request: {key} must be a string.")),
    }
}

fn path_arg(args: &Value, key: &str) -> Result<String, String> {
    let raw = string_arg(args, key, true)?.unwrap_or_default();
    normalize_path(&raw).map_err(|message| format!("invalid_path: {message}"))
}

/// Parse `memory.fs` arguments. `Err` is `<code>: <sentence>`, the caller's to fix.
pub fn parse(args: &Value) -> Result<FsCommand, String> {
    if !args.is_object() {
        return Err("invalid_request: the body must be a JSON object.".into());
    }
    let command = string_arg(args, "command", true)?.unwrap_or_default();
    match command.as_str() {
        "view" => {
            let view_range = match args.get("view_range") {
                None | Some(Value::Null) => None,
                Some(Value::Array(pair)) if pair.len() == 2 => {
                    let start = pair[0].as_i64().filter(|n| *n >= 1);
                    let end = pair[1].as_i64().filter(|n| *n == -1 || *n >= 1);
                    match (start, end) {
                        (Some(start), Some(end)) if end == -1 || end >= start => Some((start, end)),
                        _ => return Err("invalid_view_range: view_range is [start, end], 1-based, end -1 for the end of the file.".into()),
                    }
                }
                Some(_) => return Err("invalid_view_range: view_range is [start, end], 1-based, end -1 for the end of the file.".into()),
            };
            let offset = match args.get("offset") {
                None | Some(Value::Null) => 0,
                Some(v) => v
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .ok_or_else(|| "invalid_offset: offset is a non-negative integer (characters).".to_string())?,
            };
            Ok(FsCommand::View { path: path_arg(args, "path")?, view_range, offset })
        }
        "create" => Ok(FsCommand::Create {
            path: path_arg(args, "path")?,
            file_text: string_arg(args, "file_text", true)?.unwrap_or_default(),
        }),
        "str_replace" => Ok(FsCommand::StrReplace {
            path: path_arg(args, "path")?,
            old_str: string_arg(args, "old_str", true)?.unwrap_or_default(),
            new_str: string_arg(args, "new_str", false)?.unwrap_or_default(),
        }),
        "insert" => {
            let insert_line = args
                .get("insert_line")
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(|| "invalid_request: insert_line is required, a non-negative integer.".to_string())?;
            Ok(FsCommand::Insert {
                path: path_arg(args, "path")?,
                insert_line,
                insert_text: string_arg(args, "insert_text", true)?.unwrap_or_default(),
            })
        }
        "delete" => Ok(FsCommand::Delete { path: path_arg(args, "path")? }),
        "rename" => Ok(FsCommand::Rename { old_path: path_arg(args, "old_path")?, new_path: path_arg(args, "new_path")? }),
        _ => Err("invalid_command: command is view, create, str_replace, insert, delete or rename.".into()),
    }
}

/// `str_replace` on a text, with the memory tool's errors.
pub fn apply_str_replace(path: &str, content: &str, old_str: &str, new_str: &str) -> Result<String, String> {
    if old_str.is_empty() {
        return Err(format!("No replacement was performed: old_str is empty for {path}."));
    }
    let hits: Vec<usize> = content.match_indices(old_str).map(|(at, _)| at).collect();
    match hits.as_slice() {
        [] => Err(format!("No replacement was performed, old_str `{old_str}` did not appear verbatim in {path}.")),
        [at] => Ok(format!("{}{new_str}{}", &content[..*at], &content[at + old_str.len()..])),
        many => {
            let lines: Vec<String> = many.iter().map(|at| (content[..*at].matches('\n').count() + 1).to_string()).collect();
            Err(format!(
                "No replacement was performed. Multiple occurrences of old_str `{old_str}` in lines: [{}]. Please ensure it is unique",
                lines.join(", ")
            ))
        }
    }
}

/// `insert` into a text after line `insert_line` (0 = before the first line).
pub fn apply_insert(content: &str, insert_line: usize, insert_text: &str) -> Result<String, String> {
    let mut lines: Vec<&str> = if content.is_empty() { Vec::new() } else { content.split('\n').collect() };
    // A trailing newline is not a line of its own.
    let trailing = content.ends_with('\n');
    if trailing {
        lines.pop();
    }
    if insert_line > lines.len() {
        return Err(format!(
            "Error: Invalid `insert_line` parameter: {insert_line}. It should be within the range of lines of the file: [0, {}]",
            lines.len()
        ));
    }
    let inserted: Vec<&str> = insert_text.trim_end_matches('\n').split('\n').collect();
    let mut out: Vec<&str> = Vec::with_capacity(lines.len() + inserted.len());
    out.extend(&lines[..insert_line]);
    out.extend(inserted);
    out.extend(&lines[insert_line..]);
    let mut text = out.join("\n");
    if trailing || content.is_empty() {
        text.push('\n');
    }
    Ok(text)
}

/// Cut `rendered` to [`VIEW_MAX_CHARS`] from `offset` (characters), with a marker naming the next
/// offset when more remains.
fn page(rendered: &str, offset: usize) -> String {
    let total = rendered.chars().count();
    if offset == 0 && total <= VIEW_MAX_CHARS {
        return rendered.to_string();
    }
    let shown: String = rendered.chars().skip(offset).take(VIEW_MAX_CHARS).collect();
    let end = (offset + VIEW_MAX_CHARS).min(total);
    if end < total {
        format!("{shown}\n[truncated: characters {offset}-{end} of {total} shown; call view again with offset={end} for the rest]")
    } else {
        format!("{shown}\n[end: characters {offset}-{end} of {total}]")
    }
}

/// The file view: a header, then each line as `{n:>6}\t{line}`, within `view_range`, paged.
pub fn render_file(path: &str, content: &str, view_range: Option<(i64, i64)>, offset: usize) -> Result<String, String> {
    let lines: Vec<&str> = content.lines().collect();
    let (start, end) = match view_range {
        None => (1, lines.len()),
        Some((start, end)) => {
            let start = usize::try_from(start).unwrap_or(1);
            let end = if end == -1 { lines.len() } else { usize::try_from(end).unwrap_or(lines.len()) };
            if start > lines.len().max(1) || end > lines.len() {
                return Err(format!(
                    "Error: Invalid `view_range`: [{start}, {end}]. The file {path} has {} lines.",
                    lines.len()
                ));
            }
            (start, end)
        }
    };
    let body: Vec<String> = lines
        .iter()
        .enumerate()
        .skip(start.saturating_sub(1))
        .take(end + 1 - start.min(end + 1))
        .map(|(index, line)| format!("{:>6}\t{line}", index + 1))
        .collect();
    let rendered = format!("Here's the content of {path} with line numbers:\n{}", body.join("\n"));
    Ok(page(&rendered, offset))
}

fn human_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}K", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}M", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// The directory view: `dir` and everything up to two levels below it, each `{size}\t{path}`
/// (a directory's size is the sum of its files). `files` is every file of the tree.
pub fn render_dir(dir: &str, files: &[(String, usize)], offset: usize) -> String {
    let prefix = if dir == "/" { "/".to_string() } else { format!("{dir}/") };
    let mut rows: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    let mut total = 0usize;
    for (path, bytes) in files {
        let Some(rest) = path.strip_prefix(&prefix) else { continue };
        total += bytes;
        let parts: Vec<&str> = rest.split('/').collect();
        // Every directory on the way down, up to two levels, and the file itself if it is shallow.
        for depth in 1..=parts.len().min(2) {
            let shown = format!("{prefix}{}", parts[..depth].join("/"));
            *rows.entry(shown).or_default() += bytes;
        }
    }
    let mut out = format!(
        "Here're the files and directories up to 2 levels deep in {dir}, excluding hidden items and node_modules:\n{}\t{dir}",
        human_size(total)
    );
    for (path, bytes) in rows {
        out.push_str(&format!("\n{}\t{path}", human_size(bytes)));
    }
    page(&out, offset)
}

/// The text for a path that names nothing.
pub fn missing(path: &str) -> String {
    format!("The path {path} does not exist. Please provide a valid path.")
}

/// Why a call did not produce an envelope (the same two kinds as `memory.ingest`).
pub type Refusal = crate::ingest_api::Refusal;

/// The `memory.fs` envelope (plan §7.2): `result.output` is the text the memory tool returns,
/// `result.error` whether it is a tool-level error, `usage` counts the events a world write
/// recorded (billed as ingest; reads and scratch edits are free).
pub fn render(command: &FsCommand, reply: &FsReply, latency_ms: u64) -> String {
    json!({
        "tool": Tool::MemoryFs.wire_name(),
        "result": {
            "command": command.name(),
            "output": reply.output,
            "error": reply.is_error,
            "event_ids": reply.event_ids,
        },
        "usage": { "unit": "events", "quantity": reply.event_ids.len() },
        "latency_ms": latency_ms,
        "next_action": Value::Null,
    })
    .to_string()
}

/// Parse, run and render one call for any face.
pub fn serve<B: crate::backend::MemoryBackend + ?Sized>(args: &Value, backend: &B) -> Result<(String, bool), Refusal> {
    let started = std::time::Instant::now();
    let command = parse(args).map_err(Refusal::BadArguments)?;
    let reply = backend.memory_fs(&command).map_err(|_| Refusal::Unavailable("memory_fs_unavailable"))?;
    let latency = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok((render(&command, &reply, latency), reply.is_error))
}

/// The MCP `inputSchema` of `memory.fs`: the memory tool's arguments.
pub fn schema() -> Value {
    json!({
        "command": { "type": "string", "enum": ["view", "create", "str_replace", "insert", "delete", "rename"] },
        "path": { "type": "string", "maxLength": MAX_PATH_CHARS, "description": "Starts with /memories" },
        "view_range": { "type": "array", "items": { "type": "integer" }, "minItems": 2, "maxItems": 2, "description": "view: [start, end] lines, 1-based; end -1 = end of file" },
        "offset": { "type": "integer", "minimum": 0, "description": "view: character offset to continue a truncated view" },
        "file_text": { "type": "string", "maxLength": MAX_TEXT_CHARS },
        "old_str": { "type": "string", "maxLength": MAX_TEXT_CHARS },
        "new_str": { "type": "string", "maxLength": MAX_TEXT_CHARS },
        "insert_line": { "type": "integer", "minimum": 0 },
        "insert_text": { "type": "string", "maxLength": MAX_TEXT_CHARS },
        "old_path": { "type": "string", "maxLength": MAX_PATH_CHARS },
        "new_path": { "type": "string", "maxLength": MAX_PATH_CHARS }
    })
}

/// The agent-facing description.
pub const DESCRIPTION: &str = "Your memory as files, with the operations of Anthropic's memory tool (memory_20250818): view, create, str_replace, insert, delete, rename under /memories. /memories/INDEX.md (at most 200 lines) lists what is there: commitments.md, open_loops.md, people/<slug>.md and projects/<slug>.md are generated from the world model with confidence bands and proving quotes; /memories/scratch/<your agent id>/ holds your own files, which only you see. Writing outside your scratch directory records an event for the hourly review instead of editing the file; delete and rename work only in your scratch. view truncates at 16,000 characters and names the offset to continue. Not a search (memory.recall). Reads and scratch edits are free; a world write is metered per event (ingest).";

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn paths_must_stay_under_memories() {
        assert_eq!(normalize_path("/memories").unwrap(), "/memories");
        assert_eq!(normalize_path("/memories/").unwrap(), "/memories");
        assert_eq!(normalize_path("/memories/scratch/codex/a.md").unwrap(), "/memories/scratch/codex/a.md");
        for bad in ["/etc/passwd", "memories/a.md", "/memories/../etc", "/memories//a", "/memories/./a", "/memoriesx/a", "/memories/a\\b"] {
            assert!(normalize_path(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_six_commands_parse_like_the_memory_tool() {
        assert_eq!(
            parse(&json!({ "command": "view", "path": "/memories" })).unwrap(),
            FsCommand::View { path: "/memories".into(), view_range: None, offset: 0 }
        );
        assert_eq!(
            parse(&json!({ "command": "view", "path": "/memories/a.md", "view_range": [2, -1], "offset": 10 })).unwrap(),
            FsCommand::View { path: "/memories/a.md".into(), view_range: Some((2, -1)), offset: 10 }
        );
        assert!(parse(&json!({ "command": "view", "path": "/memories/a.md", "view_range": [3, 2] })).is_err());
        assert_eq!(
            parse(&json!({ "command": "create", "path": "/memories/a.md", "file_text": "x" })).unwrap(),
            FsCommand::Create { path: "/memories/a.md".into(), file_text: "x".into() }
        );
        assert_eq!(
            parse(&json!({ "command": "str_replace", "path": "/memories/a.md", "old_str": "x" })).unwrap(),
            FsCommand::StrReplace { path: "/memories/a.md".into(), old_str: "x".into(), new_str: String::new() },
            "new_str is optional: empty deletes"
        );
        assert_eq!(
            parse(&json!({ "command": "insert", "path": "/memories/a.md", "insert_line": 0, "insert_text": "top" })).unwrap(),
            FsCommand::Insert { path: "/memories/a.md".into(), insert_line: 0, insert_text: "top".into() }
        );
        assert_eq!(parse(&json!({ "command": "delete", "path": "/memories/a" })).unwrap(), FsCommand::Delete { path: "/memories/a".into() });
        assert_eq!(
            parse(&json!({ "command": "rename", "old_path": "/memories/a", "new_path": "/memories/b" })).unwrap(),
            FsCommand::Rename { old_path: "/memories/a".into(), new_path: "/memories/b".into() }
        );
        assert!(parse(&json!({ "command": "chmod", "path": "/memories" })).unwrap_err().starts_with("invalid_command:"));
        assert!(parse(&json!({ "command": "create", "path": "/tmp/x", "file_text": "x" })).unwrap_err().starts_with("invalid_path:"));
        assert!(parse(&json!({ "command": "create", "path": "/memories/x" })).unwrap_err().starts_with("invalid_request:"));
    }

    #[test]
    fn str_replace_needs_exactly_one_occurrence() {
        let text = "alpha\nbeta\nalpha\n";
        assert_eq!(apply_str_replace("/m", "one two", "two", "three").unwrap(), "one three");
        assert_eq!(apply_str_replace("/m", "one two", "two", "").unwrap(), "one ");
        let missing = apply_str_replace("/m", text, "gamma", "x").unwrap_err();
        assert!(missing.contains("did not appear verbatim"), "{missing}");
        let twice = apply_str_replace("/m", text, "alpha", "x").unwrap_err();
        assert!(twice.contains("Multiple occurrences") && twice.contains("[1, 3]"), "{twice}");
    }

    #[test]
    fn insert_goes_after_the_named_line_and_checks_the_range() {
        assert_eq!(apply_insert("a\nb\n", 0, "top").unwrap(), "top\na\nb\n");
        assert_eq!(apply_insert("a\nb\n", 1, "mid").unwrap(), "a\nmid\nb\n");
        assert_eq!(apply_insert("a\nb\n", 2, "end\n").unwrap(), "a\nb\nend\n");
        assert_eq!(apply_insert("", 0, "first").unwrap(), "first\n");
        let out_of_range = apply_insert("a\nb\n", 3, "x").unwrap_err();
        assert!(out_of_range.contains("[0, 2]"), "{out_of_range}");
    }

    #[test]
    fn file_views_number_lines_and_cut_at_16k_with_an_offset_marker() {
        let view = render_file("/memories/a.md", "first\nsecond\nthird", None, 0).unwrap();
        assert_eq!(view, "Here's the content of /memories/a.md with line numbers:\n     1\tfirst\n     2\tsecond\n     3\tthird");
        let ranged = render_file("/memories/a.md", "first\nsecond\nthird", Some((2, 2)), 0).unwrap();
        assert!(ranged.ends_with("     2\tsecond") && !ranged.contains("first"), "{ranged}");
        assert!(render_file("/memories/a.md", "one", Some((1, 5)), 0).is_err());

        let long: String = (0..3_000).map(|n| format!("line {n} of a long page\n")).collect();
        let first = render_file("/memories/big.md", &long, None, 0).unwrap();
        let marker = first.rsplit('\n').next().unwrap();
        assert!(marker.starts_with("[truncated: characters 0-16000 of "), "{marker}");
        assert!(first.chars().count() <= VIEW_MAX_CHARS + marker.chars().count() + 1);
        let next = render_file("/memories/big.md", &long, None, 16_000).unwrap();
        assert!(next.contains("[truncated: characters 16000-32000") || next.contains("[end: characters 16000-"), "the offset continues the view");
        let short = render_file("/memories/a.md", "x", None, 0).unwrap();
        assert!(!short.contains("[truncated"));
    }

    #[test]
    fn directory_views_list_two_levels_with_sizes() {
        let files = vec![
            ("/memories/INDEX.md".to_string(), 100),
            ("/memories/people/oren-vale-1.md".to_string(), 2_048),
            ("/memories/scratch/codex/notes/deep/a.md".to_string(), 10),
        ];
        let view = render_dir("/memories", &files, 0);
        assert!(view.starts_with("Here're the files and directories up to 2 levels deep in /memories"), "{view}");
        assert!(view.contains("100\t/memories/INDEX.md"));
        assert!(view.contains("2.0K\t/memories/people\n") || view.contains("2.0K\t/memories/people"));
        assert!(view.contains("\t/memories/people/oren-vale-1.md"));
        assert!(view.contains("\t/memories/scratch/codex"));
        assert!(!view.contains("notes/deep"), "deeper than two levels is not listed: {view}");
        assert_eq!(missing("/memories/x"), "The path /memories/x does not exist. Please provide a valid path.");
    }

    #[test]
    fn the_envelope_carries_output_error_and_usage() {
        let command = FsCommand::Create { path: "/memories/notes.md".into(), file_text: "x".into() };
        let reply = FsReply { output: "Recorded".into(), is_error: false, event_ids: vec![4] };
        let value: Value = serde_json::from_str(&render(&command, &reply, 2)).unwrap();
        assert_eq!(value["tool"], "memory.fs");
        assert_eq!(value["result"]["command"], "create");
        assert_eq!(value["usage"], json!({ "unit": "events", "quantity": 1 }));
        assert_eq!(value["latency_ms"], 2);
    }
}
