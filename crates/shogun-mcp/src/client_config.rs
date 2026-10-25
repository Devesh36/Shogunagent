//! The MCP client entry that mounts SHOGUN memory in another agent (issue #400): what a user pastes
//! into Claude Code, Codex CLI / the ChatGPT desktop app, Cursor or Claude Desktop so it launches
//! the stdio `shogun-mcp` server.
//!
//! Rendered here, once, so the docs snippets, the `docs/examples/` files, the Settings "Add to …"
//! panel and the `mount` block of `api.tokens.list` ([`mount_setup`]) all come from the same code.
//! The tests pin every docs copy to this output, so the examples are generated, not
//! hand-maintained.
//!
//! Only each client's documented MCP servers entry (and, where the vendor documents one, its own
//! CLI command) is produced. Nothing here reads or writes a client's config file or runs a
//! client's CLI: copying is the user's; the one-click setup behind a confirmation dialog is
//! [`crate::client_install`], which builds on these renderers.
//!
//! The mount setup never carries a token (issue #963). The entry names the app it was written for
//! (`args = ["--client", <id>]`, [`TokenValue::Connection`]); whether that app may connect, and
//! with what scope, is its connection in the Keychain, made in Settings ("Add to …"). Nothing in
//! the entry is secret, so nothing has to travel through the clipboard, a shell profile or the
//! login session's environment, and SHOGUN can write every client's file itself — Claude Desktop
//! included. The older form, where the entry references `SHOGUN_API_TOKEN`
//! ([`TokenValue::FromEnvironment`]), still renders for tests and still works when the server is
//! started that way.
//!
//! The env block carries exactly what the release `shogun-mcp` binary reads: the database path,
//! and optionally the toolsets filter (and, in the older form, the API token). The settings file is found next to the
//! database (`SHOGUN_MEMORY_API_SETTINGS` is a debug-build override only,
//! [`crate::memory_api_settings::resolve_settings_path`]), so it is not part of the entry.

use std::fmt::Write as _;

/// The server name every client lists SHOGUN under.
pub const SERVER_NAME: &str = "shogun-memory";

/// The database path the stdio binary opens (`crates/shogun-core/src/bin/shogun_mcp.rs`).
pub const DB_PATH_ENV: &str = "SHOGUN_DB_PATH";

/// The issued Memory API token the stdio binary checks before serving.
pub const API_TOKEN_ENV: &str = "SHOGUN_API_TOKEN";

/// What stands in for the token wherever a client can only take a literal value.
pub const TOKEN_PLACEHOLDER: &str = "shogun_PASTE_ISSUED_TOKEN";

/// The argument that names the app an entry was written for (issue #963).
pub const CLIENT_ARG: &str = "--client";

/// The app named on `shogun-mcp`'s command line (`--client <id>` or `--client=<id>`), or `None`
/// for an entry in the older form. A malformed id, a missing value or a second `--client` is an
/// error: an entry that says something unclear about who it is must not fall back to a token.
/// Other arguments are left alone, as they always were.
pub fn client_arg(args: impl IntoIterator<Item = String>) -> Result<Option<String>, String> {
    let mut args = args.into_iter();
    let mut found: Option<String> = None;
    while let Some(arg) = args.next() {
        let value = if arg == CLIENT_ARG {
            args.next().ok_or_else(|| format!("{CLIENT_ARG} needs the app's id"))?
        } else if let Some(value) = arg.strip_prefix(&format!("{CLIENT_ARG}=")) {
            value.to_string()
        } else {
            continue;
        };
        let valid = !value.is_empty()
            && value.len() <= crate::memory_api_settings::MAX_CLIENT_ID_BYTES
            && value.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !valid {
            return Err(format!("{CLIENT_ARG} must be an app id such as cursor"));
        }
        if found.replace(value).is_some() {
            return Err(format!("{CLIENT_ARG} is given twice"));
        }
    }
    Ok(found)
}

