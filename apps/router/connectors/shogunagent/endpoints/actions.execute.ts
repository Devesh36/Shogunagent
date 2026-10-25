import { defineEndpoint, z, zodInput } from "../../define.js";

/**
 * The approval-requiring write example (E8-T8). Sends, posts and calendar creation leave the
 * user's world, so the class is `external_send` and the level is L3 whatever the caller is
 * (invariant 3). `run` never executes it directly: it returns an approval URL.
 */
export const actionsExecute = defineEndpoint({
  id: "actions.execute",
  meta: {
    summary: "Send an email, post a message or create a calendar event on the user's behalf, after a human approves it.",
    description:
      "Queues an action that leaves the user's world — send an email reply, post a chat message, create a calendar event — for explicit human approval, then executes it through the user's connected services and records the outcome as state. " +
      "Use for: reply to this thread, tell X that Y, put this on the calendar. " +
      "Always L3: the call returns an approval URL; nothing is sent until a human approves. First-party tool run, $0.002 per executed action.",
    exclusions: "Not for drafting (draft locally and keep it as state), reading mail or messages (use the read tools), or anything the user has not asked to send.",
    categories: ["approval", "communication", "state"],
    notes: [
      "The approval URL expires after 15 minutes; call again with the same idempotency_key to get a fresh one.",
      "Approval is the human's: no agent, including the one that queued it, can approve on the user's behalf.",
    ],
    examples: [
      {
        input: { action: { kind: "send_email", to: "mika@example.com", subject: "Revised deck", body: "Attached is the revised deck." } },
        output: { resultType: "approval_required", approval: { url: "https://api.shogunaios.com/approve/…" } },
      },
    ],
  },
  request: { method: "POST", path: "/v1/actions/execute" },
  input: {
    schema: zodInput(
      z
        .object({
          action: z.discriminatedUnion("kind", [
            z.object({ kind: z.literal("send_email"), to: z.string().min(3), subject: z.string().min(1).max(300), body: z.string().min(1).max(20_000) }).strict(),
            z.object({ kind: z.literal("post_message"), channel: z.string().min(1), text: z.string().min(1).max(20_000) }).strict(),
            z.object({ kind: z.literal("create_calendar_event"), title: z.string().min(1).max(300), start: z.string(), end: z.string(), attendees: z.array(z.string()).optional() }).strict(),
          ]),
          reason: z.string().max(500).optional().describe("Why the agent proposes this; shown to the human on the approval page."),
        })
        .strict(),
    ),
  },
  output: {},
  usage: { model: "per_call", cost_micro_usd: 2000 },
  scope: { class: "external_send" },
  fixtures: [
    {
      scenario: "send-email-queued",
      input: { action: { kind: "send_email", to: "mika@example.com", subject: "Revised deck", body: "Attached is the revised deck." } },
      response: { status: 202, body: { approval_id: "ap_1", status: "pending" } },
    },
  ],
});
