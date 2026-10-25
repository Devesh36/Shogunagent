//! "Add to …" with one click (issue #400): the change SHOGUN makes to another app's MCP setup, as a
//! plan the user sees in full before anything is written.
//!
//! Three ways a client gets the entry, one per client ([`install_method`]):
//! - **Vendor CLI** (Claude Code): the vendor's own `claude mcp add … --scope user`, run as a local
//!   subprocess ([`claude_code_add_runs`]) for Add, automatic connection, Remove and Undo.
//!   Its user config is read only, to avoid removing a custom entry.
//! - **Config file** (Cursor `~/.cursor/mcp.json`; Codex CLI and the ChatGPT desktop app share
//!   `~/.codex/config.toml`; Claude Desktop `claude_desktop_config.json`): a read-modify-write that
//!   touches only the `shogun-memory` entry ([`plan_add`] / [`plan_remove`]). Every other byte of
//!   the file is kept; the result is parsed again and compared with the original, entry by entry,
//!   before it may be written; a file that does not parse is refused, never "repaired".
//! - **Copy only** ([`InstallMethod::CopyOnly`]): no client uses it since issue #963. An entry
//!   names the app's connection and holds nothing secret, so SHOGUN writes it.
//!
//! Never a token (invariant 7): a plan is built from the executable path, the database path and
//! the app's connection id only ([`TokenValue::Connection`], issue #963); there is no input a
//! caller could put a token into. What the plan shows of an existing SHOGUN entry is masked
//! ([`mask_existing`]): an entry pasted by hand earlier may hold a token.
//!
//! Nothing of the rest of the file is shown either: another server's entry can carry its own key.
//! The preview is SHOGUN's entry before and after, plus how many other entries stay as they were.
//!
//! Applying is in-app only, behind a confirmation dialog showing this plan (like issuing a token or
//! raising its scope): no MCP / REST / CLI call writes another app's configuration. The API faces
//! see the same method and the text that would be added through `api.tokens.list` (`mount`).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::client_config::{
    render_json_entry, render_toml_entry, McpClient, ServerParams, TokenValue, DB_PATH_ENV,
    SERVER_NAME,
};

/// How the one-click setup reaches `client`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallMethod {
    /// The vendor's own CLI adds and removes the entry.
    VendorCli,
    /// SHOGUN edits the client's documented config file, SHOGUN's entry only.
    ConfigFile,
    /// SHOGUN shows the entry; the user pastes it (and the token) themselves.
    CopyOnly,
}

/// The method for `client` (see the module docs for why each).
pub fn install_method(client: McpClient) -> InstallMethod {
    match client {
        McpClient::ClaudeCode => InstallMethod::VendorCli,
        McpClient::CodexCli
        | McpClient::ChatGptDesktop
        | McpClient::Cursor
        | McpClient::ClaudeDesktop => InstallMethod::ConfigFile,
    }
}

/// The config file SHOGUN edits for `client`, under the user's home directory; `None` for the
/// clients it does not edit.
pub fn config_file(client: McpClient, home: &Path) -> Option<PathBuf> {
    match client {
        McpClient::Cursor => Some(home.join(".cursor").join("mcp.json")),
        McpClient::CodexCli | McpClient::ChatGptDesktop => {
            Some(home.join(".codex").join("config.toml"))
        }
        McpClient::ClaudeDesktop => Some(
            home.join("Library")
                .join("Application Support")
                .join("Claude")
                .join("claude_desktop_config.json"),
        ),
        McpClient::ClaudeCode => None,
    }
}

/// `path` with the home directory shown as `~`, the way the dialog names the file.
pub fn display_path(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// What a plan does to SHOGUN's entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    /// There was no entry; one is added (the file is created if it did not exist).
    Add,
    /// An entry that differs is replaced.
    Replace,
    /// The entry is already exactly this; nothing is written.
    Unchanged,
    /// The entry is removed.
    Remove,
    /// There is no entry to remove; nothing is written.
    Absent,
}

/// One line of the preview.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DiffLine {
    pub op: DiffOp,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffOp {
    Context,
    Add,
    Remove,
}

/// Why SHOGUN will not edit a file. Each is a stop, not a fallback: the user still has the entry
/// to copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    /// This client is copy-only ([`InstallMethod::CopyOnly`]) or edited by its own CLI.
    CopyOnly,
    /// The file is not valid JSON / TOML (or not UTF-8). SHOGUN does not repair another app's file.
    Unparseable,
    /// The file parses but is not an object with an `mcpServers` object (JSON), or `mcp_servers`
    /// is not a table (TOML).
    UnexpectedShape,
    /// SHOGUN's entry appears more than once.
    AmbiguousEntry,
    /// SHOGUN's entry is written in a form this editor does not rewrite (TOML dotted keys or an
    /// inline table).
    UnsupportedEntryForm,
    /// The edited file did not read back as "the original with only SHOGUN's entry changed".
    NotVerified,
}

impl Refusal {
    pub fn id(self) -> &'static str {
        match self {
            Refusal::CopyOnly => "copy_only",
            Refusal::Unparseable => "unparseable",
            Refusal::UnexpectedShape => "unexpected_shape",
            Refusal::AmbiguousEntry => "ambiguous_entry",
            Refusal::UnsupportedEntryForm => "unsupported_entry_form",
            Refusal::NotVerified => "not_verified",
        }
    }
}

/// A planned edit of one config file. The new contents stay here: they hold the whole file, other
/// servers' entries included, so only [`FilePlan::diff`] and the counts are ever shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePlan {
    pub change: Change,
    /// SHA-256 (hex) of the bytes the plan was made from, or [`ABSENT_DIGEST`]. Applying refuses
    /// when the file no longer has this digest, so what is written is what the user saw.
    pub base_digest: String,
    /// SHOGUN's entry before (masked) and after, as lines.
    pub diff: Vec<DiffLine>,
    /// Other servers in the file, left exactly as they were.
    pub other_entries: usize,
    new_contents: Option<String>,
}

impl FilePlan {
    /// The whole new file, or `None` when nothing is to be written.
    pub fn new_contents(&self) -> Option<&str> {
        self.new_contents.as_deref()
    }
}

/// The digest of a file that does not exist.
pub const ABSENT_DIGEST: &str = "absent";

/// SHA-256 (hex) of `existing`, or [`ABSENT_DIGEST`].
pub fn digest(existing: Option<&str>) -> String {
    match existing {
        None => ABSENT_DIGEST.to_string(),
        Some(text) => Sha256::digest(text.as_bytes()).iter().fold(String::new(), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        }),
    }
}

/// The parameters of a written entry: the app's connection (issue #963), never a token.
fn install_params<'a>(command: &'a str, db_path: &'a str) -> ServerParams<'a> {
    ServerParams { command, db_path, api_token: TokenValue::Connection, toolsets: None }
}

/// Plan adding (or refreshing) SHOGUN's entry in `client`'s config file, whose current text is
/// `existing` (`None` = no file yet).
pub fn plan_add(
    client: McpClient,
    existing: Option<&str>,
    command: &str,
    db_path: &str,
) -> Result<FilePlan, Refusal> {
    if install_method(client) != InstallMethod::ConfigFile {
        return Err(Refusal::CopyOnly);
    }
    let params = install_params(command, db_path);
    match client.format() {
        crate::client_config::ConfigFormat::Json => json::plan(client, existing, Some(&params)),
        crate::client_config::ConfigFormat::Toml => toml_file::plan(client, existing, Some(&params)),
    }
}

/// Plan removing SHOGUN's entry from `client`'s config file.
pub fn plan_remove(client: McpClient, existing: Option<&str>) -> Result<FilePlan, Refusal> {
    if install_method(client) != InstallMethod::ConfigFile {
        return Err(Refusal::CopyOnly);
    }
    match client.format() {
        crate::client_config::ConfigFormat::Json => json::plan(client, existing, None),
        crate::client_config::ConfigFormat::Toml => toml_file::plan(client, existing, None),
    }
}

/// The text of SHOGUN's entry `client` would get, as the dialog and `api.tokens.list` show it
/// (without the rest of any file).
pub fn entry_text(client: McpClient, command: &str, db_path: &str) -> String {
    let params = install_params(command, db_path);
    match client.format() {
        crate::client_config::ConfigFormat::Json => {
            format!("\"{SERVER_NAME}\": {}\n", render_json_entry(client, &params, "", "  "))
        }
        crate::client_config::ConfigFormat::Toml => render_toml_entry(client, &params),
    }
}

/// What stands in for a masked value in the preview.
pub const MASK: &str = "••••••";

