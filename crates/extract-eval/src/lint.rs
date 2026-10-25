//! "No real data" lint over golden days and replay fixtures (#317 acceptance: the golden set
//! contains no real user data *by construction*).
//!
//! The construction is the generator (goldens must equal its output; `tests/gate.rs`). This lint
//! is the second line, for the text a human does write — templates, pools and replay fixtures:
//! contact details must be reserved example values and people must come from the fictional pools.

use crate::golden::{SynthDay, Table};
use crate::templates::PERSONAS;

/// What the lint objects to. Carries no text: a finding names the rule and the location only, so
/// a failure message can never echo the offending value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub rule: &'static str,
    pub location: String,
}

/// A chat handle from the fictional pools: the local part of a pool person's `.example` address
/// (`@saeki` for `saeki@koyomi.example`). Templates write it as the `{handle}` slot.
fn is_pool_handle(handle: &str) -> bool {
    PERSONAS.iter().flat_map(|d| d.people).any(|p| p.email.split('@').next() == Some(handle))
}

fn check_text(text: &str, location: &str, out: &mut Vec<Finding>) {
    // Email addresses: the domain must be an RFC 2606 `.example` name.
    for token in text.split(|c: char| c.is_whitespace() || "<>()[]{},;\"'「」、（）".contains(c)) {
        // A chat mention (`@saeki`) names a person, so it must be one from the pools.
        if let Some(handle) = token.strip_prefix('@') {
            let handle = handle.trim_end_matches(|c: char| !c.is_alphanumeric());
            if !is_pool_handle(handle) {
                out.push(Finding { rule: "handle not from the pool", location: location.to_string() });
            }
            continue;
        }
        if let Some((_, domain)) = token.split_once('@') {
            let domain = domain.trim_end_matches(|c: char| !c.is_alphanumeric());
            if !domain.ends_with(".example") {
                out.push(Finding { rule: "email outside .example", location: location.to_string() });
            }
        }
    }
    // URLs: only `.example` hosts.
    let lower = text.to_lowercase();
    for scheme in ["http://", "https://", "www."] {
        let mut rest = lower.as_str();
        while let Some(pos) = rest.find(scheme) {
            let tail = &rest[pos..];
            let end = tail.find(char::is_whitespace).unwrap_or(tail.len());
            if !tail[..end].contains(".example") {
                out.push(Finding { rule: "url outside .example", location: location.to_string() });
            }
            rest = &rest[pos + scheme.len()..];
        }
    }
    // Long digit runs look like phone, account or card numbers.
    let mut run = 0;
    for c in text.chars() {
        run = if c.is_ascii_digit() { run + 1 } else { 0 };
        if run >= 7 {
            out.push(Finding { rule: "digit run of 7+", location: location.to_string() });
            break;
        }
    }
}

/// Lint one golden day.
pub fn lint_day(day: &SynthDay) -> Vec<Finding> {
    let mut out = Vec::new();
    let p = &day.persona;
    for (field, v) in [("persona.name", &p.name), ("persona.org", &p.org), ("persona.role", &p.role)] {
        check_text(v, &format!("{}:{field}", day.id), &mut out);
    }
    let Some(def) = PERSONAS.iter().find(|d| d.id == p.id) else {
        out.push(Finding { rule: "persona not in the template pool", location: day.id.clone() });
        return out;
    };
    if def.name != p.name {
        out.push(Finding { rule: "persona name not from the template pool", location: day.id.clone() });
    }
    for e in &day.events {
        for (field, v) in [("app", &e.app), ("window", &e.window), ("text", &e.text)] {
            check_text(v, &format!("{}:event {}:{field}", day.id, e.id), &mut out);
        }
        if crate::templates::template(&e.template).is_none() {
            out.push(Finding { rule: "event from an unknown template", location: format!("{}:event {}", day.id, e.id) });
        }
    }
    for g in &day.gold {
        for (k, v) in &g.fields {
            check_text(v, &format!("{}:{}:{k}", day.id, g.id), &mut out);
        }
        if g.table == Table::People {
            let name = g.fields.get("name").map(String::as_str).unwrap_or("");
            if !def.people.iter().any(|pp| pp.name == name) {
                out.push(Finding { rule: "gold person not from the pool", location: format!("{}:{}", day.id, g.id) });
            }
        }
        if g.table == Table::Projects {
            let name = g.fields.get("name").map(String::as_str).unwrap_or("");
            if !def.projects.contains(&name) {
                out.push(Finding { rule: "gold project not from the pool", location: format!("{}:{}", day.id, g.id) });
            }
        }
        for ev in &g.evidence {
            if day.event(*ev).is_none() {
                out.push(Finding { rule: "gold evidence cites a missing event", location: format!("{}:{}", day.id, g.id) });
            }
        }
    }
    out
}

