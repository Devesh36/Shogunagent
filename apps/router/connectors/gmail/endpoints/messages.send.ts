import { defineEndpoint, z, zodInput } from "../../define.js";
import { gmailMessage, messageFields, pickIds } from "../input.js";

/** Sending leaves the user's world: `external_send`, L3. `run` returns an approval URL and never sends directly. */
export const gmailMessagesSend = defineEndpoint({
  id: "messages.send",
  meta: {
    summary: "Send an email from the user's Gmail after a human approves it.",
    description:
      "Sends a plain-text email from the user's Gmail account: to, cc, subject and body, optionally as a reply inside an existing thread. " +
      "Use for: send this email, reply to the thread, email X the summary, send the follow-up. " +
      "Always L3: the call returns an approval URL and nothing is sent until a human approves. $0.002 per sent email.",
    exclusions: "Not for drafting for review (use gmail.drafts.create, L2), bulk or marketing mail, reading mail, HTML or attachments.",
    categories: ["communication", "approval"],
    notes: ["The approval URL expires after 15 minutes; call again with the same idempotency_key for a fresh one.", "Returns message_id and thread_id once sent."],
    examples: [
      {
        input: { to: ["mika@example.com"], subject: "Revised deck", body: "Hi Mika, the revised deck is in the shared folder." },
        output: { resultType: "approval_required", approval: { url: "https://api.shogunaios.com/approve/…" } },
      },
    ],
    docsUrl: "https://developers.google.com/gmail/api/reference/rest/v1/users.messages/send",
  },
  request: { method: "POST", path: "/gmail/v1/users/me/messages/send", timeout_ms: 15_000 },
  input: {
    schema: zodInput(z.object(messageFields).strict()),
    toRequest: ({ input, request }) => ({ method: request.method, path: request.path, body: gmailMessage(input) }),
  },
  output: {
    fromResponse: ({ response }) => pickIds(response.body),
  },
  usage: { model: "per_call", cost_micro_usd: 2000 },
  scope: { class: "external_send" },
  fixtures: [
    {
      scenario: "synthetic-sent",
      input: { to: ["mika@example.com"], subject: "Revised deck", body: "Hi Mika, the revised deck is in the shared folder." },
      response: { status: 200, body: { id: "18c2f0a1d4", threadId: "18c2f0a1d4", labelIds: ["SENT"] } },
      expect: { output: { message_id: "18c2f0a1d4", thread_id: "18c2f0a1d4" }, usage: { quantity: 1, cost_micro_usd: 2000 } },
    },
    {
      scenario: "synthetic-quota-exceeded",
      input: { to: ["mika@example.com"], subject: "x", body: "y" },
      response: { status: 429, body: { error: { code: 429, status: "RESOURCE_EXHAUSTED" } } },
      expect: { error: true },
    },
  ],
});