/// Whether `value` is a reference a client resolves at launch (`${NAME}`, `${env:NAME}`) rather
/// than a value.
fn is_reference(value: &str) -> bool {
    value.starts_with("${") && value.ends_with('}') && !value[2..value.len() - 1].contains(['{', '}'])
}

/// The strings in an existing SHOGUN entry the preview must not show: every env value except the
/// database path, the toolsets filter and references, and anything shaped like an issued token.
fn secrets_in(entry: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(env) = entry.get("env").and_then(serde_json::Value::as_object) {
        for (key, value) in env {
            let Some(value) = value.as_str() else { continue };
            let shown = key == DB_PATH_ENV || key == crate::toolsets::ENV_VAR;
            if !shown && !is_reference(value) && !value.is_empty() {
                out.push(value.to_string());
            }
        }
    }
    collect_token_shaped(entry, &mut out);
    out.sort_by_key(|s| std::cmp::Reverse(s.len()));
    out.dedup();
    out
}

fn collect_token_shaped(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) if looks_like_token(s) => out.push(s.clone()),
        serde_json::Value::Array(items) => items.iter().for_each(|v| collect_token_shaped(v, out)),
        serde_json::Value::Object(map) => map.values().for_each(|v| collect_token_shaped(v, out)),
        _ => {}
    }
}

/// `shogun_` followed by at least 16 hex digits: the shape of an issued Memory API token.
pub fn looks_like_token(s: &str) -> bool {
    s.match_indices("shogun_").any(|(i, _)| {
        s[i + "shogun_".len()..].chars().take_while(char::is_ascii_hexdigit).count() >= 16
    })
}

/// `raw` (the existing entry's text in the file) with every secret of `entry` masked, in each of
/// the spellings it can take in the file (as-is, JSON-escaped).
pub fn mask_existing(raw: &str, entry: &serde_json::Value) -> String {
    let mut out = raw.to_string();
    for secret in secrets_in(entry) {
        let escaped = serde_json::to_string(&secret).unwrap_or_default();
        let inner = escaped.trim_matches('"').to_string();
        for spelling in [secret.as_str(), inner.as_str()] {
            if !spelling.is_empty() {
                out = out.replace(spelling, MASK);
            }
        }
    }
    out
}

/// A line diff of `before` → `after` (longest common subsequence; entries are a few lines).
pub fn line_diff(before: &str, after: &str) -> Vec<DiffLine> {
    let a: Vec<&str> = before.lines().collect();
    let b: Vec<&str> = after.lines().collect();
    let mut lcs = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    let line = |op, text: &str| DiffLine { op, text: text.to_string() };
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            out.push(line(DiffOp::Context, a[i]));
            i += 1;
            j += 1;
        } else if j < b.len() && (i == a.len() || lcs[i][j + 1] >= lcs[i + 1][j]) {
            out.push(line(DiffOp::Add, b[j]));
            j += 1;
        } else {
            out.push(line(DiffOp::Remove, a[i]));
            i += 1;
        }
    }
    out
}

