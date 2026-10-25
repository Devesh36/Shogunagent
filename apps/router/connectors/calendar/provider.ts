/**
 * First-party Google Calendar connector (E22). Reading the calendar is L1; creating an event
 * sends invitations and is L3 by type (invariant 3). Ranked by the same score as any other
 * provider and labeled `first_party` (invariant 13).
 *
 * Auth: the user's Google OAuth access token from `GOOGLE_OAUTH_ACCESS_TOKEN`, injected by the
 * transport only (invariant 7).
 */
import { defineProvider } from "../define.js";
import { calendarEventsCreate } from "./endpoints/events.create.js";
import { calendarEventsList } from "./endpoints/events.list.js";
import manifest from "./provider.json" with { type: "json" };

export const calendarProvider = defineProvider({
  id: "calendar",
  name: "Google Calendar (ShogunAgent connector)",
  first_party: true,
  meta: {
    description: "Read (L1) and create (L3) events on the user's Google Calendar through Google's Calendar API.",
    docsUrl: "https://developers.google.com/calendar/api",
    homepage: "https://shogunai.si",
  },
  request: { baseUrl: "https://www.googleapis.com" },
  auth: { kind: "bearer", credential_env: "GOOGLE_OAUTH_ACCESS_TOKEN" },
  manifest,
  categories: ["calendar"],
  health: { p50_ms: 250, p95_ms: 900, success_rate: 0.997 },
  endpoints: [calendarEventsList, calendarEventsCreate],
});