/// A client that can launch a local stdio MCP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpClient {
    ClaudeCode,
    CodexCli,
    /// Reads the same configuration as Codex CLI on the same Mac (OpenAI's MCP docs: the ChatGPT
    /// desktop app, Codex CLI and the IDE extension share MCP configuration for one Codex host).
    ChatGptDesktop,
    Cursor,
    ClaudeDesktop,
}

/// Every client, in the order the docs and Settings list them.
pub const ALL_CLIENTS: &[McpClient] = &[
    McpClient::ClaudeCode,
    McpClient::CodexCli,
    McpClient::ChatGptDesktop,
    McpClient::Cursor,
    McpClient::ClaudeDesktop,
];

/// The file format of a client's MCP config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigFormat {
    /// `{ "mcpServers": { … } }`
    Json,
    /// `[mcp_servers.<name>]` tables in Codex's `config.toml`.
    Toml,
}

impl ConfigFormat {
    /// The Markdown code-fence language for a snippet in this format.
    pub fn fence(self) -> &'static str {
        match self {
            ConfigFormat::Json => "json",
            ConfigFormat::Toml => "toml",
        }
    }
}

impl McpClient {
    /// A stable id (Settings, mount telemetry).
    pub fn id(self) -> &'static str {
        match self {
            McpClient::ClaudeCode => "claude-code",
            McpClient::CodexCli => "codex-cli",
            McpClient::ChatGptDesktop => "chatgpt-desktop",
            McpClient::Cursor => "cursor",
            McpClient::ClaudeDesktop => "claude-desktop",
        }
    }

    /// The client's name as shown to the user.
    pub fn label(self) -> &'static str {
        match self {
            McpClient::ClaudeCode => "Claude Code",
            McpClient::CodexCli => "Codex CLI",
            McpClient::ChatGptDesktop => "ChatGPT desktop",
            McpClient::Cursor => "Cursor",
            McpClient::ClaudeDesktop => "Claude Desktop",
        }
    }

    /// The format of the client's MCP config.
    pub fn format(self) -> ConfigFormat {
        match self {
            McpClient::CodexCli | McpClient::ChatGptDesktop => ConfigFormat::Toml,
            McpClient::ClaudeCode | McpClient::Cursor | McpClient::ClaudeDesktop => {
                ConfigFormat::Json
            }
        }
    }

    /// Where the client documents its MCP config on macOS.
    pub fn config_location(self) -> &'static str {
        match self {
            // Project scope. `claude mcp add-json --scope user` keeps it out of the repository.
            McpClient::ClaudeCode => ".mcp.json",
            McpClient::CodexCli | McpClient::ChatGptDesktop => "~/.codex/config.toml",
            McpClient::Cursor => "~/.cursor/mcp.json",
            McpClient::ClaudeDesktop => "~/Library/Application Support/Claude/claude_desktop_config.json",
        }
    }

    /// Where the token comes from when the entry references it rather than holding it.
    pub fn token_source(self) -> TokenSource {
        match self {
            // Started from a terminal, so the shell's environment is theirs.
            McpClient::ClaudeCode | McpClient::CodexCli => TokenSource::Shell,
            // Apps opened from the Dock see the login session's environment, not a shell's.
            McpClient::ChatGptDesktop | McpClient::Cursor => TokenSource::LoginSession,
            // No documented way to read a variable into its config: the value goes in the file.
            McpClient::ClaudeDesktop => TokenSource::Paste,
        }
    }

    /// The generated example under `docs/examples/`. Clients that share a config share a file.
    pub fn example_file(self) -> &'static str {
        match self {
            McpClient::ClaudeCode => "claude-code-mcp.shogun.json",
            McpClient::CodexCli | McpClient::ChatGptDesktop => "codex-config.shogun.toml",
            McpClient::Cursor => "cursor-mcp.shogun.json",
            McpClient::ClaudeDesktop => "claude-desktop.shogun.json",
        }
    }
}

