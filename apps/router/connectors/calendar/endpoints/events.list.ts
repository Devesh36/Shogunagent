import { defineEndpoint, z, zodInput } from "../../define.js";
import type { Json } from "../../types.js";

interface ListInput {
  calendar_id?: string;
  time_min: string;
  time_max?: string;
  query?: string;
  max_results?: number;
}

type GEvent = Record<string, Json>;

function when(v: Json | undefined): Json {
  const o = (v ?? {}) as Record<string, Json>;
  return o["dateTime"] ?? o["date"] ?? null;
}

/** Google's event resource → the fields an agent plans with. */
export function mapEvent(e: GEvent): Json {
  const attendees = Array.isArray(e["attendees"]) ? (e["attendees"] as GEvent[]).map((a) => a["email"] ?? null) : [];
  return {
    id: e["id"] ?? null,
    title: e["summary"] ?? null,
    start: when(e["start"]),
    end: when(e["end"]),
    location: e["location"] ?? null,
    attendees,
    status: e["status"] ?? null,
    url: e["htmlLink"] ?? null,
  };
}

export const calendarEventsList = defineEndpoint({
  id: "events.list",
  meta: {
    summary: "List the user's Google Calendar events in a date range: meetings, start and end, attendees and locations.",
    description:
      "Reads events from the user's Google Calendar in a date range, expanded to single occurrences and ordered by start, with title, start, end, location and attendee emails. " +
      "Use for: what meetings does the user have tomorrow, is the user free on Friday afternoon, when is the user meeting X, what is on the calendar this week. " +
      "L1 read. $0.002 per call.",
    exclusions: "Not for creating or changing events (use calendar.events.create, L3) or for the user's commitments and deadlines (use state.commitments.list).",
    categories: ["calendar"],
    notes: ["time_min and time_max are RFC 3339 timestamps with an offset, e.g. 2026-10-27T00:00:00+09:00.", "calendar_id defaults to the user's primary calendar."],
    examples: [
      {
        input: { time_min: "2026-10-27T00:00:00+09:00", time_max: "2026-10-28T00:00:00+09:00" },
        output: { items: [{ id: "ev1", title: "Deck review with Mika", start: "2026-10-27T10:00:00+09:00", end: "2026-10-27T10:30:00+09:00", location: null, attendees: ["mika@example.com"], status: "confirmed", url: "https://calendar.google.com/…" }], next_page_token: null },
      },
    ],
    docsUrl: "https://developers.google.com/calendar/api/v3/reference/events/list",
  },
  request: { method: "GET", path: "/calendar/v3/calendars/primary/events", timeout_ms: 15_000 },
  input: {
    schema: zodInput(
      z
        .object({
          calendar_id: z.string().min(1).max(200).optional().describe("Default: primary."),
          time_min: z.string().datetime({ offset: true }).describe("Start of the window (RFC 3339)."),
          time_max: z.string().datetime({ offset: true }).optional().describe("End of the window (RFC 3339)."),
          query: z.string().min(1).max(200).optional().describe("Free-text filter on title, description, location and attendees."),
          max_results: z.number().int().min(1).max(250).optional().describe("Default 50."),
        })
        .strict(),
    ),
    toRequest: ({ input }) => {
      const i = input as unknown as ListInput;
      const query: Record<string, string> = { timeMin: i.time_min, singleEvents: "true", orderBy: "startTime", maxResults: String(i.max_results ?? 50) };
      if (i.time_max) query["timeMax"] = i.time_max;
      if (i.query) query["q"] = i.query;
      return { method: "GET", path: `/calendar/v3/calendars/${encodeURIComponent(i.calendar_id ?? "primary")}/events`, query };
    },
  },
  output: {
    fromResponse: ({ response }) => {
      const b = (response.body ?? {}) as Record<string, Json>;
      const items = Array.isArray(b["items"]) ? (b["items"] as GEvent[]).map(mapEvent) : [];
      return { items, next_page_token: b["nextPageToken"] ?? null };
    },
  },
  usage: { model: "per_call", cost_micro_usd: 2000 },
  scope: { class: "read" },
  fixtures: [
    {
      scenario: "synthetic-two-events",
      input: { time_min: "2026-10-27T00:00:00+09:00", time_max: "2026-10-28T00:00:00+09:00" },
      response: {
        status: 200,
        body: {
          kind: "calendar#events",
          items: [
            { id: "ev1", status: "confirmed", summary: "Deck review with Mika", start: { dateTime: "2026-10-27T10:00:00+09:00" }, end: { dateTime: "2026-10-27T10:30:00+09:00" }, attendees: [{ email: "mika@example.com" }], htmlLink: "https://calendar.google.com/event?eid=ev1" },
            { id: "ev2", status: "confirmed", summary: "Offsite", start: { date: "2026-10-27" }, end: { date: "2026-10-28" }, location: "Kamakura" },
          ],
        },
      },
      expect: {
        output: {
          items: [
            { id: "ev1", title: "Deck review with Mika", start: "2026-10-27T10:00:00+09:00", end: "2026-10-27T10:30:00+09:00", location: null, attendees: ["mika@example.com"], status: "confirmed", url: "https://calendar.google.com/event?eid=ev1" },
            { id: "ev2", title: "Offsite", start: "2026-10-27", end: "2026-10-28", location: "Kamakura", attendees: [], status: "confirmed", url: null },
          ],
          next_page_token: null,
        },
        usage: { quantity: 1, cost_micro_usd: 2000 },
      },
    },
    {
      scenario: "synthetic-free-day",
      input: { time_min: "2026-11-01T00:00:00+09:00", time_max: "2026-11-02T00:00:00+09:00" },
      response: { status: 200, body: { kind: "calendar#events", items: [] } },
      expect: { output: { items: [], next_page_token: null }, usage: { quantity: 0, cost_micro_usd: 0 } },
    },
    {
      scenario: "synthetic-calendar-not-found",
      input: { calendar_id: "nobody@example.com", time_min: "2026-11-01T00:00:00Z" },
      response: { status: 404, body: { error: { code: 404, message: "Not Found" } } },
      expect: { error: true },
    },
  ],
});
