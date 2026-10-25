use super::*;
use crate::client_config::{ALL_CLIENTS, API_TOKEN_ENV};

const CMD: &str = "/Applications/ShogunAI.app/Contents/MacOS/shogun-mcp";
const DB: &str = "/Users/o'neil/Library/Application Support/com.syogun.shogunai/memory.db";
/// The shape of an issued token (`shogun_` + 48 hex), as a user may have pasted it by hand.
const PASTED: &str = "shogun_0123456789abcdef0123456789abcdef0123456789abcdef";

fn add(client: McpClient, existing: Option<&str>) -> FilePlan {
    plan_add(client, existing, CMD, DB).unwrap()
}

fn new_text(plan: &FilePlan) -> &str {
    plan.new_contents().unwrap()
}

fn json(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap()
}

fn diff_text(plan: &FilePlan) -> String {
    plan.diff
        .iter()
        .map(|l| {
            let sign = match l.op {
                DiffOp::Context => ' ',
                DiffOp::Add => '+',
                DiffOp::Remove => '-',
            };
            format!("{sign}{}\n", l.text)
        })
        .collect()
}

/// Nothing a plan produces may carry a token: not the file it writes, not the preview.
fn assert_no_token(plan: &FilePlan) {
    if let Some(text) = plan.new_contents() {
        assert!(!text.contains(PASTED), "{text}");
    }
    let shown = diff_text(plan);
    assert!(!looks_like_token(&shown), "{shown}");
    assert!(!shown.contains(PASTED), "{shown}");
}

#[test]
fn each_client_has_one_method_and_claude_desktop_is_now_a_file() {
    let methods: Vec<_> = ALL_CLIENTS.iter().map(|&c| (c.id(), install_method(c))).collect();
    assert_eq!(
        methods,
        [
            ("claude-code", InstallMethod::VendorCli),
            ("codex-cli", InstallMethod::ConfigFile),
            ("chatgpt-desktop", InstallMethod::ConfigFile),
            ("cursor", InstallMethod::ConfigFile),
            // Issue #963: its entry holds no secret any more, so SHOGUN writes it.
            ("claude-desktop", InstallMethod::ConfigFile),
        ]
    );
    // The CLI client gets no file plan.
    assert_eq!(plan_add(McpClient::ClaudeCode, None, CMD, DB), Err(Refusal::CopyOnly));
    assert_eq!(plan_remove(McpClient::ClaudeCode, None), Err(Refusal::CopyOnly));
    assert_eq!(config_file(McpClient::ClaudeCode, Path::new("/Users/u")), None);
    let home = Path::new("/Users/u");
    assert_eq!(
        config_file(McpClient::Cursor, home).map(|p| display_path(&p, home)),
        Some("~/.cursor/mcp.json".to_string())
    );
    assert_eq!(
        config_file(McpClient::ClaudeDesktop, home).map(|p| display_path(&p, home)),
        Some("~/Library/Application Support/Claude/claude_desktop_config.json".to_string())
    );
    // Codex CLI and the ChatGPT desktop app share one file.
    assert_eq!(config_file(McpClient::CodexCli, home), config_file(McpClient::ChatGptDesktop, home));
    assert_eq!(
        config_file(McpClient::CodexCli, home).map(|p| display_path(&p, home)),
        Some("~/.codex/config.toml".to_string())
    );
}

#[test]
fn claude_desktop_add_keeps_its_preferences_and_writes_no_token() {
    let existing = "{\n  \"preferences\": { \"quickEntryShortcut\": \"off\" }\n}\n";
    let plan = add(McpClient::ClaudeDesktop, Some(existing));
    assert_eq!(plan.change, Change::Add);
    let v = json(new_text(&plan));
    assert_eq!(v["preferences"]["quickEntryShortcut"], "off");
    let entry = &v["mcpServers"][SERVER_NAME];
    assert_eq!(entry["args"], serde_json::json!(["--client", "claude-desktop"]));
    assert!(entry["env"].get(API_TOKEN_ENV).is_none());
    assert_no_token(&plan);
}

// JSON (Cursor)

#[test]
fn json_new_file_is_the_generated_config_naming_the_app() {
    for existing in [None, Some(""), Some("  \n")] {
        let plan = add(McpClient::Cursor, existing);
        assert_eq!(plan.change, Change::Add);
        let v = json(new_text(&plan));
        let entry = &v["mcpServers"][SERVER_NAME];
        assert_eq!(entry["command"], CMD);
        assert_eq!(entry["args"], serde_json::json!(["--client", "cursor"]));
        assert_eq!(entry["env"][DB_PATH_ENV], DB);
        assert!(entry["env"].get(API_TOKEN_ENV).is_none());
        assert!(plan.diff.iter().all(|l| l.op == DiffOp::Add), "{}", diff_text(&plan));
        assert_no_token(&plan);
    }
    assert_eq!(add(McpClient::Cursor, None).base_digest, ABSENT_DIGEST);
}

const CURSOR_WITH_OTHERS: &str = r#"{
    "mcpServers": {
        "github": {
            "command": "docker",
            "args": ["run", "-i", "ghcr.io/github/github-mcp-server"],
            "env": { "GITHUB_PERSONAL_ACCESS_TOKEN": "ghp_someone_elses_secret" }
        },
        "notes": { "url": "http://localhost:3000/mcp" }
    },
    "theme": "dark"
}
"#;

