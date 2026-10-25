//! Which program may start `shogun-mcp --client <id>` (issue #979, stage 2 of #963).
//!
//! An app's connection is not a secret: any program running as the user can start
//! `shogun-mcp --client cursor` and use Cursor's access. For a desktop app whose code signature is
//! stable, `shogun-mcp` can check who started it: the nearest program above it that Apple did not
//! sign itself (a shell, `env` or `launchd` between them is looked through) must satisfy the code
//! requirement SHOGUN expects for that app. Anything else is refused at start-up, so the check can
//! only take access away, never add it.
//!
//! What is expected is compiled into the binary ([`EXPECTED_SIGNERS`]), not read from a file: a
//! rewritten expectation would undo the check, and the binary is as trusted as the check itself.
//!
//! Apps started from a terminal (Claude Code, Codex CLI, Gemini CLI and other npm-installed CLIs)
//! cannot be checked: the program above `shogun-mcp` is `node` or a shell, which any program can
//! start too. Their connections stay usable by any program running as the user
//! ([`LauncherCheck::AnyProgram`]), and Settings / `api.tokens.list` say so. The same goes for a
//! connection shared with a CLI (`codex-cli`, which the ChatGPT desktop app shares).
//!
//! The requirement is per vendor team, not the app's own designated requirement: Electron apps
//! start MCP servers from a helper process, which is signed by the same team under a different
//! identifier. [`team_requirement`] builds the Developer ID form. A vendor that keeps its Team ID
//! across updates keeps passing; one that changes it is refused until SHOGUN ships the new value
//! (the refusal says to update SHOGUN).
//!
//! **Status.** The table is empty: no app's Team ID and launch chain has been confirmed on a Mac
//! yet, and an unconfirmed entry would lock that app out. With an empty table every connection is
//! [`LauncherPolicy::AnyProgram`] and start-up does not check the launcher. To turn the check on for
//! an app, add its connection id and team here after confirming on a Mac (see
//! `docs/ondevice-qa-2026-09-26.md` §3-11) that the app's MCP launch chain reaches a process
//! signed by that team.

/// What SHOGUN expects of the program that starts `shogun-mcp --client <id>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpectedSigner {
    /// The vendor's Apple Developer Team ID (10 upper-case letters and digits).
    pub team_id: &'static str,
}

impl ExpectedSigner {
    /// The code requirement a launcher must satisfy ([`team_requirement`]).
    pub fn requirement(&self) -> String {
        team_requirement(self.team_id)
    }
}

/// The connections whose launcher is checked, by connection id
/// ([`crate::memory_api_settings::grant_id`]), and the team that must have signed it.
///
/// Empty on purpose until confirmed on a Mac (see the module docs). Never list a connection a CLI
/// shares (`claude-code`, `codex-cli`): its launcher is `node`.
///
/// TODO(audit 2026-10-20 F4): fill in `claude-desktop` and `cursor` once a Mac confirms, with
/// `codesign -dv --verbose=4` on each installed app and on the process that actually spawns
/// `shogun-mcp` (Electron helpers), the Team ID and that the launch chain reaches it. Public
/// sources did not settle either value reliably, and Cursor is now also distributed through the
/// Mac App Store, whose copies are re-signed by Apple and would fail a Developer ID requirement:
/// a wrong entry locks a paying user's connection out until the next release. ChatGPT desktop
/// shares the `codex-cli` connection and so can never be checked. Residual until then: any
/// program running as the user can start `shogun-mcp --client <id>` for an app that is
/// connected, within that connection's scope (read by default) and the per-client LLM rate limit
/// (`crate::rate_limit`). Settings and `api.tokens.list` already say "any program" for these.
pub const EXPECTED_SIGNERS: &[(&str, ExpectedSigner)] = &[];

/// Connection ids that are always started from a terminal program and so can never be checked,
/// even if listed in a table by mistake.
const CLI_CONNECTIONS: &[&str] = &["claude-code", "codex-cli"];