/// Common leading whitespace removed from every non-blank line, so an entry nested deep in a file
/// reads as it would on its own.
fn dedent(text: &str) -> String {
    let indent = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    text.lines()
        .map(|l| l.get(indent..).unwrap_or(l.trim_start()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn change_for(had_entry: bool, adding: bool, same: bool) -> Change {
    match (adding, had_entry) {
        (true, false) => Change::Add,
        (true, true) if same => Change::Unchanged,
        (true, true) => Change::Replace,
        (false, true) => Change::Remove,
        (false, false) => Change::Absent,
    }
}

// JSON (Cursor)

mod json {
    use super::*;
    use serde_json::Value;

    /// A member of a JSON object, by byte offsets into the text.
    struct Member {
        key: String,
        /// The opening quote of the key.
        start: usize,
        value_start: usize,
        value_end: usize,
    }

    struct Object {
        close: usize,
        members: Vec<Member>,
    }

    fn skip_ws(b: &[u8], mut i: usize) -> usize {
        while matches!(b.get(i), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            i += 1;
        }
        i
    }

    /// `i` at an opening quote; the index after the closing one.
    fn string_end(b: &[u8], mut i: usize) -> Option<usize> {
        i += 1;
        while let Some(&c) = b.get(i) {
            match c {
                b'\\' => i += 2,
                b'"' => return Some(i + 1),
                _ => i += 1,
            }
        }
        None
    }

    fn value_end(b: &[u8], i: usize) -> Option<usize> {
        match b.get(i)? {
            b'"' => string_end(b, i),
            b'{' | b'[' => {
                let mut depth = 0usize;
                let mut j = i;
                while let Some(&c) = b.get(j) {
                    match c {
                        b'"' => {
                            j = string_end(b, j)?;
                            continue;
                        }
                        b'{' | b'[' => depth += 1,
                        b'}' | b']' => {
                            depth -= 1;
                            if depth == 0 {
                                return Some(j + 1);
                            }
                        }
                        _ => {}
                    }
                    j += 1;
                }
                None
            }
            _ => {
                let mut j = i;
                while let Some(&c) = b.get(j) {
                    if matches!(c, b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r') {
                        break;
                    }
                    j += 1;
                }
                Some(j)
            }
        }
    }

    /// The object whose `{` is at `open`. The text has already parsed, so anything unexpected
    /// here is a scanner limit and ends in a refusal, not a guess.
    fn object_at(text: &str, open: usize) -> Option<Object> {
        let b = text.as_bytes();
        if b.get(open) != Some(&b'{') {
            return None;
        }
        let mut members = Vec::new();
        let mut i = skip_ws(b, open + 1);
        if b.get(i) == Some(&b'}') {
            return Some(Object { close: i, members });
        }
        loop {
            if b.get(i) != Some(&b'"') {
                return None;
            }
            let key_end = string_end(b, i)?;
            let key: String = serde_json::from_str(text.get(i..key_end)?).ok()?;
            let colon = skip_ws(b, key_end);
            if b.get(colon) != Some(&b':') {
                return None;
            }
            let value_start = skip_ws(b, colon + 1);
            let value_end = value_end(b, value_start)?;
            members.push(Member { key, start: i, value_start, value_end });
            let next = skip_ws(b, value_end);
            match b.get(next) {
                Some(b',') => i = skip_ws(b, next + 1),
                Some(b'}') => return Some(Object { close: next, members }),
                _ => return None,
            }
        }
    }

    /// The whitespace before `pos` on its line, when only whitespace precedes it.
    fn line_indent(text: &str, pos: usize) -> Option<&str> {
        let line_start = text.get(..pos)?.rfind('\n').map_or(0, |n| n + 1);
        let indent = text.get(line_start..pos)?;
        indent.chars().all(|c| c == ' ' || c == '\t').then_some(indent)
    }

    fn quoted_key(key: &str) -> String {
        serde_json::to_string(key).unwrap_or_else(|_| format!("\"{key}\""))
    }

    pub(super) fn plan(
        client: McpClient,
        existing: Option<&str>,
        params: Option<&ServerParams<'_>>,
    ) -> Result<FilePlan, Refusal> {
        let base_digest = digest(existing);
        let blank = match existing {
            None => true,
            Some(t) => t.trim().is_empty(),
        };
        if blank {
            let Some(params) = params else {
                return Ok(FilePlan {
                    change: Change::Absent,
                    base_digest,
                    diff: Vec::new(),
                    other_entries: 0,
                    new_contents: None,
                });
            };
            let whole = crate::client_config::render_client_config(client, params);
            let entry = format!("\"{SERVER_NAME}\": {}", render_json_entry(client, params, "", "  "));
            return Ok(FilePlan {
                change: Change::Add,
                base_digest,
                diff: line_diff("", &entry),
                other_entries: 0,
                new_contents: Some(whole),
            });
        }
        let text = existing.unwrap_or_default();
        let original: Value = serde_json::from_str(text).map_err(|_| Refusal::Unparseable)?;
        if !original.is_object() {
            return Err(Refusal::UnexpectedShape);
        }
        let servers_value = original.get("mcpServers");
        if servers_value.is_some_and(|v| !v.is_object()) {
            return Err(Refusal::UnexpectedShape);
        }
        let b = text.as_bytes();
        let top_open = skip_ws(b, 0);
        let top = object_at(text, top_open).ok_or(Refusal::Unparseable)?;
        let servers_members: Vec<&Member> =
            top.members.iter().filter(|m| m.key == "mcpServers").collect();
        if servers_members.len() > 1 {
            return Err(Refusal::AmbiguousEntry);
        }
        let servers = match servers_members.first() {
            Some(m) => Some((*m, object_at(text, m.value_start).ok_or(Refusal::Unparseable)?)),
            None => None,
        };
        let ours: Vec<&Member> = servers
            .as_ref()
            .map(|(_, obj)| obj.members.iter().filter(|m| m.key == SERVER_NAME).collect())
            .unwrap_or_default();
        if ours.len() > 1 {
            return Err(Refusal::AmbiguousEntry);
        }
        let other_entries = servers
            .as_ref()
            .map_or(0, |(_, obj)| obj.members.iter().filter(|m| m.key != SERVER_NAME).count());
        let existing_entry = original.get("mcpServers").and_then(|s| s.get(SERVER_NAME));
        let before_raw = ours
            .first()
            .and_then(|m| {
                let from = if line_indent(text, m.start).is_some() {
                    text.get(..m.start)?.rfind('\n').map_or(0, |n| n + 1)
                } else {
                    m.start
                };
                text.get(from..m.value_end)
            })
            .map(|raw| mask_existing(raw, existing_entry.unwrap_or(&Value::Null)))
            .unwrap_or_default();

        // Indentation, taken from the file so the new lines sit like the old ones.
        let top_indent = top
            .members
            .first()
            .and_then(|m| line_indent(text, m.start))
            .unwrap_or("  ")
            .to_string();
        let unit = if top_indent.is_empty() { "  ".to_string() } else { top_indent.clone() };

        let (new_text, after_member) = match (params, &servers, ours.first()) {
            // Remove.
            (None, _, None) => (None, String::new()),
            (None, Some((_, obj)), Some(member)) => {
                let idx = obj.members.iter().position(|m| m.start == member.start).unwrap_or(0);
                let (cut_start, cut_end) = if idx > 0 {
                    // From the end of the previous member: its comma goes with ours.
                    (obj.members.get(idx - 1).map_or(member.start, |p| p.value_end), member.value_end)
                } else if let Some(next) = obj.members.get(1) {
                    (member.start, next.start)
                } else {
                    // The only member: leave `{}` behind, with the closing brace where it was.
                    let open = text
                        .get(..member.start)
                        .and_then(|t| t.rfind('{'))
                        .ok_or(Refusal::Unparseable)?;
                    (open + 1, obj.close)
                };
                let mut out = String::with_capacity(text.len());
                out.push_str(&text[..cut_start]);
                out.push_str(&text[cut_end..]);
                (Some(out), String::new())
            }
            (None, None, Some(_)) => return Err(Refusal::Unparseable),
            // Add or replace.
            (Some(params), Some((servers_member, obj)), found) => {
                let parent_indent = line_indent(text, servers_member.start).unwrap_or("").to_string();
                let member_indent = found
                    .copied()
                    .or(obj.members.first())
                    .and_then(|m| line_indent(text, m.start))
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("{parent_indent}{unit}"));
                let value = render_json_entry(client, params, &member_indent, &unit);
                let member_text = format!("{}: {value}", quoted_key(SERVER_NAME));
                let shown = format!("{member_indent}{member_text}");
                let out = match found {
                    Some(member) => {
                        let mut out = String::with_capacity(text.len() + value.len());
                        out.push_str(&text[..member.value_start]);
                        out.push_str(&value);
                        out.push_str(&text[member.value_end..]);
                        out
                    }
                    None => match obj.members.last() {
                        Some(last) => format!(
                            "{},\n{member_indent}{member_text}{}",
                            &text[..last.value_end],
                            &text[last.value_end..]
                        ),
                        None => {
                            let open = text
                                .get(..obj.close)
                                .and_then(|t| t.rfind('{'))
                                .ok_or(Refusal::Unparseable)?;
                            format!(
                                "{}\n{member_indent}{member_text}\n{parent_indent}{}",
                                &text[..=open],
                                &text[obj.close..]
                            )
                        }
                    },
                };
                (Some(out), shown)
            }
            (Some(params), None, _) => {
                // No `mcpServers` yet: add it as the last member of the top-level object.
                let member_indent = format!("{top_indent}{unit}");
                let value = render_json_entry(client, params, &member_indent, &unit);
                let member_text = format!("{}: {value}", quoted_key(SERVER_NAME));
                let block = format!(
                    "\"mcpServers\": {{\n{member_indent}{member_text}\n{top_indent}}}"
                );
                let out = match top.members.last() {
                    Some(last) => format!(
                        "{},\n{top_indent}{block}{}",
                        &text[..last.value_end],
                        &text[last.value_end..]
                    ),
                    None => format!(
                        "{}\n{top_indent}{block}\n{}",
                        &text[..=top_open],
                        &text[top.close..]
                    ),
                };
                (Some(out), format!("{member_indent}{member_text}"))
            }
        };

        let same = match (params, existing_entry) {
            (Some(params), Some(current)) => {
                let mut intended = intended_entry(client, params)?;
                let same = current == &intended;
                // Claude's CLI persists the explicit transport; older Shogun entries omit it.
                if client == McpClient::ClaudeCode {
                    intended["type"] = Value::String("stdio".into());
                }
                same || current == &intended
            }
            _ => false,
        };
        let change = change_for(existing_entry.is_some(), params.is_some(), same);
        let new_contents = match change {
            Change::Unchanged | Change::Absent => None,
            _ => new_text,
        };
        if let Some(new) = &new_contents {
            verify(&original, new, params.map(|p| intended_entry(client, p)).transpose()?)?;
        }
        let diff = match change {
            Change::Unchanged => line_diff(&dedent(&before_raw), &dedent(&before_raw)),
            _ => line_diff(&dedent(&before_raw), &dedent(&after_member)),
        };
        Ok(FilePlan { change, base_digest, diff, other_entries, new_contents })
    }

    fn intended_entry(client: McpClient, params: &ServerParams<'_>) -> Result<Value, Refusal> {
        let whole = crate::client_config::render_client_config(client, params);
        let v: Value = serde_json::from_str(&whole).map_err(|_| Refusal::NotVerified)?;
        v.get("mcpServers")
            .and_then(|s| s.get(SERVER_NAME))
            .cloned()
            .ok_or(Refusal::NotVerified)
    }

    /// The written file must read back as the original with only SHOGUN's entry changed (and,
    /// when there was none, an `mcpServers` object created to hold it).
    fn verify(original: &Value, new: &str, intended: Option<Value>) -> Result<(), Refusal> {
        let written: Value = serde_json::from_str(new).map_err(|_| Refusal::NotVerified)?;
        let strip = |v: &Value, created: bool| -> Value {
            let mut v = v.clone();
            if let Some(obj) = v.as_object_mut() {
                let emptied = obj
                    .get_mut("mcpServers")
                    .and_then(Value::as_object_mut)
                    .map(|servers| {
                        servers.remove(SERVER_NAME);
                        servers.is_empty()
                    })
                    .unwrap_or(false);
                if created && emptied {
                    obj.remove("mcpServers");
                }
            }
            v
        };
        let created = original.get("mcpServers").is_none();
        if strip(original, false) != strip(&written, created) {
            return Err(Refusal::NotVerified);
        }
        let now = written.get("mcpServers").and_then(|s| s.get(SERVER_NAME));
        if now != intended.as_ref() {
            return Err(Refusal::NotVerified);
        }
        Ok(())
    }
}

// TOML (Codex)

mod toml_file {
    use super::*;
    use serde_json::Value;

    /// The dotted path of a table header line (`[a.b]` / `[[a.b]]`), or `None` if the line is not
    /// one.
    pub(super) fn header_path(line: &str) -> Option<Vec<String>> {
        let t = line.trim_start();
        let (inner, close) = if let Some(rest) = t.strip_prefix("[[") {
            (rest, "]]")
        } else {
            (t.strip_prefix('[')?, "]")
        };
        let mut segments = Vec::new();
        let mut current = String::new();
        let mut chars = inner.chars().peekable();
        let mut rest_after = None;
        while let Some(c) = chars.next() {
            match c {
                '"' | '\'' => {
                    let quote = c;
                    loop {
                        match chars.next()? {
                            '\\' if quote == '"' => current.push(chars.next()?),
                            q if q == quote => break,
                            other => current.push(other),
                        }
                    }
                }
                '.' => segments.push(std::mem::take(&mut current).trim().to_string()),
                ']' => {
                    segments.push(std::mem::take(&mut current).trim().to_string());
                    rest_after = Some(chars.collect::<String>());
                    break;
                }
                c => current.push(c),
            }
        }
        let rest = rest_after?;
        let rest = if close == "]]" { rest.strip_prefix(']')? } else { rest.as_str() };
        let rest = rest.trim();
        if !(rest.is_empty() || rest.starts_with('#')) || segments.iter().any(String::is_empty) {
            return None;
        }
        Some(segments)
    }

    fn is_ours(path: &[String]) -> bool {
        path.first().is_some_and(|s| s == "mcp_servers") && path.get(1).is_some_and(|s| s == SERVER_NAME)
    }

    fn parse(text: &str) -> Result<Value, Refusal> {
        let table: toml::Table = toml::from_str(text).map_err(|_| Refusal::Unparseable)?;
        serde_json::to_value(table).map_err(|_| Refusal::Unparseable)
    }

    /// Byte ranges of the tables that make up SHOGUN's entry: from its header to the next header,
    /// less the blank and comment lines just before that header (they introduce the next table).
    fn our_blocks(text: &str) -> Vec<(usize, usize)> {
        let mut lines = Vec::new();
        let mut offset = 0;
        for line in text.split_inclusive('\n') {
            lines.push((offset, line));
            offset += line.len();
        }
        let mut blocks = Vec::new();
        let mut i = 0;
        while i < lines.len() {
            let (start, line) = lines[i];
            match header_path(line) {
                Some(path) if is_ours(&path) => {
                    let mut j = i + 1;
                    while j < lines.len() && header_path(lines[j].1).is_none() {
                        j += 1;
                    }
                    let mut end_line = j;
                    // Blank and comment lines before the next header introduce it, unless it is
                    // SHOGUN's own next table: then they are part of the entry.
                    let next_is_ours = lines
                        .get(j)
                        .and_then(|(_, l)| header_path(l))
                        .is_some_and(|p| is_ours(&p));
                    if j < lines.len() && !next_is_ours {
                        while end_line > i + 1 {
                            let t = lines[end_line - 1].1.trim();
                            if t.is_empty() || t.starts_with('#') {
                                end_line -= 1;
                            } else {
                                break;
                            }
                        }
                    }
                    let end = lines.get(end_line).map_or(text.len(), |(o, _)| *o);
                    blocks.push((start, end));
                    i = j;
                }
                _ => i += 1,
            }
        }
        blocks
    }

    /// `text` without `blocks`, and the offset (in the result) where the first one was.
    fn cut(text: &str, blocks: &[(usize, usize)]) -> (String, usize) {
        let mut out = String::with_capacity(text.len());
        let mut last = 0;
        let mut at = None;
        for &(start, end) in blocks {
            out.push_str(&text[last..start]);
            at.get_or_insert(out.len());
            last = end;
        }
        out.push_str(&text[last..]);
        let at = at.unwrap_or(out.len());
        (out, at)
    }

    pub(super) fn plan(
        client: McpClient,
        existing: Option<&str>,
        params: Option<&ServerParams<'_>>,
    ) -> Result<FilePlan, Refusal> {
        let base_digest = digest(existing);
        let text = existing.unwrap_or_default();
        let original = parse(text)?;
        let servers = original.get("mcp_servers");
        if servers.is_some_and(|v| !v.is_object()) {
            return Err(Refusal::UnexpectedShape);
        }
        let existing_entry = servers.and_then(|s| s.get(SERVER_NAME));
        let other_entries = servers
            .and_then(Value::as_object)
            .map_or(0, |s| s.keys().filter(|k| *k != SERVER_NAME).count());
        let blocks = our_blocks(text);
        if existing_entry.is_some() && blocks.is_empty() {
            return Err(Refusal::UnsupportedEntryForm);
        }
        // A header for our entry whose values are not all under it (a later `[mcp_servers]`
        // dotted key, say) reads back differently once cut; `verify` refuses that below.
        let before_raw = blocks
            .iter()
            .filter_map(|&(s, e)| text.get(s..e))
            .collect::<Vec<_>>()
            .join("");
        let before_raw = mask_existing(&before_raw, existing_entry.unwrap_or(&Value::Null));
        let block = params.map(|p| render_toml_entry(client, p));
        let intended = block.as_deref().map(parse).transpose()?.and_then(|v| {
            v.get("mcp_servers").and_then(|s| s.get(SERVER_NAME)).cloned()
        });
        let same = matches!((existing_entry, &intended), (Some(a), Some(b)) if a == b);
        let change = change_for(existing_entry.is_some(), params.is_some(), same);

        let new_contents = match (change, &block) {
            (Change::Unchanged | Change::Absent, _) => None,
            (Change::Remove, _) => {
                let (mut out, at) = cut(text, &blocks);
                // Keep one blank line where the entry was, not two.
                // The blank line that separated the entry goes with it: the one after it, or
                // (when it was the last thing in the file) the one SHOGUN added before it.
                let blank_before = at == 0 || out[..at].ends_with("\n\n");
                if blank_before && out[at..].starts_with('\n') {
                    out.remove(at);
                } else if at > 0 && at == out.len() && out.ends_with("\n\n") {
                    out.pop();
                }
                Some(out)
            }
            (Change::Replace, Some(block)) => {
                let (out, at) = cut(text, &blocks);
                let mut insert = block.clone();
                if at < out.len() && !out[at..].starts_with('\n') {
                    insert.push('\n');
                }
                Some(format!("{}{insert}{}", &out[..at], &out[at..]))
            }
            (Change::Add, Some(block)) => {
                let mut out = text.to_string();
                if !out.trim().is_empty() {
                    if !out.ends_with('\n') {
                        out.push('\n');
                    }
                    if !out.ends_with("\n\n") {
                        out.push('\n');
                    }
                } else {
                    out.clear();
                }
                out.push_str(block);
                Some(out)
            }
            (Change::Add | Change::Replace, None) => None,
        };
        if let Some(new) = &new_contents {
            verify(&original, new, intended.as_ref())?;
        }
        let after = match change {
            Change::Unchanged => before_raw.clone(),
            _ => block.unwrap_or_default(),
        };
        Ok(FilePlan {
            change,
            base_digest,
            diff: line_diff(before_raw.trim_end(), after.trim_end()),
            other_entries,
            new_contents,
        })
    }

    fn verify(original: &Value, new: &str, intended: Option<&Value>) -> Result<(), Refusal> {
        let written = parse(new).map_err(|_| Refusal::NotVerified)?;
        let strip = |v: &Value, created: bool| -> Value {
            let mut v = v.clone();
            if let Some(obj) = v.as_object_mut() {
                let emptied = obj
                    .get_mut("mcp_servers")
                    .and_then(Value::as_object_mut)
                    .map(|servers| {
                        servers.remove(SERVER_NAME);
                        servers.is_empty()
                    })
                    .unwrap_or(false);
                if created && emptied {
                    obj.remove("mcp_servers");
                }
            }
            v
        };
        let created = original.get("mcp_servers").is_none();
        if strip(original, false) != strip(&written, created) {
            return Err(Refusal::NotVerified);
        }
        let now = written.get("mcp_servers").and_then(|s| s.get(SERVER_NAME));
        if now != intended {
            return Err(Refusal::NotVerified);
        }
        Ok(())
    }
}

/// One subprocess run: the argv (an absolute program first) and whether a failure is expected and
/// harmless (removing an entry that is not there).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CliRun {
    pub argv: Vec<String>,
    pub may_fail: bool,
}

impl CliRun {
    /// The command as the dialog shows it: the vendor CLI's own words, shell-quoted, without the
    /// `/usr/bin/env PATH=…` wrapper.
    pub fn display(&self) -> String {
        let words = match self.argv.first().map(String::as_str) {
            Some(ENV_PROGRAM) => self.argv.get(2..).unwrap_or_default(),
            _ => &self.argv[..],
        };
        words.iter().map(|w| shell_word(w)).collect::<Vec<_>>().join(" ")
    }
}

/// The wrapper every vendor CLI run goes through, so the CLI finds its own interpreter: an app
/// opened from the Dock starts with `/usr/bin:/bin:/usr/sbin:/sbin` only, and an npm-installed
/// `claude` is a `#!/usr/bin/env node` script.
pub const ENV_PROGRAM: &str = "/usr/bin/env";

/// The directories searched for a vendor CLI, in order: the vendor's native installer, then
/// Homebrew and the usual npm prefixes. SHOGUN runs the first that exists and shows its path.
pub fn vendor_cli_candidates(program: &str, home: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![home.join(".local/bin")];
    if program == "claude" {
        dirs.push(home.join(".claude/local"));
    }
    dirs.extend(
        ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"].into_iter().map(PathBuf::from),
    );
    dirs.push(home.join(".npm-global/bin"));
    dirs.into_iter().map(|d| d.join(program)).collect()
}

fn env_prefix(program: &Path) -> Vec<String> {
    let dir = program.parent().map(|d| d.display().to_string()).unwrap_or_default();
    let mut path = vec![dir];
    for d in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"] {
        if !path.iter().any(|p| p == d) {
            path.push(d.to_string());
        }
    }
    vec![ENV_PROGRAM.to_string(), format!("PATH={}", path.join(":")), program.display().to_string()]
}

