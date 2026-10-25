//! Capture exclusion policy (FR-CAP-05/06) — the privacy gate that decides, before any event
//! is generated, whether the focused app/window must not be captured at all.
//!
//! Pure decision logic over `(bundle_id, window_title, website)`: the macOS capture layer calls
//! [`ExclusionPolicy::is_excluded_with_website`] on every focus change and simply does not emit a
//! `capture.text` event while the answer is `Some`. Defaults (password managers, the macOS
//! auth agent) are non-removable (FR-CAP-06); users may add apps and title/URL patterns.
//! Terminals are captured like any app (owner decision 2026-10-19, Goldfish parity; secret
//! redaction still runs on their text); a user who wants one out adds it as an app exclusion.
//! Form reads and fills stay closed to terminals ([`ExclusionPolicy::form_exclusion`]).
//!
//! Private-browsing detection is heuristic and deliberately conservative: only known browsers
//! with a recognised private-mode title marker are excluded; an unknown browser is captured
//! normally (FR-CAP-05).

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

static EXCLUSIONS_WRITE_LOCK: Mutex<()> = Mutex::new(());
static EXCLUSIONS_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Why a focus was excluded from capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExclusionReason {
    /// SHOGUN itself, including isolated local builds (non-removable).
    OwnApp,
    /// A password manager (non-removable default).
    PasswordManager,
    /// The macOS authentication dialog / SecurityAgent (non-removable default).
    AuthDialog,
    /// A known browser in a private/incognito window.
    PrivateBrowsing,
    /// A terminal / dev console: never a form to read or fill ([`ExclusionPolicy::form_exclusion`]).
    /// Capture no longer excludes terminals (owner decision 2026-10-19).
    Terminal,
    /// A user-added app (by bundle id).
    UserApp,
    /// A user-added title/URL substring pattern.
    UserPattern,
    /// A user website rule.
    UserWebsite,
    /// Website rules exist, but the browser did not expose a verifiable active host.
    BrowserUrlUnavailable,
}

/// Non-removable default bundle ids (FR-CAP-05, FR-CAP-06 "cannot be deleted").
const PASSWORD_MANAGERS: &[&str] = &[
    "com.1password.1password",
    "com.agilebits.onepassword7",
    "com.agilebits.onepassword4",
    "com.bitwarden.desktop",
    "org.keepassxc.keepassxc",
    "com.dashlane.Dashlane",
    "in.sinew.Enpass-Desktop",
    "com.apple.keychainaccess",
    "com.apple.Passwords", // macOS 15+: the built-in password manager
];

/// The macOS auth agent (non-removable default).
const AUTH_AGENTS: &[&str] = &["com.apple.SecurityAgent"];

/// Terminals / dev consoles. Captured like any app since 2026-10-19; closed to form mode.
const TERMINALS: &[&str] = &[
    "com.apple.Terminal",
    "com.googlecode.iterm2",
    "dev.warp.Warp-Stable",
    "io.alacritty",
    "net.kovidgoyal.kitty",
    "com.github.wez.wezterm",
    "co.zeit.hyper",
    "org.tabby",
];

/// Known browsers whose private windows we can detect by title marker.
const KNOWN_BROWSERS: &[&str] = &[
    "com.apple.Safari",
    "com.google.Chrome",
    "com.google.Chrome.beta",
    "com.google.Chrome.canary",
    "com.microsoft.edgemac",
    "com.brave.Browser",
    "company.thebrowser.Browser", // Arc
    "company.thebrowser.dia",     // Dia
    "org.mozilla.firefox",
];

/// Case-insensitive title markers that indicate a private/incognito window.
const PRIVATE_TITLE_MARKERS: &[&str] = &[
    "incognito",        // Chrome/Brave/Edge: "… (Incognito)"
    "private browsing", // Safari/Firefox
    "inprivate",        // Edge legacy
];

/// User-configurable exclusions layered over the non-removable defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExclusionPolicy {
    user_bundles: BTreeSet<String>,
    user_title_patterns: Vec<String>, // lowercased substrings
    user_websites: BTreeSet<String>,
}

impl ExclusionPolicy {
    /// A policy with only the non-removable defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Every user exclusion either policy has (audit 2026-10-20 M6: what an unsealed edit of
    /// `exclusions.json` cannot remove).
    pub fn union(&self, other: &Self) -> Self {
        let mut merged = self.clone();
        merged.user_bundles.extend(other.user_bundles.iter().cloned());
        merged.user_websites.extend(other.user_websites.iter().cloned());
        for pattern in &other.user_title_patterns {
            if !merged.user_title_patterns.contains(pattern) {
                merged.user_title_patterns.push(pattern.clone());
            }
        }
        merged
    }

    /// Add a user app exclusion by bundle id. No-op if it is already a default (those are
    /// always on anyway).
    pub fn add_app(&mut self, bundle_id: impl Into<String>) {
        let bundle_id = bundle_id.into();
        if is_default_excluded(&bundle_id) {
            return;
        }
        self.user_bundles.insert(bundle_id);
    }

    /// Remove a user app exclusion. Defaults cannot be removed (FR-CAP-06) — attempting to
    /// remove one is a no-op and returns `false`.
    pub fn remove_app(&mut self, bundle_id: &str) -> bool {
        if is_default_excluded(bundle_id) {
            return false;
        }
        self.user_bundles.remove(bundle_id)
    }

