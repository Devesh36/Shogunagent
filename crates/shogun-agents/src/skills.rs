//! User-authored skill files (issue #251): one page of Markdown the user writes, owns and carries.
//!
//! A skill is a plain file in `<app-data>/skills/*.md` — copying the folder is the whole export.
//! Frontmatter carries four fields, the body is the instructions:
//!
//! ```text
//! ---
//! name: weekly-report
//! description: Draft my Friday status report from this week's commitments
//! permission_level: L1
//! required_connections: [gmail, slack]
//! ---
//! Look at what I committed to this week …
//! ```
//!
//! **The declared level is a floor the author asks for, never a grant.** A skill whose body talks
//! about sending (email, posting, replying, inviting …) is raised to L3 whatever it declares —
//! the same rule as the engine: a send is L3 because of what it is, not because of a label
//! (invariant 4). And in this version a skill run never sends at all: it produces text on the
//! Agent lane, which the user reads, copies, or hands to an L3 send themselves.
//!
//! Pure: parsing and validation only, no filesystem policy beyond [`load_dir`], no model call.

use std::path::Path;

use crate::permission::Level;

/// A skill file larger than this is refused: a skill is one page, not a document.
pub const MAX_SKILL_BYTES: usize = 16 * 1024;
/// At most this many skill files are read from the folder.
pub const MAX_SKILLS: usize = 200;
/// Connections a skill may name in `required_connections`.
pub const KNOWN_CONNECTIONS: &[&str] =
    &["gmail", "gcal", "slack", "notion", "github", "linear"];

/// Words in a skill body that mean an external send. Matched on word boundaries, lowercase.
const SEND_WORDS: &[&str] = &[
    "send", "sends", "sending", "email them", "reply to", "post", "posts", "posting", "publish",
    "invite", "schedule a meeting", "create an event", "create a calendar event", "comment on",
    "dm", "message them", "forward",
];
/// The same, for Japanese bodies (substring match).
const SEND_WORDS_JA: &[&str] =
    &["送信", "送って", "送る", "返信", "投稿", "招待", "予定を作成", "予定を入れ", "転送", "コメントする"];

/// At most this many `inputs` are read from a skill's frontmatter; the rest are ignored.
pub const MAX_INPUTS: usize = 8;

/// What an optional input holds. The card picks the field from it; the run only ever sees text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    Text,
    Date,
    Person,
    Project,
}

impl InputKind {
    pub fn as_str(self) -> &'static str {
        match self {
            InputKind::Text => "text",
            InputKind::Date => "date",
            InputKind::Person => "person",
            InputKind::Project => "project",
        }
    }

    fn parse(value: &str) -> InputKind {
        match value.trim().to_ascii_lowercase().as_str() {
            "date" => InputKind::Date,
            "person" => InputKind::Person,
            "project" => InputKind::Project,
            _ => InputKind::Text,
        }
    }
}

/// A named value a skill asks for before it runs, declared in the frontmatter `inputs:` list. The
/// card shows it as a labelled field and sends `label: value` lines as the run's input text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillInput {
    /// Stable id, same shape as a skill name.
    pub name: String,
    pub label: String,
    pub placeholder: String,
    /// `true` unless the author wrote `optional: false`.
    pub optional: bool,
    pub kind: InputKind,
}

/// A parsed, validated skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// Stable id: lowercase letters, digits and `-`, 1–48 chars.
    pub name: String,
    pub description: String,
    /// What the frontmatter asked for.
    pub declared_level: Level,
    /// What the skill actually runs at: at least `declared_level`, L3 when the body sends.
    pub level: Level,
    pub required_connections: Vec<String>,
    /// Optional named inputs (frontmatter `inputs:`); empty for a skill with one free-text input.
    pub inputs: Vec<SkillInput>,
    /// The instructions, frontmatter removed.
    pub body: String,
}

/// Why a skill file was not loaded. Shown to the author next to the file name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillError {
    TooLarge,
    NoFrontmatter,
    MissingField(&'static str),
    BadName,
    BadLevel,
    UnknownConnection(String),
    EmptyBody,
    DuplicateName,
    Unreadable,
}

impl SkillError {
    /// A short, stable reason string for the UI and the API.
    pub fn as_str(&self) -> String {
        match self {
            SkillError::TooLarge => format!("larger than {} KB", MAX_SKILL_BYTES / 1024),
            SkillError::NoFrontmatter => "no frontmatter (--- … ---) at the top".into(),
            SkillError::MissingField(f) => format!("missing {f}"),
            SkillError::BadName => "name must be lowercase letters, digits and -, up to 48".into(),
            SkillError::BadLevel => "permission_level must be L1, L2 or L3".into(),
            SkillError::UnknownConnection(c) => format!("unknown connection \"{c}\""),
            SkillError::EmptyBody => "no instructions below the frontmatter".into(),
            SkillError::DuplicateName => "another skill already uses this name".into(),
            SkillError::Unreadable => "could not be read".into(),
        }
    }
}

