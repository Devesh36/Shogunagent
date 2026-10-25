//! ChatGPT "Computer History" memories → `shogun-memory-import/v1` (issue #391).
//!
//! The ChatGPT Mac app writes plaintext Markdown memories under a folder in the user's home
//! directory. **SHOGUN never looks there on its own**: there is no default path in this module, no
//! directory walk beyond a folder the user named, no watcher. [`read_explicit_paths`] reads only
//! the files / folders it is handed (a drop onto the Import memory screen, or a path typed on the
//! CLI), and the API faces never take a path at all — they take file *contents*
//! ([`MarkdownFile`]), so an AI caller cannot make SHOGUN open another app's data directory.
//!
//! The conversion is deterministic and local (no model call). The real file layout was not
//! available when this was written, so the parser targets a small **assumed** structure and is
//! tolerant of everything else:
//!
//! ```text
//! ---                         optional front matter: skipped
//! title: …
//! ---
//! # Anything                  headings of any level open a section
//! ## Preferences              → profile.preferences        (also: likes, dislikes)
//! ## Working style            → profile.working_style      (how I work, habits, workflow)
//! ## Goals                    → profile.goals              (objectives, priorities)
//! ## Constraints              → profile.constraints        (limitations, boundaries)
//! ## People                   → entities, type person      (contacts, collaborators, team)
//! ## Projects                 → entities, type project
//! ## Tools                    → entities, type tool        (apps, software, stack)
//! ## Organizations            → entities, type organization (companies, clients, employers)
//! ## Topics                   → entities, type topic       (interests, themes)
//! ## Open loops               → open_loops                 (todo, tasks, next steps, follow-ups,
//!                                                            action items, pending)
//! ## <anything else>          → open_loops, status uncertain, confidence low
//! - item                      one bullet (-, *, +, 1.) = one item; `- [x]` = done
//!   continuation / sub-bullet appended to the item above
//! **Name** — summary          entity bullets split at —, –, " - " or ":" into name / summary
//! ```
//!
//! Text before the first heading is an unknown section. Paragraphs (non-bullet lines) are one item
//! each. Code fences are skipped. Recognised sections are `medium` confidence, unknown ones `low`
//! and `uncertain`; the import cap then applies as for any package. Every item's evidence names the
//! file (base name only) and the section heading.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{json, Value};

use super::{normalize_key, ImportError, FORMAT, MAX_REQUEST_BYTES};

/// Most files one conversion takes.
pub const MAX_FILES: usize = 200;
/// Longest file name kept (base name only).
const MAX_FILE_NAME_CHARS: usize = 255;
/// Evidence is capped to the package's text limit.
const MAX_EVIDENCE_CHARS: usize = super::package::MAX_TEXT_CHARS;