/// How a client gets `SHOGUN_API_TOKEN` when the entry references it (the Settings panel says
/// how to set it for each).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenSource {
    /// The environment of the terminal the client is started from.
    Shell,
    /// The login session's environment (`launchctl setenv`), which apps opened from the Dock see.
    LoginSession,
    /// The user replaces [`TOKEN_PLACEHOLDER`] in the entry with the token.
    Paste,
    /// No token at all (issue #963): the entry names the app, and the app's connection in SHOGUN
    /// decides. What every generated setup uses.
    Connection,
}

/// What the entry puts under `SHOGUN_API_TOKEN`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenValue<'a> {
    /// This exact text (the docs placeholder; a test value).
    Literal(&'a str),
    /// A reference to the client's own `SHOGUN_API_TOKEN`, in the syntax the client documents:
    /// `${SHOGUN_API_TOKEN}` (Claude Code), `${env:SHOGUN_API_TOKEN}` (Cursor), Codex's
    /// `env_vars` forwarding list. Claude Desktop documents none, so it gets
    /// [`TOKEN_PLACEHOLDER`]. The older form.
    FromEnvironment,
    /// No token (issue #963): the entry passes `--client <connection id>` and nothing secret.
    Connection,
}

/// The values that go into a client entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerParams<'a> {
    /// Absolute path of the `shogun-mcp` executable.
    pub command: &'a str,
    /// Absolute path of `memory.db`.
    pub db_path: &'a str,
    /// The token, how the client finds it, or (what SHOGUN writes) the app's connection.
    pub api_token: TokenValue<'a>,
    /// `SHOGUN_MCP_TOOLSETS`; `None` leaves it out (every tool is listed).
    pub toolsets: Option<&'a str>,
}

impl ServerParams<'static> {
    /// The placeholders the public docs show.
    pub const DOCS: ServerParams<'static> = ServerParams {
        command: "/ABS/PATH/TO/target/release/shogun-mcp",
        db_path: "/Users/YOU/Library/Application Support/com.syogun.shogunai/memory.db",
        api_token: TokenValue::Connection,
        toolsets: None,
    };
}

impl ServerParams<'_> {
    /// The value written under `SHOGUN_API_TOKEN` in `client`'s env block, or `None` when the
    /// client forwards the variable instead of setting it (Codex's `env_vars`).
    fn token_entry(&self, client: McpClient) -> Option<&str> {
        match self.api_token {
            TokenValue::Literal(value) => Some(value),
            TokenValue::FromEnvironment => match client {
                McpClient::ClaudeCode => Some("${SHOGUN_API_TOKEN}"),
                McpClient::Cursor => Some("${env:SHOGUN_API_TOKEN}"),
                McpClient::ClaudeDesktop => Some(TOKEN_PLACEHOLDER),
                McpClient::CodexCli | McpClient::ChatGptDesktop => None,
            },
            TokenValue::Connection => None,
        }
    }

    /// Whether the entry forwards `SHOGUN_API_TOKEN` by name (Codex's `env_vars`, older form).
    fn forwards_token(&self, client: McpClient) -> bool {
        self.api_token == TokenValue::FromEnvironment && self.token_entry(client).is_none()
    }

    /// The server's arguments in `client`'s entry: `--client <connection id>` (issue #963), or
    /// none in the older form.
    pub fn args(&self, client: McpClient) -> Vec<String> {
        match self.api_token {
            TokenValue::Connection => vec![
                CLIENT_ARG.to_string(),
                crate::memory_api_settings::grant_id(client.id())
                    .unwrap_or(client.id())
                    .to_string(),
            ],
            TokenValue::Literal(_) | TokenValue::FromEnvironment => Vec::new(),
        }
    }

    /// The env block for `client`, in the order it is written.
    pub(crate) fn env(&self, client: McpClient) -> Vec<(&'static str, &str)> {
        let mut env = vec![(DB_PATH_ENV, self.db_path)];
        if let Some(token) = self.token_entry(client) {
            env.push((API_TOKEN_ENV, token));
        }
        if let Some(toolsets) = self.toolsets {
            env.push((crate::toolsets::ENV_VAR, toolsets));
        }
        env
    }
}

