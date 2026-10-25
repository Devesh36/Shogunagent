//! The scenario format: one focused screen, the memory rows that exist at that moment, and what a
//! good action row would hold. Every scenario is synthetic (example.com / example.co.jp addresses,
//! invented names) and checked in under `scenarios/`.

use serde::Deserialize;

/// The fixed "now" every scenario is evaluated at. Due times are relative to it.
pub const NOW_MS: i64 = 1_790_000_000_000;
const HOUR_MS: f64 = 3_600_000.0;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub id: String,
    pub lang: String,
    pub about: String,
    /// Written after the matcher, to probe its known weak spots (same first name, compound
    /// splits, paraphrase). Reported separately and not part of the gate floors.
    #[serde(default)]
    pub hard: bool,
    pub screen: Screen,
    #[serde(default)]
    pub people: Vec<Person>,
    #[serde(default)]
    pub projects: Vec<Project>,
    #[serde(default)]
    pub commitments: Vec<Commitment>,
    #[serde(default)]
    pub open_loops: Vec<OpenLoop>,
    pub expect: Expect,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Screen {
    pub bundle_id: String,
    pub title: String,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub excerpt: String,
    #[serde(default)]
    pub selection: Option<String>,
    #[serde(default)]
    pub reply_language: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Person {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub emails: Vec<String>,
    pub confidence: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    pub id: String,
    pub name: String,
    #[serde(default = "active")]
    pub status: String,
    pub confidence: f64,
}

fn active() -> String {
    "active".into()
}

fn open() -> String {
    "open".into()
}

fn mine() -> String {
    "mine".into()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Commitment {
    pub id: String,
    pub description: String,
    #[serde(default = "mine")]
    pub direction: String,
    #[serde(default)]
    pub counterparty: Option<String>,
    #[serde(default)]
    pub due_in_hours: Option<f64>,
    #[serde(default = "open")]
    pub status: String,
    pub confidence: f64,
    #[serde(default)]
    pub snoozed_hours: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenLoop {
    pub id: String,
    pub kind: String,
    pub description: String,
    #[serde(default)]
    pub counterparty: Option<String>,
    pub confidence: f64,
    #[serde(default)]
    pub inferred: bool,
    #[serde(default)]
    pub snoozed_hours: Option<f64>,
}

/// What a good row holds. Targets are row ids, `reply_draft` (the FR-TL-10 screen candidate) or
/// `generic` (the FR-CF-04 search seeded from the screen).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    /// Buttons a person on this screen would want. Order does not matter except for `first`.
    pub relevant: Vec<String>,
    /// Buttons that are fine to show but earn nothing (a real but unrelated deadline, a weaker
    /// related item). Never counted as irrelevant.
    #[serde(default)]
    pub acceptable: Vec<String>,
    /// The button that should lead the row, when one clearly should.
    #[serde(default)]
    pub first: Option<String>,
    /// Nothing in memory belongs on this screen: the row should be the generic one.
    #[serde(default)]
    pub generic: bool,
}

impl Scenario {
    /// An absolute instant `hours` from [`NOW_MS`].
    pub fn at(hours: f64) -> i64 {
        NOW_MS + (hours * HOUR_MS) as i64
    }

    pub fn person(&self, id: &str) -> Option<&Person> {
        self.people.iter().find(|p| p.id == id)
    }
}

/// Every checked-in scenario, English first.
pub const SCENARIO_FILES: &[(&str, &str)] = &[
    ("en", include_str!("../scenarios/en.json")),
    ("ja", include_str!("../scenarios/ja.json")),
    ("hard", include_str!("../scenarios/hard.json")),
];

pub fn all() -> Result<Vec<Scenario>, String> {
    let mut out = Vec::new();
    for (name, json) in SCENARIO_FILES {
        let mut set: Vec<Scenario> =
            serde_json::from_str(json).map_err(|e| format!("scenarios/{name}.json: {e}"))?;
        out.append(&mut set);
    }
    Ok(out)
}