/// One Computer History file the user handed over: its base name and its contents.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarkdownFile {
    pub name: String,
    pub markdown: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Profile(&'static str),
    Entity(&'static str),
    OpenLoops,
    Unknown,
}

/// Exact (normalised) headings, then single keywords anywhere in the heading.
const EXACT: &[(&str, Target)] = &[
    ("working style", Target::Profile("working_style")),
    ("work style", Target::Profile("working_style")),
    ("how i work", Target::Profile("working_style")),
    ("open loops", Target::OpenLoops),
    ("to do", Target::OpenLoops),
    ("next steps", Target::OpenLoops),
    ("follow ups", Target::OpenLoops),
    ("action items", Target::OpenLoops),
    ("areas of interest", Target::Entity("topic")),
];
const KEYWORDS: &[(&str, Target)] = &[
    ("preferences", Target::Profile("preferences")),
    ("preference", Target::Profile("preferences")),
    ("likes", Target::Profile("preferences")),
    ("dislikes", Target::Profile("preferences")),
    ("habits", Target::Profile("working_style")),
    ("workflow", Target::Profile("working_style")),
    ("goals", Target::Profile("goals")),
    ("goal", Target::Profile("goals")),
    ("objectives", Target::Profile("goals")),
    ("priorities", Target::Profile("goals")),
    ("constraints", Target::Profile("constraints")),
    ("limitations", Target::Profile("constraints")),
    ("boundaries", Target::Profile("constraints")),
    ("people", Target::Entity("person")),
    ("contacts", Target::Entity("person")),
    ("collaborators", Target::Entity("person")),
    ("team", Target::Entity("person")),
    ("colleagues", Target::Entity("person")),
    ("projects", Target::Entity("project")),
    ("project", Target::Entity("project")),
    ("tools", Target::Entity("tool")),
    ("apps", Target::Entity("tool")),
    ("software", Target::Entity("tool")),
    ("stack", Target::Entity("tool")),
    ("organizations", Target::Entity("organization")),
    ("organisations", Target::Entity("organization")),
    ("companies", Target::Entity("organization")),
    ("clients", Target::Entity("organization")),
    ("employers", Target::Entity("organization")),
    ("topics", Target::Entity("topic")),
    ("interests", Target::Entity("topic")),
    ("themes", Target::Entity("topic")),
    ("todo", Target::OpenLoops),
    ("todos", Target::OpenLoops),
    ("tasks", Target::OpenLoops),
    ("pending", Target::OpenLoops),
    ("follow-ups", Target::OpenLoops),
];

fn classify(heading: &str) -> Target {
    let key = normalize_key(heading).replace('-', " ");
    if let Some((_, t)) = EXACT.iter().find(|(k, _)| *k == key) {
        return *t;
    }
    let raw = normalize_key(heading);
    raw.split(|c: char| !c.is_alphanumeric() && c != '-')
        .find_map(|word| KEYWORDS.iter().find(|(k, _)| *k == word).map(|(_, t)| *t))
        .unwrap_or(Target::Unknown)
}

/// One raw item: its text and whether it was a checked box.
struct RawItem {
    text: String,
    done: bool,
}

struct Section {
    heading: String,
    target: Target,
    items: Vec<RawItem>,
}

/// `- x`, `* x`, `+ x`, `1. x`, `1) x` → the text after the marker.
fn bullet_text(line: &str) -> Option<&str> {
    let t = line.trim_start();
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = t.strip_prefix(marker) {
            return Some(rest);
        }
    }
    let digits = t.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0 && digits <= 3 {
        let rest = &t[digits..];
        if let Some(rest) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return Some(rest);
        }
    }
    None
}

fn heading_text(line: &str) -> Option<&str> {
    let t = line.trim_start();
    let hashes = t.bytes().take_while(|b| *b == b'#').count();
    if (1..=6).contains(&hashes) && t[hashes..].starts_with(' ') {
        Some(t[hashes..].trim().trim_end_matches('#').trim())
    } else {
        None
    }
}

/// Split one file into sections of raw items.
fn sections(markdown: &str) -> Vec<Section> {
    let mut out = vec![Section { heading: String::new(), target: Target::Unknown, items: Vec::new() }];
    let mut lines = markdown.lines().peekable();
    // Front matter: only when the very first line is `---`.
    if lines.peek().map(|l| l.trim()) == Some("---") {
        lines.next();
        for line in lines.by_ref() {
            if line.trim() == "---" {
                break;
            }
        }
    }
    let mut in_fence = false;
    let mut paragraph_open = false;
    for line in lines {
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            paragraph_open = false;
            continue;
        }
        if in_fence {
            continue;
        }
        let Some(section) = out.last_mut() else { continue };
        if trimmed.is_empty() {
            paragraph_open = false;
            continue;
        }
        if let Some(h) = heading_text(line) {
            out.push(Section { heading: h.to_string(), target: classify(h), items: Vec::new() });
            paragraph_open = false;
            continue;
        }
        let indented = line.starts_with("  ") || line.starts_with('\t');
        if let Some(text) = bullet_text(line) {
            let (text, done) = match text.get(..4) {
                Some("[ ] ") => (&text[4..], false),
                Some("[x] " | "[X] ") => (&text[4..], true),
                _ => (text, false),
            };
            match section.items.last_mut() {
                // A sub-bullet describes the item above it.
                Some(prev) if indented => {
                    prev.text.push_str("; ");
                    prev.text.push_str(text.trim());
                }
                _ => section.items.push(RawItem { text: text.trim().to_string(), done }),
            }
            paragraph_open = false;
            continue;
        }
        // Plain text: continues the item above when indented or inside a paragraph.
        match section.items.last_mut() {
            Some(prev) if indented || paragraph_open => {
                prev.text.push(' ');
                prev.text.push_str(trimmed);
            }
            _ => section.items.push(RawItem { text: trimmed.to_string(), done: false }),
        }
        paragraph_open = true;
    }
    out.retain(|s| !s.items.is_empty());
    out
}

