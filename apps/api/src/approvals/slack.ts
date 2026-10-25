/**
 * The Slack approval adapter (plan E15).
 *
 * - When an approval is created, `postApprovalMessage` posts the redacted summary to the
 *   configured channel or user (`SLACK_APPROVAL_CHANNEL`) with `chat.postMessage` on the bot
 *   token (`SLACK_BOT_TOKEN`), with two buttons whose value is the decision token.
 * - `POST /v1/slack/interactions` receives the button press. The request is verified against
 *   Slack's signing secret (`SLACK_SIGNING_SECRET`: `v0=HMAC-SHA256(secret, "v0:<ts>:<raw body>")`,
 *   a timestamp within five minutes) before anything is parsed; the token is verified with the
 *   shared approval key, and the decision goes to the sidecar through the one client
 *   (`sidecar-client.ts`) as `slack:<user id>`. The original message is replaced with the outcome
 *   through `response_url`.
 *
 * Nothing from Slack is logged or stored; the message carries the redacted summary only.
 */
import { Buffer } from "node:buffer";
import { createHmac, timingSafeEqual } from "node:crypto";

import type { ApprovalSummary, FetchFn } from "./sidecar-client.js";

export const SLACK_POST_MESSAGE = "https://slack.com/api/chat.postMessage";
export const ACTION_APPROVE = "shogun_approve";
export const ACTION_REJECT = "shogun_reject";
export const SLACK_TOLERANCE_SECONDS = 300;

export interface SlackDeps {
  botToken: string;
  signingSecret: string;
  /** A channel id (`C…`) or a user id (`U…`, a DM). */
  channel: string;
  fetch?: FetchFn;
}

/** Verify Slack's request signature over the raw body. */
export function verifySlackSignature(signingSecret: string, rawBody: string, timestamp: string | undefined, signature: string | undefined, nowSeconds: number): boolean {
  if (!timestamp || !signature || !/^\d{1,12}$/.test(timestamp)) return false;
  if (Math.abs(nowSeconds - Number(timestamp)) > SLACK_TOLERANCE_SECONDS) return false;
  if (!/^v0=[0-9a-f]{64}$/.test(signature)) return false;
  const expected = createHmac("sha256", signingSecret).update(`v0:${timestamp}:${rawBody}`).digest();
  const given = Buffer.from(signature.slice(3), "hex");
  return given.length === expected.length && timingSafeEqual(given, expected);
}

/** Plain text for Slack: the control characters Slack treats as markup are escaped. */
export function slackText(text: string): string {
  return text.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

function summaryLines(summary: ApprovalSummary): string {
  const lines = [
    `*Approval needed* (${slackText(summary.level)}): ${slackText(summary.kind ?? "an action")}`,
    `To: ${slackText(summary.recipient_class ?? "unknown")}`,
  ];
  if (summary.preview) lines.push(`> ${slackText(summary.preview)}`);
  if (summary.expires_at) lines.push(`Expires ${slackText(summary.expires_at)}`);
  return lines.join("\n");
}

/** The `chat.postMessage` payload for one approval. */
export function approvalMessage(channel: string, summary: ApprovalSummary, token: string): Record<string, unknown> {
  const text = summaryLines(summary);
  return {
    channel,
    text: `Approval needed: ${summary.kind ?? "an action"} (${summary.level})`,
    blocks: [
      { type: "section", text: { type: "mrkdwn", text } },
      {
        type: "actions",
        block_id: `shogun_approval_${summary.approval_id}`,
        elements: [
          { type: "button", action_id: ACTION_APPROVE, style: "primary", text: { type: "plain_text", text: "Approve" }, value: token },
          { type: "button", action_id: ACTION_REJECT, style: "danger", text: { type: "plain_text", text: "Reject" }, value: token },
        ],
      },
    ],
  };
}

/** Post the approval message. Resolves to whether Slack accepted it (`ok: true`). */
export async function postApprovalMessage(slack: SlackDeps, summary: ApprovalSummary, token: string): Promise<boolean> {
  const fetchFn = slack.fetch ?? ((input, init) => fetch(input, init));
  try {
    const res = await fetchFn(SLACK_POST_MESSAGE, {
      method: "POST",
      headers: { "content-type": "application/json; charset=utf-8", authorization: `Bearer ${slack.botToken}` },
      body: JSON.stringify(approvalMessage(slack.channel, summary, token)),
    });
    const body = (await res.json().catch(() => ({}))) as { ok?: unknown };
    return res.ok && body.ok === true;
  } catch {
    return false;
  }
}

/** One button press, parsed from the interaction payload. */
export interface SlackPress {
  decision: "approve" | "reject";
  token: string;
  userId: string;
  responseUrl?: string;
}

/** Parse `payload=<json>` of a `block_actions` interaction. `undefined` for anything else. */
export function parseInteraction(rawBody: string): SlackPress | undefined {
  const payload = new URLSearchParams(rawBody).get("payload");
  if (!payload) return undefined;
  let value: unknown;
  try {
    value = JSON.parse(payload);
  } catch {
    return undefined;
  }
  if (typeof value !== "object" || value === null) return undefined;
  const v = value as { type?: unknown; user?: { id?: unknown }; actions?: Array<{ action_id?: unknown; value?: unknown }>; response_url?: unknown };
  if (v.type !== "block_actions") return undefined;
  const action = v.actions?.[0];
  const decision = action?.action_id === ACTION_APPROVE ? "approve" : action?.action_id === ACTION_REJECT ? "reject" : undefined;
  const userId = typeof v.user?.id === "string" && /^[A-Z0-9]{1,32}$/.test(v.user.id) ? v.user.id : undefined;
  if (!decision || !userId || typeof action?.value !== "string") return undefined;
  const responseUrl = typeof v.response_url === "string" && v.response_url.startsWith("https://hooks.slack.com/") ? v.response_url : undefined;
  return { decision, token: action.value, userId, ...(responseUrl ? { responseUrl } : {}) };
}

/** Replace the original message with the outcome (best effort). */
export async function replaceMessage(slack: SlackDeps, responseUrl: string, text: string): Promise<void> {
  const fetchFn = slack.fetch ?? ((input, init) => fetch(input, init));
  try {
    await fetchFn(responseUrl, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ replace_original: true, text }) });
  } catch {
    // Best effort: the decision already stands.
  }
}