/// The arguments of `claude mcp add` for SHOGUN's entry, user scope. Shared with the copy-only
/// text ([`crate::client_config::cli_command`]), so what the user copies and what SHOGUN runs are
/// the same words.
pub fn claude_add_args(params: &ServerParams<'_>) -> Vec<String> {
    let mut args = vec!["mcp".to_string(), "add".to_string()];
    for (key, value) in params.env(McpClient::ClaudeCode) {
        args.push("--env".to_string());
        args.push(format!("{key}={value}"));
    }
    // `--scope user` keeps the entry out of a repository's `.mcp.json`. Another option must sit
    // between the last `--env` pair and the server name (the CLI reads a name right after `--env`
    // as one more pair), and everything after `--` goes to the server untouched.
    for word in ["--transport", "stdio", "--scope", "user", SERVER_NAME, "--"] {
        args.push(word.to_string());
    }
    args.push(params.command.to_string());
    args.extend(params.args(McpClient::ClaudeCode));
    args
}

fn claude_remove_args() -> Vec<String> {
    ["mcp", "remove", SERVER_NAME, "--scope", "user"].iter().map(|s| s.to_string()).collect()
}

/// Add SHOGUN to Claude Code: remove SHOGUN's own user-scope entry if there is one (`claude mcp
/// add` refuses a name that exists), then add it. The server is started with `--client
/// claude-code` (issue #963); no token is involved.
pub fn claude_code_add_runs(claude: &Path, command: &str, db_path: &str) -> Vec<CliRun> {
    let params = install_params(command, db_path);
    let mut remove = env_prefix(claude);
    remove.extend(claude_remove_args());
    let mut add = env_prefix(claude);
    add.extend(claude_add_args(&params));
    vec![CliRun { argv: remove, may_fail: true }, CliRun { argv: add, may_fail: false }]
}