/// `**Name** — summary` / `Name: summary` → (name, summary).
fn split_entity(text: &str) -> (String, Option<String>) {
    let strip = |s: &str| {
        s.trim()
            .trim_matches(|c| matches!(c, '*' | '_' | '`'))
            .trim()
            .to_string()
    };
    for sep in [" — ", " – ", " - ", "—", "–", ": "] {
        if let Some((name, rest)) = text.split_once(sep) {
            let name = strip(name);
            let rest = rest.trim();
            if !name.is_empty() && !rest.is_empty() {
                return (name, Some(rest.to_string()));
            }
        }
    }
    (strip(text), None)
}

fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

fn base_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    truncate(base, MAX_FILE_NAME_CHARS)
}

/// Convert the files the user handed over into one v1 package (as JSON, so the shared
/// [`super::parse_package`] validates it exactly like a pasted package).
pub fn convert(files: &[MarkdownFile]) -> Result<Value, ImportError> {
    if files.is_empty() {
        return Err(ImportError::Invalid("invalid computer_history: no files"));
    }
    if files.len() > MAX_FILES {
        return Err(ImportError::Invalid("invalid computer_history: too many files"));
    }
    if files.iter().map(|f| f.markdown.len()).sum::<usize>() > MAX_REQUEST_BYTES {
        return Err(ImportError::Invalid("invalid computer_history: too large"));
    }
    let mut profile = json!({ "preferences": [], "working_style": [], "goals": [], "constraints": [] });
    let mut entities = Vec::new();
    let mut open_loops = Vec::new();
    for file in files {
        let name = base_name(&file.name);
        for section in sections(&file.markdown) {
            let evidence = |heading: &str| {
                let where_ = if heading.is_empty() { "top of file".to_string() } else { format!("section \"{heading}\"") };
                truncate(&format!("Computer History file {name}, {where_}"), MAX_EVIDENCE_CHARS)
            };
            let evidence = evidence(&section.heading);
            for item in section.items {
                match section.target {
                    Target::Profile(key) => {
                        if let Some(list) = profile.get_mut(key).and_then(Value::as_array_mut) {
                            list.push(json!({
                                "value": item.text, "confidence": "medium",
                                "evidence": evidence, "status": "active",
                            }));
                        }
                    }
                    Target::Entity(kind) => {
                        let (entity, summary) = split_entity(&item.text);
                        entities.push(json!({
                            "name": entity, "type": kind, "summary": summary, "aliases": [],
                            "confidence": "medium", "evidence": evidence, "status": "active",
                        }));
                    }
                    Target::OpenLoops => open_loops.push(json!({
                        "summary": item.text,
                        "status": if item.done { "done" } else { "open" },
                        "confidence": "medium", "evidence": evidence,
                    })),
                    Target::Unknown => open_loops.push(json!({
                        "summary": item.text, "status": "uncertain",
                        "confidence": "low", "evidence": evidence,
                    })),
                }
            }
        }
    }
    Ok(json!({
        "format": FORMAT,
        "generated_at": null,
        "source": {
            "platform": "computer_history",
            "coverage_note": format!(
                "{} Computer History file(s) the user provided, converted on this Mac without a model.",
                files.len()
            ),
        },
        "profile": profile,
        "entities": entities,
        "relationships": [],
        "open_loops": open_loops,
        "corrections_needed": [],
    }))
}

/// Why explicit paths could not be read. Fixed messages — never the path (it names the user).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadError {
    NoPaths,
    NotFound,
    NotMarkdown,
    TooLarge,
    TooMany,
    Unreadable,
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ReadError::NoPaths => "invalid path: name the Computer History files or folder to import",
            ReadError::NotFound => "invalid path: not found",
            ReadError::NotMarkdown => "invalid path: not a Markdown file",
            ReadError::TooLarge => "invalid path: files too large to import",
            ReadError::TooMany => "invalid path: too many files",
            ReadError::Unreadable => "invalid path: could not read the file as UTF-8 text",
        })
    }
}

fn is_markdown(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"))
}

