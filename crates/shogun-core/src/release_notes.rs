//! What's new in this build (issue #67).
//!
//! The owner's decision (#67, 2026-09-23): *「アプリに何がアップデートされたかを表示する。起動時に
//! モーダルで出して、何が新しいかを出せるようにする。1クリックで消せるようにする」* — after an
//! update, the app says what changed, once, and one click puts it away forever.
//!
//! **The notes ship inside the build.** `release-notes.json` is compiled in, so the modal needs no
//! network at launch. That is not only a privacy position (invariant 3 — nothing leaves the device
//! to render this) but a correctness one: the notes for version X describe *the binary the user is
//! now running*, and a fetched feed can disagree with it. It also means the release notes are
//! written as part of the release, in the same commit as the code they describe.
//!
//! Everything here is pure — the catalogue, the version comparison, and the decision of whether a
//! launch owes the user a modal. The shell owns only the seen-stamp file and the window. That split
//! is what lets the rules below be tested on Linux, and it is why the same catalogue can answer the
//! MCP / REST / CLI faces (invariant 6) without a second copy of the data.

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

/// The catalogue as shipped. Authored before the tag; see the `_comment` in the file itself.
const CATALOG_JSON: &str = include_str!("../release-notes.json");

/// The most versions one modal will cover.
///
/// Someone who skipped four releases wants to know what changed, not to read a changelog standing
/// between them and their Mac. The newest three are the ones they can still act on; older entries
/// stay available on the release-notes page.
pub const MAX_VERSIONS_SHOWN: usize = 3;

/// One line of a release — a change named in the user's terms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseItem {
    pub title: String,
    pub detail: String,
}

/// One released version and what it brought.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseNote {
    /// "0.1.0" — the version stamped into the build this entry describes.
    pub version: String,
    /// ISO date, shown as the release's date rather than "3 days ago".
    pub date: String,
    /// One line for the top of the modal.
    pub headline: String,
    pub items: Vec<ReleaseItem>,
}

#[derive(Debug, Deserialize)]
struct Catalog {
    releases: Vec<ReleaseNote>,
}

/// Every shipped release note, newest first.
///
/// A catalogue that fails to parse yields an empty list rather than a panic: a malformed data file
/// must cost the user their modal, never their launch. The test below is what keeps the shipped
/// file from being that case.
pub fn catalog() -> &'static [ReleaseNote] {
    static PARSED: OnceLock<Vec<ReleaseNote>> = OnceLock::new();
    PARSED.get_or_init(|| {
        serde_json::from_str::<Catalog>(CATALOG_JSON)
            .map(|c| c.releases)
            .unwrap_or_default()
    })
}

/// The entry for one version, if it has one.
pub fn note_for(version: &str) -> Option<&'static ReleaseNote> {
    catalog().iter().find(|n| n.version == version)
}

/// A comparable version: `major.minor.patch`, with a pre-release suffix ranking *below* the
/// release it precedes (`0.2.0-beta.1` < `0.2.0`), as semver says it must.
///
/// Deliberately not a semver crate: this compares two strings the release pipeline itself
/// produces, and a dependency whose failure mode is "the update modal stopped appearing" is not
/// worth the supply chain.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Version {
    major: u64,
    minor: u64,
    patch: u64,
    /// 1 for a final release, 0 for a pre-release: a release orders *after* every pre-release of
    /// the same numbers, which `Option`'s own ordering would get backwards.
    release_rank: u8,
    /// Dot-separated pre-release identifiers, compared as semver §11 says: numeric ones
    /// numerically (`beta.10` > `beta.9`), and a numeric identifier below an alphanumeric one.
    pre: Vec<PreId>,
}

/// One pre-release identifier. Variant order is the precedence order: numeric first, so
/// `Num < Alpha` falls out of the derive.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum PreId {
    Num(u64),
    Alpha(String),
}

fn parse_pre(pre: &str) -> Vec<PreId> {
    pre.split('.')
        .filter(|p| !p.is_empty())
        .map(|p| match p.parse::<u64>() {
            Ok(n) if p.chars().all(|c| c.is_ascii_digit()) => PreId::Num(n),
            _ => PreId::Alpha(p.to_string()),
        })
        .collect()
}

fn parse_version(raw: &str) -> Option<Version> {
    let raw = raw.trim().trim_start_matches('v');
    // Build metadata ("+darwin") never affects precedence.
    let raw = raw.split('+').next().unwrap_or(raw);
    let (core, pre) = match raw.split_once('-') {
        Some((core, pre)) => (core, parse_pre(pre)),
        None => (raw, Vec::new()),
    };
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(Version {
        major,
        minor,
        patch,
        release_rank: u8::from(pre.is_empty()),
        pre,
    })
}

/// What this launch owes the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WhatsNew {
    /// Show these notes, newest first, and stamp the running version once dismissed.
    Show(Vec<&'static ReleaseNote>),
    /// Nothing to show, but record the running version as seen — so the *next* update is the
    /// first one this install can be told about. Covers a fresh install (nobody wants "what's
    /// new" in software they have never run), a downgrade, and an upgrade across versions that
    /// carry no notes.
    StampSilently,
    /// Already seen at this version. No file write, no window: the overwhelmingly common launch.
    UpToDate,
}