/// Remove SHOGUN from Claude Code (user scope).
pub fn claude_code_remove_runs(claude: &Path) -> Vec<CliRun> {
    let mut remove = env_prefix(claude);
    remove.extend(claude_remove_args());
    vec![CliRun { argv: remove, may_fail: false }]
}

/// A POSIX shell word: as-is when it has nothing the shell would read, else single-quoted.
pub fn shell_word(word: &str) -> String {
    let plain = !word.is_empty()
        && word.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c));
    if plain {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

/// Why a confirmed plan was not written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyError {
    /// The client's folder does not exist: it has not run on this Mac. SHOGUN does not create it.
    AppNotFound,
    /// The file changed after the preview; the user reviews again.
    Changed,
    Refused(Refusal),
    Io(String),
}

/// What [`apply_file_plan`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub change: Change,
    /// Where the file as it was before is kept, when there was one and it changed.
    pub backup: Option<PathBuf>,
}

/// The backup kept next to a config file before SHOGUN changes it (one, replaced each time).
pub fn backup_path(path: &Path) -> PathBuf {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    path.with_file_name(format!("{name}.shogun-backup"))
}

/// Read a client's config file: `Ok(None)` when it does not exist; not UTF-8 is unparseable.
pub fn read_config(path: &Path) -> Result<Option<String>, ApplyError> {
    match std::fs::read(path) {
        Ok(bytes) => String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| ApplyError::Refused(Refusal::Unparseable)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ApplyError::Io(e.kind().to_string())),
    }
}

/// Apply the plan the user confirmed. `path` is the client's config file; `expected_digest` the
/// [`FilePlan::base_digest`] the dialog showed; `plan` re-plans from what is on disk now (the same
/// pure function that made the preview). Writes only when the file is still exactly what was
/// previewed; keeps a backup of the old file; replaces the file atomically (temporary file in the
/// same folder, synced, renamed), keeping its permissions. A symlinked file is written through.
pub fn apply_file_plan(
    path: &Path,
    expected_digest: &str,
    plan: impl FnOnce(Option<&str>) -> Result<FilePlan, Refusal>,
) -> Result<Applied, ApplyError> {
    apply_file_plan_with_backup(path, expected_digest, true, plan)
}

/// Apply a guarded edit while keeping the original backup from the first automatic connection.
fn apply_file_plan_with_backup(
    path: &Path,
    expected_digest: &str,
    update_backup: bool,
    plan: impl FnOnce(Option<&str>) -> Result<FilePlan, Refusal>,
) -> Result<Applied, ApplyError> {
    let target = match std::fs::canonicalize(path) {
        Ok(real) => real,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => path.to_path_buf(),
        Err(e) => return Err(ApplyError::Io(e.kind().to_string())),
    };
    let dir = target.parent().ok_or(ApplyError::AppNotFound)?;
    if !dir.is_dir() {
        return Err(ApplyError::AppNotFound);
    }
    let current = read_config(&target)?;
    if digest(current.as_deref()) != expected_digest {
        return Err(ApplyError::Changed);
    }
    let plan = plan(current.as_deref()).map_err(ApplyError::Refused)?;
    let Some(contents) = plan.new_contents() else {
        return Ok(Applied { change: plan.change, backup: None });
    };
    let mode = std::fs::metadata(&target).ok().map(|m| permissions_mode(&m));
    let backup = match (&current, update_backup) {
        (Some(old), true) => {
            let backup = backup_path(&target);
            write_atomically(&backup, old, mode)?;
            Some(backup)
        }
        _ => None,
    };
    write_atomically(&target, contents, mode)?;
    Ok(Applied { change: plan.change, backup })
}

#[cfg(unix)]
fn permissions_mode(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o777
}

#[cfg(not(unix))]
fn permissions_mode(_meta: &std::fs::Metadata) -> u32 {
    0o600
}

fn write_atomically(path: &Path, contents: &str, mode: Option<u32>) -> Result<(), ApplyError> {
    use std::io::Write;
    let io = |e: std::io::Error| ApplyError::Io(e.kind().to_string());
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.shogun-{}.tmp", std::process::id()));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // A new file is private to the user; an existing one keeps its mode.
            options.mode(mode.unwrap_or(0o600));
        }
        let mut file = options.open(&tmp).map_err(io)?;
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(mode)).map_err(io)?;
        }
        file.write_all(contents.as_bytes()).map_err(io)?;
        file.sync_all().map_err(io)?;
        std::fs::rename(&tmp, path).map_err(io)?;
        if let Some(dir) = path.parent().and_then(|d| std::fs::File::open(d).ok()) {
            let _ = dir.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// What `api.tokens.list` (`mount.clients[].install`) says about one-click setup for a client:
/// the method, what would be added (entry text or command), and that applying is in-app only.
/// Built from the placeholder paths the API faces use; reads no file.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct InstallPreview {
    pub method: InstallMethod,
    /// The file SHOGUN would edit (`~/…`), for [`InstallMethod::ConfigFile`].
    pub file: Option<String>,
    /// The entry text added to that file, or the commands run, for the methods that change
    /// something. Never a token.
    pub adds: Option<String>,
    /// Always `"in_app_only"`: applying needs the confirmation dialog in SHOGUN Settings.
    pub apply: &'static str,
}

/// [`InstallPreview`] for `client` with the given executable and database paths.
pub fn install_preview(client: McpClient, command: &str, db_path: &str) -> InstallPreview {
    let method = install_method(client);
    let home = Path::new("/HOME");
    let (file, adds) = match method {
        InstallMethod::ConfigFile => (
            config_file(client, home).map(|p| display_path(&p, home)),
            Some(entry_text(client, command, db_path)),
        ),
        InstallMethod::VendorCli => (
            None,
            Some(
                claude_code_add_runs(Path::new("claude"), command, db_path)
                    .iter()
                    .map(|run| run.display())
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
        ),
        InstallMethod::CopyOnly => (None, None),
    };
    InstallPreview { method, file, adds, apply: "in_app_only" }
}

/// Where the `shogun-mcp` server the app ships is, from the app's own executable path. The config
/// SHOGUN writes points at this file, so it must be one that stays put.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerLocation {
    /// Next to the app's executable (`ShogunAI.app/Contents/MacOS/shogun-mcp`).
    Bundled(PathBuf),
    /// The app runs from a mounted disk image: the path is gone once it is ejected.
    DiskImage,
    /// macOS runs the app from a randomised read-only copy (App Translocation): the path changes
    /// with every launch until the app is moved to Applications.
    Translocated,
    /// This build does not ship the server.
    Missing,
}