/// Read exactly the files the user named: a Markdown file is read; a folder contributes the
/// Markdown files directly inside it (not its subfolders, and not hidden ones Finder does not
/// show, such as the binary `._name.md` copies macOS leaves on non-APFS drives). There is no
/// default: an empty list is an error, and nothing outside the given paths is read.
///
/// Links are never followed: a symbolic link inside a named folder is skipped like a subfolder,
/// and a named path that is itself a link (only its last component is checked; links in its
/// parent folders resolve as usual), or is not a regular file or folder, is `Unreadable`. A file
/// is read only if the opened file is the same regular file that was listed (device + inode), so
/// a link swapped in after listing can still be opened — a FIFO can block that open — but is not
/// read. Reads stop at the byte cap, counting the bytes actually read.
pub fn read_explicit_paths(paths: &[PathBuf]) -> Result<Vec<MarkdownFile>, ReadError> {
    if paths.is_empty() {
        return Err(ReadError::NoPaths);
    }
    // Each file with its identity as listed, without following links.
    let mut files: Vec<(PathBuf, (u64, u64))> = Vec::new();
    for path in paths {
        let meta = std::fs::symlink_metadata(path).map_err(|_| ReadError::NotFound)?;
        if meta.is_dir() {
            let mut inside: Vec<(PathBuf, (u64, u64))> = std::fs::read_dir(path)
                .map_err(|_| ReadError::Unreadable)?
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_str().is_some_and(|n| !n.starts_with('.')))
                .filter(|entry| is_markdown(&entry.path()))
                // `DirEntry::metadata` does not follow links: a link child is not a file here.
                .filter_map(|entry| {
                    let meta = entry.metadata().ok().filter(std::fs::Metadata::is_file)?;
                    Some((entry.path(), file_id(&meta)))
                })
                .collect();
            inside.sort();
            files.extend(inside);
        } else if !meta.is_file() {
            return Err(ReadError::Unreadable); // a link, FIFO, socket or device
        } else if is_markdown(path) {
            files.push((path.clone(), file_id(&meta)));
        } else {
            return Err(ReadError::NotMarkdown);
        }
        if files.len() > MAX_FILES {
            return Err(ReadError::TooMany);
        }
    }
    let mut total = 0usize;
    let mut out = Vec::with_capacity(files.len());
    for (file, id) in files {
        let opened = std::fs::File::open(&file).map_err(|_| ReadError::Unreadable)?;
        let meta = opened.metadata().map_err(|_| ReadError::Unreadable)?;
        if !meta.is_file() || file_id(&meta) != id {
            return Err(ReadError::Unreadable);
        }
        let markdown = read_capped(opened, MAX_REQUEST_BYTES - total)?;
        total += markdown.len();
        let name = file
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string)
            .unwrap_or_default();
        out.push(MarkdownFile { name, markdown });
    }
    Ok(out)
}

