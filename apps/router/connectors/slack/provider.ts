/**
 * First-party Slack connector (E22): posting a message, an external send (L3). Ranked by the same
 * score as any other provider and labeled `first_party` (invariant 13).
 *
 * Auth: a Slack token from `SLACK_TOKEN` (Bearer), injected by the transport only (invariant 7).
 */
import { defineProvider } from "../define.js";
import { slackMessagesPost } from "./endpoints/messages.post.js";
import manifest from "./provider.json" with { type: "json" };

export const slackProvider = defineProvider({
  id: "slack",
  name: "Slack (ShogunAgent connector)",
  first_party: true,
  meta: {
    description: "Post (L3) messages to Slack channels, DMs and threads through the Slack Web API.",
    docsUrl: "https://api.slack.com/web",
    homepage: "https://shogunai.si",
  },
  request: { baseUrl: "https://slack.com" },
  auth: { kind: "bearer", credential_env: "SLACK_TOKEN" },
  manifest,
  categories: ["communication"],
  health: { p50_ms: 200, p95_ms: 700, success_rate: 0.997 },
  endpoints: [slackMessagesPost],
});