    /// The user's own app exclusions in stable order. Defaults are not included — they are always
    /// on and cannot be removed, so they are not part of anything a user could edit.
    pub fn user_apps(&self) -> Vec<&str> {
        self.user_bundles.iter().map(String::as_str).collect()
    }

    pub fn user_websites(&self) -> Vec<&str> {
        self.user_websites.iter().map(String::as_str).collect()
    }

    pub fn has_user_websites(&self) -> bool {
        !self.user_websites.is_empty()
    }

    /// The exclusion categories and how many rules each currently covers, read from THIS live
    /// policy. Onboarding renders these directly (issue #6, "reads" step): a UI that hardcoded the
    /// list would tell the user a lie about today's behaviour the moment a default is added. Only
    /// the non-removable defaults are reported, counted from their live tables — onboarding
    /// presents them as built in. Title patterns are not a category: no shipped path loads or
    /// persists them, so listing one would claim a protection the app does not apply. Ids are
    /// stable keys looked up in the UI's string catalogue (strings.ts) — never display text
    /// (CLAUDE.md: copy stays out of code).
    pub fn category_counts(&self) -> Vec<(&'static str, usize)> {
        vec![
            ("password_managers", PASSWORD_MANAGERS.len()),
            ("auth_dialog", AUTH_AGENTS.len()),
            ("private_browsing", KNOWN_BROWSERS.len()),
        ]
    }

    /// Add a title/URL substring pattern (matched case-insensitively against the window title).
    pub fn add_title_pattern(&mut self, pattern: impl Into<String>) {
        let p = pattern.into().to_lowercase();
        if !p.is_empty() && !self.user_title_patterns.contains(&p) {
            self.user_title_patterns.push(p);
        }
    }

    pub fn add_website(&mut self, website: impl AsRef<str>) -> bool {
        normalize_website(website.as_ref()).is_some_and(|host| self.user_websites.insert(host))
    }

    /// App/title preflight only; use `is_excluded_with_website` before reading content.
    pub fn is_excluded(
        &self,
        bundle_id: &str,
        window_title: Option<&str>,
    ) -> Option<ExclusionReason> {
        if super::own_app::is_own_app(bundle_id, "") {
            return Some(ExclusionReason::OwnApp);
        }
        if PASSWORD_MANAGERS.contains(&bundle_id) {
            return Some(ExclusionReason::PasswordManager);
        }
        if AUTH_AGENTS.contains(&bundle_id) {
            return Some(ExclusionReason::AuthDialog);
        }
        if self.user_bundles.iter().any(|app| app == bundle_id) {
            return Some(ExclusionReason::UserApp);
        }
        let title_lc = window_title.map(|t| t.to_lowercase());
        if let Some(title) = &title_lc {
            if is_known_browser(bundle_id)
                && PRIVATE_TITLE_MARKERS.iter().any(|m| title.contains(m))
            {
                return Some(ExclusionReason::PrivateBrowsing);
            }
            if self.user_title_patterns.iter().any(|p| title.contains(p)) {
                return Some(ExclusionReason::UserPattern);
            }
        }
        None
    }

    /// The gate form mode reads and fills through: the capture gate, plus terminals, which form
    /// mode never touches even though capture reads them.
    pub fn form_exclusion(
        &self,
        bundle_id: &str,
        window_title: Option<&str>,
        website: Option<&str>,
    ) -> Option<ExclusionReason> {
        self.is_excluded_with_website(bundle_id, window_title, website)
            .or_else(|| is_terminal(bundle_id).then_some(ExclusionReason::Terminal))
    }

    /// Full content gate. A known browser with website rules must supply a usable host.
    pub fn is_excluded_with_website(
        &self,
        bundle_id: &str,
        window_title: Option<&str>,
        website: Option<&str>,
    ) -> Option<ExclusionReason> {
        if let Some(reason) = self.is_excluded(bundle_id, window_title) {
            return Some(reason);
        }
        if self.has_user_websites() {
            match website.and_then(normalize_website) {
                Some(host) => {
                    if self.user_websites.iter().any(|rule| {
                        host == *rule
                            || host
                                .strip_suffix(rule.as_str())
                                .is_some_and(|prefix| prefix.ends_with('.'))
                    }) {
                        return Some(ExclusionReason::UserWebsite);
                    }
                }
                None if is_known_browser(bundle_id) => {
                    return Some(ExclusionReason::BrowserUrlUnavailable)
                }
                None => {}
            }
        }
        None
    }
}

impl ExclusionPolicy {
    /// Whether a window other than the focused one must be left out of a full-display Visual
    /// Recall preview (#969). The same rules capture applies, minus what only the focused window
    /// can prove: a background browser window exposes no verifiable URL, so it is dropped whenever
    /// website rules exist, and without a title its private-mode marker cannot be checked.
    /// Notification banners are always dropped: they show another app's content (an excluded
    /// app's message included) under Notification Center's identity.
    pub fn excludes_background_window(
        &self,
        bundle_id: &str,
        app_name: &str,
        window_title: Option<&str>,
    ) -> bool {
        bundle_id.eq_ignore_ascii_case(NOTIFICATION_CENTER)
            || super::own_app::is_own_app(bundle_id, app_name)
            || self.is_excluded(bundle_id, window_title).is_some()
            || (is_known_browser(bundle_id) && (self.has_user_websites() || window_title.is_none()))
    }
}