impl ServerLocation {
    /// Why this copy of the app cannot add itself to another app: the same id a blocked Add
    /// preview carries (`server_missing`, or `server_not_installed` for a disk image or a
    /// translocated copy, which do ship the server but at a path that will not last). `None` when
    /// the server is bundled where it will stay.
    pub fn unavailable_reason(&self) -> Option<&'static str> {
        match self {
            ServerLocation::Bundled(_) => None,
            ServerLocation::DiskImage | ServerLocation::Translocated => {
                Some(Blocked::ServerNotInstalled.id())
            }
            ServerLocation::Missing => Some(Blocked::ServerMissing.id()),
        }
    }
}

/// The file name of the bundled server (Tauri `externalBin` drops the target triple).
pub const SERVER_BINARY: &str = "shogun-mcp";

/// Where the bundled server is for an app whose executable is `current_exe`. `is_file` is the
/// filesystem probe (a closure so the rule is testable without an app bundle).
///
/// Only an executable inside an app bundle (`*.app/Contents/MacOS/`) has a bundled server. A
/// development run from a build tree (`target/debug/…`) may find a `shogun-mcp` next to it, but
/// writing that path into another app's config leaves an entry that breaks as soon as the tree is
/// rebuilt or cleaned: the stale launcher of issue #744. Such a run reports `Missing` and offers
/// the copyable setup instead.
pub fn bundled_server(current_exe: &Path, is_file: impl Fn(&Path) -> bool) -> ServerLocation {
    let text = current_exe.to_string_lossy();
    if text.starts_with("/Volumes/") {
        return ServerLocation::DiskImage;
    }
    if text.contains("/AppTranslocation/") {
        return ServerLocation::Translocated;
    }
    let Some(dir) = current_exe.parent().filter(|dir| inside_app_bundle(dir)) else {
        return ServerLocation::Missing;
    };
    let server = dir.join(SERVER_BINARY);
    if is_file(&server) {
        ServerLocation::Bundled(server)
    } else {
        ServerLocation::Missing
    }
}

/// Whether `dir` is an app bundle's executable folder: `<name>.app/Contents/MacOS`.
fn inside_app_bundle(dir: &Path) -> bool {
    let mut up = dir.components().rev().map(|c| c.as_os_str().to_string_lossy());
    matches!(
        (up.next(), up.next(), up.next()),
        (Some(macos), Some(contents), Some(app))
            if macos == "MacOS" && contents == "Contents" && app.ends_with(".app") && app.len() > ".app".len()
    )
}

/// Add or remove.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MountAction {
    Add,
    Remove,
}

impl MountAction {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "add" => Some(MountAction::Add),
            "remove" => Some(MountAction::Remove),
            _ => None,
        }
    }
}

/// Why the confirmation dialog has nothing to confirm. The user still has the entry to copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Blocked {
    /// Copy only (Claude Desktop).
    CopyOnly,
    /// This build has no bundled server to point the entry at.
    ServerMissing,
    /// The app runs from a disk image or a translocated copy: move it to Applications first.
    ServerNotInstalled,
    /// The vendor's CLI was not found in the places its installers use.
    CliNotFound,
    /// The client's folder does not exist: it has not run on this Mac.
    AppNotFound,
    /// The file could not be read.
    Unreadable,
    /// [`Refusal`] from the plan.
    Refused(Refusal),
}

impl Blocked {
    pub fn id(self) -> &'static str {
        match self {
            Blocked::CopyOnly => "copy_only",
            Blocked::ServerMissing => "server_missing",
            Blocked::ServerNotInstalled => "server_not_installed",
            Blocked::CliNotFound => "cli_not_found",
            Blocked::AppNotFound => "app_not_found",
            Blocked::Unreadable => "unreadable",
            Blocked::Refused(r) => r.id(),
        }
    }
}

/// What the confirmation dialog shows. Never a token and never another server's entry: the
/// commands, or SHOGUN's entry before and after.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MountPreview {
    pub client: &'static str,
    pub label: &'static str,
    pub action: MountAction,
    pub method: InstallMethod,
    /// `~/.cursor/mcp.json`, or the CLI path that runs.
    pub target: String,
    /// What happens: add / replace / unchanged / remove / absent. For the CLI, add or remove.
    pub change: Option<Change>,
    /// The commands run, in order (vendor CLI only).
    pub commands: Vec<String>,
    /// SHOGUN's entry before and after (config file only).
    pub diff: Vec<DiffLine>,
    /// Other servers in the file, left as they are.
    pub other_entries: usize,
    /// Where the old file is kept (config file only, when there is one and it changes).
    pub backup: Option<String>,
    /// Pass back to apply: it refuses unless the file (or the command list) is still this.
    pub digest: String,
    /// Set when there is nothing to confirm; `blocked_reason` is its id.
    #[serde(skip)]
    pub blocked: Option<Blocked>,
    pub blocked_reason: Option<&'static str>,
}

/// Everything the flow needs to know about this Mac, found by the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountContext {
    pub home: PathBuf,
    pub server: ServerLocation,
    pub db_path: String,
    /// The vendor CLI for [`InstallMethod::VendorCli`] clients, when found.
    pub cli: Option<PathBuf>,
}

impl MountContext {
    fn server_path(&self) -> Result<String, Blocked> {
        match &self.server {
            ServerLocation::Bundled(path) => Ok(path.display().to_string()),
            ServerLocation::DiskImage | ServerLocation::Translocated => {
                Err(Blocked::ServerNotInstalled)
            }
            ServerLocation::Missing => Err(Blocked::ServerMissing),
        }
    }
}

/// The CLI runs for `action`.
fn cli_runs(ctx: &MountContext, action: MountAction) -> Result<Vec<CliRun>, Blocked> {
    let cli = ctx.cli.as_deref().ok_or(Blocked::CliNotFound)?;
    Ok(match action {
        MountAction::Add => claude_code_add_runs(cli, &ctx.server_path()?, &ctx.db_path),
        MountAction::Remove => claude_code_remove_runs(cli),
    })
}

/// The digest of a command list: apply runs these commands only if they are still exactly what
/// the dialog showed.
fn runs_digest(runs: &[CliRun]) -> String {
    let joined = runs.iter().map(|r| r.argv.join("\u{0}")).collect::<Vec<_>>().join("\n");
    digest(Some(&joined))
}

/// The plan for a config-file client, from the file's current text.
pub fn file_plan(
    client: McpClient,
    action: MountAction,
    ctx: &MountContext,
    existing: Option<&str>,
) -> Result<FilePlan, Blocked> {
    match action {
        MountAction::Add => plan_add(client, existing, &ctx.server_path()?, &ctx.db_path),
        MountAction::Remove => guarded_remove_plan(client, ctx, existing),
    }
    .map_err(Blocked::Refused)
}

/// The confirmation dialog's content for `client` and `action`. `read` reads the client's config
/// file (the app passes [`read_config`]), including read-only inspection before a CLI Remove.
pub fn mount_preview(
    client: McpClient,
    action: MountAction,
    ctx: &MountContext,
    read: impl FnOnce(&Path) -> Result<Option<String>, ApplyError>,
) -> MountPreview {
    let method = install_method(client);
    let mut preview = MountPreview {
        client: client.id(),
        label: client.label(),
        action,
        method,
        target: String::new(),
        change: None,
        commands: Vec::new(),
        diff: Vec::new(),
        other_entries: 0,
        backup: None,
        digest: String::new(),
        blocked: None,
        blocked_reason: None,
    };
    let result: Result<(), Blocked> = (|| {
        match method {
            InstallMethod::CopyOnly => Err(Blocked::CopyOnly),
            InstallMethod::VendorCli => {
                if action == MountAction::Add {
                    ctx.server_path()?;
                }
                let runs = cli_runs(ctx, action)?;
                let removal = if action == MountAction::Remove {
                    let existing = read(&connection_path(client, &ctx.home))
                        .map_err(|_| Blocked::Unreadable)?;
                    Some(connection_plan(client, action, ctx, existing.as_deref())
                        .map_err(Blocked::Refused)?)
                } else {
                    None
                };
                preview.target = ctx.cli.as_deref().map(|p| display_path(p, &ctx.home)).unwrap_or_default();
                preview.commands = runs.iter().map(CliRun::display).collect();
                preview.digest = removal.as_ref().map(|p| removal_digest(&runs, p))
                    .unwrap_or_else(|| runs_digest(&runs));
                preview.change = Some(removal.map(|p| p.change).unwrap_or(Change::Add));
                Ok(())
            }
            InstallMethod::ConfigFile => {
                let path = connection_path(client, &ctx.home);
                preview.target = display_path(&path, &ctx.home);
                if !path.parent().is_some_and(Path::is_dir) {
                    return Err(Blocked::AppNotFound);
                }
                let existing = read(&path).map_err(|e| match e {
                    ApplyError::Refused(r) => Blocked::Refused(r),
                    ApplyError::AppNotFound => Blocked::AppNotFound,
                    ApplyError::Changed | ApplyError::Io(_) => Blocked::Unreadable,
                })?;
                let plan = file_plan(client, action, ctx, existing.as_deref())?;
                preview.change = Some(plan.change);
                preview.diff = plan.diff.clone();
                preview.other_entries = plan.other_entries;
                preview.digest = plan.base_digest.clone();
                if existing.is_some() && plan.new_contents().is_some() {
                    preview.backup = Some(display_path(&backup_path(&path), &ctx.home));
                }
                Ok(())
            }
        }
    })();
    if let Err(blocked) = result {
        preview.blocked = Some(blocked);
        preview.blocked_reason = Some(blocked.id());
    }
    preview
}

