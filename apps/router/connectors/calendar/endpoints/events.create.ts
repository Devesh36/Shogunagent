import { defineEndpoint, z, zodInput } from "../../define.js";
import type { Json } from "../../types.js";
import { mapEvent } from "./events.list.js";

interface CreateInput {
  calendar_id?: string;
  title: string;
  start: string;
  end: string;
  time_zone?: string;
  description?: string;
  location?: string;
  attendees?: string[];
  notify_attendees?: boolean;
}

/**
 * Creating an event is visible to others (attendees get invitations): `external_send`, L3 by type
 * (invariant 3 names calendar creation explicitly). `run` returns an approval URL.
 */
export const calendarEventsCreate = defineEndpoint({
  id: "events.create",
  meta: {
    summary: "Create a Google Calendar event and invite attendees, after a human approves it.",
    description:
      "Creates an event on the user's Google Calendar with title, start, end, time zone, location, description and attendee emails, and sends the invitations. " +
      "Use for: set up a meeting with X, put this on the calendar, book a call on Tuesday at 3pm, block time for the review. " +
      "Always L3: the call returns an approval URL and nothing is created until a human approves. $0.002 per created event.",
    exclusions: "Not for reading the calendar (use calendar.events.list), finding free time across other people's calendars, or editing and deleting existing events.",
    categories: ["calendar", "approval"],
    notes: ["start and end are RFC 3339 timestamps with an offset.", "Set notify_attendees: false to add attendees without emailing them (default true)."],
    examples: [
      {
        input: { title: "Deck review", start: "2026-10-27T15:00:00+09:00", end: "2026-10-27T15:30:00+09:00", attendees: ["mika@example.com"] },
        output: { resultType: "approval_required", approval: { url: "https://api.shogunaios.com/approve/…" } },
      },
    ],
    docsUrl: "https://developers.google.com/calendar/api/v3/reference/events/insert",
  },
  request: { method: "POST", path: "/calendar/v3/calendars/primary/events", timeout_ms: 15_000 },
  input: {
    schema: zodInput(
      z
        .object({
          calendar_id: z.string().min(1).max(200).optional().describe("Default: primary."),
          title: z.string().min(1).max(300),
          start: z.string().datetime({ offset: true }),
          end: z.string().datetime({ offset: true }),
          time_zone: z.string().min(1).max(64).optional().describe("IANA time zone, e.g. Asia/Tokyo."),
          description: z.string().max(8000).optional(),
          location: z.string().max(500).optional(),
          attendees: z.array(z.string().email()).max(100).optional(),
          notify_attendees: z.boolean().optional(),
        })
        .strict(),
    ),
    toRequest: ({ input }) => {
      const i = input as unknown as CreateInput;
      const tz = i.time_zone ? { timeZone: i.time_zone } : {};
      const body: Record<string, Json> = { summary: i.title, start: { dateTime: i.start, ...tz }, end: { dateTime: i.end, ...tz } };
      if (i.description) body["description"] = i.description;
      if (i.location) body["location"] = i.location;
      if (i.attendees?.length) body["attendees"] = i.attendees.map((email) => ({ email }));
      return {
        method: "POST",
        path: `/calendar/v3/calendars/${encodeURIComponent(i.calendar_id ?? "primary")}/events`,
        query: { sendUpdates: i.notify_attendees === false ? "none" : "all" },
        body,
      };
    },
  },
  output: {
    fromResponse: ({ response }) => mapEvent((response.body ?? {}) as Record<string, Json>),
  },
  usage: { model: "per_call", cost_micro_usd: 2000 },
  scope: { class: "external_send" },
  fixtures: [
    {
      scenario: "synthetic-created",
      input: { title: "Deck review", start: "2026-10-27T15:00:00+09:00", end: "2026-10-27T15:30:00+09:00", attendees: ["mika@example.com"] },
      response: {
        status: 200,
        body: { id: "ev9", status: "confirmed", summary: "Deck review", start: { dateTime: "2026-10-27T15:00:00+09:00" }, end: { dateTime: "2026-10-27T15:30:00+09:00" }, attendees: [{ email: "mika@example.com", responseStatus: "needsAction" }], htmlLink: "https://calendar.google.com/event?eid=ev9" },
      },
      expect: {
        output: { id: "ev9", title: "Deck review", start: "2026-10-27T15:00:00+09:00", end: "2026-10-27T15:30:00+09:00", location: null, attendees: ["mika@example.com"], status: "confirmed", url: "https://calendar.google.com/event?eid=ev9" },
        usage: { quantity: 1, cost_micro_usd: 2000 },
      },
    },
    {
      scenario: "synthetic-end-before-start",
      input: { title: "Bad", start: "2026-10-27T15:00:00+09:00", end: "2026-10-27T14:00:00+09:00" },
      response: { status: 400, body: { error: { code: 400, message: "The specified time range is empty." } } },
      expect: { error: true },
    },
  ],
});