/// The config entry for `client`, as the text of a whole config file (ending in a newline). A user
/// with an existing file merges the `shogun-memory` entry into it.
pub fn render_client_config(client: McpClient, params: &ServerParams<'_>) -> String {
    match client.format() {
        ConfigFormat::Json => render_json(client, params),
        ConfigFormat::Toml => render_toml(client, params),
    }
}

fn render_json(client: McpClient, params: &ServerParams<'_>) -> String {
    format!(
        "{{\n  \"mcpServers\": {{\n    {}: {}\n  }}\n}}\n",
        quoted(SERVER_NAME),
        render_json_entry(client, params, "    ", "  ")
    )
}

/// SHOGUN's entry value (`{ "command": …, "args": [], "env": { … } }`) as JSON text: the first line
/// is the `{` alone, inner lines start with `indent` plus `unit` per level, and the closing `}`
/// with `indent`. The whole-file render and the one-click merge
/// ([`crate::client_install::plan_add`]) both use it.
pub fn render_json_entry(
    client: McpClient,
    params: &ServerParams<'_>,
    indent: &str,
    unit: &str,
) -> String {
    // Written by hand, not through serde_json's map: key order must be stable whatever serde_json
    // features the workspace unifies on, because the docs are compared byte for byte.
    let inner = format!("{indent}{unit}");
    let mut out = String::from("{\n");
    let _ = writeln!(out, "{inner}\"command\": {},", quoted(params.command));
    let _ = writeln!(out, "{inner}\"args\": {},", args_list(&params.args(client)));
    let _ = writeln!(out, "{inner}\"env\": {{");
    let env = params.env(client);
    for (i, (key, value)) in env.iter().enumerate() {
        let comma = if i + 1 < env.len() { "," } else { "" };
        let _ = writeln!(out, "{inner}{unit}{}: {}{comma}", quoted(key), quoted(value));
    }
    let _ = write!(out, "{inner}}}\n{indent}}}");
    out
}

fn render_toml(client: McpClient, params: &ServerParams<'_>) -> String {
    render_toml_entry(client, params)
}

/// SHOGUN's entry as Codex `config.toml` tables: the server table, then its `.env` sub-table,
/// ending in a newline. It is also the whole generated file.
pub fn render_toml_entry(client: McpClient, params: &ServerParams<'_>) -> String {
    // The layout of Codex's documented example: a server table, then its `.env` sub-table.
    let mut out = String::new();
    let _ = writeln!(out, "[mcp_servers.{SERVER_NAME}]");
    let _ = writeln!(out, "command = {}", quoted(params.command));
    let _ = writeln!(out, "args = {}", args_list(&params.args(client)));
    if params.forwards_token(client) {
        // Codex's documented forwarding list: the variable is read from Codex's own environment.
        let _ = writeln!(out, "env_vars = [{}]", quoted(API_TOKEN_ENV));
    }
    out.push('\n');
    let _ = writeln!(out, "[mcp_servers.{SERVER_NAME}.env]");
    for (key, value) in params.env(client) {
        let _ = writeln!(out, "{key} = {}", quoted(value));
    }
    out
}

