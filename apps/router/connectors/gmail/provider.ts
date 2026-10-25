/**
 * First-party Gmail connector (E22): approval-requiring writes through the scope table. A draft
 * is the user's own data (L2); a send leaves the user's world (L3). Ranked by the same score as
 * any other provider and labeled `first_party` (invariant 13).
 *
 * Auth: the user's Google OAuth access token, injected by src/transport.ts from
 * `GOOGLE_OAUTH_ACCESS_TOKEN`. No definition or hook holds it (invariant 7). In the hosted router
 * the transport resolves the calling principal's token; this file only names the variable.
 */
import { defineProvider } from "../define.js";
import { gmailDraftsCreate } from "./endpoints/drafts.create.js";
import { gmailMessagesSend } from "./endpoints/messages.send.js";
import manifest from "./provider.json" with { type: "json" };

export const gmailProvider = defineProvider({
  id: "gmail",
  name: "Gmail (ShogunAgent connector)",
  first_party: true,
  meta: {
    description: "Draft (L2) and send (L3) plain-text email from the user's Gmail account through Google's Gmail API.",
    docsUrl: "https://developers.google.com/gmail/api",
    homepage: "https://shogunai.si",
  },
  request: { baseUrl: "https://gmail.googleapis.com" },
  auth: { kind: "bearer", credential_env: "GOOGLE_OAUTH_ACCESS_TOKEN" },
  manifest,
  categories: ["communication"],
  health: { p50_ms: 350, p95_ms: 1200, success_rate: 0.995 },
  endpoints: [gmailDraftsCreate, gmailMessagesSend],
});
