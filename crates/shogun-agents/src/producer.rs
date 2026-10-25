//! The send producer (§6.6.3): turn a preset agent's drafted proposal into an L3 entry on the
//! approval queue. This is the piece that connects an agent (Reply Drafter, Calendar Scheduler,
//! Issue Triage, Note Capture) to the confirmed-send execution path — the agent drafts, the human
//! confirms (FR-AG-03), the executor sends.
//!
//! Pure: it builds the [`SendAction`] + [`Preview`] and enqueues. The actual body drafting (BYOK
//! LLM) and execution are the effectful layers around it. Centralizing the action/preview
//! construction here means the routing (every send, Gmail included, over the user's own direct
//! connection, #713) and the Gmail "Subject: …\n\n…" body shape are defined once.

use crate::approval::{ApprovalId, ApprovalOrigin, ApprovalQueue, Preview, Route};
use crate::permission::SendAction;

/// A drafted, ready-to-confirm send produced by an agent. The body is already generated (by the
/// BYOK LLM upstream); this type only carries the addressing + content needed to enqueue it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProposedSend {
    /// Gmail reply/compose — over the user's own direct Gmail connection (#713).
    Email { to: String, subject: String, body: String },
    /// A Slack channel/thread post (first layer, L3).
    SlackPost { channel: String, body: String },
    /// A Google Calendar event (first layer, L3; irreversible → invariant 4).
    CalendarEvent { title: String, body: String },
    /// A GitHub/Linear issue comment (first layer, L3).
    IssueComment { target: String, body: String },
}

impl ProposedSend {
    /// The [`SendAction`], the full preview body (the exact text the executor will send), and the
    /// route. Email uses the `Subject: …\n\n…` shape the Gmail executor splits back
    /// ([`crate::approval`] / shogun-mcp `parse_gmail_full_body`). Every route is
    /// [`Route::DirectMcp`]: the confirm path refuses any other ([`Route::executable`]), so a
    /// proposal stamped for the retired Composio mail connection could never be confirmed.
    pub fn parts(&self) -> (SendAction, String, Route) {
        match self {
            ProposedSend::Email { to, subject, body } => (
                SendAction::SendEmail { to: to.clone() },
                // The subject must never contain a newline: the executor splits this shape back
                // on the FIRST blank line, so an embedded "\n\n" would make the approved preview
                // and the sent mail disagree (part of the subject would become body).
                format!("Subject: {}\n\n{body}", subject.replace(['\r', '\n'], " ")),
                Route::DirectMcp,
            ),
            ProposedSend::SlackPost { channel, body } => {
                (SendAction::PostMessage { channel: channel.clone() }, body.clone(), Route::DirectMcp)
            }
            ProposedSend::CalendarEvent { title, body } => {
                (SendAction::CreateCalendarEvent { title: title.clone() }, body.clone(), Route::DirectMcp)
            }
            ProposedSend::IssueComment { target, body } => {
                (SendAction::PostComment { target: target.clone() }, body.clone(), Route::DirectMcp)
            }
        }
    }
}

/// Whether `to` can be an email recipient: one `@` with a non-empty local part and a dotted
/// domain, and no spaces, angle brackets, quotes or list separators. Not an RFC 5322 parser. Its
/// job is to refuse what has reached this point before: a person's name, or an open loop's
/// description, which became an L3 card that could never send (#764).
pub fn is_email_address(to: &str) -> bool {
    let to = to.trim();
    let Some((local, domain)) = to.split_once('@') else {
        return false;
    };
    if local.is_empty()
        || domain.contains('@')
        || to.chars().any(|c| {
            c.is_whitespace()
                || c.is_control()
                || matches!(c, '<' | '>' | ',' | ';' | '"' | '(' | ')' | '[' | ']' | '\\')
        })
    {
        return false;
    }
    let labels: Vec<&str> = domain.split('.').collect();
    labels.len() >= 2
        && labels
            .iter()
            .all(|label| !label.is_empty() && !label.starts_with('-') && !label.ends_with('-'))
}

