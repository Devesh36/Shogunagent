/**
 * First-party Slack connector (E22): posting a message, an external send (L3). Ranked by the same
 * score as any other provider and labeled `first_party` (invariant 13).
 *
 * Auth: the calling principal's own Slack token (slot `slack-tokenset`, Bearer), fetched from
 * apps/api at run time and injected by the transport only (invariant 7; E48,
 * docs/api/connector-tokens.md). `SLACK_TOKEN` is read in development only, behind
 * `ROUTER_DEV_SHARED_CREDENTIALS=1`.
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
  auth: { kind: "caller_connector", slot: "slack-tokenset", credential_env: "SLACK_TOKEN" },
  manifest,
  categories: ["communication"],
  health: { p50_ms: 200, p95_ms: 700, success_rate: 0.997 },
  endpoints: [slackMessagesPost],
});