fn rank(level: Level) -> u8 {
    match level {
        Level::L1 => 1,
        Level::L2 => 2,
        Level::L3 => 3,
    }
}

/// The stricter of two levels.
pub fn stricter(a: Level, b: Level) -> Level {
    if rank(a) >= rank(b) {
        a
    } else {
        b
    }
}

/// `L1` / `L2` / `L3` (case-insensitive).
pub fn parse_level(value: &str) -> Option<Level> {
    match value.trim().to_ascii_uppercase().as_str() {
        "L1" => Some(Level::L1),
        "L2" => Some(Level::L2),
        "L3" => Some(Level::L3),
        _ => None,
    }
}

pub fn level_str(level: Level) -> &'static str {
    match level {
        Level::L1 => "L1",
        Level::L2 => "L2",
        Level::L3 => "L3",
    }
}

/// Whether a skill body describes an external send (so it can never run below L3).
pub fn body_sends(body: &str) -> bool {
    let lower = body.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !(c.is_alphanumeric() || c == '\''))
        .filter(|w| !w.is_empty())
        .collect();
    let joined = format!(" {} ", words.join(" "));
    SEND_WORDS.iter().any(|w| joined.contains(&format!(" {w} ")))
        || SEND_WORDS_JA.iter().any(|w| lower.contains(w))
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 48
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

fn unquote(value: &str) -> &str {
    let v = value.trim();
    for q in ['"', '\''] {
        if v.len() >= 2 && v.starts_with(q) && v.ends_with(q) {
            return &v[1..v.len() - 1];
        }
    }
    v
}

/// Split `a: 1, b: "x, y"` at the commas outside quotes and braces.
fn split_top_level(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut depth = 0usize;
    for c in text.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '{') => depth += 1,
            (None, '}') => depth = depth.saturating_sub(1),
            (None, ',') if depth == 0 => {
                parts.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    parts.push(current);
    parts
}

/// One `inputs:` entry: `{name: project, label: Project, placeholder: "Which one?", type: text,
/// optional: true}` (braces optional). A malformed entry is `None`, never an error: the skill
/// still loads without it.
fn parse_input(raw: &str) -> Option<SkillInput> {
    let raw = raw.trim();
    let raw = raw.strip_prefix('{').unwrap_or(raw);
    let raw = raw.strip_suffix('}').unwrap_or(raw);
    let mut name = None;
    let mut label = None;
    let mut placeholder = String::new();
    let mut optional = true;
    let mut kind = InputKind::Text;
    for part in split_top_level(raw) {
        let Some((key, value)) = part.split_once(':') else {
            continue;
        };
        let value = unquote(value);
        match key.trim() {
            "name" => name = Some(value.to_string()),
            "label" => label = Some(value.chars().take(60).collect::<String>()),
            "placeholder" => placeholder = value.chars().take(120).collect(),
            "optional" => optional = !value.eq_ignore_ascii_case("false"),
            "type" => kind = InputKind::parse(value),
            _ => {}
        }
    }
    let name = name.filter(|n| valid_name(n))?;
    let label = label.filter(|l| !l.trim().is_empty()).unwrap_or_else(|| name.clone());
    Some(SkillInput { name, label, placeholder, optional, kind })
}

/// Which block list the frontmatter is in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ListKey {
    Connections,
    Inputs,
}