/// Decide what a launch of `current` owes someone whose last stamp was `seen`.
///
/// `seen` is `None` on the very first launch of an install that has never stamped — including
/// installs that predate this feature, which is why that case is silent rather than a modal
/// describing versions they have already been using.
pub fn decide(current: &str, seen: Option<&str>) -> WhatsNew {
    let Some(now) = parse_version(current) else {
        // An unparseable running version cannot be compared or stamped honestly. Do nothing:
        // whatever is on disk stays valid for the next launch that knows its own version.
        return WhatsNew::UpToDate;
    };
    let Some(seen) = seen.and_then(parse_version) else {
        return WhatsNew::StampSilently;
    };
    if seen >= now {
        // Equal is the ordinary launch; greater is a downgrade, where re-stamping keeps the file
        // describing the binary that actually ran.
        return if seen == now {
            WhatsNew::UpToDate
        } else {
            WhatsNew::StampSilently
        };
    }
    let notes = notes_between(catalog(), &seen, &now);
    if notes.is_empty() {
        WhatsNew::StampSilently
    } else {
        WhatsNew::Show(notes)
    }
}

/// The entries a user crossed: newer than what they last saw, no newer than what they are running.
///
/// The upper bound matters as much as the lower one. A build carries notes for versions released
/// after it whenever someone runs an older binary against a newer checkout, and announcing a
/// feature the running app does not have is worse than announcing nothing.
fn notes_between<'a>(
    catalog: &'a [ReleaseNote],
    seen: &Version,
    now: &Version,
) -> Vec<&'a ReleaseNote> {
    catalog
        .iter()
        .filter(|n| parse_version(&n.version).is_some_and(|v| v > *seen && v <= *now))
        .take(MAX_VERSIONS_SHOWN)
        .collect()
}

