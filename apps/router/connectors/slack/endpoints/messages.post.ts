import { defineEndpoint, z, zodInput } from "../../define.js";
import type { Json, RunResponse } from "../../types.js";

type Row = Record<string, Json>;

/** Slack Web API answers errors with HTTP 200 and `ok: false`. */
export function slackFailed(response: RunResponse): boolean {
  if (response.status >= 400) return true;
  return ((response.body ?? {}) as Row)["ok"] !== true;
}

/** A posted message is read by everyone in the channel: `external_send`, L3. */
export const slackMessagesPost = defineEndpoint({
  id: "messages.post",
  meta: {
    summary: "Post a message to a Slack channel or thread, after a human approves it.",
    description:
      "Posts a message to a Slack channel, direct message or thread in the user's workspace, as plain text with Slack mrkdwn formatting. " +
      "Use for: post the update in #team, tell the channel the deploy is done, reply in this Slack thread, message X on Slack. " +
      "Always L3: the call returns an approval URL and nothing is posted until a human approves. $0.002 per posted message.",
    exclusions: "Not for reading channel history or searching Slack, editing or deleting messages, or email (use gmail.messages.send).",
    categories: ["communication", "approval"],
    notes: ["channel is a channel id (C…), a DM id (D…) or a user id (U…); pass thread_ts to reply in a thread.", "Returns channel, ts (the message id) and thread_ts."],
    examples: [
      {
        input: { channel: "C0123456789", text: "Deploy finished: router v0.3 is live." },
        output: { resultType: "approval_required", approval: { url: "https://api.shogunaios.com/approve/…" } },
      },
    ],
    docsUrl: "https://api.slack.com/methods/chat.postMessage",
  },
  request: { method: "POST", path: "/api/chat.postMessage", timeout_ms: 10_000 },
  input: {
    schema: zodInput(
      z
        .object({
          channel: z.string().regex(/^[CDGU][A-Z0-9]{6,20}$/, "a Slack channel, DM or user id").describe("Channel, DM or user id."),
          text: z.string().min(1).max(40_000),
          thread_ts: z.string().regex(/^\d{10}\.\d{6}$/).optional().describe("Parent message ts, to reply in a thread."),
          reply_broadcast: z.boolean().optional().describe("Also show a thread reply in the channel."),
        })
        .strict(),
    ),
  },
  output: {
    isFailure: slackFailed,
    fromResponse: ({ response }) => {
      const b = (response.body ?? {}) as Row;
      const msg = (b["message"] ?? {}) as Row;
      return { channel: b["channel"] ?? null, ts: b["ts"] ?? null, thread_ts: msg["thread_ts"] ?? null };
    },
  },
  usage: { model: "per_call", cost_micro_usd: 2000 },
  scope: { class: "external_send" },
  fixtures: [
    {
      scenario: "synthetic-posted",
      input: { channel: "C0123456789", text: "Deploy finished: router v0.3 is live." },
      response: { status: 200, body: { ok: true, channel: "C0123456789", ts: "1761357600.000100", message: { type: "message", ts: "1761357600.000100" } } },
      expect: { output: { channel: "C0123456789", ts: "1761357600.000100", thread_ts: null }, usage: { quantity: 1, cost_micro_usd: 2000 } },
    },
    {
      scenario: "synthetic-thread-reply",
      input: { channel: "C0123456789", text: "Fixed in #44.", thread_ts: "1761357600.000100" },
      response: { status: 200, body: { ok: true, channel: "C0123456789", ts: "1761357700.000200", message: { type: "message", ts: "1761357700.000200", thread_ts: "1761357600.000100" } } },
      expect: { output: { channel: "C0123456789", ts: "1761357700.000200", thread_ts: "1761357600.000100" }, usage: { quantity: 1, cost_micro_usd: 2000 } },
    },
    {
      scenario: "synthetic-not-in-channel",
      input: { channel: "C0999999999", text: "hello" },
      response: { status: 200, body: { ok: false, error: "not_in_channel" } },
      expect: { error: true },
    },
  ],
});