/// Draws every app's notification banners, so its windows carry other apps' content.
const NOTIFICATION_CENTER: &str = "com.apple.notificationcenterui";

/// Exact lower-case host; schemes, paths, queries, and fragments are ignored.
pub fn normalize_website(input: &str) -> Option<String> {
    let input = input.trim();
    let authority = input
        .split_once("://")
        .map_or(input, |(_, rest)| rest)
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let (host, port) = authority
        .split_once(':')
        .map_or((authority, None), |(host, port)| (host, Some(port)));
    if port.is_some_and(|port| port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit())) {
        return None;
    }
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    (!host.is_empty()
        && !host.starts_with('.')
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-')))
    .then_some(host)
}

#[derive(Default, serde::Serialize)]
struct StoredExclusions {
    apps: Vec<String>,
    websites: Vec<String>,
}

impl<'de> serde::Deserialize<'de> for StoredExclusions {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Legacy(Vec<String>),
            Current {
                #[serde(default)]
                apps: Vec<String>,
                #[serde(default)]
                websites: Vec<String>,
            },
        }
        Ok(match Raw::deserialize(deserializer)? {
            Raw::Legacy(apps) => Self {
                apps,
                websites: Vec::new(),
            },
            Raw::Current { apps, websites } => Self { apps, websites },
        })
    }
}

/// The policy in force at launch. A missing file is the empty list, checked against the seal like
/// any other (audit 2026-10-20 M6): deleting the file does not drop sealed exclusions.
pub fn load_policy(path: &Path) -> ExclusionPolicy {
    try_load_policy(path).unwrap_or_else(|| match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => sealed_view(path, ExclusionPolicy::new()),
        _ => ExclusionPolicy::new(),
    })
}

/// A reloadable document. `None` preserves an installed last-good policy on missing/corrupt data.
/// What it returns is checked against the file's seal ([`crate::settings_seal`], audit
/// 2026-10-20 M6): an exclusion removed by editing the file stays in force.
pub fn try_load_policy(path: &Path) -> Option<ExclusionPolicy> {
    let text = std::fs::read_to_string(path).ok()?;
    parse_policy(&text).map(|policy| sealed_view(path, policy))
}

fn sealed_view(path: &Path, policy: ExclusionPolicy) -> ExclusionPolicy {
    crate::settings_seal::trusted(path, policy, crate::settings_seal::now_ms())
}

/// The canonical document of `policy` (what `exclusions.json` holds), for the seal.
pub fn policy_document(policy: &ExclusionPolicy) -> String {
    let stored = StoredExclusions {
        apps: policy.user_apps().into_iter().map(str::to_string).collect(),
        websites: policy.user_websites().into_iter().map(str::to_string).collect(),
    };
    serde_json::to_string(&stored).unwrap_or_default()
}

/// Read a document written by [`policy_document`] (or `exclusions.json` itself).
pub fn parse_policy_document(text: &str) -> Option<ExclusionPolicy> {
    parse_policy(text)
}

fn parse_policy(text: &str) -> Option<ExclusionPolicy> {
    serde_json::from_str::<StoredExclusions>(text)
        .ok()
        .map(|stored| requested_policy(&stored.apps, &stored.websites))
}

/// The policy a pair of editable lists describes (defaults dropped, website rules normalized).
fn requested_policy(apps: &[String], websites: &[String]) -> ExclusionPolicy {
    let mut policy = ExclusionPolicy::new();
    for app in apps {
        policy.add_app(app.as_str());
    }
    for website in websites {
        policy.add_website(website);
    }
    policy
}

/// The lists on disk as a writer must see them (issue #657). A missing file is the empty policy.
/// An unreadable or corrupt one is an error, never "nothing excluded": the desktop keeps enforcing
/// its last good policy in that case, so treating the file as empty would let a replacement that
/// looks like a pure addition silently drop exclusions that are still in force.
fn read_current(path: &Path) -> Result<ExclusionPolicy, String> {
    let file = match std::fs::read_to_string(path) {
        Ok(text) => parse_policy(&text).ok_or_else(|| "capture exclusions file is unreadable".to_string())?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => ExclusionPolicy::new(),
        Err(_) => return Err("capture exclusions file is unreadable".to_string()),
    };
    // The lists in force (M6): what an unsealed edit removed still counts as excluded, so an API
    // replacement that "only adds" relative to the edited file still needs approval to drop it.
    Ok(sealed_view(path, file))
}

/// What replacing the user's editable lists with a request would do (issue #657). `apps` /
/// `websites` are the requested lists as they would be stored; `resumed_*` are the exclusions in
/// force now that the replacement drops, i.e. what would start being captured again. Both empty
/// means the request only adds (or changes nothing).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Replacement {
    pub apps: Vec<String>,
    pub websites: Vec<String>,
    pub resumed_apps: Vec<String>,
    pub resumed_websites: Vec<String>,
}

