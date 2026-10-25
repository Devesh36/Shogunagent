//! What kind of surface the focused window is (mail, chat, calendar, …), from the app's bundle id
//! and — for a browser — the verified website host. Pure lookups over static tables: no content
//! is read, so it is safe on the 150ms action path and in prompt building alike.

/// The broad kind of the focused surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    Mail,
    /// Team chat (Slack, Teams, Discord, …).
    Chat,
    /// Personal messaging (Messages, WhatsApp, LINE, Telegram, …).
    Messaging,
    Calendar,
    /// Pull requests and code review (GitHub, GitLab).
    CodeReview,
    /// Issue trackers (Linear, Jira).
    IssueTracker,
    /// Documents and notes.
    Docs,
    /// Code editors and terminals.
    Code,
    /// A browser on a site this table does not know.
    Browser,
    Other,
}

/// Browsers whose window titles end with the browser or site name.
const BROWSERS: &[&str] = &[
    "com.google.chrome",
    "com.google.chrome.canary",
    "com.apple.safari",
    "com.apple.safaritechnologypreview",
    "org.mozilla.firefox",
    "com.microsoft.edgemac",
    "company.thebrowser.browser",
    "com.brave.browser",
    "com.operasoftware.opera",
    "com.vivaldi.vivaldi",
];

/// Known native apps by bundle id (lowercased).
const APPS: &[(&str, Surface)] = &[
    ("com.apple.mail", Surface::Mail),
    ("com.microsoft.outlook", Surface::Mail),
    ("com.readdle.smartemail-mac", Surface::Mail),
    ("com.superhuman.electron", Surface::Mail),
    ("com.tinyspeck.slackmacgap", Surface::Chat),
    ("com.microsoft.teams", Surface::Chat),
    ("com.microsoft.teams2", Surface::Chat),
    ("com.hnc.discord", Surface::Chat),
    ("com.apple.mobilesms", Surface::Messaging),
    ("net.whatsapp.whatsapp", Surface::Messaging),
    ("desktop.whatsapp", Surface::Messaging),
    ("ru.keepcoder.telegram", Surface::Messaging),
    ("jp.naver.line.mac", Surface::Messaging),
    ("org.whispersystems.signal-desktop", Surface::Messaging),
    ("com.apple.ical", Surface::Calendar),
    ("com.flexibits.fantastical2.mac", Surface::Calendar),
    ("com.linear", Surface::IssueTracker),
    ("notion.id", Surface::Docs),
    ("md.obsidian", Surface::Docs),
    ("com.apple.notes", Surface::Docs),
    ("com.apple.iwork.pages", Surface::Docs),
    ("com.microsoft.word", Surface::Docs),
    ("com.apple.dt.xcode", Surface::Code),
    ("com.microsoft.vscode", Surface::Code),
    ("com.todesktop.230313mzl4w4u92", Surface::Code),
    ("dev.zed.zed", Surface::Code),
    ("com.apple.terminal", Surface::Code),
    ("com.googlecode.iterm2", Surface::Code),
];

/// Web apps by host. A host matches an entry exactly or as a subdomain of it.
const HOSTS: &[(&str, Surface)] = &[
    ("mail.google.com", Surface::Mail),
    ("outlook.live.com", Surface::Mail),
    ("outlook.office.com", Surface::Mail),
    ("outlook.office365.com", Surface::Mail),
    ("mail.yahoo.com", Surface::Mail),
    ("app.fastmail.com", Surface::Mail),
    ("mail.proton.me", Surface::Mail),
    ("app.slack.com", Surface::Chat),
    ("teams.microsoft.com", Surface::Chat),
    ("teams.live.com", Surface::Chat),
    ("discord.com", Surface::Chat),
    ("chat.google.com", Surface::Chat),
    ("web.whatsapp.com", Surface::Messaging),
    ("web.telegram.org", Surface::Messaging),
    ("messages.google.com", Surface::Messaging),
    ("calendar.google.com", Surface::Calendar),
    ("github.com", Surface::CodeReview),
    ("gitlab.com", Surface::CodeReview),
    ("linear.app", Surface::IssueTracker),
    ("atlassian.net", Surface::IssueTracker),
    ("docs.google.com", Surface::Docs),
    ("notion.so", Surface::Docs),
    ("www.notion.so", Surface::Docs),
];

/// Whether the bundle id is a browser.
pub fn is_browser(bundle_id: &str) -> bool {
    let b = bundle_id.trim().to_ascii_lowercase();
    BROWSERS.iter().any(|x| *x == b)
}

/// The surface of a web host, when it is a known web app.
pub fn surface_of_host(host: &str) -> Option<Surface> {
    let h = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if h.is_empty() {
        return None;
    }
    HOSTS
        .iter()
        .find(|(known, _)| h == *known || h.ends_with(&format!(".{known}")))
        .map(|(_, s)| *s)
}

/// The surface for a focused window: a known web app by host first (a browser on Gmail is mail),
/// then the native app by bundle id, then "a browser" or "other".
pub fn surface_of(bundle_id: &str, host: Option<&str>) -> Surface {
    if let Some(s) = host.and_then(surface_of_host) {
        return s;
    }
    let b = bundle_id.trim().to_ascii_lowercase();
    if let Some((_, s)) = APPS.iter().find(|(known, _)| *known == b) {
        return *s;
    }
    if b.starts_with("com.jetbrains.") {
        return Surface::Code;
    }
    if is_browser(&b) {
        return Surface::Browser;
    }
    Surface::Other
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_apps_are_known_by_host_inside_any_browser() {
        assert_eq!(surface_of("com.google.Chrome", Some("mail.google.com")), Surface::Mail);
        assert_eq!(surface_of("com.apple.Safari", Some("app.slack.com")), Surface::Chat);
        assert_eq!(surface_of("com.google.Chrome", Some("github.com")), Surface::CodeReview);
        assert_eq!(surface_of("com.google.Chrome", Some("linear.app")), Surface::IssueTracker);
        assert_eq!(surface_of("com.google.Chrome", Some("calendar.google.com")), Surface::Calendar);
        assert_eq!(surface_of("com.google.Chrome", Some("example.atlassian.net")), Surface::IssueTracker);
        // A lookalike is not a subdomain.
        assert_eq!(surface_of("com.google.Chrome", Some("notgithub.com")), Surface::Browser);
        assert_eq!(surface_of("com.google.Chrome", None), Surface::Browser);
    }

    #[test]
    fn native_apps_are_known_by_bundle_id() {
        assert_eq!(surface_of("com.apple.MobileSMS", None), Surface::Messaging);
        assert_eq!(surface_of("com.tinyspeck.slackmacgap", None), Surface::Chat);
        assert_eq!(surface_of("com.apple.dt.Xcode", None), Surface::Code);
        assert_eq!(surface_of("com.jetbrains.intellij", None), Surface::Code);
        assert_eq!(surface_of("com.linear", None), Surface::IssueTracker);
        assert_eq!(surface_of("com.example.unknown", None), Surface::Other);
    }
}