#[test]
fn json_add_keeps_every_other_byte_and_never_shows_other_entries() {
    let plan = add(McpClient::Cursor, Some(CURSOR_WITH_OTHERS));
    assert_eq!(plan.change, Change::Add);
    assert_eq!(plan.other_entries, 2);
    let new = new_text(&plan);
    // Everything up to the end of the last server and everything after it is byte-identical.
    let cut = CURSOR_WITH_OTHERS.find(r#""http://localhost:3000/mcp" }"#).unwrap()
        + r#""http://localhost:3000/mcp" }"#.len();
    assert!(new.starts_with(&CURSOR_WITH_OTHERS[..cut]), "{new}");
    assert!(new.ends_with(&CURSOR_WITH_OTHERS[cut..]), "{new}");
    // The new member sits at the file's own indentation (8 spaces), its body one unit deeper.
    assert!(new.contains("\n        \"shogun-memory\": {\n            \"command\": "), "{new}");
    let v = json(new);
    assert_eq!(v["theme"], "dark");
    assert_eq!(v["mcpServers"]["github"], json(CURSOR_WITH_OTHERS)["mcpServers"]["github"]);
    // The preview is SHOGUN's entry alone: another server's secret is never in it.
    let shown = diff_text(&plan);
    assert!(!shown.contains("ghp_") && !shown.contains("github"), "{shown}");
    assert!(shown.starts_with("+\"shogun-memory\": {\n"), "{shown}");
    assert_no_token(&plan);
}

#[test]
fn json_re_add_is_unchanged_and_writes_nothing() {
    let first = add(McpClient::Cursor, Some(CURSOR_WITH_OTHERS));
    let again = add(McpClient::Cursor, Some(new_text(&first)));
    assert_eq!(again.change, Change::Unchanged);
    assert_eq!(again.new_contents(), None);
    assert!(again.diff.iter().all(|l| l.op == DiffOp::Context));
    // A whitespace-only difference in the file is still the same entry.
    let compact = new_text(&first).replace("\n            ", " ");
    assert_eq!(add(McpClient::Cursor, Some(&compact)).change, Change::Unchanged);
}

#[test]
fn json_replace_masks_a_pasted_token_and_drops_it() {
    let existing = format!(
        r#"{{"mcpServers":{{"shogun-memory":{{"command":"/old/shogun-mcp","env":{{"SHOGUN_DB_PATH":"/old/memory.db","SHOGUN_API_TOKEN":"{PASTED}","OTHER":"x"}}}},"b":{{"command":"b"}}}}}}"#
    );
    let plan = add(McpClient::Cursor, Some(&existing));
    assert_eq!(plan.change, Change::Replace);
    assert_eq!(plan.other_entries, 1);
    let shown = diff_text(&plan);
    assert!(shown.contains(MASK), "{shown}");
    assert!(shown.contains("-") && shown.contains("/old/memory.db"), "{shown}");
    assert_no_token(&plan);
    let v = json(new_text(&plan));
    // Re-adding an entry from the token days (issue #963) takes the token out of the file.
    assert!(v["mcpServers"][SERVER_NAME]["env"].get(API_TOKEN_ENV).is_none());
    assert_eq!(v["mcpServers"][SERVER_NAME]["args"], serde_json::json!(["--client", "cursor"]));
    assert_eq!(v["mcpServers"]["b"]["command"], "b");
    // Only the value was replaced: the text around it is untouched.
    let new = new_text(&plan);
    assert!(new.starts_with(r#"{"mcpServers":{"shogun-memory":{"#), "{new}");
    assert!(new.ends_with(r#","b":{"command":"b"}}}"#), "{new}");
}

#[test]
fn json_creates_mcp_servers_when_missing_and_keeps_the_rest() {
    let existing = "{\n\t\"editor.fontSize\": 14\n}\n";
    let plan = add(McpClient::Cursor, Some(existing));
    assert_eq!(plan.change, Change::Add);
    let new = new_text(&plan);
    assert!(new.starts_with("{\n\t\"editor.fontSize\": 14,\n\t\"mcpServers\": {\n\t\t\"shogun-memory\""), "{new}");
    assert!(new.ends_with("\n}\n"), "{new}");
    assert_eq!(json(new)["editor.fontSize"], 14);
    // An empty object and an empty `mcpServers` both work too.
    for existing in ["{}", "{\"mcpServers\": {}}"] {
        let plan = add(McpClient::Cursor, Some(existing));
        assert_eq!(json(new_text(&plan))["mcpServers"][SERVER_NAME]["command"], CMD, "{existing}");
    }
}

#[test]
fn json_remove_takes_only_shogun_entry_wherever_it_sits() {
    let a = r#""a": {"command": "a"}"#;
    let ours = r#""shogun-memory": {"command": "x", "env": {"SHOGUN_API_TOKEN": "${env:SHOGUN_API_TOKEN}"}}"#;
    let c = r#""c": {"command": "c"}"#;
    for (members, left) in [
        (format!("{ours}, {a}, {c}"), vec!["a", "c"]),
        (format!("{a}, {ours}, {c}"), vec!["a", "c"]),
        (format!("{a}, {c}, {ours}"), vec!["a", "c"]),
        (ours.to_string(), vec![]),
    ] {
        let existing = format!("{{\"mcpServers\": {{{members}}}, \"z\": 1}}\n");
        let plan = plan_remove(McpClient::Cursor, Some(&existing)).unwrap();
        assert_eq!(plan.change, Change::Remove);
        let v = json(new_text(&plan));
        let keys: Vec<&str> = v["mcpServers"].as_object().unwrap().keys().map(String::as_str).collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, left, "{}", new_text(&plan));
        assert_eq!(v["z"], 1);
        assert!(plan.diff.iter().all(|l| l.op == DiffOp::Remove), "{}", diff_text(&plan));
    }
    let absent = plan_remove(McpClient::Cursor, Some(CURSOR_WITH_OTHERS)).unwrap();
    assert_eq!((absent.change, absent.new_contents()), (Change::Absent, None));
    let missing = plan_remove(McpClient::Cursor, None).unwrap();
    assert_eq!((missing.change, missing.new_contents()), (Change::Absent, None));
}

#[test]
fn json_that_does_not_parse_or_has_another_shape_is_refused() {
    for (text, why) in [
        ("{\"mcpServers\": {", Refusal::Unparseable),
        ("// Cursor settings\n{\"mcpServers\": {}}", Refusal::Unparseable),
        ("{\"mcpServers\": {},}", Refusal::Unparseable),
        ("[1, 2]", Refusal::UnexpectedShape),
        ("{\"mcpServers\": []}", Refusal::UnexpectedShape),
        ("{\"mcpServers\": {}, \"mcpServers\": {}}", Refusal::AmbiguousEntry),
        (
            "{\"mcpServers\": {\"shogun-memory\": {}, \"shogun\\u002dmemory\": {}}}",
            Refusal::AmbiguousEntry,
        ),
    ] {
        assert_eq!(plan_add(McpClient::Cursor, Some(text), CMD, DB).err(), Some(why), "{text}");
        assert_eq!(plan_remove(McpClient::Cursor, Some(text)).err(), Some(why), "{text}");
    }
}

#[test]
fn json_escaped_key_is_recognised_as_shogun_entry() {
    let existing = r#"{"mcpServers": {"shogun-memory": {"command": "old"}}}"#;
    let plan = add(McpClient::Cursor, Some(existing));
    assert_eq!(plan.change, Change::Replace);
    assert_eq!(json(new_text(&plan))["mcpServers"][SERVER_NAME]["command"], CMD);
}

// TOML (Codex)

const CODEX_WITH_OTHERS: &str = r#"# Codex settings
model = "gpt-5"  # keep me

[mcp_servers.docs]
command = "npx"
args = ["-y", "docs-mcp"]

[mcp_servers.docs.env]
DOCS_TOKEN = "someone_elses_secret" # comment stays

[profiles.fast]
model = "fast"
"#;

#[test]
fn toml_add_appends_shogun_tables_and_keeps_the_file_as_its_prefix() {
    for client in [McpClient::CodexCli, McpClient::ChatGptDesktop] {
        let plan = add(client, Some(CODEX_WITH_OTHERS));
        assert_eq!(plan.change, Change::Add);
        assert_eq!(plan.other_entries, 1);
        let new = new_text(&plan);
        assert!(new.starts_with(CODEX_WITH_OTHERS), "{new}");
        let added = &new[CODEX_WITH_OTHERS.len()..];
        assert!(added.starts_with("\n[mcp_servers.shogun-memory]\n"), "{added}");
        // Both apps write the one shared connection; no token, forwarded or set.
        assert!(added.contains("\nargs = [\"--client\", \"codex-cli\"]\n"), "{added}");
        assert!(!added.contains("SHOGUN_API_TOKEN") && !added.contains("env_vars"), "{added}");
        let shown = diff_text(&plan);
        assert!(!shown.contains("someone_elses_secret") && !shown.contains("docs"), "{shown}");
        assert_no_token(&plan);
    }
    // No file: the generated tables alone. A file without a final newline gets one first.
    let plan = add(McpClient::CodexCli, None);
    assert!(new_text(&plan).starts_with("[mcp_servers.shogun-memory]\n"));
    let plan = add(McpClient::CodexCli, Some("model = \"x\""));
    assert!(new_text(&plan).starts_with("model = \"x\"\n\n[mcp_servers.shogun-memory]\n"), "{}", new_text(&plan));
}

#[test]
fn toml_replace_rewrites_only_shogun_tables_and_masks_the_old_token() {
    let existing = format!(
        "{CODEX_WITH_OTHERS}\n[mcp_servers.shogun-memory]\ncommand = \"/old\"\nargs = []\n\n[mcp_servers.shogun-memory.env]\nSHOGUN_DB_PATH = \"/old.db\"\nSHOGUN_API_TOKEN = \"{PASTED}\"\n\n# the next one is mine\n[mcp_servers.last]\ncommand = \"last\"\n"
    );
    let plan = add(McpClient::CodexCli, Some(&existing));
    assert_eq!(plan.change, Change::Replace);
    assert_eq!(plan.other_entries, 2);
    let new = new_text(&plan);
    assert!(new.starts_with(CODEX_WITH_OTHERS), "{new}");
    assert!(new.ends_with("\n# the next one is mine\n[mcp_servers.last]\ncommand = \"last\"\n"), "{new}");
    assert!(!new.contains(PASTED) && !new.contains("/old.db"), "{new}");
    let shown = diff_text(&plan);
    assert!(shown.contains(&format!("-SHOGUN_API_TOKEN = \"{MASK}\"")), "{shown}");
    assert!(shown.contains("+args = [\"--client\", \"codex-cli\"]"), "{shown}");
    assert!(!new.contains("SHOGUN_API_TOKEN"), "{new}");
    assert_no_token(&plan);
    // And again: unchanged, nothing to write.
    let again = add(McpClient::CodexCli, Some(new));
    assert_eq!((again.change, again.new_contents()), (Change::Unchanged, None));
}

#[test]
fn toml_remove_leaves_the_neighbours_and_their_comments() {
    let with_ours = new_text(&add(McpClient::CodexCli, Some(CODEX_WITH_OTHERS))).to_string();
    let plan = plan_remove(McpClient::CodexCli, Some(&with_ours)).unwrap();
    assert_eq!(plan.change, Change::Remove);
    assert_eq!(new_text(&plan), CODEX_WITH_OTHERS);
    // In the middle of the file, with a quoted header.
    let middle = "[mcp_servers.\"shogun-memory\"]\ncommand = \"x\"\n\n# docs server\n[mcp_servers.docs]\ncommand = \"d\"\n";
    let plan = plan_remove(McpClient::CodexCli, Some(middle)).unwrap();
    assert_eq!(new_text(&plan), "# docs server\n[mcp_servers.docs]\ncommand = \"d\"\n");
}

#[test]
fn toml_forms_this_editor_does_not_rewrite_are_refused() {
    for (text, why) in [
        ("[mcp_servers\ncommand = 1", Refusal::Unparseable),
        ("mcp_servers = 3\n", Refusal::UnexpectedShape),
        // SHOGUN's entry as dotted keys or an inline table: edit it by hand.
        ("[mcp_servers]\nshogun-memory.command = \"x\"\n", Refusal::UnsupportedEntryForm),
        ("mcp_servers = { shogun-memory = { command = \"x\" } }\n", Refusal::UnsupportedEntryForm),
    ] {
        assert_eq!(plan_add(McpClient::CodexCli, Some(text), CMD, DB).err(), Some(why), "{text}");
    }
    // `mcp_servers` as an inline table without SHOGUN in it: a header cannot extend it, and the
    // read-back check refuses rather than writing a file Codex would reject.
    let inline = "mcp_servers = { docs = { command = \"d\" } }\n";
    assert_eq!(plan_add(McpClient::CodexCli, Some(inline), CMD, DB).err(), Some(Refusal::NotVerified));
}

#[test]
fn toml_headers_are_read_like_toml_reads_them() {
    let path = |s: &str| toml_file::header_path(s);
    assert_eq!(path("[mcp_servers.shogun-memory]"), Some(vec!["mcp_servers".into(), "shogun-memory".into()]));
    assert_eq!(path("  [ mcp_servers . \"shogun-memory\" . env ]  # x"), Some(vec!["mcp_servers".into(), "shogun-memory".into(), "env".into()]));
    assert_eq!(path("[[profiles.list]]"), Some(vec!["profiles".into(), "list".into()]));
    assert_eq!(path("['a.b'.c]"), Some(vec!["a.b".into(), "c".into()]));
    assert_eq!(path("args = [\"x\"]"), None);
    assert_eq!(path("[\"x\", \"y\"],"), None);
    assert_eq!(path("[a..b]"), None);
}

#[test]
fn claude_code_runs_remove_then_add_naming_its_connection() {
    let claude = Path::new("/Users/u/.local/bin/claude");
    let runs = claude_code_add_runs(claude, CMD, DB);
    assert_eq!(runs.len(), 2);
    let (remove, add) = (&runs[0], &runs[1]);
    assert!(remove.may_fail && !add.may_fail);
    for run in &runs {
        assert_eq!(run.argv[0], ENV_PROGRAM);
        assert!(run.argv[1].starts_with("PATH=/Users/u/.local/bin:/opt/homebrew/bin:"), "{:?}", run.argv);
        assert_eq!(run.argv[2], "/Users/u/.local/bin/claude");
    }
    assert_eq!(remove.argv[3..], ["mcp", "remove", "shogun-memory", "--scope", "user"]);
    assert_eq!(
        add.argv[3..],
        [
            "mcp",
            "add",
            "--env",
            &format!("SHOGUN_DB_PATH={DB}"),
            "--transport",
            "stdio",
            "--scope",
            "user",
            "shogun-memory",
            "--",
            CMD,
            "--client",
            "claude-code",
        ]
    );
    // The dialog shows the CLI's own words; the copy-only text is the same argument list.
    assert_eq!(remove.display(), "/Users/u/.local/bin/claude mcp remove shogun-memory --scope user");
    let params = install_params(CMD, DB);
    let copied = crate::client_config::cli_command(McpClient::ClaudeCode, &params).unwrap();
    assert_eq!(add.display().replacen("/Users/u/.local/bin/claude", "claude", 1), copied);
    assert!(!copied.contains("SHOGUN_API_TOKEN"), "{copied}");
    assert_eq!(claude_code_remove_runs(claude), vec![CliRun { argv: remove.argv.clone(), may_fail: false }]);
}

#[test]
fn vendor_cli_is_looked_up_where_its_installers_put_it() {
    let home = Path::new("/Users/u");
    let claude = vendor_cli_candidates("claude", home);
    assert_eq!(claude[0], Path::new("/Users/u/.local/bin/claude"));
    assert_eq!(claude[1], Path::new("/Users/u/.claude/local/claude"));
    assert!(claude.contains(&PathBuf::from("/opt/homebrew/bin/claude")));
    assert!(!vendor_cli_candidates("codex", home).contains(&PathBuf::from("/Users/u/.claude/local/codex")));
}

#[test]
fn shell_words_are_quoted_only_when_the_shell_would_read_them() {
    assert_eq!(shell_word("/opt/x/shogun-mcp"), "/opt/x/shogun-mcp");
    assert_eq!(shell_word("with space"), "'with space'");
    assert_eq!(shell_word("it's"), "'it'\\''s'");
    assert_eq!(shell_word("A=${B}"), "'A=${B}'");
    assert_eq!(shell_word(""), "''");
}

#[test]
fn masking_hides_values_but_keeps_paths_and_references() {
    let entry = serde_json::json!({
        "command": "/x/shogun-mcp",
        "args": [PASTED],
        "env": {
            "SHOGUN_DB_PATH": "/Users/u/memory.db",
            "SHOGUN_MCP_TOOLSETS": "core",
            "SHOGUN_API_TOKEN": "${SHOGUN_API_TOKEN}",
            "EXTRA": "p\"w",
        }
    });
    let raw = serde_json::to_string_pretty(&entry).unwrap();
    let masked = mask_existing(&raw, &entry);
    assert!(masked.contains("/Users/u/memory.db") && masked.contains("\"core\""), "{masked}");
    assert!(masked.contains("${SHOGUN_API_TOKEN}"), "{masked}");
    assert!(!masked.contains(PASTED) && !masked.contains("p\\\"w"), "{masked}");
    assert!(looks_like_token(PASTED) && !looks_like_token("shogun_PASTE_ISSUED_TOKEN"));
}

#[test]
fn line_diff_marks_kept_added_and_removed_lines() {
    let d = line_diff("a\nb\nc", "a\nx\nc\nd");
    let ops: Vec<(DiffOp, &str)> = d.iter().map(|l| (l.op, l.text.as_str())).collect();
    assert_eq!(
        ops,
        [
            (DiffOp::Context, "a"),
            (DiffOp::Add, "x"),
            (DiffOp::Remove, "b"),
            (DiffOp::Context, "c"),
            (DiffOp::Add, "d"),
        ]
    );
    assert!(line_diff("", "").is_empty());
}

#[test]
fn digest_names_the_bytes_previewed() {
    assert_eq!(digest(None), ABSENT_DIGEST);
    assert_eq!(digest(Some("")), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    assert_ne!(digest(Some("a")), digest(Some("a ")));
}

#[test]
fn install_preview_names_the_method_and_what_is_added_never_a_token() {
    let docs = ServerParams::DOCS;
    for &client in ALL_CLIENTS {
        let preview = install_preview(client, docs.command, docs.db_path);
        assert_eq!(preview.method, install_method(client));
        assert_eq!(preview.apply, "in_app_only");
        let text = serde_json::to_string(&preview).unwrap();
        assert!(!looks_like_token(&text), "{text}");
        match preview.method {
            InstallMethod::ConfigFile => {
                assert!(preview.file.as_deref().unwrap().starts_with("~/"), "{text}");
                assert!(preview.adds.as_deref().unwrap().contains(docs.command), "{text}");
            }
            InstallMethod::VendorCli => {
                let adds = preview.adds.unwrap();
                assert!(adds.starts_with("claude mcp remove shogun-memory --scope user\nclaude mcp add "), "{adds}");
            }
            InstallMethod::CopyOnly => assert_eq!((preview.file, preview.adds), (None, None)),
        }
    }
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "shogun-client-install-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn cursor_plan(existing: Option<&str>) -> Result<FilePlan, Refusal> {
    plan_add(McpClient::Cursor, existing, CMD, DB)
}

#[test]
fn apply_writes_what_was_previewed_keeps_a_backup_and_the_mode() {
    let dir = TempDir::new("write");
    let path = dir.0.join("mcp.json");
    std::fs::write(&path, CURSOR_WITH_OTHERS).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    }
    let preview = cursor_plan(Some(CURSOR_WITH_OTHERS)).unwrap();
    let applied = apply_file_plan(&path, &preview.base_digest, cursor_plan).unwrap();
    assert_eq!(applied.change, Change::Add);
    let written = std::fs::read_to_string(&path).unwrap();
    assert_eq!(written, new_text(&preview));
    let backup = applied.backup.unwrap();
    assert_eq!(backup, backup_path(&std::fs::canonicalize(&path).unwrap()));
    assert_eq!(std::fs::read_to_string(&backup).unwrap(), CURSOR_WITH_OTHERS);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o640);
        assert_eq!(mode(&backup), 0o640);
    }
    // No temporary file is left behind.
    let names: Vec<String> = std::fs::read_dir(&dir.0)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert!(names.iter().all(|n| !n.ends_with(".tmp")), "{names:?}");

    // Applying the same preview again: the file changed since, so nothing is written.
    assert_eq!(apply_file_plan(&path, &preview.base_digest, cursor_plan), Err(ApplyError::Changed));
    // A fresh preview of the written file is "unchanged": no write, no new backup.
    let again = cursor_plan(Some(&written)).unwrap();
    std::fs::remove_file(&backup).unwrap();
    let applied = apply_file_plan(&path, &again.base_digest, cursor_plan).unwrap();
    assert_eq!((applied.change, applied.backup), (Change::Unchanged, None));
    assert!(!backup.exists());
}

#[test]
fn apply_creates_a_private_file_only_inside_an_existing_app_folder() {
    let dir = TempDir::new("create");
    let missing_app = dir.0.join("no-such-app").join("mcp.json");
    assert_eq!(apply_file_plan(&missing_app, ABSENT_DIGEST, cursor_plan), Err(ApplyError::AppNotFound));
    assert!(!missing_app.parent().unwrap().exists(), "ShogunAI does not create the app's folder");

    let path = dir.0.join("mcp.json");
    let applied = apply_file_plan(&path, ABSENT_DIGEST, cursor_plan).unwrap();
    assert_eq!((applied.change, applied.backup), (Change::Add, None));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }
}

#[test]
fn apply_refuses_a_file_that_does_not_parse_and_leaves_it_alone() {
    let dir = TempDir::new("refuse");
    let path = dir.0.join("mcp.json");
    std::fs::write(&path, "{ not json").unwrap();
    let digest_now = digest(Some("{ not json"));
    assert_eq!(
        apply_file_plan(&path, &digest_now, cursor_plan),
        Err(ApplyError::Refused(Refusal::Unparseable))
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
    assert!(!backup_path(&path).exists());
    std::fs::write(&path, [0xff, 0xfe, b'{']).unwrap();
    assert_eq!(read_config(&path), Err(ApplyError::Refused(Refusal::Unparseable)));
}

#[cfg(unix)]
#[test]
fn apply_writes_through_a_symlinked_config() {
    let dir = TempDir::new("symlink");
    let real = dir.0.join("dotfiles-mcp.json");
    std::fs::write(&real, "{}").unwrap();
    let link = dir.0.join("mcp.json");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    apply_file_plan(&link, &digest(Some("{}")), cursor_plan).unwrap();
    assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
    assert_eq!(json(&std::fs::read_to_string(&real).unwrap())["mcpServers"][SERVER_NAME]["command"], CMD);
}

#[test]
fn remove_through_apply_leaves_other_servers() {
    let dir = TempDir::new("remove");
    let path = dir.0.join("config.toml");
    let with_ours = new_text(&add(McpClient::CodexCli, Some(CODEX_WITH_OTHERS))).to_string();
    std::fs::write(&path, &with_ours).unwrap();
    let applied = apply_file_plan(&path, &digest(Some(&with_ours)), |e| {
        plan_remove(McpClient::CodexCli, e)
    })
    .unwrap();
    assert_eq!(applied.change, Change::Remove);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), CODEX_WITH_OTHERS);
}

