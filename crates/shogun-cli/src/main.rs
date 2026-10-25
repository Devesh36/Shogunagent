//! The `shogun` binary — parses argv and calls the local Memory API server over loopback.
//!
//! Config via env: `SHOGUN_API_PORT` (port resolution is in `shogun_cli::port`), `SHOGUN_API_TOKEN`
//! (used when `--token` is not given). The token is sent as a `Bearer` header, except to the
//! open status and metrics routes; without one, tool calls get 401 (as the daemon requires,
//! FR-API-03). Without `SHOGUN_API_PORT` or a port the running server recorded, nothing is sent
//! (no guessed port). Exit code mirrors the outcome: 0 success, 1 an HTTP error status, 2 a parse
//! error, 3 the daemon is not running or unreachable.

use std::process::ExitCode;

use shogun_cli::command::{self, Command};
use shogun_cli::parse::parse;
use shogun_cli::{hint, http, port, wire};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // `models …` (E24) runs locally, like `principal import`: no server, no token.
    if args.first().map(String::as_str) == Some("models") {
        return shogun_cli::models::run(&args[1..]);
    }

    let invocation = match parse(&args) {
        Ok(inv) => inv,
        Err(err) => {
            eprintln!("error: {}\n\n{}", err.message(), command::USAGE);
            return ExitCode::from(2);
        }
    };

    if invocation.command == Command::Help {
        println!("{}", command::USAGE);
        return ExitCode::SUCCESS;
    }

    if let command::Command::Config { action } = &invocation.command {
        use shogun_core::user_config::{default_path, load_report};
        let Some(path) = default_path() else {
            eprintln!("error: could not resolve home dir");
            return ExitCode::from(1);
        };
        match action {
            command::ConfigAction::Path => {
                println!("{}", path.display());
            }
            command::ConfigAction::Show => match load_report(&path) {
                Ok((cfg, _)) => match serde_json::to_string_pretty(&cfg) {
                    Ok(json) => println!("{json}"),
                    Err(e) => {
                        eprintln!("error: {e}");
                        return ExitCode::from(1);
                    }
                },
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::from(1);
                }
            },
            command::ConfigAction::Validate => match load_report(&path) {
                Ok((_, report)) => {
                    if report.ok {
                        println!("ok");
                    } else {
                        for e in &report.section_errors {
                            // log-hygiene-ok: the CLI's output to the user who ran `config validate`
                            println!("{}:{} {}", e.section, e.line, e.message);
                        }
                        return ExitCode::from(1);
                    }
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::from(1);
                }
            },
        }
        return ExitCode::SUCCESS;
    }

    // `principal import` (ShogunAgent E2-T6) runs locally: it copies a single-user store into the
    // principal layout and applies the pending migrations. No server, no token.
    if let command::Command::Principal(command::PrincipalCommand::Import { from, root, principal }) =
        &invocation.command
    {
        use shogun_core::tenancy::{import_single_user_store, EnvKeyProvider, PrincipalId};
        let principal = match PrincipalId::parse(principal) {
            Ok(id) => id,
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::from(2);
            }
        };
        let keys = match EnvKeyProvider::from_env() {
            Ok(keys) => keys,
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::from(1);
            }
        };
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        return match import_single_user_store(
            std::path::Path::new(from),
            std::path::Path::new(root),
            &principal,
            &keys,
            now_ms,
        ) {
            Ok(report) => {
                // log-hygiene-ok: paths and schema versions the user asked for, never a row
                println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::from(1)
            }
        };
    }

    // `audit verify <file>` (E26) recomputes an export's hash chain locally: no server, no token.
    if let command::Command::AuditVerify { file } = &invocation.command {
        let text = match std::fs::read_to_string(file) {
            Ok(text) => text,
            Err(e) => {
                eprintln!("error: cannot read the export: {e}");
                return ExitCode::from(1);
            }
        };
        return match shogun_mcp::audit_export::verify(&text) {
            Ok(report) => {
                // log-hygiene-ok: counts and a hash of the user's own export, printed to them
                println!(
                    "{}",
                    serde_json::json!({ "verified": true, "records": report.records, "head_hash": report.head_hash, "id_gaps": report.id_gaps })
                );
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("verification failed: {e}");
                ExitCode::from(1)
            }
        };
    }

    // `voice-dictionary import <file|->` reads its rows here, before the call is built.
    let command = match wire::resolve_import_source(invocation.command) {
        Ok(command) => command,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::from(1);
        }
    };
    let Some(call) = wire::to_call(&command, invocation.include_low) else {
        println!("{}", command::USAGE);
        return ExitCode::SUCCESS;
    };

    let db_path = std::env::var_os("SHOGUN_DB_PATH").filter(|p| !p.is_empty());
    // No recorded port: the server is not running. Never guess one (audit 2026-10-20): whatever
    // holds the default port would receive the bearer token.
    let Some(port) = port::resolve(
        std::env::var("SHOGUN_API_PORT").ok().as_deref(),
        db_path.as_deref().map(std::path::Path::new),
        shogun_mcp::plan_source::app_data_dir().as_deref(),
    ) else {
        eprintln!("{}", port::NOT_RUNNING);
        eprintln!("{}", start_hint());
        return ExitCode::from(3);
    };
    // The operator's routes (deciding an approval, changing a grant, teams) take the admin
    // credential, never the agent token (E15 / E18).
    let token_var = if port::needs_admin(&call.path) { "SHOGUN_ADMIN_TOKEN" } else { "SHOGUN_API_TOKEN" };
    let token = invocation
        .token
        .or_else(|| std::env::var(token_var).ok().filter(|t| !t.is_empty()))
        .filter(|_| port::needs_token(&call.path));

    match http::request(port, call.method, &call.path, token.as_deref(), call.body.as_deref()) {
        // Status 0 = the daemon closed the socket without a parseable response (crash mid-request,
        // reset). That is the unreachable-daemon class (exit 3), never success — a bare `status < 400`
        // check would report it as exit 0 with an empty body.
        Ok(resp) if resp.status == 0 => {
            eprintln!("empty or malformed response from the daemon on 127.0.0.1:{port}");
            ExitCode::from(3)
        }
        // E26: the export goes to --out (or stdout) as received, and is checked before the CLI
        // reports success: a stream cut short has no trailer.
        Ok(resp) if matches!(command, Command::AuditExport { .. }) && (200..300).contains(&resp.status) => {
            let Command::AuditExport { out, .. } = &command else {
                return ExitCode::FAILURE;
            };
            let verified = shogun_mcp::audit_export::verify(&resp.body);
            match out {
                Some(path) => {
                    if let Err(e) = std::fs::write(path, &resp.body) {
                        eprintln!("error: cannot write the export: {e}");
                        return ExitCode::from(1);
                    }
                }
                // log-hygiene-ok: the export the operator asked for, printed by the CLI they ran
                None => print!("{}", resp.body),
            }
            match verified {
                Ok(report) => {
                    eprintln!("audit export: {} records, head hash {}", report.records, report.head_hash);
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("audit export is incomplete: {e}");
                    ExitCode::from(1)
                }
            }
        }
        Ok(resp) => {
            // `state export` prints the Markdown document itself unless `--json` asked for the
            // whole response (issue #290).
            let markdown = (command == Command::State(command::StateCommand::Export)
                && !invocation.json
                && (200..300).contains(&resp.status))
                .then(|| serde_json::from_str::<serde_json::Value>(&resp.body).ok())
                .flatten()
                .and_then(|v| v["result"]["markdown"].as_str().map(str::to_string));
            match markdown {
                Some(markdown) => print!("{markdown}"),
                // log-hygiene-ok: the response the user asked for, printed by the CLI they ran
                None => println!("{}", resp.body),
            }
            if (200..400).contains(&resp.status) {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(e) => {
            eprintln!("could not reach the daemon on 127.0.0.1:{port}: {e}");
            eprintln!("{}", start_hint());
            ExitCode::from(3)
        }
    }
}

/// How to start the REST server, for when none answers or none recorded its port.
fn start_hint() -> String {
    // The app ships `shogun-api` beside this binary (issue #762). `shogun` is usually run
    // through a link on PATH, so resolve that first.
    let bundled = std::env::current_exe()
        .and_then(|exe| exe.canonicalize())
        .ok()
        .map(|exe| hint::api_beside(&exe))
        .filter(|api| api.is_file());
    let db = shogun_mcp::plan_source::app_data_dir().map(|dir| dir.join("memory.db"));
    hint::start_api(bundled.as_deref(), db.as_deref())
}