/// Device + inode: what ties the opened file to the listed one. Unix only (the shipped target is
/// macOS); elsewhere the open-time check is only that it is a regular file.
#[cfg(unix)]
fn file_id(meta: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

#[cfg(not(unix))]
fn file_id(_meta: &std::fs::Metadata) -> (u64, u64) {
    (0, 0)
}

/// Read at most `remaining` bytes as UTF-8. One byte past it is `TooLarge` (checked before the
/// UTF-8 decode), so a file that grows or is replaced after listing cannot read past the cap.
fn read_capped(reader: impl std::io::Read, remaining: usize) -> Result<String, ReadError> {
    use std::io::Read;

    let mut buf = Vec::new();
    reader
        .take(u64::try_from(remaining).unwrap_or(u64::MAX).saturating_add(1))
        .read_to_end(&mut buf)
        .map_err(|_| ReadError::Unreadable)?;
    if buf.len() > remaining {
        return Err(ReadError::TooLarge);
    }
    String::from_utf8(buf).map_err(|_| ReadError::Unreadable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_import::{parse_package, parse_request, preview};

    const FIXTURE: &str = "---\ntitle: Work memory\nupdated: 2026-09-01\n---\n\
# Work memory\n\
Intro paragraph that has no section yet\n\
\n\
## Preferences\n\
- Prefers short written updates over meetings\n\
- Dark mode everywhere\n\
\n\
## How I work\n\
* Deep work in the morning,\n  meetings after 2pm\n\
\n\
## Current projects\n\
- **Atlas** — mobile banking app redesign\n  - owned with Priya\n\
- Launch site: marketing pages for v1\n\
\n\
## People\n\
1. Priya Shah: design lead on Atlas\n\
2. Tom\n\
\n\
## Tools I use\n\
- Figma\n\
- Linear – issue tracking\n\
\n\
## Next steps\n\
- [ ] Send the Atlas deck to Priya\n\
- [x] Book the offsite\n\
\n\
```\n## Not a heading\n- not an item\n```\n\
\n\
## Recent browsing and misc\n\
Looked at flight prices to Lisbon.\n\
It continued on this line.\n";

    fn fixture_package() -> Value {
        convert(&[MarkdownFile { name: "/Users/me/x/memory-2026-09.md".into(), markdown: FIXTURE.into() }]).unwrap()
    }

    #[test]
    fn the_assumed_structure_maps_to_package_sections() {
        let pkg = fixture_package();
        let pref: Vec<&str> = pkg["profile"]["preferences"].as_array().unwrap().iter().map(|v| v["value"].as_str().unwrap()).collect();
        assert_eq!(pref, ["Prefers short written updates over meetings", "Dark mode everywhere"]);
        assert_eq!(pkg["profile"]["working_style"][0]["value"], "Deep work in the morning, meetings after 2pm");
        let ents: Vec<(&str, &str, Option<&str>)> = pkg["entities"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| (e["type"].as_str().unwrap(), e["name"].as_str().unwrap(), e["summary"].as_str()))
            .collect();
        assert_eq!(
            ents,
            [
                ("project", "Atlas", Some("mobile banking app redesign; owned with Priya")),
                ("project", "Launch site", Some("marketing pages for v1")),
                ("person", "Priya Shah", Some("design lead on Atlas")),
                ("person", "Tom", None),
                ("tool", "Figma", None),
                ("tool", "Linear", Some("issue tracking")),
            ]
        );
        let loops: Vec<(&str, &str, &str)> = pkg["open_loops"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| (l["summary"].as_str().unwrap(), l["status"].as_str().unwrap(), l["confidence"].as_str().unwrap()))
            .collect();
        assert_eq!(
            loops,
            [
                ("Intro paragraph that has no section yet", "uncertain", "low"),
                ("Send the Atlas deck to Priya", "open", "medium"),
                ("Book the offsite", "done", "medium"),
                ("Looked at flight prices to Lisbon. It continued on this line.", "uncertain", "low"),
            ],
            "front matter, the H1 title and fenced code are not items; unknown sections are uncertain"
        );
        assert_eq!(
            pkg["open_loops"][1]["evidence"],
            "Computer History file memory-2026-09.md, section \"Next steps\"",
            "base name only"
        );
    }

    #[test]
    fn converted_packages_go_through_the_shared_validation_and_cap() {
        let conn = crate::open_in_memory().unwrap();
        let body = serde_json::json!({ "computer_history": [{ "name": "m.md", "markdown": FIXTURE }] }).to_string();
        let req = parse_request(&body).unwrap();
        let pkg = parse_package(&req.package_json).unwrap();
        assert!(pkg.rejected.is_empty(), "{:?}", pkg.rejected);
        let pv = preview(&conn, &pkg).unwrap();
        assert_eq!(pv.provenance, "imported:computer_history");
        assert!(pv.items.iter().all(|i| i.confidence <= crate::memory_import::CONFIDENCE_MEDIUM));
        // A secret in a memory file is rejected like any package value.
        let leaky = convert(&[MarkdownFile { name: "a.md".into(), markdown: "## Tools\n- Deploy key sk-ant-abcdefghijklmnopqrstuv\n".into() }]).unwrap();
        let pkg = parse_package(&leaky.to_string()).unwrap();
        assert_eq!(pkg.rejected[0].reason, "sensitive_value");
    }

    #[test]
    fn tolerant_of_empty_odd_and_oversized_input() {
        let pkg = convert(&[MarkdownFile { name: "e.md".into(), markdown: String::new() }]).unwrap();
        assert!(parse_package(&pkg.to_string()).unwrap().items.is_empty());
        let odd = convert(&[MarkdownFile { name: "o.md".into(), markdown: "#nospace\n####### seven\n\t\n- \n".into() }]).unwrap();
        assert!(parse_package(&odd.to_string()).is_ok());
        assert!(convert(&[]).is_err());
        let big = MarkdownFile { name: "b.md".into(), markdown: "a".repeat(MAX_REQUEST_BYTES + 1) };
        assert_eq!(convert(&[big]).unwrap_err().to_string(), "invalid computer_history: too large");
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("shogun_ch_{}_{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// #391 acceptance: SHOGUN never opens `~/.codex` unless a path inside it was explicitly
    /// provided. A fake home holds a Computer History folder; only what is named is read.
    #[test]
    fn only_explicitly_given_paths_are_read_never_a_default_or_codex() {
        let home = temp_dir("home");
        let skysight = home.join(".codex/memories/extensions/skysight");
        std::fs::create_dir_all(&skysight).unwrap();
        std::fs::write(skysight.join("private.md"), "## Goals\n- from codex\n").unwrap();
        let dropped = home.join("Downloads/dropped");
        std::fs::create_dir_all(dropped.join("nested")).unwrap();
        std::fs::write(dropped.join("b.md"), "## Goals\n- b\n").unwrap();
        std::fs::write(dropped.join("a.MD"), "## Goals\n- a\n").unwrap();
        std::fs::write(dropped.join("notes.txt"), "not markdown").unwrap();
        std::fs::write(dropped.join("nested/deep.md"), "## Goals\n- deep\n").unwrap();
        // An AppleDouble copy is not UTF-8; reading it would fail the whole folder.
        std::fs::write(dropped.join("._b.md"), b"\x00\x05\x16\x07\xff\xfe").unwrap();
        // A link in the dropped folder pointing back into .codex, and a socket named `.md`.
        #[cfg(unix)]
        let _socket = {
            std::os::unix::fs::symlink(skysight.join("private.md"), dropped.join("outside.md")).unwrap();
            std::os::unix::net::UnixListener::bind(home.join("s.md")).unwrap()
        };

        // No default: nothing named, nothing read.
        assert_eq!(read_explicit_paths(&[]), Err(ReadError::NoPaths));
        // A named folder: its own Markdown files only — no subfolders, no siblings, no ~/.codex.
        let got = read_explicit_paths(std::slice::from_ref(&dropped)).unwrap();
        let names: Vec<&str> = got.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["a.MD", "b.md"]);
        assert!(got.iter().all(|f| !f.markdown.contains("codex") && !f.markdown.contains("deep")));
        // Naming the home folder does not descend into the hidden .codex tree either.
        assert!(read_explicit_paths(std::slice::from_ref(&home)).unwrap().is_empty());
        // A path inside .codex is read only because the user named it.
        let named = read_explicit_paths(&[skysight.join("private.md")]).unwrap();
        assert_eq!(named[0].markdown, "## Goals\n- from codex\n");
        // Non-Markdown and missing paths are refused without echoing the path.
        assert_eq!(read_explicit_paths(&[dropped.join("notes.txt")]), Err(ReadError::NotMarkdown));
        // A named link or special file is refused before it is read (a FIFO would block).
        if cfg!(unix) {
            for special in [dropped.join("outside.md"), home.join("s.md")] {
                assert_eq!(read_explicit_paths(&[special]), Err(ReadError::Unreadable));
            }
        }
        let err = read_explicit_paths(&[home.join("missing.md")]).unwrap_err();
        assert!(!err.to_string().contains("missing"));
        let _ = std::fs::remove_dir_all(&home);

        // And the code has no way to find one on its own: no home lookup, no env, no .codex
        // literal outside this test module.
        let src = include_str!("computer_history.rs");
        let code: String = src
            .split("#[cfg(test)]")
            .next()
            .unwrap()
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for needle in ["home_dir", "env::var", "HOME", ".codex", "skysight", "walkdir", "glob("] {
            assert!(!code.contains(needle), "converter must not locate files itself: {needle}");
        }
    }

    /// The cap counts bytes actually read: a reader that never ends (a file growing after it was
    /// listed) stops one byte past it, and two files under the cap alone are refused together.
    #[test]
    fn reads_stop_at_the_byte_cap() {
        assert_eq!(read_capped(std::io::repeat(b'a'), 10), Err(ReadError::TooLarge));
        assert_eq!(read_capped(&b"## Goals"[..], 8).as_deref(), Ok("## Goals"));
        assert_eq!(read_capped(&[0xff, 0xfe][..], 10), Err(ReadError::Unreadable));

        let dir = temp_dir("cap");
        let half = format!("## Goals\n{}\n", "a".repeat(MAX_REQUEST_BYTES / 2 + 16));
        std::fs::write(dir.join("a.md"), &half).unwrap();
        assert_eq!(read_explicit_paths(&[dir.join("a.md")]).unwrap()[0].markdown, half);
        std::fs::write(dir.join("b.md"), &half).unwrap();
        assert_eq!(read_explicit_paths(std::slice::from_ref(&dir)), Err(ReadError::TooLarge));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