/// A Developer ID requirement pinned to one team: Apple's Developer ID chain, and the leaf
/// certificate issued to `team_id`. Satisfied by every program that team signs (an app and its
/// helpers alike), and by nothing anyone else signs.
pub fn team_requirement(team_id: &str) -> String {
    format!(
        "anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] exists and certificate leaf[field.1.2.840.113635.100.6.1.13] exists and certificate leaf[subject.OU] = \"{team_id}\""
    )
}

/// The code requirement that marks a program Apple signed as part of macOS (shells, `env`,
/// `login`, `launchd`): such a program is looked through, never accepted as the launcher.
pub const APPLE_REQUIREMENT: &str = "anchor apple";

/// A Team ID as Apple issues them. A malformed value would make the requirement string match
/// something else, so the table is checked against this in tests.
pub fn valid_team_id(team_id: &str) -> bool {
    team_id.len() == 10
        && team_id
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// How the program that starts a connection's server is checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LauncherPolicy {
    /// Not checked: any program running as the user can use this connection.
    AnyProgram,
    /// Only a program signed by this team may start the server.
    SignedApp(ExpectedSigner),
}

/// The policy for the app an entry names (`--client <id>`), by its connection id.
pub fn policy_for(client: &str) -> LauncherPolicy {
    policy_in(EXPECTED_SIGNERS, client)
}

fn policy_in(table: &[(&str, ExpectedSigner)], client: &str) -> LauncherPolicy {
    let id = crate::memory_api_settings::grant_id(client).unwrap_or(client);
    if CLI_CONNECTIONS.contains(&id) {
        return LauncherPolicy::AnyProgram;
    }
    table
        .iter()
        .find(|(connection, _)| *connection == id)
        .map_or(LauncherPolicy::AnyProgram, |(_, signer)| {
            LauncherPolicy::SignedApp(*signer)
        })
}

/// What Settings and `api.tokens.list` show for a connection: who can use it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LauncherCheck {
    /// Any program running as the user on this Mac.
    AnyProgram,
    /// Only the app itself, checked by its code signature.
    SignedApp,
}

impl LauncherPolicy {
    pub fn check(self) -> LauncherCheck {
        match self {
            LauncherPolicy::AnyProgram => LauncherCheck::AnyProgram,
            LauncherPolicy::SignedApp(_) => LauncherCheck::SignedApp,
        }
    }
}

/// One process above `shogun-mcp`, as the macOS glue read it (nearest first).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ancestor {
    /// Signed by Apple as part of macOS ([`APPLE_REQUIREMENT`]): looked through.
    Apple,
    /// Signed by the expected team.
    Expected,
    /// Anything else: another team, ad-hoc signed, unsigned, or a signature that is no longer
    /// valid.
    Other,
    /// Could not be read (it exited, or the system would not say).
    Unreadable,
}

/// How far up the process tree the check looks before giving up.
pub const MAX_ANCESTORS: usize = 8;

/// Why a launcher was refused. Each one fails closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LauncherRefusal {
    /// The nearest non-Apple program above `shogun-mcp` is not signed by the expected team.
    WrongSigner,
    /// No program other than Apple's own was found above `shogun-mcp` (started from Terminal, by
    /// `launchd`, or the app exited).
    NoAppLauncher,
    /// A process above `shogun-mcp` could not be read.
    Unreadable,
    /// This build cannot check signatures (not macOS).
    Unsupported,
}

/// The outcome of the start-up check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LauncherVerdict {
    /// No signature is expected for this app: allowed.
    NotChecked,
    /// The app that started `shogun-mcp` is signed by the expected team.
    Matches,
    Refused(LauncherRefusal),
}

impl LauncherVerdict {
    pub fn allowed(self) -> bool {
        matches!(self, LauncherVerdict::NotChecked | LauncherVerdict::Matches)
    }
}

/// Judge the program that started `shogun-mcp --client <client>`. `next_ancestor` is asked for
/// the processes above it one at a time, nearest first, with the requirement to test each against;
/// `None` means there is none further up. It is never called for an app that is not checked.
pub fn judge(client: &str, next_ancestor: impl FnMut(&str) -> Option<Ancestor>) -> LauncherVerdict {
    judge_policy(policy_for(client), next_ancestor)
}

