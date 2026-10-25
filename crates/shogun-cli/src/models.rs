//! `shogun models …` (E24): the local embedding model, managed by the operator.
//!
//! Runs locally, like `principal import`: no server, no token. `list`, `path` and `verify` read
//! the model table and the files on disk; `fetch` is the **only** command anywhere that downloads
//! a model. It needs a CLI built with `--features embed-local` (the traced egress lives in
//! `shogun-core`); without it, it says so and does nothing.

use std::process::ExitCode;

use shogun_core::embed_models::{installed, model_dir, models_dir, verify};
use shogun_memory::embed::{model_spec, ModelSpec, DEFAULT_MODEL, MODELS};

/// One parsed `models` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelsCommand {
    List,
    Path,
    Fetch { model: Option<String> },
    Verify { model: Option<String> },
}

pub const USAGE: &str = "\
shogun models list               Known local embedding models (default marked *), and whether each
                                 is installed under the models dir
shogun models path               The models dir (SHOGUN_MODELS_DIR, else the user data dir)
shogun models fetch [--model M]  Download the pinned model files (default: the default model),
                                 checking each SHA-256. The only command that downloads a model;
                                 needs a CLI built with --features embed-local. Refuses a model
                                 listed as unpinned (docs/embedding-model-setup.md)
shogun models verify [--model M] Check installed files against their pinned SHA-256";

/// Parse the words after `models`.
pub fn parse(args: &[String]) -> Result<ModelsCommand, String> {
    let (verb, rest) = args.split_first().ok_or("models needs list, path, fetch or verify")?;
    let model = match rest {
        [] => None,
        [flag, value] if flag == "--model" => Some(value.clone()),
        _ => return Err(format!("unexpected arguments after `models {verb}`")),
    };
    match (verb.as_str(), model) {
        ("list", None) => Ok(ModelsCommand::List),
        ("path", None) => Ok(ModelsCommand::Path),
        ("fetch", model) => Ok(ModelsCommand::Fetch { model }),
        ("verify", model) => Ok(ModelsCommand::Verify { model }),
        (other, _) => Err(format!("unknown models command {other:?}")),
    }
}

fn spec(model: Option<&str>) -> Result<&'static ModelSpec, String> {
    match model {
        None => Ok(DEFAULT_MODEL),
        Some(name) => model_spec(name).ok_or_else(|| format!("no model named {name:?}; see `shogun models list`")),
    }
}

/// The `models list` state column. An unpinned model (placeholder pins, E43) cannot be fetched or
/// verified; it is listed as such, installed by hand or not.
fn state_label(no_files: bool, pinned: bool, installed: bool) -> &'static str {
    match (no_files, pinned, installed) {
        (true, _, _) => "manual",
        (false, false, true) => "hand-placed",
        (false, false, false) => "unpinned",
        (false, true, true) => "installed",
        (false, true, false) => "not installed",
    }
}

/// Run a `models` command. Exit codes follow the CLI's: 0 ok, 1 failure, 2 a usage error.
pub fn run(args: &[String]) -> ExitCode {
    let command = match parse(args) {
        Ok(command) => command,
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let root = models_dir();
    match command {
        ModelsCommand::List => {
            for model in MODELS {
                let default = if std::ptr::eq(*model, DEFAULT_MODEL) { "*" } else { " " };
                let state = state_label(model.files.is_empty(), model.is_pinned(), installed(&root, model));
                // log-hygiene-ok: the built-in model table, no user data
                println!("{default} {:<24} {:<11} {:<13} {}", model.name, model.license, state, model.space_id);
            }
            ExitCode::SUCCESS
        }
        ModelsCommand::Path => {
            println!("{}", root.display());
            ExitCode::SUCCESS
        }
        ModelsCommand::Verify { model } => match spec(model.as_deref()).and_then(|spec| verify(&root, spec).map(|()| spec)) {
            Ok(spec) => {
                println!("{} ok ({})", spec.name, model_dir(&root, spec).display());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::from(1)
            }
        },
        ModelsCommand::Fetch { model } => match spec(model.as_deref()) {
            Ok(spec) => fetch(&root, spec),
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::from(2)
            }
        },
    }
}

#[cfg(feature = "embed-local")]
fn fetch(root: &std::path::Path, spec: &'static ModelSpec) -> ExitCode {
    match shogun_core::embed_models::fetch(root, spec, &shogun_core::embed_models::OperatorTrace) {
        Ok(report) => {
            println!(
                "{} ready in {} (downloaded {}, already present {})",
                spec.name,
                report.dir.display(),
                report.downloaded.len(),
                report.kept.len()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

#[cfg(not(feature = "embed-local"))]
fn fetch(_root: &std::path::Path, spec: &'static ModelSpec) -> ExitCode {
    eprintln!(
        "error: this `shogun` was built without `embed-local`, so it cannot download {}; \
         rebuild with `cargo build -p shogun-cli --features embed-local` (the server image has it)",
        spec.name
    );
    ExitCode::from(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_string()).collect()
    }

    #[test]
    fn parses_every_verb() {
        assert_eq!(parse(&args(&["list"])), Ok(ModelsCommand::List));
        assert_eq!(parse(&args(&["path"])), Ok(ModelsCommand::Path));
        assert_eq!(parse(&args(&["fetch"])), Ok(ModelsCommand::Fetch { model: None }));
        assert_eq!(
            parse(&args(&["fetch", "--model", "nomic-embed-text-v1.5"])),
            Ok(ModelsCommand::Fetch { model: Some("nomic-embed-text-v1.5".into()) })
        );
        assert_eq!(parse(&args(&["verify"])), Ok(ModelsCommand::Verify { model: None }));
        assert!(parse(&args(&[])).is_err());
        assert!(parse(&args(&["list", "--model", "x"])).is_err());
        assert!(parse(&args(&["fetch", "--bogus"])).is_err());
        assert!(parse(&args(&["download"])).is_err());
    }

    #[test]
    fn the_default_spec_is_the_default_model() {
        assert_eq!(spec(None).unwrap().name, DEFAULT_MODEL.name);
        assert!(spec(Some("nope")).is_err());
    }

    #[test]
    fn unpinned_models_are_labelled_and_the_multilingual_model_is_one() {
        assert_eq!(state_label(false, false, false), "unpinned");
        assert_eq!(state_label(false, false, true), "hand-placed");
        assert_eq!(state_label(false, true, true), "installed");
        assert_eq!(state_label(false, true, false), "not installed");
        assert_eq!(state_label(true, false, false), "manual");
        let e5 = spec(Some("multilingual-e5-small")).unwrap();
        assert!(!e5.is_pinned());
        assert!(spec(None).unwrap().is_pinned());
    }

    #[test]
    fn the_main_usage_lists_the_models_commands() {
        assert!(crate::command::USAGE.contains("models fetch"));
    }
}