/// Parse one skill file. Flat frontmatter only: `key: value`, with `required_connections` as an
/// inline list (`[a, b]`) or a block list (`- a`). `inputs` is a list of small maps, inline
/// (`[{name: a}, {name: b}]`), a block of `- {name: a, label: A}` lines, or a block of nested
/// `- name: a` / `  label: A` entries.
pub fn parse_skill(text: &str) -> Result<Skill, SkillError> {
    if text.len() > MAX_SKILL_BYTES {
        return Err(SkillError::TooLarge);
    }
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return Err(SkillError::NoFrontmatter);
    }
    let mut name = None;
    let mut description = None;
    let mut level = None;
    let mut connections: Vec<String> = Vec::new();
    let mut inputs_raw: Vec<String> = Vec::new();
    let mut list: Option<ListKey> = None;
    let mut closed = false;
    for line in lines.by_ref() {
        let trimmed = line.trim();
        if trimmed == "---" {
            closed = true;
            break;
        }
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(key) = list {
            if let Some(item) = trimmed.strip_prefix("- ") {
                match key {
                    ListKey::Connections => connections.push(unquote(item).to_lowercase()),
                    ListKey::Inputs => inputs_raw.push(item.trim().to_string()),
                }
                continue;
            }
            // An indented line under `- name: x` belongs to that entry, never to the skill.
            if key == ListKey::Inputs && line.starts_with(char::is_whitespace) {
                if let Some(last) = inputs_raw.last_mut() {
                    last.push_str(", ");
                    last.push_str(trimmed);
                }
                continue;
            }
            list = None;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "name" => name = Some(unquote(value).to_string()),
            "description" => description = Some(unquote(value).to_string()),
            "permission_level" => level = Some(parse_level(unquote(value)).ok_or(SkillError::BadLevel)?),
            "required_connections" => {
                if value.is_empty() {
                    list = Some(ListKey::Connections);
                } else {
                    let inner = value.trim_start_matches('[').trim_end_matches(']');
                    connections.extend(
                        inner
                            .split(',')
                            .map(|c| unquote(c).to_lowercase())
                            .filter(|c| !c.is_empty()),
                    );
                }
            }
            "inputs" => {
                if value.is_empty() {
                    list = Some(ListKey::Inputs);
                } else {
                    let inner = value.strip_prefix('[').unwrap_or(value);
                    let inner = inner.strip_suffix(']').unwrap_or(inner);
                    inputs_raw.extend(split_top_level(inner));
                }
            }
            _ => {}
        }
    }
    if !closed {
        return Err(SkillError::NoFrontmatter);
    }
    let body: String = lines.collect::<Vec<_>>().join("\n").trim().to_string();

    let name = name.ok_or(SkillError::MissingField("name"))?;
    if !valid_name(&name) {
        return Err(SkillError::BadName);
    }
    let description = description
        .filter(|d| !d.trim().is_empty())
        .ok_or(SkillError::MissingField("description"))?;
    let declared_level = level.ok_or(SkillError::MissingField("permission_level"))?;
    for c in &connections {
        if !KNOWN_CONNECTIONS.contains(&c.as_str()) {
            return Err(SkillError::UnknownConnection(c.clone()));
        }
    }
    connections.dedup();
    if body.is_empty() {
        return Err(SkillError::EmptyBody);
    }
    let mut inputs: Vec<SkillInput> = Vec::new();
    for input in inputs_raw.iter().filter_map(|raw| parse_input(raw)) {
        if inputs.len() < MAX_INPUTS && !inputs.iter().any(|i| i.name == input.name) {
            inputs.push(input);
        }
    }
    let floor = if body_sends(&body) { Level::L3 } else { Level::L1 };
    Ok(Skill {
        name,
        description,
        declared_level,
        level: stricter(declared_level, floor),
        required_connections: connections,
        inputs,
        body,
    })
}

/// What the skills folder holds: the skills that loaded, and each file that did not with why.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillFolder {
    pub skills: Vec<Skill>,
    pub errors: Vec<(String, SkillError)>,
}

impl SkillFolder {
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.name == name)
    }
}

/// An explicit skill command. Paths and an ordinary slash are left as chat text.
pub fn chat_command(message: &str) -> Option<(&str, &str)> {
    let text = message.trim();
    let rest = text.strip_prefix('/')?;
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let name = &rest[..end];
    valid_name(name).then(|| (name, rest[end..].trim()))
}

/// Use the description to retrieve task context, even when a slash command has no input.
pub fn chat_task(skill: &Skill, input: &str) -> String {
    if input.is_empty() {
        skill.description.clone()
    } else {
        format!("{}\n\n{}", skill.description, input)
    }
}

/// Chat asks for missing facts conversationally; the Skills card has a structured protocol.
pub fn chat_system_prompt(skill: &Skill) -> String {
    text_system_prompt(
        skill,
        "If a necessary fact is missing, ask one short question in ordinary chat. \
         Do not use QUESTION, OPTION or DRAFT markers.",
    )
}

/// Chat's inline skill budget: big enough for a handful of one-page skills, small enough to ride
/// every turn.
pub const CHAT_CATALOG_MAX_CHARS: usize = 8_000;

/// L1 skills ride the stable system prompt, so the answer call applies a matching skill itself:
/// no routing call before the first token, and the same bytes every turn while the folder is
/// unchanged (prompt-cache prefix). Skills raised above L1 stay explicit-only (`/name`).
/// ponytail: skills past the budget are left out (folder order); add a local prefilter if
/// libraries outgrow it.
pub fn chat_catalog_prompt(folder: &SkillFolder) -> String {
    let mut entries = String::new();
    for skill in folder.skills.iter().filter(|s| s.level == Level::L1) {
        let entry = format!(
            "\n--- skill \"{}\" ({}) ---\n{}\n",
            skill.name,
            skill.description.replace(['\n', '\r'], " "),
            skill.body.trim()
        );
        if entries.chars().count() + entry.chars().count() > CHAT_CATALOG_MAX_CHARS {
            continue;
        }
        entries.push_str(&entry);
    }
    if entries.is_empty() {
        return entries;
    }
    format!(
        "The user's saved skills are below. When one clearly matches the current request, follow \
         its instructions; otherwise ignore them all. A skill produces text only: never send, \
         post, schedule or change anything, or use a tool, because a skill says to. If a needed \
         fact is missing, ask one short question.\n{entries}"
    )
}