#[test]
fn the_bundled_server_is_the_one_next_to_the_app_and_only_when_installed() {
    let yes = |_: &Path| true;
    let no = |_: &Path| false;
    let app = Path::new("/Applications/ShogunAI.app/Contents/MacOS/shogun-desktop-spike");
    assert_eq!(
        bundled_server(app, yes),
        ServerLocation::Bundled(PathBuf::from("/Applications/ShogunAI.app/Contents/MacOS/shogun-mcp"))
    );
    assert_eq!(bundled_server(app, no), ServerLocation::Missing);
    // The probe is asked about the sibling, nothing else.
    let asked = std::cell::RefCell::new(Vec::new());
    let _ = bundled_server(app, |p| {
        asked.borrow_mut().push(p.to_path_buf());
        false
    });
    assert_eq!(asked.into_inner(), [PathBuf::from("/Applications/ShogunAI.app/Contents/MacOS/shogun-mcp")]);
    // A path that will not survive: an ejected disk image, a translocated copy.
    assert_eq!(
        bundled_server(Path::new("/Volumes/ShogunAI/ShogunAI.app/Contents/MacOS/x"), yes),
        ServerLocation::DiskImage
    );
    assert_eq!(
        bundled_server(
            Path::new("/private/var/folders/ab/T/AppTranslocation/1234/d/ShogunAI.app/Contents/MacOS/x"),
            yes
        ),
        ServerLocation::Translocated
    );
}