/// Lint one replay fixture against its day: only the day's event ids, only example contact data,
/// and every person entity from the persona's pool.
pub fn lint_replay(fixture: &crate::runners::ReplayFixture, day: &SynthDay) -> Vec<Finding> {
    let mut out = Vec::new();
    let pool: Vec<&str> = PERSONAS
        .iter()
        .find(|d| d.id == day.persona.id)
        .map(|d| d.people.iter().flat_map(|p| [p.name, p.short]).collect())
        .unwrap_or_default();
    for r in &fixture.results {
        let loc = format!("{}:result {}", fixture.day, r.custom_id);
        // A session id: `s` + 16 hex digits of the session key's hash — never event text.
        let id_shape = r.custom_id.len() == 17
            && r.custom_id.starts_with('s')
            && r.custom_id[1..].bytes().all(|b| b.is_ascii_hexdigit());
        if !id_shape {
            out.push(Finding { rule: "replay result id is not a session id", location: loc.clone() });
        }
        if r.events.is_empty() {
            out.push(Finding { rule: "replay result names no events", location: loc.clone() });
        }
        if r.events.iter().any(|id| day.event(*id).is_none()) {
            out.push(Finding { rule: "replay result for an event not in the day", location: loc.clone() });
        }
        // Anchors are 40-character prefixes of lines the golden day already linted, so a domain
        // or a digit run cut mid-way is not new data; everything else in the response is checked.
        match (&r.response, &r.raw_text) {
            (Some(response), _) => check_text(&without_anchors(response).to_string(), &loc, &mut out),
            (None, Some(raw)) => check_text(raw, &loc, &mut out),
            (None, None) => {}
        }
        // `line` is the `[n]` number of the supporting line: a positive integer or absent (the
        // one deliberately unresolvable anchor), never text.
        for array in ["commitments", "open_loops", "entities"] {
            let items = r
                .response
                .as_ref()
                .and_then(|v| v.get(array))
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default();
            for item in items {
                if let Some(line) = item.get("line") {
                    if !line.as_u64().is_some_and(|n| n >= 1) {
                        out.push(Finding { rule: "replay item line is not a positive integer", location: loc.clone() });
                    }
                }
            }
        }
        let entities = r
            .response
            .as_ref()
            .and_then(|v| v.get("entities"))
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        for e in entities {
            if e.get("kind").and_then(serde_json::Value::as_str) == Some("person") {
                let name = e.get("name").and_then(serde_json::Value::as_str).unwrap_or("");
                if !pool.contains(&name) {
                    out.push(Finding { rule: "replay person not from the pool", location: loc.clone() });
                }
            }
        }
    }
    out
}

/// `response` with every `anchor` field removed (see [`lint_replay`]).
fn without_anchors(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .filter(|(k, _)| k.as_str() != "anchor")
                .map(|(k, v)| (k.clone(), without_anchors(v)))
                .collect(),
        ),
        serde_json::Value::Array(items) => serde_json::Value::Array(items.iter().map(without_anchors).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn findings(text: &str) -> Vec<&'static str> {
        let mut out = Vec::new();
        check_text(text, "t", &mut out);
        out.into_iter().map(|f| f.rule).collect()
    }

    #[test]
    fn example_contacts_pass() {
        assert!(findings("To: Oren Vale <oren.vale@larkspur.example>").is_empty());
        assert!(findings("see https://wiki.larkspur.example/page").is_empty());
        assert!(findings("Pull request #412").is_empty());
    }

    #[test]
    fn real_looking_contacts_fail() {
        assert_eq!(findings("mail me at someone@gmail.com"), vec!["email outside .example"]);
        assert_eq!(findings("https://intranet.corp/secret"), vec!["url outside .example"]);
        assert_eq!(findings("call 0312345678"), vec!["digit run of 7+"]);
    }

    #[test]
    fn handles_must_come_from_the_pools() {
        assert!(findings("明日修正します @saeki").is_empty());
        assert!(findings("cc @oren.vale").is_empty(), "a dotted handle is a pool address local part");
        assert_eq!(findings("cc @somebody"), vec!["handle not from the pool"]);
    }
}
