//! SHOGUN CLI (`shogun`) — the terminal/script face of the Memory API (§6.11, FR-API-01). One of
//! three symmetric faces (MCP / CLI / REST) over the same shared dispatcher, so the CLI grants no
//! capability the UI doesn't (invariant 6).
//!
//! The grammar ([`parse`]), the command → tool/level model ([`command`]), and the call resolution
//! ([`plan`]) are pure and Linux-testable; `main.rs` is a thin runner over them. Executing a
//! resolved call round-trips to the running daemon's REST endpoint — wired when the REST face lands.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod command;
pub mod hint;
pub mod models;
pub mod http;
pub mod parse;
pub mod plan;
pub mod port;
pub mod wire;

#[cfg(test)]
mod forms_tests {
    use crate::{
        command::{Command, FormOperation, USAGE},
        parse, plan, wire,
    };
    use serde_json::{json, Value};
    use shogun_agents::permission::Level;
    use shogun_mcp::{
        forms_api,
        memory_api::{plan_allows, scope_allows, tool_level, ApiLevel, TokenScope, Tool},
    };

    fn parse_args(args: &[&str]) -> parse::Invocation {
        parse::parse(
            &args
                .iter()
                .map(|value| (*value).to_owned())
                .collect::<Vec<_>>(),
        )
        .unwrap()
    }

    #[test]
    fn forms_parser_keeps_global_flags_and_complete_instruction() {
        let invocation = parse_args(&[
            "--token",
            "test-token",
            "forms",
            "plan",
            "7",
            "0",
            "Fill",
            "for",
            "Mira",
            "--json",
        ]);
        assert_eq!(
            invocation.command,
            Command::Forms(FormOperation::Plan {
                session_id: 7,
                revision: 0,
                instruction: "Fill for Mira".into()
            })
        );
        assert!(invocation.json);
        assert_eq!(invocation.token.as_deref(), Some("test-token"));
        let max_id = u64::MAX.to_string();
        assert_eq!(
            parse_args(&["forms", "status", &max_id]).command,
            Command::Forms(FormOperation::Status {
                session_id: u64::MAX
            })
        );
    }

    #[test]
    fn forms_parser_rejects_ids_revisions_missing_instructions_and_confirmation_flags() {
        for args in [
            vec!["forms", "status", "0"],
            vec!["forms", "status", "-1"],
            vec!["forms", "apply", "7", "-1"],
            vec!["forms", "plan", "7", "0"],
            vec!["forms", "inspect", "--approve"],
            vec!["forms", "apply", "7", "0", "--answers", "{}"],
            vec!["forms", "apply", "7", "0", "--confirm"],
            vec!["forms", "close", "7", "--approved"],
            vec!["forms", "apply", "7", "0", "true"],
            vec!["forms", "approve", "7"],
        ] {
            assert!(
                parse::parse(&args.into_iter().map(str::to_owned).collect::<Vec<_>>()).is_err()
            );
        }
        let long = "日".repeat(4001);
        assert!(parse::parse(
            &["forms", "plan", "7", "0", &long]
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        )
        .is_err());
    }

    #[test]
    fn forms_wire_uses_strict_shared_operations_and_permission_levels() {
        for (args, tool, path, method, level) in [
            (
                vec!["forms", "inspect"],
                Tool::FormsInspect,
                "/v1/forms/inspect",
                "POST",
                ApiLevel::Write(Level::L2),
            ),
            (
                vec!["forms", "plan", "7", "0", "Fill for Mira"],
                Tool::FormsPlan,
                "/v1/forms/plan",
                "POST",
                ApiLevel::Write(Level::L1),
            ),
            (
                vec!["forms", "status", "7"],
                Tool::FormsStatus,
                "/v1/forms/status/7",
                "GET",
                ApiLevel::Read,
            ),
            (
                vec!["forms", "apply", "7", "0"],
                Tool::FormsApply,
                "/v1/forms/apply",
                "POST",
                ApiLevel::Write(Level::L3),
            ),
            (
                vec!["forms", "undo", "7"],
                Tool::FormsUndo,
                "/v1/forms/undo",
                "POST",
                ApiLevel::Write(Level::L3),
            ),
            (
                vec!["forms", "close", "7"],
                Tool::FormsClose,
                "/v1/forms/close",
                "POST",
                ApiLevel::Write(Level::L1),
            ),
        ] {
            let invocation = parse_args(&args);
            let Command::Forms(expected) = &invocation.command else {
                panic!("not forms")
            };
            let call = wire::to_call(&invocation.command, true).unwrap();
            assert_eq!((call.path.as_str(), call.method), (path, method));
            assert_eq!(invocation.command.tool(), Some(tool));
            assert_eq!(tool_level(tool), level);
            assert_eq!(
                scope_allows(TokenScope::Read, tool),
                tool == Tool::FormsStatus
            );
            let operation = forms_api::operation(
                tool,
                call.body.as_deref().unwrap_or("{}"),
                (tool == Tool::FormsStatus).then_some(7),
            )
            .unwrap();
            assert_eq!(&operation, expected);
            assert!(plan::describe(&invocation).starts_with(tool.wire_name()));
        }
        let apply =
            wire::to_call(&parse_args(&["forms", "apply", "7", "0"]).command, false).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(apply.body.as_deref().unwrap()).unwrap(),
            json!({"session_id":7,"revision":0})
        );
    }

    #[test]
    fn forms_plan_and_help_keep_review_authorization_boundary() {
        let mut ent =
            shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0);
        ent.agent_execution = false;
        assert!(!plan_allows(Tool::FormsPlan, &ent));
        assert!(plan_allows(Tool::FormsStatus, &ent));
        ent.memory_api = false;
        for tool in [
            Tool::FormsInspect,
            Tool::FormsPlan,
            Tool::FormsStatus,
            Tool::FormsApply,
            Tool::FormsClose,
            Tool::FormsUndo,
        ] {
            assert!(!plan_allows(tool, &ent));
        }
        for text in [
            "forms inspect",
            "forms plan",
            "forms status",
            "forms apply",
            "forms close",
            "forms undo",
            "cannot confirm or submit",
        ] {
            assert!(USAGE.contains(text));
        }
        assert_eq!(
            crate::http::read_timeout_for("/v1/forms/plan"),
            std::time::Duration::from_secs(10)
        );
        assert_eq!(
            crate::http::read_timeout_for("/v1/forms/status/7"),
            std::time::Duration::from_secs(10)
        );
    }
}