impl Replacement {
    pub fn resumes_capture(&self) -> bool {
        !self.resumed_apps.is_empty() || !self.resumed_websites.is_empty()
    }
}

fn replacement(current: &ExclusionPolicy, requested: &ExclusionPolicy) -> Replacement {
    let owned = |items: Vec<&str>| items.into_iter().map(str::to_string).collect::<Vec<_>>();
    Replacement {
        apps: owned(requested.user_apps()),
        websites: owned(requested.user_websites()),
        resumed_apps: current
            .user_bundles
            .difference(&requested.user_bundles)
            .cloned()
            .collect(),
        resumed_websites: current
            .user_websites
            .difference(&requested.user_websites)
            .cloned()
            .collect(),
    }
}

/// Classify a replacement request against the lists on disk without writing anything (issue
/// #657): the API faces use it to decide between a direct write (only additions) and an L3
/// approval (anything removed).
pub fn plan_replacement(
    path: &Path,
    apps: &[String],
    websites: &[String],
) -> Result<Replacement, String> {
    validate_user_exclusions(apps, websites)?;
    let current = read_current(path)?;
    Ok(replacement(&current, &requested_policy(apps, websites)))
}

/// Why [`replace_policy_resuming_at_most`] refused. Starts with "invalid " so the REST face
/// answers 400: the caller asked for something it may not do on its own.
pub const RESUME_NOT_APPROVED: &str = "invalid capture exclusions request: removing an exclusion \
needs approval in the ShogunAI app, and only the apps and websites shown for approval can be removed";

/// Replace the editable lists for a caller that is not the user at the Settings screen (issue
/// #657). The lists on disk are re-read under the write lock, and the write is refused unless
/// every exclusion it would drop is in `approved_apps` / `approved_websites`. The API faces pass
/// nothing (additions only, L1); a confirmed approval passes exactly what its card showed, so a
/// list that changed after the request — a new exclusion the card never named — fails closed.
pub fn replace_policy_resuming_at_most(
    path: &Path,
    apps: &[String],
    websites: &[String],
    approved_apps: &[String],
    approved_websites: &[String],
) -> Result<ExclusionPolicy, String> {
    let _guard = EXCLUSIONS_WRITE_LOCK
        .lock()
        .map_err(|_| "capture exclusions lock poisoned".to_string())?;
    validate_user_exclusions(apps, websites)?;
    let requested = requested_policy(apps, websites);
    let plan = replacement(&read_current(path)?, &requested);
    let approved_websites: BTreeSet<String> = approved_websites
        .iter()
        .filter_map(|website| normalize_website(website))
        .collect();
    if plan
        .resumed_apps
        .iter()
        .any(|app| !approved_apps.contains(app))
        || plan
            .resumed_websites
            .iter()
            .any(|website| !approved_websites.contains(website))
    {
        return Err(RESUME_NOT_APPROVED.to_string());
    }
    write_policy(path, requested)
}

/// Validate, replace, and atomically persist the editable lists. The desktop Settings screen's
/// path: the user is the actor there, so removals apply directly. API callers go through
/// [`replace_policy_resuming_at_most`] instead (issue #657).
pub fn replace_policy(
    path: &Path,
    apps: &[String],
    websites: &[String],
) -> Result<ExclusionPolicy, String> {
    let _guard = EXCLUSIONS_WRITE_LOCK
        .lock()
        .map_err(|_| "capture exclusions lock poisoned".to_string())?;
    validate_user_exclusions(apps, websites)?;
    write_policy(path, requested_policy(apps, websites))
}

/// Persist `policy` atomically (temp file + rename). The caller holds `EXCLUSIONS_WRITE_LOCK`.
fn write_policy(path: &Path, policy: ExclusionPolicy) -> Result<ExclusionPolicy, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create exclusions directory: {e}"))?;
    }
    let stored = StoredExclusions {
        apps: policy.user_apps().into_iter().map(str::to_string).collect(),
        websites: policy
            .user_websites()
            .into_iter()
            .map(str::to_string)
            .collect(),
    };
    let json = serde_json::to_string_pretty(&stored).map_err(|e| e.to_string())?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("exclusions.json");
    let temp = path.with_file_name(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        EXCLUSIONS_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&temp, json).map_err(|e| format!("save exclusions temp file: {e}"))?;
    std::fs::rename(&temp, path).map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        format!("replace exclusions: {e}")
    })?;
    // The app's or an approved API write: seal it (M6).
    crate::settings_seal::seal(path, &policy);
    Ok(policy)
}

pub fn validate_user_exclusions(apps: &[String], websites: &[String]) -> Result<(), String> {
    if apps
        .iter()
        .any(|app| app.trim().is_empty() || app.chars().any(char::is_whitespace))
    {
        return Err("invalid capture exclusion app".into());
    }
    if websites
        .iter()
        .any(|website| normalize_website(website).is_none())
    {
        return Err("invalid capture exclusion website".into());
    }
    Ok(())
}

