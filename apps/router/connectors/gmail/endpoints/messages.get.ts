import { defineEndpoint, z, zodInput } from "../../define.js";
import type { Json } from "../../types.js";

/** Characters of message text returned (the rest is cut and `truncated` is true). */
export const MAX_BODY_CHARS = 20_000;

type Part = Record<string, Json>;

/** base64url → UTF-8 text (Workers and Node: atob + TextDecoder). */
function decodeBase64url(data: string): string {
  const b64 = data.replace(/-/g, "+").replace(/_/g, "/");
  const bin = atob(b64 + "=".repeat((4 - (b64.length % 4)) % 4));
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  return new TextDecoder().decode(bytes);
}

function isAttachment(p: Part): boolean {
  const body = (p["body"] ?? {}) as Part;
  return (typeof p["filename"] === "string" && p["filename"] !== "") || typeof body["attachmentId"] === "string";
}

/** The text/plain parts, depth-first; an attachment (a file name or an attachment id) is never
 * read, only counted. */
function walk(p: Part, out: { text: string[]; attachments: number }, depth: number): void {
  if (depth > 8) return;
  if (isAttachment(p)) {
    out.attachments += 1;
    return;
  }
  const body = (p["body"] ?? {}) as Part;
  if (p["mimeType"] === "text/plain" && typeof body["data"] === "string") {
    try {
      out.text.push(decodeBase64url(body["data"]));
    } catch {
      // Malformed part: skipped.
    }
  }
  if (Array.isArray(p["parts"])) for (const child of p["parts"] as Part[]) walk(child, out, depth + 1);
}

/** Gmail's full message resource → the fields an agent reads. Pure. */
export function mapMessage(m: Part): Json {
  const payload = (m["payload"] ?? {}) as Part;
  const headers = Array.isArray(payload["headers"]) ? (payload["headers"] as Part[]) : [];
  const header = (name: string): Json => {
    const h = headers.find((x) => typeof x["name"] === "string" && x["name"].toLowerCase() === name);
    return typeof h?.["value"] === "string" ? h["value"] : null;
  };
  const found = { text: [] as string[], attachments: 0 };
  walk(payload, found, 0);
  const full = found.text.length > 0 ? found.text.join("\n\n") : typeof m["snippet"] === "string" ? m["snippet"] : "";
  const truncated = full.length > MAX_BODY_CHARS;
  return {
    message_id: m["id"] ?? null,
    thread_id: m["threadId"] ?? null,
    labels: Array.isArray(m["labelIds"]) ? m["labelIds"] : [],
    from: header("from"),
    to: header("to"),
    cc: header("cc"),
    subject: header("subject"),
    date: header("date"),
    body_text: truncated ? full.slice(0, MAX_BODY_CHARS) : full,
    truncated,
    attachment_count: found.attachments,
  };
}

/**
 * E45: one message of the user's own mailbox, as text (L1 by the scope table). Attachments are
 * counted, never fetched or returned; HTML-only parts are left out (the snippet stands in).
 */
export const gmailMessagesGet = defineEndpoint({
  id: "messages.get",
  meta: {
    summary: "Read one email from the user's Gmail as plain text: sender, recipients, subject, date and body.",
    description:
      "Reads one message from the user's own Gmail by id: from, to, cc, subject, date, labels and the plain-text body (cut at 20,000 characters), with the number of attachments. " +
      "Use for: read the email Mika sent about the contract, what does this message say, summarize the latest email in this thread. " +
      "L1 read. $0.002 per call.",
    exclusions: "Not for finding messages (use gmail.messages.search), attachments or HTML, sending or drafting (gmail.messages.send, gmail.drafts.create).",
    categories: ["communication"],
    notes: ["message_id comes from gmail.messages.search.", "Attachments are counted, never downloaded."],
    examples: [
      {
        input: { message_id: "18c2f0a1b2" },
        output: { message_id: "18c2f0a1b2", thread_id: "18c2f0a1b2", labels: ["INBOX"], from: "Mika <mika@example.com>", to: "me@example.com", cc: null, subject: "Deck", date: "Mon, 26 Oct 2026 09:00:00 +0900", body_text: "Can you send the deck by Friday?", truncated: false, attachment_count: 0 },
      },
    ],
    docsUrl: "https://developers.google.com/gmail/api/reference/rest/v1/users.messages/get",
  },
  request: { method: "GET", path: "/gmail/v1/users/me/messages/{message_id}", timeout_ms: 15_000 },
  input: {
    schema: zodInput(
      z
        .object({
          message_id: z.string().regex(/^[A-Za-z0-9]{1,64}$/).describe("The Gmail message id."),
        })
        .strict(),
    ),
    toRequest: ({ input }) => {
      const id = (input as { message_id: string }).message_id;
      return { method: "GET", path: `/gmail/v1/users/me/messages/${id}`, query: { format: "full" } };
    },
  },
  output: {
    fromResponse: ({ response }) => mapMessage((response.body ?? {}) as Part),
  },
  usage: { model: "per_call", cost_micro_usd: 2000 },
  scope: { class: "read" },
  fixtures: [
    {
      scenario: "synthetic-plain-text-with-attachment",
      input: { message_id: "18c2f0a1b2" },
      response: {
        status: 200,
        body: {
          id: "18c2f0a1b2",
          threadId: "18c2f0a1b2",
          labelIds: ["INBOX"],
          snippet: "Can you send the deck",
          payload: {
            mimeType: "multipart/mixed",
            headers: [
              { name: "From", value: "Mika <mika@example.com>" },
              { name: "To", value: "me@example.com" },
              { name: "Subject", value: "Deck" },
              { name: "Date", value: "Mon, 26 Oct 2026 09:00:00 +0900" },
            ],
            parts: [
              { mimeType: "text/plain", body: { data: "Q2FuIHlvdSBzZW5kIHRoZSBkZWNrIGJ5IEZyaWRheT8" } },
              { mimeType: "application/pdf", filename: "deck.pdf", body: { attachmentId: "ANGjdJ8", size: 52000 } },
            ],
          },
        },
      },
      expect: {
        output: { message_id: "18c2f0a1b2", thread_id: "18c2f0a1b2", labels: ["INBOX"], from: "Mika <mika@example.com>", to: "me@example.com", cc: null, subject: "Deck", date: "Mon, 26 Oct 2026 09:00:00 +0900", body_text: "Can you send the deck by Friday?", truncated: false, attachment_count: 1 },
        usage: { quantity: 1, cost_micro_usd: 2000 },
      },
    },
    {
      scenario: "synthetic-html-only-uses-snippet",
      input: { message_id: "18c2f0a1c3" },
      response: { status: 200, body: { id: "18c2f0a1c3", threadId: "18c2f0a1b2", labelIds: ["INBOX"], snippet: "Lunch moved to 1pm", payload: { mimeType: "text/html", headers: [{ name: "Subject", value: "Lunch" }], body: { data: "PGI-THVuY2g8L2I-" } } } },
      expect: {
        output: { message_id: "18c2f0a1c3", thread_id: "18c2f0a1b2", labels: ["INBOX"], from: null, to: null, cc: null, subject: "Lunch", date: null, body_text: "Lunch moved to 1pm", truncated: false, attachment_count: 0 },
        usage: { quantity: 1, cost_micro_usd: 2000 },
      },
    },
    {
      scenario: "synthetic-message-not-found",
      input: { message_id: "missing1" },
      response: { status: 404, body: { error: { code: 404, message: "Requested entity was not found." } } },
      expect: { error: true },
    },
  ],
});