/// [`judge`] for a build that cannot read code signatures (not macOS): an app that is not checked
/// is allowed, and one that is checked is refused, never waved through.
pub fn judge_without_signatures(client: &str) -> LauncherVerdict {
    unsupported_verdict(policy_for(client))
}

fn unsupported_verdict(policy: LauncherPolicy) -> LauncherVerdict {
    match policy {
        LauncherPolicy::AnyProgram => LauncherVerdict::NotChecked,
        LauncherPolicy::SignedApp(_) => LauncherVerdict::Refused(LauncherRefusal::Unsupported),
    }
}

/// [`judge`] for a given policy.
pub fn judge_policy(
    policy: LauncherPolicy,
    mut next_ancestor: impl FnMut(&str) -> Option<Ancestor>,
) -> LauncherVerdict {
    let LauncherPolicy::SignedApp(signer) = policy else {
        return LauncherVerdict::NotChecked;
    };
    let requirement = signer.requirement();
    for _ in 0..MAX_ANCESTORS {
        match next_ancestor(&requirement) {
            Some(Ancestor::Apple) => continue,
            Some(Ancestor::Expected) => return LauncherVerdict::Matches,
            Some(Ancestor::Other) => return LauncherVerdict::Refused(LauncherRefusal::WrongSigner),
            Some(Ancestor::Unreadable) => {
                return LauncherVerdict::Refused(LauncherRefusal::Unreadable)
            }
            None => break,
        }
    }
    LauncherVerdict::Refused(LauncherRefusal::NoAppLauncher)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEAM: ExpectedSigner = ExpectedSigner {
        team_id: "ABCDE12345",
    };

    fn chain(ancestors: &[Ancestor]) -> impl FnMut(&str) -> Option<Ancestor> + '_ {
        let mut rest = ancestors.iter();
        move |_| rest.next().copied()
    }

    #[test]
    fn no_app_is_checked_until_confirmed_on_a_mac() {
        // Shipping an unconfirmed team would lock that app out (issue #979).
        for client in crate::client_config::ALL_CLIENTS {
            assert_eq!(policy_for(client.id()), LauncherPolicy::AnyProgram);
            assert_eq!(policy_for(client.id()).check(), LauncherCheck::AnyProgram);
        }
    }

    #[test]
    fn listed_teams_are_well_formed_and_never_a_cli() {
        for (connection, signer) in EXPECTED_SIGNERS {
            assert!(valid_team_id(signer.team_id), "{connection}");
            assert!(
                !CLI_CONNECTIONS.contains(connection),
                "{connection} is started by a CLI"
            );
            assert_eq!(
                crate::memory_api_settings::grant_id(connection),
                Some(*connection)
            );
        }
    }

    #[test]
    fn an_unchecked_app_never_reads_the_process_tree() {
        let verdict = judge_policy(LauncherPolicy::AnyProgram, |_| panic!("must not be called"));
        assert_eq!(verdict, LauncherVerdict::NotChecked);
        assert!(verdict.allowed());
        assert_eq!(
            judge("cursor", |_| panic!("must not be called")),
            LauncherVerdict::NotChecked
        );
        // A build without signature checks allows every unchecked app.
        for client in crate::client_config::ALL_CLIENTS {
            assert_eq!(
                judge_without_signatures(client.id()),
                LauncherVerdict::NotChecked
            );
        }
        // A checked app is refused there, never waved through.
        assert_eq!(
            unsupported_verdict(LauncherPolicy::SignedApp(TEAM)),
            LauncherVerdict::Refused(LauncherRefusal::Unsupported)
        );
    }

    #[test]
    fn cli_connections_stay_unchecked_even_if_listed() {
        let table = [("codex-cli", TEAM), ("claude-code", TEAM), ("cursor", TEAM)];
        assert_eq!(policy_in(&table, "codex-cli"), LauncherPolicy::AnyProgram);
        // ChatGPT desktop shares Codex CLI's connection, so it is unchecked too.
        assert_eq!(
            policy_in(&table, "chatgpt-desktop"),
            LauncherPolicy::AnyProgram
        );
        assert_eq!(policy_in(&table, "claude-code"), LauncherPolicy::AnyProgram);
        assert_eq!(policy_in(&table, "cursor"), LauncherPolicy::SignedApp(TEAM));
        assert_eq!(
            policy_in(&table, "cursor").check(),
            LauncherCheck::SignedApp
        );
        assert_eq!(
            policy_in(&table, "claude-desktop"),
            LauncherPolicy::AnyProgram
        );
    }

    #[test]
    fn the_app_itself_or_through_apple_shells_matches() {
        let policy = LauncherPolicy::SignedApp(TEAM);
        assert_eq!(
            judge_policy(policy, chain(&[Ancestor::Expected])),
            LauncherVerdict::Matches
        );
        assert_eq!(
            judge_policy(
                policy,
                chain(&[Ancestor::Apple, Ancestor::Apple, Ancestor::Expected])
            ),
            LauncherVerdict::Matches
        );
    }

    #[test]
    fn the_nearest_non_apple_program_decides() {
        let policy = LauncherPolicy::SignedApp(TEAM);
        // Someone else's program starting the server through a shell, even from inside the app's
        // own process tree, is refused: only the nearest non-Apple program counts.
        let verdict = judge_policy(
            policy,
            chain(&[Ancestor::Apple, Ancestor::Other, Ancestor::Expected]),
        );
        assert_eq!(
            verdict,
            LauncherVerdict::Refused(LauncherRefusal::WrongSigner)
        );
        assert!(!verdict.allowed());
    }

    #[test]
    fn fails_closed() {
        let policy = LauncherPolicy::SignedApp(TEAM);
        // Started from Terminal or by launchd: only Apple's programs above.
        assert_eq!(
            judge_policy(policy, chain(&[Ancestor::Apple, Ancestor::Apple])),
            LauncherVerdict::Refused(LauncherRefusal::NoAppLauncher)
        );
        assert_eq!(
            judge_policy(policy, chain(&[])),
            LauncherVerdict::Refused(LauncherRefusal::NoAppLauncher)
        );
        assert_eq!(
            judge_policy(
                policy,
                chain(&[Ancestor::Apple, Ancestor::Unreadable, Ancestor::Expected])
            ),
            LauncherVerdict::Refused(LauncherRefusal::Unreadable)
        );
        // A chain longer than the limit is not walked forever.
        let mut asked = 0;
        let verdict = judge_policy(policy, |_| {
            asked += 1;
            Some(Ancestor::Apple)
        });
        assert_eq!(
            verdict,
            LauncherVerdict::Refused(LauncherRefusal::NoAppLauncher)
        );
        assert_eq!(asked, MAX_ANCESTORS);
    }

    #[test]
    fn each_ancestor_is_tested_against_the_team_requirement() {
        let mut seen = Vec::new();
        let _ = judge_policy(LauncherPolicy::SignedApp(TEAM), |requirement| {
            seen.push(requirement.to_string());
            Some(Ancestor::Expected)
        });
        assert_eq!(seen, vec![team_requirement("ABCDE12345")]);
    }

    #[test]
    fn team_requirement_pins_developer_id_and_the_team() {
        let requirement = team_requirement("ABCDE12345");
        assert!(requirement.starts_with("anchor apple generic "));
        assert!(requirement.contains("certificate 1[field.1.2.840.113635.100.6.2.6] exists"));
        assert!(requirement.contains("certificate leaf[field.1.2.840.113635.100.6.1.13] exists"));
        assert!(requirement.ends_with("certificate leaf[subject.OU] = \"ABCDE12345\""));
        assert!(valid_team_id("ABCDE12345"));
        for bad in [
            "",
            "abcde12345",
            "ABCDE1234",
            "ABCDE123456",
            "ABCDE\"1234",
            "ABCDE 1234",
        ] {
            assert!(!valid_team_id(bad), "{bad:?}");
        }
    }
}
