//! Recipient scopes from explicit compose labels already captured for a rewrite.
//! Never infer recipients from draft text, mentioned names, or historical memory.

/// An opaque local key, not an instruction or a canonical People identity. Unknown layouts,
/// groups and ambiguous labels return None so callers retain unscoped app behavior.
pub fn from_field(_app: &str, label: &str, _window_title: Option<&str>) -> Option<String> {
    let label = label.trim();
    if label.len() > 256 || label.chars().any(char::is_control) {
        return None;
    }
    let lower = label.to_lowercase();
    for prefix in ["to: ", "reply to "] {
        if lower.starts_with(prefix) {
            let recipient = label.get(prefix.len()..)?.trim();
            if recipient.contains([',', ';']) {
                return None;
            }
            let address = if let Some((_, rest)) = recipient.split_once('<') {
                rest.strip_suffix('>')?
            } else {
                recipient
            };
            let (local, domain) = address.split_once('@')?;
            if local.is_empty()
                || !domain.contains('.')
                || domain.starts_with('.')
                || domain.ends_with('.')
                || domain.contains('@')
                || !address
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "@.!#$%&'*+-/=?^_`{|}~".contains(c))
            {
                return None;
            }
            return Some(format!("email:{}", address.to_ascii_lowercase()));
        }
    }
    // ponytail: display names collide, even within one workspace. Add chat scoping only
    // when captured metadata provides a stable recipient ID rather than a title.
    None
}

/// Resolve first-person lookup language using the name the user explicitly saved. This only
/// expands a search; it never establishes that a retrieved same-name person is the owner.
pub fn personalize_query(query: &str, owner: Option<&str>) -> String {
    let personal = query.split_whitespace().any(|word| {
        matches!(
            word.trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
                .as_str(),
            "my" | "me" | "myself"
        )
    });
    match owner
        .map(str::trim)
        .filter(|name| !name.is_empty() && name.len() <= 160)
    {
        Some(name) if personal => format!("{query} {name}"),
        _ => query.to_string(),
    }
}

/// The rewrite-loop partner key (#1066, owner OK 2026-10-12): an `email:` key from the compose
/// label, else a `thread:` key from the chat title / DM name in the window title. Looser than
/// [`from_field`] on purpose: it only scopes the user's own learned history and a prompt hint, and
/// never reaches the writing-style examples or any identity claim.
pub fn partner_key(app: &str, label: &str, window_title: Option<&str>) -> Option<String> {
    if let Some(key) = from_field(app, label, window_title) {
        return Some(key);
    }
    let title = normalize_title(window_title?);
    let title = title.as_str();
    if title.len() < 2 || title.len() > 100 || title.chars().any(char::is_control) {
        return None;
    }
    Some(format!("thread:{title}"))
}

/// Window title -> stable conversation name: drops the "(n)" unread badge and trailing app /
/// browser / profile fragments so one conversation is one key. Lowercased.
fn normalize_title(raw: &str) -> String {
    let mut t = raw.trim().to_string();
    // "(2) " / "(12) " leading unread badge
    if let Some(rest) = t.strip_prefix('(') {
        if let Some((n, tail)) = rest.split_once(')') {
            if !n.is_empty() && n.len() <= 4 && n.chars().all(|c| c.is_ascii_digit()) {
                t = tail.trim_start().to_string();
            }
        }
    }
    // Repeatedly strip trailing " - X" / " – X" / " — X" / " | X" where X is a known app/browser
    // or (right after a browser) a profile name.
    const APPS: [&str; 14] = [
        "google chrome", "chrome", "brave", "brave browser", "mozilla firefox", "firefox", "safari",
        "microsoft edge", "arc", "slack", "discord", "whatsapp", "messages", "telegram",
    ];
    loop {
        let lower = t.to_lowercase();
        let mut cut = None;
        for sep in [" - ", " – ", " — ", " | "] {
            if let Some(i) = lower.rfind(sep) {
                let tail = lower[i + sep.len()..].trim();
                let is_app = APPS.contains(&tail);
                // "<title> - Google Chrome – profile": tail after the browser is the profile.
                let after_browser = APPS.iter().any(|a| {
                    lower[..i].ends_with(&format!(" - {a}"))
                        || lower[..i].ends_with(&format!(" – {a}"))
                        || lower[..i].ends_with(&format!(" — {a}"))
                });
                if (is_app || after_browser) && i > 0 {
                    cut = Some(i);
                    break;
                }
            }
        }
        match cut {
            Some(i) => t.truncate(i),
            None => break,
        }
    }
    t.trim().to_lowercase()
}