/// Why applying failed, as the dialog reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountError {
    Blocked(Blocked),
    /// The file or the command list changed since the preview: review again.
    Changed,
    /// Writing or a command failed; the text is safe to show (no file content, no token).
    Failed(String),
}

impl MountError {
    pub fn id(&self) -> &'static str {
        match self {
            MountError::Blocked(b) => b.id(),
            MountError::Changed => "changed",
            MountError::Failed(_) => "failed",
        }
    }
}

/// Apply the change the user confirmed. `run` executes one [`CliRun`] and says whether it
/// succeeded (the app passes shogun-core's subprocess runner). For a config file, the write is
/// [`apply_file_plan`]: refused unless the file still has the previewed digest.
pub fn mount_apply(
    client: McpClient,
    action: MountAction,
    ctx: &MountContext,
    expected_digest: &str,
    mut run: impl FnMut(&CliRun) -> Result<(), String>,
) -> Result<Change, MountError> {
    let method = install_method(client);
    match method {
        InstallMethod::CopyOnly => Err(MountError::Blocked(Blocked::CopyOnly)),
        InstallMethod::VendorCli => {
            let runs = cli_runs(ctx, action).map_err(MountError::Blocked)?;
            if action == MountAction::Remove {
                let current = read_config(&connection_path(client, &ctx.home))
                    .map_err(|_| MountError::Blocked(Blocked::Unreadable))?;
                let plan = connection_plan(client, action, ctx, current.as_deref())
                    .map_err(|r| MountError::Blocked(Blocked::Refused(r)))?;
                if removal_digest(&runs, &plan) != expected_digest {
                    return Err(MountError::Changed);
                }
                if plan.change == Change::Absent {
                    return Ok(Change::Absent);
                }
            } else if runs_digest(&runs) != expected_digest {
                return Err(MountError::Changed);
            }
            for step in &runs {
                match run(step) {
                    Ok(()) => {}
                    Err(_) if step.may_fail => {}
                    Err(message) => return Err(MountError::Failed(message)),
                }
            }
            Ok(if action == MountAction::Add { Change::Add } else { Change::Remove })
        }
        InstallMethod::ConfigFile => {
            let path = connection_path(client, &ctx.home);
            if action == MountAction::Add {
                ctx.server_path().map_err(MountError::Blocked)?;
            }
            apply_file_plan(&path, expected_digest, |existing| {
                match file_plan(client, action, ctx, existing) {
                    Ok(plan) => Ok(plan),
                    Err(Blocked::Refused(r)) => Err(r),
                    Err(_) => Err(Refusal::CopyOnly),
                }
            })
            .map(|applied| applied.change)
            .map_err(|e| match e {
                ApplyError::AppNotFound => MountError::Blocked(Blocked::AppNotFound),
                ApplyError::Changed => MountError::Changed,
                ApplyError::Refused(r) => MountError::Blocked(Blocked::Refused(r)),
                ApplyError::Io(kind) => MountError::Failed(kind),
            })
        }
    }
}

/// The client for a Settings id.
pub fn client_by_id(id: &str) -> Option<McpClient> {
    crate::client_config::ALL_CLIENTS.iter().copied().find(|c| c.id() == id)
}

/// Read-only inspection path for Claude Code; only its vendor CLI may write it.
pub fn connection_path(client: McpClient, home: &Path) -> PathBuf {
    config_file(client, home).unwrap_or_else(|| {
        claude_config_path(home, std::env::var_os("CLAUDE_CONFIG_DIR").as_deref().map(Path::new))
    })
}

fn claude_config_path(home: &Path, config_dir: Option<&Path>) -> PathBuf {
    config_dir.filter(|p| !p.as_os_str().is_empty())
        .map(|dir| dir.join(".claude.json"))
        .unwrap_or_else(|| home.join(".claude.json"))
}

fn removal_digest(runs: &[CliRun], plan: &FilePlan) -> String {
    digest(Some(&format!("{}:{}", runs_digest(runs), plan.base_digest)))
}

/// Refuse to remove a same-name custom entry, including extra arguments or environment keys.
fn guarded_remove_plan(
    client: McpClient,
    ctx: &MountContext,
    existing: Option<&str>,
) -> Result<FilePlan, Refusal> {
    let removal = if client == McpClient::ClaudeCode {
        json::plan(client, existing, None)?
    } else {
        plan_remove(client, existing)?
    };
    if removal.change != Change::Absent {
        let command = ctx.server_path().map_err(|_| Refusal::NotVerified)?;
        let expected = if client == McpClient::ClaudeCode {
            json::plan(client, existing, Some(&install_params(&command, &ctx.db_path)))?
        } else {
            plan_add(client, existing, &command, &ctx.db_path)?
        };
        if expected.change != Change::Unchanged {
            return Err(Refusal::NotVerified);
        }
    }
    Ok(removal)
}

/// Plan only our entry, refusing malformed or unsupported config shapes.
pub fn connection_plan(
    client: McpClient,
    action: MountAction,
    ctx: &MountContext,
    existing: Option<&str>,
) -> Result<FilePlan, Refusal> {
    match action {
        MountAction::Remove => guarded_remove_plan(client, ctx, existing),
        MountAction::Add if client == McpClient::ClaudeCode => {
            let command = ctx.server_path().map_err(|_| Refusal::CopyOnly)?;
            json::plan(client, existing, Some(&install_params(&command, &ctx.db_path)))
        }
        MountAction::Add => plan_add(client, existing,
            &ctx.server_path().map_err(|_| Refusal::CopyOnly)?, &ctx.db_path),
    }
}

/// Cleanup after revocation. Claude Code always uses its CLI, including automatic Undo.
pub fn cleanup_connection(
    client: McpClient,
    ctx: &MountContext,
    notice: Option<&crate::memory_api_settings::AutoConnectNotice>,
    run: impl FnMut(&CliRun) -> Result<(), String>,
) -> Result<(), MountError> {
    let path = connection_path(client, &ctx.home);
    let current = read_config(&path).map_err(|_| MountError::Blocked(Blocked::Unreadable))?;
    connection_plan(client, MountAction::Remove, ctx, current.as_deref())
        .map_err(|r| MountError::Blocked(Blocked::Refused(r)))?;
    if let Some(notice) = notice {
        if notice.before_digest == notice.after_digest {
            return Ok(());
        }
        if client != McpClient::ClaudeCode {
            return undo_connection_file(&path, notice)
                .map_err(|_| MountError::Changed);
        }
    }
    let preview = mount_preview(client, MountAction::Remove, ctx, read_config);
    if let Some(blocked) = preview.blocked {
        return Err(MountError::Blocked(blocked));
    }
    mount_apply(client, MountAction::Remove, ctx, &preview.digest, run).map(|_| ())
}

/// Recheck under the caller's settings lock immediately before applying a planned edit.
pub fn auto_connect_allowed(settings: &crate::memory_api_settings::Settings, client: &str) -> bool {
    let Some(id) = crate::memory_api_settings::grant_id(client) else {
        return false;
    };
    settings.enabled
        && settings.auto_connect
        && !settings.auto_connect_declined.iter().any(|c| c == id)
}

/// Apply an automatic edit only after a fresh settings read under the caller's lock.
/// A differing entry always needs the explicit Replace flow.
pub fn apply_auto_connection_file(
    client: McpClient,
    ctx: &MountContext,
    expected_digest: &str,
    settings: &crate::memory_api_settings::Settings,
) -> Result<Option<Applied>, ApplyError> {
    if client == McpClient::ClaudeCode {
        return Err(ApplyError::Refused(Refusal::CopyOnly));
    }
    if !auto_connect_allowed(settings, client.id()) {
        return Ok(None);
    }
    apply_file_plan(
        &connection_path(client, &ctx.home),
        expected_digest,
        |existing| {
            let plan = connection_plan(client, MountAction::Add, ctx, existing)?;
            if plan.change == Change::Replace {
                return Err(Refusal::NotVerified);
            }
            Ok(plan)
        },
    )
    .map(Some)
}