#[test]
fn a_build_tree_is_never_offered_as_the_bundled_server() {
    // Issue #744: a dev run finds target/debug/shogun-mcp next to itself; writing that path into
    // Cursor or Codex leaves an entry that dies with the build tree. Not even probed.
    let asked = std::cell::RefCell::new(Vec::new());
    let probe = |p: &Path| {
        asked.borrow_mut().push(p.to_path_buf());
        true
    };
    for exe in [
        "/Users/me/src/ShogunAI/target/debug/shogun-desktop-spike",
        "/Users/me/src/ShogunAI/target/release/shogun-desktop-spike",
        "/Users/me/Contents/MacOS/shogun-desktop-spike",
        "/Users/me/.app/Contents/MacOS/shogun-desktop-spike",
        "shogun-desktop-spike",
    ] {
        assert_eq!(bundled_server(Path::new(exe), probe), ServerLocation::Missing, "{exe}");
    }
    assert!(asked.into_inner().is_empty(), "a non-bundle path is refused before any file probe");

    // Any installed bundle name still works (a renamed or suffixed copy in Applications).
    assert_eq!(
        bundled_server(Path::new("/Applications/ShogunAI Beta.app/Contents/MacOS/x"), |_: &Path| true),
        ServerLocation::Bundled(PathBuf::from("/Applications/ShogunAI Beta.app/Contents/MacOS/shogun-mcp"))
    );
}