/// The human-readable half of a partner key, for the prompt ("alice@x.com", "Alice Ng").
pub fn partner_label(key: &str) -> &str {
    key.strip_prefix("email:").or_else(|| key.strip_prefix("thread:")).unwrap_or(key)
}

#[cfg(test)]
mod tests {
    use super::from_field;

    #[test]
    fn partner_key_falls_back_to_the_thread_title() {
        use super::{partner_key, partner_label};
        assert_eq!(partner_key("a", "To: bob@x.com", None).as_deref(), Some("email:bob@x.com"));
        assert_eq!(partner_key("a", "Message", Some("Alice Ng - Slack")).as_deref(), Some("thread:alice ng"));
        assert_eq!(partner_key("a", "", None), None);
        assert_eq!(partner_label("thread:alice ng"), "alice ng");
        assert_eq!(partner_key("a", "", Some("(2) Honmaru AI - Google Chrome – Anant")).as_deref(), Some("thread:honmaru ai"));
        assert_eq!(partner_key("a", "", Some("Honmaru AI - Brave")).as_deref(), Some("thread:honmaru ai"));
        assert_eq!(partner_key("a", "", Some("Honmaru AI — Mozilla Firefox")).as_deref(), Some("thread:honmaru ai"));
        // an old stored key's title re-keys to the same partner (rewrite_rekey_partners)
        assert_eq!(partner_key("", "", Some("(2) honmaru ai - google chrome – anant")).as_deref(), Some("thread:honmaru ai"));
    }

    #[test]
    fn personal_queries_use_only_an_explicit_owner_name() {
        assert_eq!(
            super::personalize_query("write about my education", Some("Alice Ng")),
            "write about my education Alice Ng"
        );
        assert_eq!(
            super::personalize_query("write about my education", None),
            "write about my education"
        );
        assert_eq!(
            super::personalize_query("reply to Bob", Some("Alice Ng")),
            "reply to Bob"
        );
    }

    #[test]
    fn explicit_single_email_is_stable_and_group_or_topic_is_not_a_person() {
        assert_eq!(
            from_field("mail", "To: Sam <SAM@example.com>", None).as_deref(),
            Some("email:sam@example.com")
        );
        assert_eq!(
            from_field("mail", "Reply to sam@example.com", None).as_deref(),
            Some("email:sam@example.com")
        );
        for label in [
            "Reply to launch thread",
            "To: a@example.com, b@example.com",
            "To: a@@example.com",
            "To: Sam",
            "Message",
        ] {
            assert!(from_field("mail", label, None).is_none());
        }
    }

    #[test]
    fn display_names_never_establish_a_unique_chat_recipient() {
        let app = "com.tinyspeck.slackmacgap";
        let first = from_field(app, "Message @Sam", Some("Sam - Acme - Slack"));
        assert!(first.is_none());
        assert!(from_field(app, "Message @Sam", Some("Sam - Other - Slack")).is_none());
        for (label, title) in [
            ("Message #general", "#general - Acme - Slack"),
            ("Message Sam, Jo", "Sam, Jo - Acme - Slack"),
            ("Message Sam", "Jo - Acme - Slack"),
            ("Message Sam", "Slack"),
        ] {
            assert!(from_field(app, label, Some(title)).is_none());
        }
        assert!(from_field("browser", "Message Sam", Some("Sam - Acme - Slack")).is_none());
    }
}
