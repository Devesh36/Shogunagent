//! Issue #979: for an app whose code signature SHOGUN expects
//! (`shogun_mcp::launcher_check::EXPECTED_SIGNERS`), `shogun-mcp` exits unless that app started
//! it. On a Mac the process tree's signatures are read (`shogun_integrations::code_signature`);
//! elsewhere the check reports itself unsupported, which the policy table turns into the same
//! verdict the server build gives (never refuses today: the table is empty until each app is
//! confirmed on a Mac).

use shogun_mcp::launcher_check::{LauncherRefusal, LauncherVerdict};

/// `Some(refusal)` when the program that started this process is not the app `client` names.
pub fn refusal(client: &str) -> Option<LauncherRefusal> {
    #[cfg(target_os = "macos")]
    let verdict = shogun_mcp::launcher_check::judge(
        client,
        shogun_integrations::code_signature::ancestors(),
    );
    #[cfg(not(target_os = "macos"))]
    let verdict = shogun_mcp::launcher_check::judge_without_signatures(client);
    match verdict {
        LauncherVerdict::Refused(refusal) => Some(refusal),
        _ => None,
    }
}