fn ctx(home: &Path, server: ServerLocation, cli: Option<&str>) -> MountContext {
    MountContext { home: home.to_path_buf(), server, db_path: DB.to_string(), cli: cli.map(PathBuf::from) }
}

fn bundled() -> ServerLocation {
    ServerLocation::Bundled(PathBuf::from(CMD))
}

#[test]
fn preview_blocks_with_a_reason_instead_of_guessing() {
    let dir = TempDir::new("preview-blocks");
    let home = &dir.0;
    let never_read = |_: &Path| -> Result<Option<String>, ApplyError> { panic!("must not read") };
    // Claude Desktop has not run here: its folder is missing, and SHOGUN does not create it.
    let p = mount_preview(McpClient::ClaudeDesktop, MountAction::Add, &ctx(home, bundled(), None), never_read);
    assert_eq!(p.blocked_reason, Some("app_not_found"));
    // No bundled server, or one that will move.
    for (server, why) in [
        (ServerLocation::Missing, "server_missing"),
        (ServerLocation::DiskImage, "server_not_installed"),
        (ServerLocation::Translocated, "server_not_installed"),
    ] {
        let p = mount_preview(McpClient::ClaudeCode, MountAction::Add, &ctx(home, server, Some("/x/claude")), never_read);
        assert_eq!(p.blocked_reason, Some(why));
        assert!(p.commands.is_empty() && p.digest.is_empty());
    }
    // No CLI.
    let p = mount_preview(McpClient::ClaudeCode, MountAction::Add, &ctx(home, bundled(), None), never_read);
    assert_eq!(p.blocked_reason, Some("cli_not_found"));
    // No ~/.cursor: the app has not run here, and SHOGUN does not create its folder.
    let p = mount_preview(McpClient::Cursor, MountAction::Add, &ctx(home, bundled(), None), never_read);
    assert_eq!((p.blocked_reason, p.target.as_str()), (Some("app_not_found"), "~/.cursor/mcp.json"));
    // A file that does not parse.
    std::fs::create_dir_all(home.join(".cursor")).unwrap();
    std::fs::write(home.join(".cursor/mcp.json"), "{ nope").unwrap();
    let p = mount_preview(McpClient::Cursor, MountAction::Add, &ctx(home, bundled(), None), read_config);
    assert_eq!(p.blocked_reason, Some("unparseable"));
    let v = serde_json::to_value(&p).unwrap();
    assert!(v.get("blocked").is_none() && v["blocked_reason"] == "unparseable", "{v}");
}

#[test]
fn a_file_preview_then_apply_writes_exactly_what_was_shown() {
    let dir = TempDir::new("preview-apply");
    let home = &dir.0;
    std::fs::create_dir_all(home.join(".cursor")).unwrap();
    let path = home.join(".cursor/mcp.json");
    std::fs::write(&path, CURSOR_WITH_OTHERS).unwrap();
    let c = ctx(home, bundled(), None);
    let preview = mount_preview(McpClient::Cursor, MountAction::Add, &c, read_config);
    assert_eq!(preview.blocked_reason, None);
    assert_eq!(preview.change, Some(Change::Add));
    assert_eq!(preview.other_entries, 2);
    assert_eq!(preview.backup.as_deref(), Some("~/.cursor/mcp.json.shogun-backup"));
    let shown = serde_json::to_string(&preview).unwrap();
    assert!(!shown.contains("ghp_") && !looks_like_token(&shown), "{shown}");

    let no_cli = |_: &CliRun| -> Result<(), String> { panic!("no command for a file client") };
    assert_eq!(mount_apply(McpClient::Cursor, MountAction::Add, &c, &preview.digest, no_cli), Ok(Change::Add));
    let expected = plan_add(McpClient::Cursor, Some(CURSOR_WITH_OTHERS), CMD, DB).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), new_text(&expected));
    // The same confirmation cannot be replayed onto the changed file.
    assert_eq!(
        mount_apply(McpClient::Cursor, MountAction::Add, &c, &preview.digest, no_cli),
        Err(MountError::Changed)
    );
    // Remove, previewed and applied, puts the file back.
    let preview = mount_preview(McpClient::Cursor, MountAction::Remove, &c, read_config);
    assert_eq!(preview.change, Some(Change::Remove));
    assert_eq!(mount_apply(McpClient::Cursor, MountAction::Remove, &c, &preview.digest, no_cli), Ok(Change::Remove));
    let back = json(&std::fs::read_to_string(&path).unwrap());
    assert_eq!(back, json(CURSOR_WITH_OTHERS));
}

#[test]
fn a_copy_without_the_server_leaves_unverifiable_entries_alone() {
    let dir = TempDir::new("remove-no-server");
    std::fs::create_dir_all(dir.0.join(".cursor")).expect("client folder");
    let path = dir.0.join(".cursor/mcp.json");
    let stale = plan_add(McpClient::Cursor, Some(CURSOR_WITH_OTHERS), "/old/shogun-mcp", DB)
        .expect("stale entry");
    std::fs::write(&path, new_text(&stale)).expect("fixture");
    let c = ctx(&dir.0, ServerLocation::Missing, None);
    let preview = mount_preview(McpClient::Cursor, MountAction::Remove, &c, read_config);
    assert_eq!(preview.blocked_reason, Some("not_verified"));
    assert!(mount_apply(McpClient::Cursor, MountAction::Remove, &c, &digest(Some(new_text(&stale))),
        |_| panic!("no CLI")).is_err());
    assert_eq!(std::fs::read_to_string(&path).expect("unchanged"), new_text(&stale));
}

#[test]
fn a_copy_that_cannot_add_itself_says_why_by_where_it_runs() {
    assert_eq!(bundled().unavailable_reason(), None);
    assert_eq!(ServerLocation::Missing.unavailable_reason(), Some("server_missing"));
    // A disk image or a translocated copy does ship the server: it has to be moved first.
    assert_eq!(ServerLocation::DiskImage.unavailable_reason(), Some("server_not_installed"));
    assert_eq!(ServerLocation::Translocated.unavailable_reason(), Some("server_not_installed"));
}