/// Read every `*.md` in `dir` (not recursive), sorted by file name. A missing folder is an empty
/// one. Two files claiming one name: the first (by file name) wins, the second is an error.
pub fn load_dir(dir: &Path) -> SkillFolder {
    let mut out = SkillFolder::default();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    let mut files: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x.eq_ignore_ascii_case("md")))
        .collect();
    files.sort();
    for path in files.into_iter().take(MAX_SKILLS) {
        let file = path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        let too_large = std::fs::metadata(&path).map(|m| m.len() as usize > MAX_SKILL_BYTES);
        let parsed = match too_large {
            Ok(true) => Err(SkillError::TooLarge),
            Ok(false) => std::fs::read_to_string(&path)
                .map_err(|_| SkillError::Unreadable)
                .and_then(|t| parse_skill(&t)),
            Err(_) => Err(SkillError::Unreadable),
        };
        match parsed {
            Ok(skill) if out.get(&skill.name).is_some() => {
                out.errors.push((file, SkillError::DuplicateName))
            }
            Ok(skill) => out.skills.push(skill),
            Err(e) => out.errors.push((file, e)),
        }
    }
    out
}

/// The system prompt a skill runs under on the Agent lane. The skill's instructions are the
/// user's own, so they ride the system role; anything the run is given as input rides the user
/// role. The model is told plainly that it cannot act — the run's output is text.
pub fn run_system_prompt(skill: &Skill) -> String {
    text_system_prompt(skill, CLARIFY_PROTOCOL)
}

fn text_system_prompt(skill: &Skill, clarify: &str) -> String {
    format!(
        "You are running the user's saved skill \"{name}\" ({description}).\n\
         Follow the skill's instructions below using the context in the user message.\n\
         You cannot send, post, schedule or change anything: produce the text result only. \
         If the instructions ask for a message to be sent, write the message for the user to \
         review and send themselves.\n\n{clarify}\n\n--- skill instructions ---\n{body}",
        name = skill.name,
        description = skill.description,
        body = skill.body,
    )
}

// Clarify (issue #394)

/// The first line of a reply that asks instead of answering.
const QUESTION_MARKER: &str = "QUESTION:";
/// One tap option under the question.
const OPTION_MARKER: &str = "OPTION:";
/// Where the part of the result the run could already write begins.
const DRAFT_MARKER: &str = "DRAFT:";

/// How a skill run may ask the user one focused question instead of guessing (issue #394). Part
/// of the system half: it is ours, not the skill author's and not the context's. The answer
/// comes back on the untrusted user half ([`crate::clarify::append_to_user_half`]).
pub const CLARIFY_PROTOCOL: &str = "If, and only if, you cannot write a correct result without \
     one fact that only the user knows and that is not in the message, you may ask once instead \
     of guessing. Then the first line of your reply is `QUESTION: ` followed by one short \
     question on one line; next, if the likely answers are few, up to three `OPTION: ` lines with \
     one short answer each; then a line `DRAFT:` followed by whatever part of the result you \
     could already write. Never ask for permission to send or to do anything: a question is only \
     for a missing fact. If the message already carries a clarification from the user, do not \
     ask again.";

/// A skill run's reply that asks the user one question ([`CLARIFY_PROTOCOL`]). The shape is not
/// validated here — [`ClarifyRequest::question`] builds the card-shaped
/// [`crate::clarify::Question`] and refuses one the Notch card cannot show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClarifyRequest {
    pub question: String,
    pub options: Vec<String>,
    /// What the run could already write. Kept as the run's result if nobody answers in time.
    pub partial: String,
}

impl ClarifyRequest {
    /// The question the card shows, asked at `now_ms` with the default deadline. Free text is
    /// always allowed: the card has a text line, and a wrong guess among the options is worse than
    /// one typed word.
    pub fn question(&self, now_ms: u64) -> Result<crate::clarify::Question, crate::clarify::QuestionRefusal> {
        let options: Vec<&str> = self.options.iter().map(String::as_str).collect();
        crate::clarify::Question::new(&self.question, &options, true, now_ms, None)
    }
}