/// The catalogue as the API faces serve it (invariant 6: the same release notes the modal shows
/// are readable from MCP / REST / CLI, from this one source).
pub fn catalog_json() -> String {
    serde_json::json!({ "releases": catalog() }).to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        parse_version(s).unwrap()
    }

    #[test]
    fn the_shipped_catalog_parses_and_is_well_formed() {
        let notes = catalog();
        assert!(
            !notes.is_empty(),
            "release-notes.json must parse into at least one entry"
        );
        let mut previous: Option<Version> = None;
        let mut seen_versions: Vec<&str> = Vec::new();
        for note in notes {
            let parsed = parse_version(&note.version)
                .unwrap_or_else(|| panic!("version '{}' is not comparable", note.version));
            if let Some(prev) = &previous {
                assert!(
                    &parsed < prev,
                    "releases must be listed newest first: {} follows {}",
                    note.version,
                    prev.major
                );
            }
            assert!(
                !seen_versions.contains(&note.version.as_str()),
                "duplicate entry for {}",
                note.version
            );
            seen_versions.push(&note.version);
            assert!(!note.headline.trim().is_empty(), "{} has no headline", note.version);
            assert!(!note.items.is_empty(), "{} lists no changes", note.version);
            for item in &note.items {
                assert!(!item.title.trim().is_empty());
                assert!(!item.detail.trim().is_empty());
            }
            previous = Some(parsed);
        }
    }

    /// Brand rules (CLAUDE.md): user-facing copy names no competitor, no vendor and no piece of
    /// the stack. These notes are the most-read copy in the app after onboarding.
    #[test]
    fn shipped_copy_keeps_the_brand_rules() {
        const FORBIDDEN: [&str; 10] = [
            "Anthropic",
            "Claude",
            "OpenAI",
            "GPT",
            "Gemini",
            "Deepgram",
            "Tauri",
            "SQLite",
            "second brain",
            "AI-powered",
        ];
        for note in catalog() {
            let mut text = format!("{} {}", note.headline, note.date);
            for item in &note.items {
                text.push_str(&item.title);
                text.push(' ');
                text.push_str(&item.detail);
                text.push(' ');
            }
            for word in FORBIDDEN {
                assert!(
                    !text.to_lowercase().contains(&word.to_lowercase()),
                    "release note for {} mentions '{word}'",
                    note.version
                );
            }
        }
    }

    #[test]
    fn versions_compare_numerically_not_lexically() {
        assert!(v("0.10.0") > v("0.9.0"));
        assert!(v("1.0.0") > v("0.99.99"));
        assert!(v("0.1.2") > v("0.1.1"));
        assert_eq!(v("0.1.0"), v("v0.1.0"));
    }

    #[test]
    fn a_prerelease_ranks_below_the_release_it_precedes() {
        assert!(v("0.2.0-beta.1") < v("0.2.0"));
        assert!(v("0.2.0-beta.1") > v("0.1.9"));
        assert!(v("0.2.0+darwin") == v("0.2.0"));
    }

    #[test]
    fn prerelease_identifiers_compare_as_semver_says() {
        // Numeric identifiers numerically — the case a string compare gets backwards.
        assert!(v("0.2.0-beta.10") > v("0.2.0-beta.9"));
        assert!(v("0.2.0-beta.2") < v("0.2.0-beta.10"));
        // Numeric below alphanumeric; a longer identifier set wins when the prefix matches.
        assert!(v("0.2.0-1") < v("0.2.0-alpha"));
        assert!(v("0.2.0-alpha") < v("0.2.0-alpha.1"));
        assert!(v("0.2.0-alpha.1") < v("0.2.0-beta"));
        // And the decision that depends on it: beta.9 → beta.10 is an upgrade, not a downgrade.
        assert_ne!(decide("0.2.0-beta.10", Some("0.2.0-beta.9")), WhatsNew::UpToDate);
        assert!(!matches!(decide("0.2.0-beta.9", Some("0.2.0-beta.10")), WhatsNew::Show(_)));
    }

    #[test]
    fn garbage_versions_do_not_parse() {
        assert!(parse_version("").is_none());
        assert!(parse_version("1.2").is_none());
        assert!(parse_version("1.2.3.4").is_none());
        assert!(parse_version("latest").is_none());
    }

    #[test]
    fn a_fresh_install_is_stamped_without_a_modal() {
        assert_eq!(decide("0.1.0", None), WhatsNew::StampSilently);
    }

    #[test]
    fn the_ordinary_launch_writes_nothing() {
        assert_eq!(decide("0.1.0", Some("0.1.0")), WhatsNew::UpToDate);
    }

    #[test]
    fn a_dev_build_with_no_entry_is_silent() {
        // `tauri.conf.json` carries 0.0.0 until the tag stamps it, so every developer runs a
        // version the catalogue does not list. That must never open a window.
        assert_eq!(decide("0.0.0", Some("0.1.0")), WhatsNew::StampSilently);
    }

    #[test]
    fn an_upgrade_shows_the_shipped_note_for_the_version_crossed() {
        // Through the real decision, against the real catalogue: an install last stamped at
        // 0.0.1 and now running 0.1.0 is owed that release's entry.
        match decide("0.1.0", Some("0.0.1")) {
            WhatsNew::Show(notes) => {
                assert_eq!(notes.len(), 1);
                assert_eq!(notes[0].version, "0.1.0");
            }
            other => panic!("expected the 0.1.0 note, got {other:?}"),
        }
    }

    fn note(version: &str) -> ReleaseNote {
        ReleaseNote {
            version: version.into(),
            date: "2026-01-01".into(),
            headline: version.into(),
            items: vec![ReleaseItem {
                title: "t".into(),
                detail: "d".into(),
            }],
        }
    }

    #[test]
    fn only_the_versions_between_the_stamp_and_the_running_build_are_offered() {
        let catalog = vec![note("0.4.0"), note("0.3.0"), note("0.2.0"), note("0.1.0")];
        let picked = notes_between(&catalog, &v("0.1.0"), &v("0.3.0"));
        let versions: Vec<&str> = picked.iter().map(|n| n.version.as_str()).collect();
        // 0.4.0 is newer than the running build — it describes features this binary lacks.
        // 0.1.0 was already stamped.
        assert_eq!(versions, vec!["0.3.0", "0.2.0"]);
    }

    #[test]
    fn a_long_absence_is_capped_at_the_newest_few() {
        let catalog: Vec<ReleaseNote> = ["0.6.0", "0.5.0", "0.4.0", "0.3.0", "0.2.0"]
            .iter()
            .map(|v| note(v))
            .collect();
        let picked = notes_between(&catalog, &v("0.1.0"), &v("0.6.0"));
        assert_eq!(picked.len(), MAX_VERSIONS_SHOWN);
        assert_eq!(picked[0].version, "0.6.0");
    }

    #[test]
    fn a_downgrade_restamps_rather_than_announcing_the_past() {
        assert_eq!(decide("0.1.0", Some("0.9.0")), WhatsNew::StampSilently);
    }

    #[test]
    fn an_unstampable_running_version_changes_nothing_on_disk() {
        assert_eq!(decide("not-a-version", Some("0.1.0")), WhatsNew::UpToDate);
    }

    #[test]
    fn a_corrupt_stamp_is_treated_as_a_first_run() {
        assert_eq!(decide("0.1.0", Some("")), WhatsNew::StampSilently);
        assert_eq!(decide("0.1.0", Some("garbage")), WhatsNew::StampSilently);
    }

    #[test]
    fn the_api_face_serves_the_same_catalog_the_modal_reads() {
        let json: serde_json::Value = serde_json::from_str(&catalog_json()).unwrap();
        let releases = json["releases"].as_array().unwrap();
        assert_eq!(releases.len(), catalog().len());
        assert_eq!(releases[0]["version"], catalog()[0].version.as_str());
        assert!(releases[0]["items"].as_array().is_some_and(|i| !i.is_empty()));
    }

    #[test]
    fn note_for_finds_only_an_exact_version() {
        assert!(note_for(&catalog()[0].version).is_some());
        assert!(note_for("99.99.99").is_none());
    }
}