/// The non-removable exclusion categories, with how many apps each covers.
///
/// Exposed so the UI can state what is protected without hardcoding a second copy of the list —
/// a settings screen claiming "password managers are excluded" while the policy disagreed would
/// be worse than saying nothing. Ids are the same stable keys as [`ExclusionPolicy::category_counts`],
/// worded by the UI's string catalogue — never display text (CLAUDE.md: copy stays out of code).
pub fn default_categories() -> [(&'static str, usize); 3] {
    [
        ("password_managers", PASSWORD_MANAGERS.len()),
        ("auth_dialog", AUTH_AGENTS.len()),
        ("private_browsing", PRIVATE_TITLE_MARKERS.len()),
    ]
}

/// True if `bundle_id` is one of the non-removable defaults.
pub fn is_default_excluded(bundle_id: &str) -> bool {
    super::own_app::is_own_app(bundle_id, "")
        || PASSWORD_MANAGERS.contains(&bundle_id)
        || AUTH_AGENTS.contains(&bundle_id)
}

/// A terminal / dev console: captured, but never a form.
pub fn is_terminal(bundle_id: &str) -> bool {
    TERMINALS.contains(&bundle_id)
}

pub fn is_known_browser(bundle_id: &str) -> bool {
    KNOWN_BROWSERS
        .iter()
        .any(|known| bundle_id.trim().eq_ignore_ascii_case(known))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_capture_is_non_removable_for_shipping_and_isolated_builds() {
        let mut policy = ExclusionPolicy::new();
        for bundle in [
            "com.syogun.shogunai",
            "com.syogun.shogunai.test",
            "dev.shogun.spike",
        ] {
            policy.add_app(bundle);
            assert!(!policy.remove_app(bundle));
            assert!(is_default_excluded(bundle));
            assert_eq!(
                policy.is_excluded(bundle, None),
                Some(ExclusionReason::OwnApp)
            );
        }
        assert_eq!(policy.is_excluded("com.syogun.shogunaihelper", None), None);
        assert_eq!(policy.is_excluded("com.apple.Safari", None), None);
    }

    #[test]
    fn all_default_password_managers_are_excluded() {
        let p = ExclusionPolicy::new();
        for pm in PASSWORD_MANAGERS {
            assert_eq!(
                p.is_excluded(pm, None),
                Some(ExclusionReason::PasswordManager),
                "{pm}"
            );
        }
    }

    #[test]
    fn apples_passwords_app_is_a_locked_password_manager() {
        // macOS 15+ ships Passwords as every user's default password manager; it shows
        // usernames, sites and one-time codes as plain static text (issue #771).
        let mut p = ExclusionPolicy::new();
        assert_eq!(
            p.is_excluded("com.apple.Passwords", Some("Passwords")),
            Some(ExclusionReason::PasswordManager)
        );
        assert!(is_default_excluded("com.apple.Passwords"));
        assert!(!p.remove_app("com.apple.Passwords"), "a default cannot be removed");
        assert_eq!(
            p.is_excluded("com.apple.Passwords", None),
            Some(ExclusionReason::PasswordManager)
        );
    }

    #[test]
    fn terminals_are_captured_unless_the_user_excludes_one_but_never_forms() {
        let mut p = ExclusionPolicy::new();
        for term in TERMINALS {
            assert_eq!(p.is_excluded(term, None), None, "{term}");
            assert!(!is_default_excluded(term), "{term}");
            assert_eq!(p.form_exclusion(term, None, None), Some(ExclusionReason::Terminal), "{term}");
        }
        p.add_app("com.apple.Terminal");
        assert_eq!(p.user_apps(), vec!["com.apple.Terminal"]);
        assert_eq!(p.is_excluded("com.apple.Terminal", None), Some(ExclusionReason::UserApp));
        assert!(p.remove_app("com.apple.Terminal"));
        assert_eq!(p.is_excluded("com.apple.Terminal", None), None);
        assert_eq!(p.form_exclusion("com.apple.mail", None, None), None);
    }

    #[test]
    fn auth_agent_is_excluded() {
        let p = ExclusionPolicy::new();
        assert_eq!(
            p.is_excluded("com.apple.SecurityAgent", None),
            Some(ExclusionReason::AuthDialog)
        );
    }

    /// `is_default_excluded` and `user_apps` describe the policy to the outside; `is_excluded` is
    /// what capture obeys. If they drift, anything reading the first two — a settings screen, a
    /// diagnostic, the stored exclusion file — describes a policy the daemon does not enforce.
    #[test]
    fn what_the_policy_reports_matches_what_it_enforces() {
        let mut p = ExclusionPolicy::new();
        assert!(
            p.user_apps().is_empty(),
            "defaults are not the user's own list"
        );

        p.add_app("com.acme.chat");
        assert_eq!(p.user_apps(), vec!["com.acme.chat"]);
        assert_eq!(
            p.is_excluded("com.acme.chat", None),
            Some(ExclusionReason::UserApp)
        );
        assert!(p.remove_app("com.acme.chat"));
        assert_eq!(p.is_excluded("com.acme.chat", None), None);

        // Anything the policy refuses to stop excluding must report itself as a default, so
        // nothing offers a switch that would do nothing.
        for locked in PASSWORD_MANAGERS.iter().chain(AUTH_AGENTS) {
            assert!(is_default_excluded(locked), "{locked}");
            p.add_app(*locked);
            assert!(!p.remove_app(locked), "{locked} must not be removable");
            assert!(
                p.is_excluded(locked, None).is_some(),
                "{locked} stays excluded"
            );
        }
    }

    #[test]
    fn ordinary_app_is_captured() {
        let p = ExclusionPolicy::new();
        assert_eq!(
            p.is_excluded("com.apple.Safari", Some("GitHub — Pull requests")),
            None
        );
        assert_eq!(p.is_excluded("com.microsoft.VSCode", Some("main.rs")), None);
    }

    #[test]
    fn chrome_incognito_is_excluded_but_normal_chrome_is_not() {
        let p = ExclusionPolicy::new();
        assert_eq!(
            p.is_excluded("com.google.Chrome", Some("Docs (Incognito)")),
            Some(ExclusionReason::PrivateBrowsing)
        );
        assert_eq!(p.is_excluded("com.google.Chrome", Some("Docs")), None);
    }

    #[test]
    fn safari_private_browsing_marker() {
        let p = ExclusionPolicy::new();
        assert_eq!(
            p.is_excluded("com.apple.Safari", Some("Private Browsing")),
            Some(ExclusionReason::PrivateBrowsing)
        );
    }

    #[test]
    fn unknown_browser_private_is_not_detected() {
        // An unknown browser is captured normally even if its title says "incognito"
        // (FR-CAP-05: only known browsers are judged).
        let p = ExclusionPolicy::new();
        assert_eq!(
            p.is_excluded("com.unknown.browser", Some("x (Incognito)")),
            None
        );
    }

    #[test]
    fn user_app_exclusion_add_and_remove() {
        let mut p = ExclusionPolicy::new();
        p.add_app("com.example.SecretApp");
        assert_eq!(
            p.is_excluded("com.example.SecretApp", None),
            Some(ExclusionReason::UserApp)
        );
        assert!(p.remove_app("com.example.SecretApp"));
        assert_eq!(p.is_excluded("com.example.SecretApp", None), None);
    }

    #[test]
    fn website_exclusion_matches_normalized_host_and_subdomains() {
        let mut policy = ExclusionPolicy::new();
        assert!(policy.add_website("HTTPS://Private.Example.com:8443/path?token=hidden"));
        assert_eq!(
            policy.is_excluded_with_website(
                "com.apple.Safari",
                None,
                Some("https://private.example.com:9443/inbox")
            ),
            Some(ExclusionReason::UserWebsite)
        );
        assert_eq!(
            policy.is_excluded_with_website(
                "com.apple.Safari",
                None,
                Some("https://child.private.example.com")
            ),
            Some(ExclusionReason::UserWebsite)
        );
        assert_eq!(
            policy.is_excluded_with_website("com.apple.Safari", None, None),
            Some(ExclusionReason::BrowserUrlUnavailable)
        );
    }

    #[test]
    fn bare_website_inputs_canonicalize_without_losing_host_boundaries() {
        let inputs = [
            "example.com",
            "www.example.com",
            "https://EXAMPLE.com/path",
            "EXAMPLE.COM/",
            "example.com.",
        ];
        let expected = [
            "example.com",
            "www.example.com",
            "example.com",
            "example.com",
            "example.com",
        ];
        for (input, host) in inputs.into_iter().zip(expected) {
            assert_eq!(normalize_website(input).as_deref(), Some(host), "{input}");
        }

        let websites = ["example.com", "https://EXAMPLE.com/path", "EXAMPLE.COM/"].map(str::to_string);
        assert!(validate_user_exclusions(&[], &websites).is_ok());
        let policy = requested_policy(&[], &websites);
        assert_eq!(policy.user_websites(), vec!["example.com"]);
        for host in ["example.com", "www.example.com", "mail.www.example.com"] {
            assert_eq!(
                policy.is_excluded_with_website("com.apple.Safari", None, Some(host)),
                Some(ExclusionReason::UserWebsite),
                "{host}"
            );
        }
        for host in ["notexample.com", "example.com.evil.test"] {
            assert_eq!(
                policy.is_excluded_with_website("com.apple.Safari", None, Some(host)),
                None,
                "{host}"
            );
        }
        let www_only = requested_policy(&[], &["www.example.com".to_string()]);
        assert_eq!(
            www_only.is_excluded_with_website("com.apple.Safari", None, Some("example.com")),
            None
        );
    }

    #[test]
    fn website_rules_reject_lookalikes_and_allow_unrelated_hosts() {
        let mut policy = ExclusionPolicy::new();
        assert!(policy.add_website("example.com"));
        for host in ["notexample.com", "example.com.evil.test", "other.test"] {
            assert_eq!(
                policy.is_excluded_with_website("com.apple.Safari", None, Some(host)),
                None,
                "{host}"
            );
        }
    }

    #[test]
    fn website_rules_fail_closed_only_for_browsers_with_rules() {
        let mut policy = ExclusionPolicy::new();
        assert_eq!(
            policy.is_excluded_with_website("com.apple.Safari", None, None),
            None
        );
        assert!(policy.add_website("example.com"));
        for browser in KNOWN_BROWSERS {
            // Preflight must allow URL lookup before the full content gate.
            assert_eq!(policy.is_excluded(browser, None), None);
            for website in [None, Some(""), Some("https://"), Some("not a URL")] {
                assert_eq!(
                    policy.is_excluded_with_website(browser, None, website),
                    Some(ExclusionReason::BrowserUrlUnavailable)
                );
            }
        }
        assert_eq!(
            policy.is_excluded_with_website("com.apple.mail", None, None),
            None
        );
    }

    #[test]
    fn browser_variants_share_case_insensitive_website_protection() {
        let mut policy = ExclusionPolicy::new();
        assert!(policy.add_website("example.com"));
        for browser in [
            "COM.GOOGLE.CHROME.BETA",
            "com.google.Chrome.canary",
            "company.thebrowser.dia",
        ] {
            assert!(is_known_browser(browser));
            for host in ["example.com", "mail.example.com"] {
                assert_eq!(
                    policy.is_excluded_with_website(browser, None, Some(host)),
                    Some(ExclusionReason::UserWebsite)
                );
            }
            assert_eq!(
                policy.is_excluded_with_website(browser, None, None),
                Some(ExclusionReason::BrowserUrlUnavailable)
            );
            assert_eq!(
                policy.is_excluded_with_website(browser, None, Some("notexample.com")),
                None
            );
        }
    }

    #[test]
    fn browser_detection_and_dead_leading_dot_rules_are_explicit() {
        assert!(is_known_browser("com.apple.Safari"));
        assert!(!is_known_browser("com.tinyspeck.slackmacgap"));
        assert_eq!(normalize_website(".example.com"), None);
    }

    #[test]
    fn exclusion_store_round_trips_app_and_website() {
        let path = std::env::temp_dir().join(format!(
            "shogun-exclusions-{}-{}.json",
            std::process::id(),
            EXCLUSIONS_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let apps = vec!["com.example.Secret".to_string()];
        let websites = vec!["https://secret.example/path".to_string()];
        assert!(replace_policy(&path, &apps, &websites).is_ok());
        let loaded = load_policy(&path);
        assert_eq!(loaded.user_apps(), vec!["com.example.Secret"]);
        assert_eq!(loaded.user_websites(), vec!["secret.example"]);
        let _ = std::fs::remove_file(path);
    }

    fn temp_store() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "shogun-exclusions-{}-{}.json",
            std::process::id(),
            EXCLUSIONS_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    #[test]
    fn a_replacement_that_only_adds_resumes_nothing() {
        // Issue #657: supersets of the current lists (after normalization) are tightening only.
        let path = temp_store();
        assert!(replace_policy(&path, &strings(&["com.example.A"]), &strings(&["a.example"])).is_ok());
        let plan = plan_replacement(
            &path,
            &strings(&["com.example.A", "com.example.B"]),
            &strings(&["https://A.example/path", "b.example"]),
        )
        .unwrap();
        assert!(!plan.resumes_capture(), "{plan:?}");
        assert_eq!(plan.apps, strings(&["com.example.A", "com.example.B"]));
        assert_eq!(plan.websites, strings(&["a.example", "b.example"]));
        // An API caller may write it directly.
        assert!(replace_policy_resuming_at_most(
            &path,
            &plan.apps,
            &plan.websites,
            &[],
            &[],
        )
        .is_ok());
        assert_eq!(load_policy(&path).user_websites(), vec!["a.example", "b.example"]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_replacement_that_drops_an_exclusion_names_it_and_is_refused_unapproved() {
        let path = temp_store();
        assert!(replace_policy(
            &path,
            &strings(&["com.example.Secret", "com.example.Keep"]),
            &strings(&["private.example"]),
        )
        .is_ok());
        // Adds one app and drops another plus the website: the whole request resumes capture.
        let apps = strings(&["com.example.Keep", "com.example.New"]);
        let plan = plan_replacement(&path, &apps, &[]).unwrap();
        assert_eq!(plan.resumed_apps, strings(&["com.example.Secret"]));
        assert_eq!(plan.resumed_websites, strings(&["private.example"]));
        // Direct (nothing approved): refused, and nothing on disk changes — not even the addition.
        assert_eq!(
            replace_policy_resuming_at_most(&path, &apps, &[], &[], &[]).err().as_deref(),
            Some(RESUME_NOT_APPROVED)
        );
        let on_disk = load_policy(&path);
        assert_eq!(on_disk.user_apps(), vec!["com.example.Keep", "com.example.Secret"]);
        assert_eq!(on_disk.user_websites(), vec!["private.example"]);
        // Approving only part of what it drops is not enough either.
        assert!(replace_policy_resuming_at_most(
            &path,
            &apps,
            &[],
            &plan.resumed_apps,
            &[],
        )
        .is_err());
        // With exactly what the card showed, it applies atomically.
        let applied = replace_policy_resuming_at_most(
            &path,
            &apps,
            &[],
            &plan.resumed_apps,
            &plan.resumed_websites,
        )
        .unwrap();
        assert_eq!(applied.user_apps(), vec!["com.example.Keep", "com.example.New"]);
        assert!(applied.user_websites().is_empty());
        assert_eq!(load_policy(&path).user_apps(), vec!["com.example.Keep", "com.example.New"]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn an_approval_goes_stale_when_a_new_exclusion_would_be_dropped() {
        // The user approved resuming Secret. Meanwhile they excluded Later in Settings. Applying
        // the old request now would also drop Later, which the card never named: refused.
        let path = temp_store();
        assert!(replace_policy(&path, &strings(&["com.example.Secret"]), &[]).is_ok());
        let plan = plan_replacement(&path, &[], &[]).unwrap();
        assert_eq!(plan.resumed_apps, strings(&["com.example.Secret"]));
        assert!(replace_policy(&path, &strings(&["com.example.Secret", "com.example.Later"]), &[]).is_ok());
        assert_eq!(
            replace_policy_resuming_at_most(&path, &plan.apps, &plan.websites, &plan.resumed_apps, &[])
                .err()
                .as_deref(),
            Some(RESUME_NOT_APPROVED)
        );
        assert_eq!(load_policy(&path).user_apps(), vec!["com.example.Later", "com.example.Secret"]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_corrupt_store_is_never_read_as_nothing_excluded() {
        let path = temp_store();
        std::fs::write(&path, "{not json").unwrap();
        assert!(plan_replacement(&path, &strings(&["com.example.A"]), &[]).is_err());
        assert!(replace_policy_resuming_at_most(&path, &strings(&["com.example.A"]), &[], &[], &[]).is_err());
        let _ = std::fs::remove_file(&path);
        // Missing is simply empty.
        assert!(!plan_replacement(&path, &strings(&["com.example.A"]), &[]).unwrap().resumes_capture());
    }

    #[test]
    fn default_exclusions_cannot_be_removed() {
        let mut p = ExclusionPolicy::new();
        assert!(
            !p.remove_app("com.1password.1password"),
            "a default must not be removable"
        );
        assert_eq!(
            p.is_excluded("com.1password.1password", None),
            Some(ExclusionReason::PasswordManager)
        );
    }

    #[test]
    fn user_title_pattern_matches_case_insensitively() {
        let mut p = ExclusionPolicy::new();
        p.add_title_pattern("Salary");
        assert_eq!(
            p.is_excluded("com.apple.Numbers", Some("2026 salary review.numbers")),
            Some(ExclusionReason::UserPattern)
        );
        assert_eq!(
            p.is_excluded("com.apple.Numbers", Some("budget.numbers")),
            None
        );
    }

    #[test]
    fn category_counts_report_the_live_default_tables() {
        let counts = ExclusionPolicy::new().category_counts();
        assert_eq!(
            counts,
            vec![
                ("password_managers", PASSWORD_MANAGERS.len()),
                ("auth_dialog", AUTH_AGENTS.len()),
                ("private_browsing", KNOWN_BROWSERS.len()),
            ],
            "onboarding must render the live policy, not a hardcoded list"
        );
    }

    #[test]
    fn default_categories_use_the_catalog_ids_onboarding_uses() {
        // The Full UI words these ids from the same catalog entries as onboarding, so an id here
        // that `category_counts` doesn't share would show up untranslated.
        let ids: Vec<&str> = default_categories().iter().map(|(id, _)| *id).collect();
        let onboarding: Vec<&str> = ExclusionPolicy::new()
            .category_counts()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(ids, onboarding);
    }

    #[test]
    fn category_counts_never_claim_a_title_exclusion() {
        // Onboarding shows these as built-in protections. No shipped path loads title patterns,
        // so they must never surface as a category, even when set in memory.
        let mut p = ExclusionPolicy::new();
        p.add_title_pattern("Salary");
        assert!(p
            .category_counts()
            .iter()
            .all(|(id, _)| !id.contains("title")));
    }

    #[test]
    fn preview_drops_background_windows_by_the_same_rules_as_capture() {
        let mut policy = ExclusionPolicy::new();
        policy.add_app("com.tinyspeck.slackmacgap");
        let drops = |p: &ExclusionPolicy, bundle: &str, name: &str, title: Option<&str>| {
            p.excludes_background_window(bundle, name, title)
        };
        assert!(drops(&policy, "com.syogun.shogunai", "ShogunAI", Some("Settings")));
        assert!(drops(&policy, "", "shogun-desktop-spike", None), "own process without a bundle id");
        assert!(drops(&policy, "com.1password.1password", "1Password", Some("Vault")));
        assert!(!drops(&policy, "com.apple.Terminal", "Terminal", Some("zsh")), "terminals are captured");
        assert!(drops(&policy, "com.apple.SecurityAgent", "SecurityAgent", None));
        assert!(drops(&policy, "com.tinyspeck.slackmacgap", "Slack", Some("general")));
        assert!(drops(&policy, "com.google.Chrome", "Chrome", Some("Docs (Incognito)")));
        assert!(drops(&policy, "com.google.Chrome", "Chrome", None), "no title, no private check");
        assert!(!drops(&policy, "com.google.Chrome", "Chrome", Some("Docs")));
        assert!(!drops(&policy, "com.apple.mail", "Mail", None));
        assert!(!drops(&policy, "com.apple.dock", "Dock", None));
        assert!(
            drops(&policy, "com.apple.notificationcenterui", "Notification Center", None),
            "a banner can show an excluded app's message"
        );

        assert!(policy.add_website("bank.example"));
        assert!(
            drops(&policy, "com.google.Chrome", "Chrome", Some("Docs")),
            "a background tab's URL cannot be verified while website rules exist"
        );
        assert!(!drops(&policy, "com.apple.mail", "Mail", Some("Inbox")));
    }
}