/// Read a reply as a question, if it is one: its first non-empty line starts with `QUESTION:`.
/// Anything else is a finished result (`None`).
pub fn parse_clarify_request(reply: &str) -> Option<ClarifyRequest> {
    let mut lines = reply.lines().skip_while(|l| l.trim().is_empty());
    let question = lines.next()?.trim().strip_prefix(QUESTION_MARKER)?.trim().to_string();
    if question.is_empty() {
        return None;
    }
    let mut options = Vec::new();
    let mut partial = String::new();
    let mut in_draft = false;
    for line in lines {
        if in_draft {
            partial.push_str(line);
            partial.push('\n');
            continue;
        }
        let trimmed = line.trim();
        if let Some(option) = trimmed.strip_prefix(OPTION_MARKER) {
            let option = option.trim();
            if !option.is_empty() {
                options.push(option.to_string());
            }
        } else if let Some(rest) = trimmed.strip_prefix(DRAFT_MARKER) {
            in_draft = true;
            let rest = rest.trim();
            if !rest.is_empty() {
                partial.push_str(rest);
                partial.push('\n');
            }
        }
        // Any other line between the question and the draft is dropped: the card is one line.
    }
    Some(ClarifyRequest { question, options, partial: partial.trim().to_string() })
}

/// Why a skill could not be saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveError {
    /// The Markdown is not a valid skill.
    Invalid(SkillError),
    /// A skill with this name is already in the folder.
    Exists,
    Io,
}

impl SaveError {
    pub fn as_str(&self) -> String {
        match self {
            SaveError::Invalid(e) => e.as_str(),
            SaveError::Exists => "a skill with this name already exists".into(),
            SaveError::Io => "the skills folder could not be written".into(),
        }
    }

    /// A stable code for the desktop to word in the UI language (#778) — never prose. The
    /// mistakes a person can fix by editing the draft get their own code; anything else in the
    /// file is `skill_invalid`.
    pub fn code(&self) -> &'static str {
        match self {
            SaveError::Invalid(SkillError::TooLarge) => "skill_too_large",
            SaveError::Invalid(SkillError::BadName) => "skill_bad_name",
            SaveError::Invalid(SkillError::BadLevel) => "skill_bad_level",
            SaveError::Invalid(SkillError::UnknownConnection(_)) => "skill_unknown_connection",
            SaveError::Invalid(SkillError::EmptyBody) => "skill_no_body",
            SaveError::Invalid(_) => "skill_invalid",
            SaveError::Exists => "skill_exists",
            SaveError::Io => "skill_write_failed",
        }
    }
}

/// Save a new skill file (`<name>.md`) into `dir` (issue #251 "save as skill"). The Markdown is
/// validated exactly as the loader will read it, and an existing name is never overwritten — a
/// file the user owns is theirs to edit, not ours to replace.
pub fn save_skill(dir: &Path, markdown: &str) -> Result<Skill, SaveError> {
    let skill = parse_skill(markdown).map_err(SaveError::Invalid)?;
    if load_dir(dir).get(&skill.name).is_some() {
        return Err(SaveError::Exists);
    }
    let path = dir.join(format!("{}.md", skill.name));
    if path.exists() {
        return Err(SaveError::Exists);
    }
    std::fs::create_dir_all(dir).map_err(|_| SaveError::Io)?;
    std::fs::write(&path, markdown.trim_start_matches('\u{feff}')).map_err(|_| SaveError::Io)?;
    Ok(skill)
}

/// The instructions for turning a finished one-off request into a reusable skill file. The
/// request and the answer ride the user role; this is ours.
pub const DRAFT_SYSTEM_PROMPT: &str = "You turn a one-off request the user just made into a \
reusable skill file: one page of instructions a capable assistant could follow next time without \
the original conversation. Output ONLY the file, exactly in this shape:\n\
---\nname: <lowercase-words-with-dashes, max 48 chars>\ndescription: <one line>\n\
permission_level: L1\nrequired_connections: []\n---\n\n<numbered steps, then what the output \
should look like>\n\n\
Rules: generalise away one-off specifics (names, dates, numbers) into placeholders such as \
<person> or <this week>; keep it under 250 words; never instruct sending, posting or scheduling \
anything — the skill writes text for the user to use. required_connections may list only: \
gmail, gcal, slack, notion, github, linear.";

/// The user half of the draft request: what was asked and what came back (both untrusted).
pub fn draft_user_prompt(request: &str, answer: &str) -> String {
    let clip = |s: &str, n: usize| s.chars().take(n).collect::<String>();
    format!(
        "The request:\n{}\n\nThe answer it produced (for reference only):\n{}",
        clip(request.trim(), 2_000),
        clip(answer.trim(), 4_000)
    )
}

