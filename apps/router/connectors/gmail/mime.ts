/**
 * Builds the base64url RFC 5322 message Gmail's `raw` field takes. Plain text only, UTF-8, the
 * body base64-encoded. Header values are validated upstream to contain no line breaks (no header
 * injection); a non-ASCII subject is RFC 2047 encoded. Runs in Workers and Node (btoa + TextEncoder).
 */
function base64(bytes: Uint8Array): string {
  let bin = "";
  for (let i = 0; i < bytes.length; i++) bin += String.fromCharCode(bytes[i] ?? 0);
  return btoa(bin);
}

const utf8 = (s: string) => new TextEncoder().encode(s);

export function base64url(text: string): string {
  return base64(utf8(text)).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

function encodeHeader(value: string): string {
  // eslint-disable-next-line no-control-regex
  return /^[\x20-\x7e]*$/.test(value) ? value : `=?UTF-8?B?${base64(utf8(value))}?=`;
}

export interface PlainMessage {
  to: string[];
  cc?: string[];
  subject: string;
  body: string;
}

export function rfc822(m: PlainMessage): string {
  const lines = [`To: ${m.to.join(", ")}`];
  if (m.cc?.length) lines.push(`Cc: ${m.cc.join(", ")}`);
  lines.push(`Subject: ${encodeHeader(m.subject)}`, "MIME-Version: 1.0", 'Content-Type: text/plain; charset="UTF-8"', "Content-Transfer-Encoding: base64", "");
  const body = base64(utf8(m.body)).replace(/(.{76})/g, "$1\r\n");
  return `${lines.join("\r\n")}\r\n${body}`;
}

export function rawMessage(m: PlainMessage): string {
  return base64url(rfc822(m));
}
