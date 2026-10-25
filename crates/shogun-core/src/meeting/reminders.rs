//! Calendar notifications are independent of the recording lifecycle.
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Reminder {
    pub occurrence_id: i64,
    pub title: Option<String>,
    pub start_at: i64,
    pub end_at: i64,
    pub join_url: String,
}

/// Only known conference HTTPS links can leave the app. Reject credentials and alternate ports.
pub fn safe_join_url(value: &str) -> bool {
    normalized_join_url(value).is_some()
}

fn normalized_join_url(value: &str) -> Option<String> {
    let url = url::Url::parse(value).ok()?;
    (url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port_or_known_default() == Some(443)
        && super::detect::is_meeting_url(url.as_str()))
    .then(|| url.to_string())
}

#[derive(Default)]
pub struct Scheduler {
    active: Option<Reminder>,
    dismissed: BTreeSet<(i64, i64)>,
}

impl Scheduler {
    /// Recompute from the authoritative current rows, including after sleep. No timer catch-up
    /// replays an already started meeting; edits and cancellations replace the visible snapshot.
    pub fn tick(
        &mut self,
        now: i64,
        minutes: u8,
        connected: bool,
        events: Vec<Reminder>,
    ) -> Option<Reminder> {
        self.dismissed.retain(|(_, start)| *start > now);
        self.active = if connected && matches!(minutes, 1 | 5 | 10) {
            events
                .into_iter()
                .filter(|event| {
                    event.start_at > now
                        && event.start_at.saturating_sub(i64::from(minutes) * 60_000) <= now
                        && event.end_at > event.start_at
                        && !self
                            .dismissed
                            .contains(&(event.occurrence_id, event.start_at))
                })
                .filter_map(|mut event| {
                    event.join_url = normalized_join_url(&event.join_url)?;
                    Some(event)
                })
                .min_by_key(|event| (event.start_at, event.occurrence_id))
        } else {
            None
        };
        self.active.clone()
    }

    pub fn snapshot(&self) -> Option<Reminder> {
        self.active.clone()
    }

    /// The start time guards against an old webview action dismissing a rescheduled event.
    pub fn dismiss(&mut self, occurrence_id: i64, start_at: i64) -> bool {
        if self
            .active
            .as_ref()
            .is_some_and(|event| event.occurrence_id == occurrence_id && event.start_at == start_at)
        {
            self.dismissed.insert((occurrence_id, start_at));
            self.active = None;
            true
        } else {
            false
        }
    }
}

#[cfg(feature = "db")]
pub fn from_occurrence(event: shogun_memory::calendar::Occurrence) -> Option<Reminder> {
    if event.cancelled
        || event.all_day
        || event.end_at <= event.start_at
        || event.attendees.iter().any(|attendee| {
            attendee.is_self
                && attendee
                    .response
                    .as_deref()
                    .is_some_and(|response| response.eq_ignore_ascii_case("declined"))
        })
    {
        return None;
    }
    let join_url = normalized_join_url(event.location.as_deref()?)?;
    Some(Reminder {
        occurrence_id: event.id,
        title: event.title,
        start_at: event.start_at,
        end_at: event.end_at,
        join_url,
    })
}
