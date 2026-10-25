import { defineEndpoint, z, zodInput } from "../../define.js";
import type { Json } from "../../types.js";
import { gmailMessage, messageFields, pickIds } from "../input.js";

/**
 * A draft stays in the user's own mailbox and reaches nobody, so the class is `user_data_write`
 * (L2): the first call asks the agent to confirm once. Sending it is gmail.messages.send (L3).
 */
export const gmailDraftsCreate = defineEndpoint({
  id: "drafts.create",
  meta: {
    summary: "Draft an email in the user's Gmail without sending it: recipients, subject and plain-text body.",
    description:
      "Creates a Gmail draft in the user's own mailbox: to, cc, subject and a plain-text body, optionally inside an existing thread. Nothing is sent; the user reviews and sends it from Gmail. " +
      "Use for: draft a reply to this email, prepare an email for the user to review, prepare a follow-up email without sending it. " +
      "L2 (changes the user's mailbox, confirm once). $0.002 per call.",
    exclusions: "Not for sending (use gmail.messages.send, L3), reading mail, HTML or attachments.",
    categories: ["communication"],
    notes: ["Plain text only; the body is UTF-8 and the subject may contain non-ASCII characters.", "Returns draft_id, message_id and thread_id."],
    examples: [
      {
        input: { to: ["mika@example.com"], subject: "Revised deck", body: "Hi Mika, the revised deck is attached in the shared folder." },
        output: { draft_id: "r-58201", message_id: "18c2f0a1b2", thread_id: "18c2f0a1b2" },
      },
    ],
    docsUrl: "https://developers.google.com/gmail/api/reference/rest/v1/users.drafts/create",
  },
  request: { method: "POST", path: "/gmail/v1/users/me/drafts", timeout_ms: 15_000 },
  input: {
    schema: zodInput(z.object(messageFields).strict()),
    toRequest: ({ input, request }) => ({ method: request.method, path: request.path, body: { message: gmailMessage(input) } }),
  },
  output: {
    fromResponse: ({ response }) => {
      const b = (response.body ?? {}) as Record<string, Json>;
      return { draft_id: b["id"] ?? null, ...pickIds(b["message"] ?? null) };
    },
  },
  usage: { model: "per_call", cost_micro_usd: 2000 },
  scope: { class: "user_data_write" },
  fixtures: [
    {
      scenario: "synthetic-draft-created",
      input: { to: ["mika@example.com"], subject: "Revised deck", body: "Hi Mika, the revised deck is in the shared folder." },
      response: { status: 200, body: { id: "r-58201", message: { id: "18c2f0a1b2", threadId: "18c2f0a1b2", labelIds: ["DRAFT"] } } },
      expect: { output: { draft_id: "r-58201", message_id: "18c2f0a1b2", thread_id: "18c2f0a1b2" }, usage: { quantity: 1, cost_micro_usd: 2000 } },
    },
    {
      scenario: "synthetic-draft-in-thread-non-ascii",
      input: { to: ["jun@example.com"], cc: ["mika@example.com"], subject: "契約書のレビュー", body: "今夜確認します。", thread_id: "18c2f0a1b2" },
      response: { status: 200, body: { id: "r-58202", message: { id: "18c2f0a1c3", threadId: "18c2f0a1b2", labelIds: ["DRAFT"] } } },
      expect: { output: { draft_id: "r-58202", message_id: "18c2f0a1c3", thread_id: "18c2f0a1b2" }, usage: { quantity: 1, cost_micro_usd: 2000 } },
    },
    {
      scenario: "synthetic-token-expired",
      input: { to: ["mika@example.com"], subject: "x", body: "y" },
      response: { status: 401, body: { error: { code: 401, status: "UNAUTHENTICATED" } } },
      expect: { error: true },
    },
  ],
});