/// True only for the token-free connection shape SHOGUN writes automatically. Custom or
/// ambiguous entries stay under the user's control.
pub fn is_managed_auto_connection(client: McpClient, existing: Option<&str>) -> bool {
    if client == McpClient::ClaudeCode {
        return false;
    }
    let Some(text) = existing else { return false };
    let value: serde_json::Value = match client.format() {
        crate::client_config::ConfigFormat::Json => match serde_json::from_str(text) {
            Ok(value) => value,
            Err(_) => return false,
        },
        crate::client_config::ConfigFormat::Toml => {
            let parsed: toml::Table = match toml::from_str(text) {
                Ok(value) => value,
                Err(_) => return false,
            };
            match serde_json::to_value(parsed) {
                Ok(value) => value,
                Err(_) => return false,
            }
        }
    };
    let entry = match client.format() {
        crate::client_config::ConfigFormat::Json => value
            .get("mcpServers")
            .and_then(|servers| servers.get(SERVER_NAME)),
        crate::client_config::ConfigFormat::Toml => value
            .get("mcp_servers")
            .and_then(|servers| servers.get(SERVER_NAME)),
    };
    let Some(entry) = entry.and_then(serde_json::Value::as_object) else { return false };
    if entry.len() != 3
        || entry.keys().any(|key| !["command", "args", "env"].contains(&key.as_str()))
    {
        return false;
    }
    // Automatic connection only ever writes a bundled helper; a dev-build path is the user's.
    let Some(command) = entry.get("command").and_then(serde_json::Value::as_str).map(Path::new)
    else {
        return false;
    };
    if command.file_name().and_then(|name| name.to_str()) != Some(SERVER_BINARY)
        || !command.parent().is_some_and(inside_app_bundle)
    {
        return false;
    }
    let Some(expected_client) = crate::memory_api_settings::grant_id(client.id()) else {
        return false;
    };
    let args = entry.get("args").and_then(serde_json::Value::as_array);
    if !args.is_some_and(|args| {
        args.is_empty()
            || args.len() == 2
                && args[0].as_str() == Some("--client")
                && args[1].as_str() == Some(expected_client)
    }) {
        return false;
    }
    let Some(env) = entry.get("env").and_then(serde_json::Value::as_object) else {
        return false;
    };
    env.len() == 1
        && env
            .get(DB_PATH_ENV)
            .and_then(serde_json::Value::as_str)
            .is_some_and(|path| !path.is_empty())
}

/// Refresh an existing exact SHOGUN automatic entry. Missing or custom entries are not recreated
/// or replaced. Existing backup remains the user's pre-connection config.
pub fn apply_auto_refresh_file(
    client: McpClient,
    ctx: &MountContext,
    expected_digest: &str,
    settings: &crate::memory_api_settings::Settings,
) -> Result<Option<Applied>, ApplyError> {
    if client == McpClient::ClaudeCode {
        return Err(ApplyError::Refused(Refusal::CopyOnly));
    }
    if !auto_connect_allowed(settings, client.id()) {
        return Ok(None);
    }
    let path = connection_path(client, &ctx.home);
    let current = read_config(&path)?;
    if digest(current.as_deref()) != expected_digest {
        return Err(ApplyError::Changed);
    }
    if !is_managed_auto_connection(client, current.as_deref()) {
        return Ok(None);
    }
    apply_file_plan_with_backup(&path, expected_digest, false, |existing| {
        if !is_managed_auto_connection(client, existing) {
            return Err(Refusal::NotVerified);
        }
        connection_plan(client, MountAction::Add, ctx, existing)
    })
    .map(Some)
}

/// Roll back a failed refresh to its immediate pre-refresh bytes, without changing the original
/// automatic-connection backup. The digest guard preserves any edit made after the refresh.
pub fn rollback_auto_refresh_file(
    path: &Path,
    expected_after_digest: &str,
    before_refresh: &str,
) -> Result<(), ApplyError> {
    let target = std::fs::canonicalize(path).map_err(|e| ApplyError::Io(e.kind().to_string()))?;
    let current = read_config(&target)?;
    if digest(current.as_deref()) != expected_after_digest {
        return Err(ApplyError::Changed);
    }
    let mode = std::fs::metadata(&target).ok().map(|m| permissions_mode(&m));
    write_atomically(&target, before_refresh, mode)
}

/// Automatic connection uses the vendor CLI for Claude Code and guarded edits elsewhere.
pub fn apply_auto_connection(
    client: McpClient,
    ctx: &MountContext,
    expected_digest: &str,
    settings: &crate::memory_api_settings::Settings,
    mut run: impl FnMut(&CliRun) -> Result<(), String>,
) -> Result<Option<Applied>, ApplyError> {
    if client != McpClient::ClaudeCode {
        return apply_auto_connection_file(client, ctx, expected_digest, settings);
    }
    if !auto_connect_allowed(settings, client.id()) {
        return Ok(None);
    }
    let current = read_config(&connection_path(client, &ctx.home))?;
    if digest(current.as_deref()) != expected_digest {
        return Err(ApplyError::Changed);
    }
    let plan = connection_plan(client, MountAction::Add, ctx, current.as_deref())
        .map_err(ApplyError::Refused)?;
    match plan.change {
        Change::Replace => return Err(ApplyError::Refused(Refusal::NotVerified)),
        Change::Unchanged => {}
        _ => {
            // No remove step: automatic setup must never replace an existing entry.
            let steps = cli_runs(ctx, MountAction::Add).map_err(|_| ApplyError::Refused(Refusal::CopyOnly))?;
            for step in steps.iter().filter(|step| !step.may_fail) {
                run(step).map_err(ApplyError::Io)?;
            }
        }
    }
    Ok(Some(Applied { change: plan.change, backup: None }))
}

/// Undo only the bytes this automatic edit wrote. Never restore an overwritten backup or
/// overwrite a later user edit. No config content is copied into preferences.
pub fn undo_connection_file(
    path: &Path,
    notice: &crate::memory_api_settings::AutoConnectNotice,
) -> Result<(), ApplyError> {
    let target = std::fs::canonicalize(path).map_err(|e| ApplyError::Io(e.kind().to_string()))?;
    let current = read_config(&target)?;
    if digest(current.as_deref()) != notice.after_digest {
        return Err(ApplyError::Changed);
    }
    if notice.before_digest == notice.after_digest {
        return Ok(());
    }
    if notice.before_digest == ABSENT_DIGEST {
        return std::fs::remove_file(&target).map_err(|e| ApplyError::Io(e.kind().to_string()));
    }
    let before = read_config(&backup_path(&target))?;
    if digest(before.as_deref()) != notice.before_digest {
        return Err(ApplyError::Changed);
    }
    let before = before.ok_or(ApplyError::Changed)?;
    let mode = std::fs::metadata(&target)
        .ok()
        .map(|m| permissions_mode(&m));
    write_atomically(&target, &before, mode)
}

/// Which found AI apps the app adds itself to on its own (owner decision 2026-10-14), with
/// `read` access. Nothing unless the Memory API is on, automatic connection is on, the plan
/// includes the Memory API (`memory_api`) and the server is bundled where it will stay (never a
/// build-tree path, issue #744). Skips an app already connected or one the user removed. Apps
/// sharing a connection (Codex CLI and the ChatGPT desktop app) come out once. Running it again
/// after its connections were made returns nothing.
pub fn auto_connect_plan(
    discovered: &[crate::client_discovery::DiscoveredTool],
    connected: &[String],
    settings: &crate::memory_api_settings::Settings,
    server: &ServerLocation,
    memory_api: bool,
) -> Vec<McpClient> {
    use crate::memory_api_settings::grant_id;
    let bundled = matches!(server, ServerLocation::Bundled(_));
    if !(settings.enabled && settings.auto_connect && memory_api && bundled) {
        return Vec::new();
    }
    let mut seen: Vec<&str> = Vec::new();
    let mut out = Vec::new();
    for tool in discovered {
        // A tool without a setup id may still be a client (the ChatGPT desktop app).
        let Some(client) = client_by_id(tool.setup_client.unwrap_or(tool.id)) else { continue };
        let Some(id) = grant_id(client.id()) else { continue };
        let skip = seen.contains(&id)
            || connected.iter().any(|c| c == id)
            || settings.auto_connect_declined.iter().any(|c| c == id);
        seen.push(id);
        if !skip {
            out.push(client);
        }
    }
    out
}

#[cfg(test)]
mod tests;
