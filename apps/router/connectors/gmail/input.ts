import { z } from "../define.js";
import type { Json } from "../types.js";
import { rawMessage } from "./mime.js";

/** A header value: one line, so nothing can be injected into the message headers. */
export const headerLine = (max: number) => z.string().min(1).max(max).regex(/^[^\r\n]*$/, "must be a single line");

/** The plain-text message both Gmail endpoints take. */
export const messageFields = {
  to: z.array(z.string().email()).min(1).max(50).describe("Recipient addresses."),
  cc: z.array(z.string().email()).max(50).optional().describe("Cc addresses."),
  subject: headerLine(300).describe("Subject line (one line)."),
  body: z.string().min(1).max(50_000).describe("Plain-text body."),
  thread_id: z.string().regex(/^[A-Za-z0-9]{1,64}$/).optional().describe("Gmail thread id, to keep a reply in its thread."),
};

export interface MessageInput {
  to: string[];
  cc?: string[];
  subject: string;
  body: string;
  thread_id?: string;
}

/** `{ raw, threadId? }`: the Gmail `Message` resource for a plain-text message. */
export function gmailMessage(input: Json): Json {
  const m = input as unknown as MessageInput;
  return { raw: rawMessage({ to: m.to, ...(m.cc ? { cc: m.cc } : {}), subject: m.subject, body: m.body }), ...(m.thread_id ? { threadId: m.thread_id } : {}) };
}

/** The fields an agent needs back; Gmail's label ids and payload are dropped. */
export function pickIds(body: Json): { message_id: Json; thread_id: Json } {
  const b = (body ?? {}) as Record<string, Json>;
  return { message_id: b["id"] ?? null, thread_id: b["threadId"] ?? null };
}