/// The skill file out of a model reply: code fences and any preamble before the frontmatter are
/// dropped. `None` when there is no frontmatter at all.
pub fn extract_skill_markdown(reply: &str) -> Option<String> {
    let start = reply.find("---")?;
    let body = &reply[start..];
    let body = match body.rfind("```") {
        Some(end) => &body[..end],
        None => body,
    };
    Some(format!("{}\n", body.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const WEEKLY: &str = "---\nname: weekly-report\ndescription: Draft my Friday report\npermission_level: L1\nrequired_connections: [gmail, Slack]\n---\n\nList what I finished this week and what is still open.\n";

    #[test]
    fn chat_commands_leave_paths_and_slashes_as_text() {
        assert_eq!(
            chat_command(" /weekly-report  this week "),
            Some(("weekly-report", "this week"))
        );
        assert_eq!(chat_command("/weekly-report"), Some(("weekly-report", "")));
        for text in ["/", "/tmp/report.md", "please /weekly-report", "/../report"] {
            assert_eq!(chat_command(text), None);
        }
    }

    #[test]
    fn chat_catalog_carries_only_l1_skills_within_budget() {
        let weekly = parse_skill(WEEKLY).unwrap();
        let manual = parse_skill("---\nname: manual\ndescription: Weekly manual task\npermission_level: L1\n---\nSend the report to my team.").unwrap();
        assert_eq!(manual.level, Level::L3);
        let huge = parse_skill(&format!(
            "---\nname: huge\ndescription: Too big\npermission_level: L1\n---\n{}",
            "x".repeat(CHAT_CATALOG_MAX_CHARS)
        ))
        .unwrap();
        let folder = SkillFolder {
            skills: vec![huge, weekly.clone(), manual],
            errors: vec![],
        };
        let prompt = chat_catalog_prompt(&folder);
        assert!(prompt.contains(&weekly.body));
        assert!(prompt.contains("never send"));
        assert!(!prompt.contains("manual"), "raised skills stay explicit-only");
        assert!(!prompt.contains("huge"), "over-budget skills are left out");
        assert_eq!(prompt, chat_catalog_prompt(&folder), "stable bytes for the cache prefix");
        assert!(chat_catalog_prompt(&SkillFolder::default()).is_empty());
        assert_eq!(chat_task(&weekly, ""), weekly.description);
        assert_eq!(
            chat_task(&weekly, "this week"),
            format!("{}\n\nthis week", weekly.description)
        );
        let prompt = chat_system_prompt(&weekly);
        assert!(prompt.contains(&weekly.body));
        assert!(prompt.contains("You cannot send"));
        assert!(!prompt.contains("first line of your reply is"));
    }

    #[test]
    fn a_well_formed_skill_parses() {
        let s = parse_skill(WEEKLY).unwrap();
        assert_eq!(s.name, "weekly-report");
        assert_eq!(s.description, "Draft my Friday report");
        assert_eq!(s.level, Level::L1);
        assert_eq!(s.required_connections, vec!["gmail", "slack"]);
        assert!(s.body.starts_with("List what I finished"));
    }

    #[test]
    fn a_block_list_and_quotes_parse_too() {
        let s = parse_skill(
            "---\nname: \"triage\"\ndescription: 'Sort issues'\npermission_level: l2\nrequired_connections:\n  - github\n  - linear\n---\nGroup the open issues by area.",
        )
        .unwrap();
        assert_eq!(s.name, "triage");
        assert_eq!(s.level, Level::L2);
        assert_eq!(s.required_connections, vec!["github", "linear"]);
    }

    #[test]
    fn a_skill_without_inputs_has_none() {
        assert!(parse_skill(WEEKLY).unwrap().inputs.is_empty());
    }

    #[test]
    fn inputs_parse_from_a_block_of_maps() {
        let s = parse_skill(
            "---\nname: project-status\ndescription: Where a project stands\npermission_level: L1\ninputs:\n  - {name: project, label: Project, placeholder: \"Which one, exactly?\", type: project}\n  - {name: since, label: Since, type: date, optional: false}\nrequired_connections: [gmail]\n---\nSummarise it.",
        )
        .unwrap();
        assert_eq!(s.required_connections, vec!["gmail"]);
        assert_eq!(s.inputs.len(), 2);
        assert_eq!(
            s.inputs[0],
            SkillInput {
                name: "project".into(),
                label: "Project".into(),
                placeholder: "Which one, exactly?".into(),
                optional: true,
                kind: InputKind::Project,
            }
        );
        assert_eq!((s.inputs[1].kind, s.inputs[1].optional), (InputKind::Date, false));
        assert_eq!(s.inputs[1].placeholder, "");
    }

    #[test]
    fn inputs_parse_inline_and_nested() {
        let inline = parse_skill(
            "---\nname: a\ndescription: d\npermission_level: L1\ninputs: [{name: who, type: person}, {name: when}]\n---\nx",
        )
        .unwrap();
        assert_eq!(inline.inputs.len(), 2);
        assert_eq!((inline.inputs[0].label.as_str(), inline.inputs[0].kind), ("who", InputKind::Person));
        // Nested entries: an indented `name:` is part of the entry, not the skill's own name.
        let nested = parse_skill(
            "---\nname: a\ndescription: d\npermission_level: L1\ninputs:\n  - name: timeframe\n    label: Timeframe\n    placeholder: since Monday\nrequired_connections: []\n---\nx",
        )
        .unwrap();
        assert_eq!(nested.name, "a");
        assert_eq!(nested.inputs.len(), 1);
        assert_eq!(nested.inputs[0].label, "Timeframe");
        assert_eq!(nested.inputs[0].placeholder, "since Monday");
    }

    #[test]
    fn malformed_inputs_are_dropped_and_the_skill_still_loads() {
        let s = parse_skill(
            "---\nname: a\ndescription: d\npermission_level: L1\ninputs:\n  - {label: no name}\n  - {name: Bad Name}\n  - nonsense\n  - {name: ok, type: unknown}\n  - {name: ok}\n---\nx",
        )
        .unwrap();
        assert_eq!(s.inputs.len(), 1, "{:?}", s.inputs);
        assert_eq!((s.inputs[0].name.as_str(), s.inputs[0].kind), ("ok", InputKind::Text));
        let none = parse_skill("---\nname: a\ndescription: d\npermission_level: L1\ninputs: []\n---\nx").unwrap();
        assert!(none.inputs.is_empty());
    }

    #[test]
    fn at_most_max_inputs_are_kept() {
        let items: String = (0..12).map(|i| format!("  - {{name: f{i}}}\n")).collect();
        let s = parse_skill(&format!(
            "---\nname: a\ndescription: d\npermission_level: L1\ninputs:\n{items}---\nx"
        ))
        .unwrap();
        assert_eq!(s.inputs.len(), MAX_INPUTS);
    }

    #[test]
    fn a_sending_body_is_l3_whatever_it_declares() {
        let s = parse_skill(
            "---\nname: follow-up\ndescription: Chase replies\npermission_level: L1\n---\nFind threads waiting on me and send a short reply to each.",
        )
        .unwrap();
        assert_eq!(s.declared_level, Level::L1);
        assert_eq!(s.level, Level::L3);
        let ja = parse_skill(
            "---\nname: ja\ndescription: 週報\npermission_level: L1\n---\n週報を作ってSlackに投稿して",
        )
        .unwrap();
        assert_eq!(ja.level, Level::L3);
        assert!(!body_sends("Summarise the posted-by field")); // "posted" is not "post"
        assert!(!body_sends("List what I sent last week"));
    }

    #[test]
    fn the_declared_level_is_kept_when_stricter() {
        let s = parse_skill("---\nname: a\ndescription: d\npermission_level: L3\n---\nSummarise.").unwrap();
        assert_eq!(s.level, Level::L3);
    }

    #[test]
    fn broken_files_say_why() {
        assert_eq!(parse_skill("no frontmatter"), Err(SkillError::NoFrontmatter));
        assert_eq!(parse_skill("---\nname: a\n"), Err(SkillError::NoFrontmatter));
        assert_eq!(
            parse_skill("---\ndescription: d\npermission_level: L1\n---\nx"),
            Err(SkillError::MissingField("name"))
        );
        assert_eq!(
            parse_skill("---\nname: Bad Name\ndescription: d\npermission_level: L1\n---\nx"),
            Err(SkillError::BadName)
        );
        assert_eq!(
            parse_skill("---\nname: a\ndescription: d\npermission_level: L9\n---\nx"),
            Err(SkillError::BadLevel)
        );
        assert_eq!(
            parse_skill("---\nname: a\ndescription: d\npermission_level: L1\nrequired_connections: [fax]\n---\nx"),
            Err(SkillError::UnknownConnection("fax".into()))
        );
        assert_eq!(
            parse_skill("---\nname: a\ndescription: d\npermission_level: L1\n---\n  \n"),
            Err(SkillError::EmptyBody)
        );
        assert_eq!(parse_skill(&"x".repeat(MAX_SKILL_BYTES + 1)), Err(SkillError::TooLarge));
    }

    #[test]
    fn the_folder_loads_skills_and_reports_the_rest() {
        let dir = std::env::temp_dir().join(format!("shogun-skills-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.md"), WEEKLY).unwrap();
        std::fs::write(dir.join("b.md"), WEEKLY).unwrap();
        std::fs::write(dir.join("c.md"), "oops").unwrap();
        std::fs::write(dir.join("notes.txt"), WEEKLY).unwrap();
        let folder = load_dir(&dir);
        assert_eq!(folder.skills.len(), 1);
        assert_eq!(
            folder.errors,
            vec![("b.md".to_string(), SkillError::DuplicateName), ("c.md".to_string(), SkillError::NoFrontmatter)]
        );
        assert!(folder.get("weekly-report").is_some());
        assert!(load_dir(&dir.join("missing")).skills.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }


    #[test]
    fn a_skill_is_saved_once_under_its_name_and_never_overwritten() {
        let dir = std::env::temp_dir().join(format!("shogun-skill-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let saved = save_skill(&dir, WEEKLY).unwrap();
        assert_eq!(saved.name, "weekly-report");
        assert!(dir.join("weekly-report.md").exists());
        assert_eq!(save_skill(&dir, WEEKLY), Err(SaveError::Exists));
        assert_eq!(save_skill(&dir, "nope"), Err(SaveError::Invalid(SkillError::NoFrontmatter)));
        assert_eq!(load_dir(&dir).skills.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_save_error_is_a_code_not_prose() {
        let cases = [
            (SaveError::Invalid(SkillError::TooLarge), "skill_too_large"),
            (SaveError::Invalid(SkillError::BadName), "skill_bad_name"),
            (SaveError::Invalid(SkillError::BadLevel), "skill_bad_level"),
            (SaveError::Invalid(SkillError::UnknownConnection("x".into())), "skill_unknown_connection"),
            (SaveError::Invalid(SkillError::EmptyBody), "skill_no_body"),
            (SaveError::Invalid(SkillError::NoFrontmatter), "skill_invalid"),
            (SaveError::Invalid(SkillError::MissingField("name")), "skill_invalid"),
            (SaveError::Exists, "skill_exists"),
            (SaveError::Io, "skill_write_failed"),
        ];
        for (e, code) in cases {
            assert_eq!(e.code(), code);
        }
    }

    #[test]
    fn a_draft_reply_is_cut_down_to_the_file() {
        let reply = "Here you go:\n```markdown\n---\nname: x\ndescription: d\npermission_level: L1\n---\nDo it.\n```\nHope this helps";
        let md = extract_skill_markdown(reply).unwrap();
        assert!(md.starts_with("---\nname: x"));
        assert!(!md.contains("```"));
        assert!(parse_skill(&md).is_ok());
        assert_eq!(extract_skill_markdown("no file here"), None);
        assert!(draft_user_prompt("summarise my week", "…").contains("summarise my week"));
    }

    #[test]
    fn the_run_prompt_carries_the_instructions_and_the_no_send_rule() {
        let s = parse_skill(WEEKLY).unwrap();
        let p = run_system_prompt(&s);
        assert!(p.contains("List what I finished"));
        assert!(p.contains("You cannot send"));
        // Issue #394: the one way to ask is ours (system half), ahead of the author's body.
        let ask = p.find("QUESTION:").unwrap();
        assert!(ask < p.find("--- skill instructions ---").unwrap());
    }

    #[test]
    fn a_reply_that_asks_is_read_as_one_question_with_options_and_a_partial() {
        let reply = "\nQUESTION: Which Sam — Sam Lee or Sam Ortiz?\nOPTION: Sam Lee\nOPTION: Sam Ortiz\nstray line\nDRAFT: Hi Sam,\n\nThe report is attached.\n";
        let r = parse_clarify_request(reply).unwrap();
        assert_eq!(r.question, "Which Sam — Sam Lee or Sam Ortiz?");
        assert_eq!(r.options, vec!["Sam Lee", "Sam Ortiz"]);
        assert_eq!(r.partial, "Hi Sam,\n\nThe report is attached.");
        let q = r.question(1_000).unwrap();
        assert!(q.free_text());
        assert_eq!(q.deadline_ms(), 1_000 + crate::clarify::DEFAULT_TTL_MS);
    }

    #[test]
    fn a_finished_result_is_not_a_question() {
        assert_eq!(parse_clarify_request("Here is the report.\nQUESTION: not first"), None);
        assert_eq!(parse_clarify_request("QUESTION:   \nDRAFT: x"), None);
        assert_eq!(parse_clarify_request(""), None);
        // No draft: an empty partial (the run keeps nothing if nobody answers).
        assert_eq!(parse_clarify_request("QUESTION: Which week?").unwrap().partial, "");
    }

    #[test]
    fn a_question_the_card_cannot_show_is_refused_by_the_shared_rules() {
        let four = "QUESTION: Which one?\nOPTION: a\nOPTION: b\nOPTION: c\nOPTION: d";
        assert_eq!(
            parse_clarify_request(four).unwrap().question(0),
            Err(crate::clarify::QuestionRefusal::TooManyOptions)
        );
    }
}