/// Enqueue an agent-drafted send for L3 confirmation. Returns the pending [`ApprovalId`]; the send
/// runs only after a dedicated-button confirm (the existing confirm → execute path). `Err` on
/// approval-id exhaustion, and for an email whose recipient is not an address — refused rather
/// than panicking, since callers hold the shared queue lock (a panic there would poison it for
/// the whole app).
pub fn propose(
    queue: &mut ApprovalQueue,
    proposal: &ProposedSend,
    origin: ApprovalOrigin,
    now_ms: u64,
) -> Result<ApprovalId, &'static str> {
    if let ProposedSend::Email { to, .. } = proposal {
        if !is_email_address(to) {
            return Err("the recipient is not an email address");
        }
    }
    let (action, full_body, route) = proposal.parts();
    let preview = Preview::for_send(&action, full_body, route);
    queue.try_request(action, preview, origin, now_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::{ConfirmIntent, Decision};

    #[test]
    fn email_proposal_routes_direct_with_subject_body_shape() {
        let p = ProposedSend::Email {
            to: "bob@example.com".into(),
            subject: "Ship date".into(),
            body: "Friday works.".into(),
        };
        let (action, full, route) = p.parts();
        assert!(matches!(action, SendAction::SendEmail { .. }));
        assert_eq!(route, Route::DirectMcp);
        // The confirm preview names the user's own connection, not a third party.
        let preview = Preview::for_send(&action, full.clone(), route);
        assert_eq!(preview.key_kind, crate::approval::KeyKind::OauthUser);
        assert_eq!(full, "Subject: Ship date\n\nFriday works.");
    }

    #[test]
    fn only_an_address_can_be_an_email_recipient() {
        for ok in ["bob@example.com", " sarah@client.example ", "a.b+tag@mail.co.uk", "x@sub.domain.io"] {
            assert!(is_email_address(ok), "{ok}");
        }
        for bad in [
            "",
            "Sarah",
            "Reply to Sarah about the Q3 budget",
            "possibly: Reply to Sarah",
            "@example.com",
            "bob@",
            "bob@localhost",
            "bob@@example.com",
            "bob@example..com",
            "bob@-example.com",
            "Bob <bob@example.com>",
            "bob@example.com, eve@example.com",
            "bob smith@example.com",
        ] {
            assert!(!is_email_address(bad), "{bad:?}");
        }
    }

    #[test]
    fn an_email_to_a_non_address_never_reaches_the_queue() {
        // #764: the notch put an open loop's description in `to`; the card could never send.
        let mut q = ApprovalQueue::new();
        let p = ProposedSend::Email {
            to: "Reply to Sarah about the Q3 budget".into(),
            subject: "Q3 budget".into(),
            body: "Hi Sarah".into(),
        };
        assert_eq!(propose(&mut q, &p, ApprovalOrigin::Ui, 0), Err("the recipient is not an email address"));
        assert_eq!(q.pending_len(), 0);
    }

    #[test]
    fn a_newline_in_the_subject_cannot_shift_content_into_the_body() {
        // The executor splits "Subject: …\n\n…" on the first blank line. An LLM- or
        // capture-sourced subject containing "\n\n" must not smuggle text past what the
        // human approved as the subject line.
        let p = ProposedSend::Email {
            to: "bob@example.com".into(),
            subject: "Renewal\n\nplease don't quote me".into(),
            body: "Hi Dave".into(),
        };
        let (_, full, _) = p.parts();
        let (before, after) = full.split_once("\n\n").expect("shape");
        assert_eq!(before, "Subject: Renewal  please don't quote me");
        assert_eq!(after, "Hi Dave");
    }

    #[test]
    fn non_email_proposals_route_direct() {
        for (p, dest) in [
            (ProposedSend::SlackPost { channel: "#g".into(), body: "hi".into() }, "#g"),
            (ProposedSend::CalendarEvent { title: "Sync".into(), body: "agenda".into() }, "Sync"),
            (ProposedSend::IssueComment { target: "pr#12".into(), body: "lgtm".into() }, "pr#12"),
        ] {
            let (action, full, route) = p.parts();
            assert_eq!(route, Route::DirectMcp);
            assert_eq!(Preview::for_send(&action, full, route).destination, dest);
        }
    }

    /// #1005: the confirm path refuses the retired Composio route, so every kind of proposal must
    /// come out of the L3 confirm with a route it will execute.
    #[test]
    fn every_fresh_proposal_passes_the_confirm_route_check() {
        for p in [
            ProposedSend::Email { to: "bob@example.com".into(), subject: "Ship".into(), body: "Friday".into() },
            ProposedSend::SlackPost { channel: "C0123ABCD".into(), body: "hi".into() },
            ProposedSend::IssueComment {
                target: "https://github.com/owner/repo/pull/12".into(),
                body: "lgtm".into(),
            },
        ] {
            // (A calendar event is refused at the queue for now; its route is pinned above.)
            let mut q = ApprovalQueue::new();
            let id = propose(&mut q, &p, ApprovalOrigin::Ui, 0).expect("enqueue");
            // Still L3: the Enter key alone confirms nothing.
            assert!(matches!(
                q.confirm(id, ConfirmIntent::EnterKey, 1),
                Decision::RequiresDedicatedButton
            ));
            match q.confirm(id, ConfirmIntent::DedicatedButton, 2) {
                Decision::Confirmed(cs) => assert!(cs.preview.route.executable(), "{p:?}"),
                other => panic!("expected Confirmed, got {other:?}"),
            }
        }
        assert!(!Route::ViaComposio.executable());
    }

    #[test]
    fn propose_enqueues_an_l3_send_the_confirm_path_can_run() {
        let mut q = ApprovalQueue::new();
        let p = ProposedSend::SlackPost { channel: "C0123ABCD".into(), body: "agenda".into() };
        let id = propose(&mut q, &p, ApprovalOrigin::Ui, 0).expect("enqueue");
        assert_eq!(q.pending_len(), 1);
        // it is a normal L3 entry: a dedicated-button confirm yields the ConfirmedSend to execute.
        match q.confirm(id, ConfirmIntent::DedicatedButton, 1000) {
            Decision::Confirmed(cs) => {
                assert!(matches!(cs.action, SendAction::PostMessage { .. }));
                assert_eq!(cs.preview.full_body, "agenda");
            }
            other => panic!("expected Confirmed, got {other:?}"),
        }
    }

    #[test]
    fn a_send_that_could_never_run_is_not_queued() {
        let mut q = ApprovalQueue::new();
        for p in [
            ProposedSend::SlackPost { channel: "#general".into(), body: "hi".into() },
            ProposedSend::CalendarEvent { title: "Sync".into(), body: "agenda".into() },
            ProposedSend::IssueComment { target: "pr#12".into(), body: "lgtm".into() },
        ] {
            assert!(propose(&mut q, &p, ApprovalOrigin::Ui, 0).is_err());
        }
        assert_eq!(q.pending_len(), 0);
    }
}