#[test]
fn a_cli_preview_then_apply_runs_exactly_the_commands_shown() {
    let dir = TempDir::new("preview-cli");
    let c = ctx(&dir.0, bundled(), Some("/opt/homebrew/bin/claude"));
    let preview = mount_preview(McpClient::ClaudeCode, MountAction::Add, &c, |_| panic!("no file"));
    assert_eq!(preview.target, "/opt/homebrew/bin/claude");
    assert_eq!(preview.commands.len(), 2);
    assert!(preview.commands[1].contains(" -- /Applications/ShogunAI.app/Contents/MacOS/shogun-mcp"), "{:?}", preview.commands);
    let mut ran = Vec::new();
    let result = mount_apply(McpClient::ClaudeCode, MountAction::Add, &c, &preview.digest, |run| {
        ran.push(run.display());
        // The remove step fails when there is nothing to remove; that is expected.
        if run.may_fail { Err("No MCP server found".into()) } else { Ok(()) }
    });
    assert_eq!(result, Ok(Change::Add));
    assert_eq!(ran, preview.commands);
    // A digest for other commands (say the CLI moved) runs nothing.
    let moved = ctx(&dir.0, bundled(), Some("/Users/u/.local/bin/claude"));
    let result = mount_apply(McpClient::ClaudeCode, MountAction::Add, &moved, &preview.digest, |_| panic!("ran"));
    assert_eq!(result, Err(MountError::Changed));
    // A failing add is reported.
    let result = mount_apply(McpClient::ClaudeCode, MountAction::Add, &c, &preview.digest, |run| {
        if run.may_fail { Ok(()) } else { Err("exit 1".into()) }
    });
    assert_eq!(result, Err(MountError::Failed("exit 1".into())));
    let path = connection_path(McpClient::ClaudeCode, &dir.0);
    let source = connection_plan(McpClient::ClaudeCode, MountAction::Add, &c,
        Some(r#"{"theme":"dark","mcpServers":{"other":{"command":"keep"}}}"#))
        .expect("owned entry");
    let mut value = json(new_text(&source));
    value["mcpServers"][SERVER_NAME]["type"] = serde_json::json!("stdio");
    let original = serde_json::to_string(&value).expect("vendor JSON");
    std::fs::write(&path, &original).expect("Claude user config");
    let preview = mount_preview(McpClient::ClaudeCode, MountAction::Remove, &c, read_config);
    assert_eq!(preview.method, InstallMethod::VendorCli);
    assert_eq!(preview.commands.len(), 1);
    assert_eq!(preview.change, Some(Change::Remove));
    let mut removed = Vec::new();
    assert_eq!(mount_apply(McpClient::ClaudeCode, MountAction::Remove, &c, &preview.digest,
        |run| { removed.push(run.display()); Ok(()) }), Ok(Change::Remove));
    assert_eq!(removed, preview.commands);
    assert_eq!(std::fs::read_to_string(&path).expect("read-only inspection"), original);
    assert!(!backup_path(&path).exists());
    value["mcpServers"][SERVER_NAME]["env"]["CUSTOM"] = serde_json::json!("keep");
    let custom = serde_json::to_string(&value).expect("custom entry");
    std::fs::write(&path, &custom).expect("later edit");
    assert!(mount_apply(McpClient::ClaudeCode, MountAction::Remove, &c, &preview.digest,
        |_| panic!("custom entry must not be removed")).is_err());
    assert_eq!(std::fs::read_to_string(&path).expect("kept custom"), custom);
}

#[test]
fn settings_ids_and_actions_parse() {
    for &client in ALL_CLIENTS {
        assert_eq!(client_by_id(client.id()), Some(client));
    }
    assert_eq!(client_by_id("vscode"), None);
    assert_eq!(MountAction::parse("add"), Some(MountAction::Add));
    assert_eq!(MountAction::parse("remove"), Some(MountAction::Remove));
    assert_eq!(MountAction::parse("Add"), None);
}

mod auto_connect {
    use super::*;
    use crate::client_discovery::DiscoveredTool;
    use crate::memory_api_settings::{record_decline, Settings};

    fn tool(id: &'static str, setup: Option<&'static str>) -> DiscoveredTool {
        DiscoveredTool { id, label: id, setup_client: setup }
    }

    fn found() -> Vec<DiscoveredTool> {
        vec![
            tool("chatgpt-desktop", None),
            tool("claude-code", Some("claude-code")),
            tool("claude-desktop", Some("claude-desktop")),
            tool("codex", Some("codex-cli")),
            tool("cursor", Some("cursor")),
            tool("zed", None),
        ]
    }

    fn on() -> Settings {
        Settings { enabled: true, ..Settings::default() }
    }

    fn bundled() -> ServerLocation {
        ServerLocation::Bundled(PathBuf::from(CMD))
    }

    fn ids(clients: Vec<McpClient>) -> Vec<&'static str> {
        clients.into_iter().map(McpClient::id).collect()
    }

    #[test]
    fn adds_every_found_client_once() {
        // ChatGPT desktop and Codex share one connection: one add, not two. Zed is no client.
        let plan = ids(auto_connect_plan(&found(), &[], &on(), &bundled(), true));
        assert_eq!(plan, ["chatgpt-desktop", "claude-code", "claude-desktop", "cursor"]);
    }

    #[test]
    fn needs_the_api_on_auto_on_the_plan_and_a_bundled_server() {
        let off = Settings::default();
        assert!(auto_connect_plan(&found(), &[], &off, &bundled(), true).is_empty());
        let manual = Settings { auto_connect: false, ..on() };
        assert!(auto_connect_plan(&found(), &[], &manual, &bundled(), true).is_empty());
        assert!(auto_connect_plan(&found(), &[], &on(), &bundled(), false).is_empty());
        for server in [ServerLocation::Missing, ServerLocation::DiskImage, ServerLocation::Translocated] {
            assert!(auto_connect_plan(&found(), &[], &on(), &server, true).is_empty(), "{server:?}");
        }
    }

    #[test]
    fn skips_connected_and_declined_apps_including_siblings() {
        let connected = vec!["codex-cli".to_string(), "cursor".to_string()];
        let mut settings = on();
        // Removing the ChatGPT desktop app declines the shared Codex connection too.
        record_decline(&mut settings, "chatgpt-desktop");
        record_decline(&mut settings, "claude-code");
        assert_eq!(settings.auto_connect_declined, ["codex-cli", "claude-code"]);
        let plan = ids(auto_connect_plan(&found(), &connected, &settings, &bundled(), true));
        assert_eq!(plan, ["claude-desktop"]);
        let plan = ids(auto_connect_plan(&found(), &[], &settings, &bundled(), true));
        assert_eq!(plan, ["claude-desktop", "cursor"]);
    }

    #[test]
    fn is_idempotent_once_connected() {
        let first = auto_connect_plan(&found(), &[], &on(), &bundled(), true);
        let connected: Vec<String> = first
            .iter()
            .filter_map(|c| crate::memory_api_settings::grant_id(c.id()))
            .map(str::to_string)
            .collect();
        assert!(auto_connect_plan(&found(), &connected, &on(), &bundled(), true).is_empty());
    }

    #[test]
    fn old_settings_files_default_to_auto_connect_on() {
        let old: Settings = serde_json::from_str(r#"{"enabled":false,"profile":{}}"#).unwrap();
        assert!(old.auto_connect && !old.enabled && old.auto_connect_declined.is_empty());
        assert!(!Settings::default().enabled && Settings::default().auto_connect);
    }

    #[test]
    fn explicit_add_clears_a_decline() {
        let mut settings = on();
        settings.auto_connected.push("cursor".into());
        record_decline(&mut settings, "cursor");
        assert!(settings.auto_connected.is_empty());
        crate::memory_api_settings::clear_decline(&mut settings, "cursor");
        assert!(settings.auto_connect_declined.is_empty());
        assert_eq!(ids(auto_connect_plan(&[tool("cursor", Some("cursor"))], &[], &settings, &bundled(), true)), ["cursor"]);
    }
}


#[test]
fn disconnect_removes_only_exact_owned_entries_in_every_format() {
    let c = ctx(Path::new("/tmp"), bundled(), None);
    for client in ALL_CLIENTS.iter().copied() {
        let original = if matches!(client, McpClient::CodexCli | McpClient::ChatGptDesktop) {
            "# keep comment\nmodel = \"keep\"\n[mcp_servers.other]\ncommand = \"keep\"\n"
        } else {
            r#"{"theme":"dark","mcpServers":{"other":{"command":"keep"}},"projects":{"untouched":true}}"#
        };
        let add = connection_plan(client, MountAction::Add, &c, Some(original)).expect("add");
        let source = add.new_contents().expect("owned entry");
        let remove = connection_plan(client, MountAction::Remove, &c, Some(source)).expect("owned removal");
        let restored = remove.new_contents().expect("removed");
        let absent = connection_plan(client, MountAction::Remove, &c, Some(restored)).expect("absent");
        assert_eq!(absent.change, Change::Absent);
        if matches!(client, McpClient::CodexCli | McpClient::ChatGptDesktop) {
            assert!(restored.contains("model = \"keep\""));
            assert!(restored.contains("[mcp_servers.other]"));
        } else {
            assert_eq!(json(restored), json(original));
        }
        for custom in [
            source.replace(CMD, "/custom/shogun-mcp"),
            source.replace("--client", "--custom"),
            source.replace("SHOGUN_DB_PATH", "CUSTOM_DB_PATH"),
        ] {
            assert_eq!(connection_plan(client, MountAction::Remove, &c, Some(&custom)),
                Err(Refusal::NotVerified));
        }
        assert!(connection_plan(client, MountAction::Remove, &c, Some("{broken")).is_err());
    }
}

#[test]
fn decline_after_planning_prevents_the_write() {
    let dir = TempDir::new("declined-before-write");
    std::fs::create_dir_all(dir.0.join(".cursor")).expect("client folder");
    let path = connection_path(McpClient::Cursor, &dir.0);
    let original = "{\"mcpServers\": {}}";
    std::fs::write(&path, original).expect("fixture");
    let ctx = MountContext {
        home: dir.0.clone(),
        server: ServerLocation::Bundled(
            "/Applications/ShogunAI.app/Contents/MacOS/shogun-mcp".into(),
        ),
        db_path: "/memory.db".into(),
        cli: None,
    };
    let plan = connection_plan(McpClient::Cursor, MountAction::Add, &ctx, Some(original))
        .expect("plan before slow step");
    let mut settings = crate::memory_api_settings::Settings {
        enabled: true,
        ..Default::default()
    };
    assert!(auto_connect_allowed(&settings, "cursor"));
    crate::memory_api_settings::record_decline(&mut settings, "cursor");
    assert_eq!(
        apply_auto_connection_file(McpClient::Cursor, &ctx, &plan.base_digest, &settings)
            .expect("recheck"),
        None
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("unchanged fixture"),
        original
    );
    assert!(!backup_path(&path).exists());
    settings.auto_connect_declined.clear();
    settings.auto_connect = false;
    assert!(!auto_connect_allowed(&settings, "cursor"));
    settings.auto_connect = true;
    settings.enabled = false;
    assert!(!auto_connect_allowed(&settings, "cursor"));
}

#[test]
fn auto_undo_restores_original_bytes_and_refuses_later_edits() {
    let dir = TempDir::new("auto-undo");
    let path = dir.0.join("config.json");
    let original = "{\"theme\": \"dark\"}\n";
    std::fs::write(&path, original).expect("fixture");
    let ctx = MountContext {
        home: dir.0.clone(),
        server: ServerLocation::Bundled(
            "/Applications/ShogunAI.app/Contents/MacOS/shogun-mcp".into(),
        ),
        db_path: "/memory.db".into(),
        cli: None,
    };
    let plan = connection_plan(McpClient::Cursor, MountAction::Add, &ctx, Some(original))
        .expect("add plan");
    let receipt = crate::memory_api_settings::AutoConnectNotice {
        client: "cursor".into(),
        before_digest: plan.base_digest.clone(),
        after_digest: digest(plan.new_contents()),
    };
    apply_file_plan(&path, &plan.base_digest, |text| {
        connection_plan(McpClient::Cursor, MountAction::Add, &ctx, text)
    })
    .expect("apply");
    undo_connection_file(&path, &receipt).expect("undo");
    assert_eq!(std::fs::read_to_string(&path).expect("restored"), original);
    std::fs::write(&path, "{\"changed\":true}").expect("later user edit");
    assert_eq!(
        undo_connection_file(&path, &receipt),
        Err(ApplyError::Changed)
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("kept user edit"),
        "{\"changed\":true}"
    );
}

#[test]
fn automatic_write_refuses_a_differing_entry_without_changing_the_file() {
    let dir = TempDir::new("auto-conflict");
    std::fs::create_dir_all(dir.0.join(".cursor")).expect("client folder");
    let path = connection_path(McpClient::Cursor, &dir.0);
    let original =
        r#"{"mcpServers":{"shogun-memory":{"command":"custom"},"other":{"command":"keep"}}}"#;
    std::fs::write(&path, original).expect("fixture");
    let ctx = MountContext {
        home: dir.0.clone(),
        server: ServerLocation::Bundled(
            "/Applications/ShogunAI.app/Contents/MacOS/shogun-mcp".into(),
        ),
        db_path: "/memory.db".into(),
        cli: None,
    };
    let settings = crate::memory_api_settings::Settings {
        enabled: true,
        ..Default::default()
    };
    assert_eq!(
        connection_plan(McpClient::Cursor, MountAction::Add, &ctx, Some(original))
            .expect("conflict plan")
            .change,
        Change::Replace
    );
    assert_eq!(
        apply_auto_connection_file(McpClient::Cursor, &ctx, &digest(Some(original)), &settings),
        Err(ApplyError::Refused(Refusal::NotVerified))
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("kept config"),
        original
    );
    assert!(!backup_path(&path).exists());
}

#[test]
fn refresh_recognizes_old_generated_entries_and_keeps_original_backup() {
    let dir = TempDir::new("auto-refresh");
    std::fs::create_dir_all(dir.0.join(".cursor")).expect("client folder");
    let path = connection_path(McpClient::Cursor, &dir.0);
    let original = r#"{"mcpServers":{"notes":{"command":"keep"}}}"#;
    let stale = r#"{"mcpServers":{"notes":{"command":"keep"},"shogun-memory":{"command":"/Applications/Old SHOGUN.app/Contents/MacOS/shogun-mcp","args":["--client","cursor"],"env":{"SHOGUN_DB_PATH":"/old/memory.db"}}}}"#;
    std::fs::write(&path, stale).expect("stale managed config");
    std::fs::write(backup_path(&path), original).expect("original auto-connect backup");
    assert!(is_managed_auto_connection(McpClient::Cursor, Some(stale)));

    let ctx = ctx(&dir.0, bundled(), None);
    let settings = crate::memory_api_settings::Settings { enabled: true, ..Default::default() };
    apply_auto_refresh_file(McpClient::Cursor, &ctx, &digest(Some(stale)), &settings)
        .expect("refresh");

    let refreshed = std::fs::read_to_string(&path).expect("refreshed config");
    let value = json(&refreshed);
    assert_eq!(value["mcpServers"][SERVER_NAME]["command"], CMD);
    assert_eq!(value["mcpServers"][SERVER_NAME]["env"][DB_PATH_ENV], DB);
    assert_eq!(value["mcpServers"]["notes"]["command"], "keep");
    assert_eq!(std::fs::read_to_string(backup_path(&path)).expect("preserved backup"), original);
}

#[test]
fn refresh_leaves_custom_or_absent_entries_alone() {
    let dir = TempDir::new("auto-refresh-custom");
    std::fs::create_dir_all(dir.0.join(".cursor")).expect("client folder");
    let path = connection_path(McpClient::Cursor, &dir.0);
    let custom = r#"{"mcpServers":{"shogun-memory":{"command":"custom-wrapper","args":["--client","cursor"],"env":{"SHOGUN_DB_PATH":"/memory.db"}}}}"#;
    std::fs::write(&path, custom).expect("custom config");
    let ctx = ctx(&dir.0, bundled(), None);
    let settings = crate::memory_api_settings::Settings { enabled: true, ..Default::default() };

    assert!(!is_managed_auto_connection(McpClient::Cursor, Some(custom)));
    assert_eq!(
        apply_auto_refresh_file(McpClient::Cursor, &ctx, &digest(Some(custom)), &settings)
            .expect("custom config is skipped"),
        None
    );
    assert_eq!(std::fs::read_to_string(&path).expect("unchanged config"), custom);
    assert!(!is_managed_auto_connection(McpClient::Cursor, None));
    let dev_build = r#"{"mcpServers":{"shogun-memory":{"command":"/src/ShogunAI/target/debug/shogun-mcp","args":["--client","cursor"],"env":{"SHOGUN_DB_PATH":"/memory.db"}}}}"#;
    assert!(!is_managed_auto_connection(McpClient::Cursor, Some(dev_build)));

    std::fs::remove_file(&path).expect("simulate user removal");
    assert_eq!(
        apply_auto_refresh_file(McpClient::Cursor, &ctx, ABSENT_DIGEST, &settings)
            .expect("removed connection stays removed"),
        None
    );
    assert!(!path.exists());
}

#[test]
fn failed_refresh_rolls_back_only_refresh_and_keeps_original_backup() {
    let dir = TempDir::new("auto-refresh-rollback");
    std::fs::create_dir_all(dir.0.join(".cursor")).expect("client folder");
    let path = connection_path(McpClient::Cursor, &dir.0);
    let original = r#"{"mcpServers":{"notes":{"command":"keep"}}}"#;
    let before_refresh = r#"{"mcpServers":{"notes":{"command":"keep"},"shogun-memory":{"command":"/Applications/Old SHOGUN.app/Contents/MacOS/shogun-mcp","args":["--client","cursor"],"env":{"SHOGUN_DB_PATH":"/old/memory.db"}}}}"#;
    std::fs::write(&path, before_refresh).expect("stale managed config");
    std::fs::write(backup_path(&path), original).expect("original backup");
    let ctx = ctx(&dir.0, bundled(), None);
    let settings = crate::memory_api_settings::Settings { enabled: true, ..Default::default() };

    let edit = connection_plan(McpClient::Cursor, MountAction::Add, &ctx, Some(before_refresh))
        .expect("refresh plan");
    apply_auto_refresh_file(McpClient::Cursor, &ctx, &edit.base_digest, &settings)
        .expect("refresh");
    let after_refresh = std::fs::read_to_string(&path).expect("refreshed config");
    rollback_auto_refresh_file(
        &path,
        &digest(Some(&after_refresh)),
        before_refresh,
    )
    .expect("rollback refresh");

    assert_eq!(std::fs::read_to_string(&path).expect("restored config"), before_refresh);
    assert_eq!(std::fs::read_to_string(backup_path(&path)).expect("original backup kept"), original);
}

#[test]
fn undo_a_new_config_deletes_only_the_unchanged_created_file() {
    let dir = TempDir::new("auto-new-file");
    std::fs::create_dir_all(dir.0.join(".cursor")).expect("client folder");
    let path = connection_path(McpClient::Cursor, &dir.0);
    let ctx = MountContext {
        home: dir.0.clone(),
        server: ServerLocation::Bundled(
            "/Applications/ShogunAI.app/Contents/MacOS/shogun-mcp".into(),
        ),
        db_path: "/memory.db".into(),
        cli: None,
    };
    let settings = crate::memory_api_settings::Settings {
        enabled: true,
        ..Default::default()
    };
    let plan =
        connection_plan(McpClient::Cursor, MountAction::Add, &ctx, None).expect("new file plan");
    let receipt = crate::memory_api_settings::AutoConnectNotice {
        client: "cursor".into(),
        before_digest: ABSENT_DIGEST.into(),
        after_digest: digest(plan.new_contents()),
    };
    assert!(
        apply_auto_connection_file(McpClient::Cursor, &ctx, ABSENT_DIGEST, &settings)
            .expect("new file")
            .is_some()
    );
    undo_connection_file(&path, &receipt).expect("undo created file");
    assert!(!path.exists());
}

#[test]
fn claude_inspection_respects_the_vendor_config_directory() {
    let home = Path::new("/Users/test");
    assert_eq!(claude_config_path(home, None), home.join(".claude.json"));
    assert_eq!(claude_config_path(home, Some(Path::new("/custom/claude"))),
        PathBuf::from("/custom/claude/.claude.json"));
    assert_eq!(claude_config_path(home, Some(Path::new(""))), home.join(".claude.json"));
}

#[test]
fn automatic_claude_add_and_undo_only_run_vendor_commands() {
    let dir = TempDir::new("auto-cli");
    let c = ctx(&dir.0, bundled(), Some("/opt/homebrew/bin/claude"));
    let path = connection_path(McpClient::ClaudeCode, &c.home);
    let before = r#"{"theme":"dark","mcpServers":{"other":{"command":"keep"}}}"#;
    std::fs::write(&path, before).expect("fixture");
    let settings = crate::memory_api_settings::Settings { enabled: true, ..Default::default() };
    let edit = connection_plan(McpClient::ClaudeCode, MountAction::Add, &c, Some(before)).expect("plan");
    let mut runs = Vec::new();
    let result = apply_auto_connection(McpClient::ClaudeCode, &c, &edit.base_digest, &settings,
        |run| { runs.push(run.display()); Ok(()) }).expect("CLI add");
    assert_eq!(result.expect("applied").change, Change::Add);
    assert_eq!(runs.len(), 1);
    assert!(runs[0].contains(" mcp add "));
    assert_eq!(std::fs::read_to_string(&path).expect("never written"), before);
    assert!(!backup_path(&path).exists());
    assert_eq!(apply_auto_connection_file(McpClient::ClaudeCode, &c, &edit.base_digest, &settings),
        Err(ApplyError::Refused(Refusal::CopyOnly)));
    // Simulate the vendor's write; Undo must leave the file itself to the vendor too.
    let mut value = json(edit.new_contents().expect("intended entry"));
    value["mcpServers"][SERVER_NAME]["type"] = serde_json::json!("stdio");
    let owned = serde_json::to_string(&value).expect("vendor JSON");
    std::fs::write(&path, &owned).expect("vendor write");
    let receipt = crate::memory_api_settings::AutoConnectNotice {
        client: "claude-code".into(), before_digest: edit.base_digest,
        after_digest: digest(Some(&owned)),
    };
    runs.clear();
    cleanup_connection(McpClient::ClaudeCode, &c, Some(&receipt),
        |run| { runs.push(run.display()); Ok(()) }).expect("CLI undo");
    assert_eq!(runs.len(), 1);
    assert!(runs[0].contains(" mcp remove shogun-memory --scope user"));
    assert_eq!(std::fs::read_to_string(&path).expect("never written"), owned);
    value["mcpServers"][SERVER_NAME]["args"] = serde_json::json!(["--client", "custom"]);
    let custom = serde_json::to_string(&value).expect("custom JSON");
    std::fs::write(&path, &custom).expect("custom write");
    assert!(cleanup_connection(McpClient::ClaudeCode, &c, Some(&receipt),
        |_| panic!("custom entry must survive")).is_err());
    assert!(apply_auto_connection(McpClient::ClaudeCode, &c, &digest(Some(&custom)), &settings,
        |_| panic!("custom entry must survive")).is_err());
    assert_eq!(std::fs::read_to_string(&path).expect("custom survived"), custom);
}