/// The vendor's own CLI command that adds the entry, where the vendor documents one taking the env
/// block as flags. Only Claude Code: `codex mcp add` has no flag for Codex's `env_vars` list, so
/// its entry is pasted; Cursor, Claude Desktop and the ChatGPT desktop app document no add
/// command. SHOGUN never runs it; the user does.
pub fn cli_command(client: McpClient, params: &ServerParams<'_>) -> Option<String> {
    if client != McpClient::ClaudeCode {
        return None;
    }
    // The same argument list the one-click setup runs (`client_install::claude_code_add_runs`).
    let words = crate::client_install::claude_add_args(params);
    Some(
        std::iter::once("claude".to_string())
            .chain(words.iter().map(|w| crate::client_install::shell_word(w)))
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// The Terminal command that puts the copied token where `client` reads it, or `None` when the
/// user pastes it into the entry. It reads the clipboard (`pbpaste`) so the token is never typed
/// into the shell history.
pub fn token_setup_command(client: McpClient) -> Option<String> {
    match client.token_source() {
        TokenSource::Shell => Some(format!("export {API_TOKEN_ENV}=\"$(pbpaste)\"")),
        TokenSource::LoginSession => Some(format!("launchctl setenv {API_TOKEN_ENV} \"$(pbpaste)\"")),
        TokenSource::Paste | TokenSource::Connection => None,
    }
}

/// One client's generated setup, as Settings and `api.tokens.list` show it. Never a token.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ClientSetup {
    pub id: &'static str,
    pub label: &'static str,
    pub format: ConfigFormat,
    /// Where the entry goes.
    pub config_location: &'static str,
    /// The entry, as a whole config file ([`render_client_config`]).
    pub config: String,
    /// The vendor's own add command ([`cli_command`]), when there is one.
    pub cli_command: Option<String>,
    pub token_source: TokenSource,
    /// The command that makes the copied token visible to the client ([`token_setup_command`]).
    pub token_setup: Option<String>,
    /// The text the user replaces with the token ([`TokenSource::Paste`] only).
    pub token_placeholder: Option<&'static str>,
    /// The connection the entry names (`--client`, issue #963): apps sharing a config file share
    /// it. Settings shows whether it exists and makes it with "Add to …".
    pub connection: &'static str,
    /// The executable path the user replaces, while the entry carries the docs placeholder
    /// (the app does not ship the server yet).
    pub command_placeholder: Option<&'static str>,
    /// One-click setup (issue #400): how SHOGUN adds this entry for the user, and what it adds.
    /// Applying is in-app only, behind a confirmation dialog.
    pub install: crate::client_install::InstallPreview,
}

/// The generated setup for every client, in [`ALL_CLIENTS`] order: the entry names the app's
/// connection ([`TokenValue::Connection`], issue #963), holds no token, and lists every tool. `command` is the `shogun-mcp`
/// executable; while the app does not ship it, callers pass [`ServerParams::DOCS`]'s placeholder.
pub fn mount_setup(command: &str, db_path: &str) -> Vec<ClientSetup> {
    let params = ServerParams { command, db_path, api_token: TokenValue::Connection, toolsets: None };
    ALL_CLIENTS
        .iter()
        .map(|&client| ClientSetup {
            id: client.id(),
            label: client.label(),
            format: client.format(),
            config_location: client.config_location(),
            config: render_client_config(client, &params),
            cli_command: cli_command(client, &params),
            // Kept in the shape for API callers that read them; a connection needs no token step.
            token_source: TokenSource::Connection,
            token_setup: None,
            token_placeholder: None,
            connection: crate::memory_api_settings::grant_id(client.id()).unwrap_or(client.id()),
            command_placeholder: (command == ServerParams::DOCS.command)
                .then_some(ServerParams::DOCS.command),
            install: crate::client_install::install_preview(client, command, db_path),
        })
        .collect()
}

/// `[]`, or `["a", "b"]`: a list both JSON and TOML read the same way.
fn args_list(args: &[String]) -> String {
    format!("[{}]", args.iter().map(|arg| quoted(arg)).collect::<Vec<_>>().join(", "))
}

/// A double-quoted string valid in both JSON and a TOML basic string: `"` and `\` are escaped, and
/// every control character (including DEL, which JSON allows raw but TOML does not) becomes
/// `\uXXXX`, an escape both formats share.
fn quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() && (c as u32) < 0x80 => {
                let _ = write!(out, "\\u{:04X}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn repo_file(relative: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(relative)
    }

    fn read(relative: &str) -> String {
        std::fs::read_to_string(repo_file(relative))
            .unwrap_or_else(|e| panic!("read {relative}: {e}"))
    }

    /// Bodies of the fenced code blocks whose info string starts with `lang` (a Mintlify title
    /// may follow it, e.g. "```json Cursor").
    fn fenced_blocks(doc: &str, lang: &str) -> Vec<String> {
        let mut blocks = Vec::new();
        let mut current: Option<String> = None;
        for line in doc.split_inclusive('\n') {
            let trimmed = line.trim_end();
            match current.as_mut() {
                None => {
                    if let Some(info) = trimmed.strip_prefix("```") {
                        if info == lang || info.starts_with(&format!("{lang} ")) {
                            current = Some(String::new());
                        }
                    }
                }
                Some(body) => {
                    if trimmed == "```" {
                        blocks.push(std::mem::take(body));
                        current = None;
                    } else {
                        body.push_str(line);
                    }
                }
            }
        }
        blocks
    }

    fn with_toolsets_all() -> ServerParams<'static> {
        ServerParams { toolsets: Some("all"), ..ServerParams::DOCS }
    }

    #[test]
    fn json_entry_parses_names_its_app_and_carries_no_token() {
        let text = render_client_config(McpClient::Cursor, &with_toolsets_all());
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        let server = &v["mcpServers"][SERVER_NAME];
        assert_eq!(server["command"], ServerParams::DOCS.command);
        assert_eq!(server["args"], serde_json::json!(["--client", "cursor"]));
        let env = server["env"].as_object().unwrap();
        assert_eq!(env.len(), 2);
        assert_eq!(env[DB_PATH_ENV], ServerParams::DOCS.db_path);
        assert_eq!(env[crate::toolsets::ENV_VAR], "all");
        assert!(env.get(API_TOKEN_ENV).is_none(), "{text}");
        // Without a toolsets value the key is absent, not empty.
        let bare: serde_json::Value =
            serde_json::from_str(&render_client_config(McpClient::Cursor, &ServerParams::DOCS)).unwrap();
        assert!(bare["mcpServers"][SERVER_NAME]["env"].get(crate::toolsets::ENV_VAR).is_none());
    }

    #[test]
    fn json_clients_differ_only_by_their_app_and_codex_clients_share_one_table() {
        let p = ServerParams::DOCS;
        let cursor = render_client_config(McpClient::Cursor, &p);
        for (client, id) in [(McpClient::ClaudeCode, "claude-code"), (McpClient::ClaudeDesktop, "claude-desktop")] {
            assert_eq!(render_client_config(client, &p), cursor.replace("\"cursor\"", &format!("\"{id}\"")));
        }
        // One file, one entry, one connection: the ChatGPT desktop app's is Codex CLI's.
        assert_eq!(
            render_client_config(McpClient::ChatGptDesktop, &p),
            render_client_config(McpClient::CodexCli, &p)
        );
        assert!(render_client_config(McpClient::ChatGptDesktop, &p).contains("\"--client\", \"codex-cli\""));
    }

    #[test]
    fn toml_entry_has_codex_layout_and_its_values_round_trip() {
        let text = render_client_config(McpClient::CodexCli, &with_toolsets_all());
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "[mcp_servers.shogun-memory]");
        assert_eq!(lines[2], "args = [\"--client\", \"codex-cli\"]");
        assert_eq!(lines[3], "");
        assert_eq!(lines[4], "[mcp_servers.shogun-memory.env]");
        let value = |key: &str| -> String {
            let line = lines.iter().find(|l| l.starts_with(&format!("{key} = "))).unwrap();
            // A TOML basic string written by `quoted` is also a JSON string.
            serde_json::from_str(line.split_once(" = ").unwrap().1).unwrap()
        };
        assert_eq!(value("command"), ServerParams::DOCS.command);
        assert_eq!(value(DB_PATH_ENV), ServerParams::DOCS.db_path);
        assert_eq!(value(crate::toolsets::ENV_VAR), "all");
        assert!(!text.contains(API_TOKEN_ENV) && !text.contains("env_vars"), "{text}");
        let parsed: toml::Table = toml::from_str(&text).unwrap();
        assert_eq!(
            parsed["mcp_servers"][SERVER_NAME]["args"],
            toml::Value::Array(vec!["--client".into(), "codex-cli".into()])
        );
    }

    #[test]
    fn the_older_token_form_still_renders() {
        // A server started with a token still works, so the renderer keeps that form: Codex
        // forwards the variable by name, Cursor references it, no `--client`.
        let older = ServerParams { api_token: TokenValue::FromEnvironment, ..ServerParams::DOCS };
        let codex = render_client_config(McpClient::CodexCli, &older);
        assert!(codex.contains("\nargs = []\nenv_vars = [\"SHOGUN_API_TOKEN\"]\n"), "{codex}");
        let cursor: serde_json::Value =
            serde_json::from_str(&render_client_config(McpClient::Cursor, &older)).unwrap();
        assert_eq!(cursor["mcpServers"][SERVER_NAME]["env"][API_TOKEN_ENV], "${env:SHOGUN_API_TOKEN}");
        assert_eq!(cursor["mcpServers"][SERVER_NAME]["args"], serde_json::json!([]));
    }

    #[test]
    fn the_client_argument_is_read_strictly() {
        let args = |words: &[&str]| client_arg(words.iter().map(|w| w.to_string()));
        assert_eq!(args(&[]), Ok(None));
        assert_eq!(args(&["--other", "x"]), Ok(None));
        assert_eq!(args(&["--client", "cursor"]), Ok(Some("cursor".into())));
        assert_eq!(args(&["--client=codex-cli"]), Ok(Some("codex-cli".into())));
        // An unclear entry never falls back to the token path.
        for bad in [
            &["--client"][..],
            &["--client", ""],
            &["--client", "Cursor"],
            &["--client", "../x"],
            &["--client=a b"],
            &["--client", "cursor", "--client", "cursor"],
        ] {
            assert!(args(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn awkward_paths_are_escaped_for_both_formats() {
        let nasty = "/Users/O\"Brien/Back\\slash/tab\there/new\nline/del\u{7f}/日本語 dir/x";
        let p = ServerParams {
            command: nasty,
            db_path: nasty,
            api_token: TokenValue::Literal("t"),
            toolsets: None,
        };
        let json: serde_json::Value =
            serde_json::from_str(&render_client_config(McpClient::ClaudeCode, &p)).unwrap();
        assert_eq!(json["mcpServers"][SERVER_NAME]["command"], nasty);
        assert_eq!(json["mcpServers"][SERVER_NAME]["env"][DB_PATH_ENV], nasty);

        let toml = render_client_config(McpClient::CodexCli, &p);
        // Every value stays on one line with no raw control character (TOML forbids them).
        assert!(!toml.chars().any(|c| c.is_control() && c != '\n'));
        let command = toml.lines().find(|l| l.starts_with("command = ")).unwrap();
        let parsed: String = serde_json::from_str(command.trim_start_matches("command = ")).unwrap();
        assert_eq!(parsed, nasty);
    }

    #[test]
    fn client_ids_are_unique() {
        for (i, a) in ALL_CLIENTS.iter().enumerate() {
            for b in &ALL_CLIENTS[i + 1..] {
                assert_ne!(a.id(), b.id());
            }
        }
    }

    #[test]
    fn mount_setup_names_each_apps_connection_and_holds_no_token() {
        let db = "/Users/O'Neil/Library/Application Support/com.syogun.shogunai/memory.db";
        let setup = mount_setup(ServerParams::DOCS.command, db);
        assert_eq!(setup.len(), ALL_CLIENTS.len());
        let by_id = |id: &str| setup.iter().find(|s| s.id == id).unwrap();

        // Claude Code: the CLI command passes `--client claude-code` after the server path.
        let claude_code = by_id("claude-code");
        let v: serde_json::Value = serde_json::from_str(&claude_code.config).unwrap();
        assert_eq!(v["mcpServers"][SERVER_NAME]["args"], serde_json::json!(["--client", "claude-code"]));
        assert_eq!(v["mcpServers"][SERVER_NAME]["env"][DB_PATH_ENV], db);
        let cli = claude_code.cli_command.as_deref().unwrap();
        assert!(cli.starts_with("claude mcp add --env "), "{cli}");
        // The apostrophe in the path survives single quoting.
        assert!(cli.contains("'SHOGUN_DB_PATH=/Users/O'\\''Neil/Library/Application Support/"), "{cli}");
        assert!(
            cli.ends_with(" --transport stdio --scope user shogun-memory -- /ABS/PATH/TO/target/release/shogun-mcp --client claude-code"),
            "{cli}"
        );

        // Every app: its connection, no token step, nothing to paste.
        for client in ALL_CLIENTS {
            let s = by_id(client.id());
            assert_eq!(s.token_source, TokenSource::Connection);
            assert!(s.token_setup.is_none() && s.token_placeholder.is_none());
            assert!(!s.config.contains(API_TOKEN_ENV), "{}", s.config);
        }
        assert_eq!(by_id("cursor").connection, "cursor");
        assert_eq!(by_id("claude-desktop").connection, "claude-desktop");
        // Codex CLI and the ChatGPT desktop app read one file, so they share a connection.
        assert_eq!(by_id("codex-cli").connection, "codex-cli");
        assert_eq!(by_id("chatgpt-desktop").connection, "codex-cli");
        assert!(by_id("cursor").cli_command.is_none());

        assert!(setup.iter().all(|s| s.command_placeholder == Some(ServerParams::DOCS.command)));
        assert!(mount_setup("/Applications/X.app/Contents/MacOS/shogun-mcp", db)
            .iter()
            .all(|s| s.command_placeholder.is_none()));
        // Nothing in any setup looks like a token, not even the placeholder.
        let text = serde_json::to_string(&setup).unwrap();
        assert!(!text.contains("shogun_"), "{text}");
    }

    #[test]
    fn shell_quoting_round_trips_through_sh() {
        for word in ["plain", "with space", "it's", "a'b'c", "$(pbpaste) `x` \"q\" \\"] {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("printf %s {}", crate::client_install::shell_word(word)))
                .output();
            // `sh` exists on every CI and dev machine; skip quietly if a sandbox hides it.
            if let Ok(out) = out {
                assert_eq!(String::from_utf8(out.stdout).unwrap(), word);
            }
        }
    }

    /// `docs/examples/` holds only generated files, one per client config, byte-identical to the
    /// renderer with the toolsets knob shown.
    #[test]
    fn docs_examples_are_generated_from_code() {
        for client in ALL_CLIENTS {
            let path = format!("docs/examples/{}", client.example_file());
            let expected = render_client_config(*client, &with_toolsets_all());
            assert_eq!(
                read(&path),
                expected,
                "{path} is generated from shogun_mcp::client_config; replace it with:\n{expected}"
            );
        }
        let generated: Vec<&str> = ALL_CLIENTS.iter().map(|c| c.example_file()).collect();
        for entry in std::fs::read_dir(repo_file("docs/examples")).unwrap() {
            let name = entry.unwrap().file_name().into_string().unwrap();
            if name.contains(".shogun.") {
                assert!(generated.contains(&name.as_str()), "docs/examples/{name} is not generated");
            }
        }
    }

    /// Every docs page that shows the entry shows the generated text, for every format.
    #[test]
    fn docs_snippets_match_the_generated_entry() {
        for doc_path in ["docs/memory-api-mcp.md", "apps/docs/memory-api/mcp.mdx"] {
            let doc = read(doc_path);
            for client in ALL_CLIENTS {
                let expected = render_client_config(*client, &ServerParams::DOCS);
                let fence = client.format().fence();
                assert!(
                    fenced_blocks(&doc, fence).contains(&expected),
                    "{doc_path} must show the generated {} entry in a ```{fence} block:\n{expected}",
                    client.label()
                );
            }
            // No stale hand-written entry lingers beside the generated ones.
            for fence in ["json", "toml"] {
                for block in fenced_blocks(&doc, fence) {
                    if block.contains("mcpServers") || block.contains("[mcp_servers.") {
                        assert!(
                            ALL_CLIENTS
                                .iter()
                                .any(|c| render_client_config(*c, &ServerParams::DOCS) == block),
                            "{doc_path} has a hand-written MCP entry:\n{block}"
                        );
                    }
                }
            }
        }
    }
}
