/**
 * E22: first-party connectors with approval-requiring writes. Levels follow the scope table; L3
 * never reaches the transport; the L2 draft asks once; credentials appear only at fetch time.
 */
import { describe, expect, it } from "vitest";
import { calendarEventsCreate } from "../connectors/calendar/endpoints/events.create.js";
import { calendarEventsList } from "../connectors/calendar/endpoints/events.list.js";
import { githubIssuesList } from "../connectors/github/endpoints/issues.list.js";
import { gmailDraftsCreate } from "../connectors/gmail/endpoints/drafts.create.js";
import { rfc822 } from "../connectors/gmail/mime.js";
import { linearIssuesCreate } from "../connectors/linear/endpoints/issues.create.js";
import { discover } from "../src/discover.js";
import { noopHealthStore } from "../src/health.js";
import { run } from "../src/run.js";
import { createFetchTransport } from "../src/transport.js";
import { bearer, fullCatalog, makeDeps } from "./helpers.js";

const EXPECTED_LEVELS: Record<string, { cls: string; level: string }> = {
  "gmail.messages.search": { cls: "read", level: "L1" },
  "gmail.messages.get": { cls: "read", level: "L1" },
  "gmail.drafts.create": { cls: "user_data_write", level: "L2" },
  "gmail.messages.send": { cls: "external_send", level: "L3" },
  "calendar.events.create": { cls: "external_send", level: "L3" },
  "calendar.events.list": { cls: "read", level: "L1" },
  "github.issues.create": { cls: "external_send", level: "L3" },
  "github.issues.list": { cls: "read", level: "L1" },
  "linear.issues.create": { cls: "external_send", level: "L3" },
  "slack.messages.post": { cls: "external_send", level: "L3" },
};

const SAMPLE_INPUT: Record<string, Record<string, unknown>> = {
  "gmail.messages.send": { to: ["mika@example.com"], subject: "Deck", body: "Here it is." },
  "calendar.events.create": { title: "Review", start: "2026-10-27T15:00:00+09:00", end: "2026-10-27T15:30:00+09:00" },
  "github.issues.create": { owner: "octo-org", repo: "api", title: "Bug" },
  "linear.issues.create": { team_id: "9cfb482a-81e3-4154-b5b9-2c805e70a02d", title: "Bug" },
  "slack.messages.post": { channel: "C0123456789", text: "Done." },
};

function decodeRaw(raw: string): string {
  return Buffer.from(raw.replace(/-/g, "+").replace(/_/g, "/"), "base64").toString("utf8");
}

describe("first-party connectors (E22)", () => {
  it("every new connector is in the scope table at its level, first-party, priced at $0.002 per call", () => {
    const catalog = fullCatalog();
    for (const [id, want] of Object.entries(EXPECTED_LEVELS)) {
      const e = catalog.get(id);
      expect(e, id).toBeDefined();
      expect(e?.scope, id).toEqual({ class: want.cls, level: want.level });
      expect(catalog.scope.get(id)?.level, id).toBe(want.level);
      expect(e?.first_party, id).toBe(true);
      expect(e?.quote_micro_usd, id).toBe(2000);
      expect(e?.description.length, id).toBeLessThanOrEqual(400);
      expect(e?.exclusions, id).toMatch(/^Not for/);
    }
  });

  it("L3 sends return an approval URL and never reach the transport, whoever the caller is", async () => {
    for (const [id, input] of Object.entries(SAMPLE_INPUT)) {
      const deps = makeDeps();
      const out = await run(deps, { input: { tool_id: id, input, idempotency_key: `l3-${id}` }, bearer: bearer() });
      expect(out.body.resultType, id).toBe("approval_required");
      if (out.body.resultType === "approval_required") expect(out.body.permission_level).toBe("L3");
      expect(deps.calls, id).toHaveLength(0);
      expect(out.body.usage.cost_micro_usd).toBe(0);
    }
  });

  it("the Gmail draft is L2: input_required first, then it runs once confirmed", async () => {
    const deps = makeDeps({ responder: () => ({ status: 200, body: { id: "r-1", message: { id: "m-1", threadId: "t-1" } } }) });
    const input = { to: ["mika@example.com"], subject: "Deck", body: "Here it is." };
    const first = await run(deps, { input: { tool_id: "gmail.drafts.create", input, idempotency_key: "d1" }, bearer: bearer() });
    expect(first.body.resultType).toBe("input_required");
    expect(deps.calls).toHaveLength(0);
    const second = await run(deps, { input: { tool_id: "gmail.drafts.create", input, idempotency_key: "d2", inputResponses: { confirm: true } }, bearer: bearer() });
    expect(second.body.resultType).toBe("result");
    if (second.body.resultType === "result") expect(second.body.result).toEqual({ draft_id: "r-1", message_id: "m-1", thread_id: "t-1" });
    expect(deps.calls).toHaveLength(1);
    const req = deps.calls[0]?.request;
    expect(req?.path).toBe("/gmail/v1/users/me/drafts");
    const raw = ((req?.body as { message: { raw: string } }).message).raw;
    expect(raw).not.toMatch(/[+/=]/);
    const mime = decodeRaw(raw);
    expect(mime).toContain("To: mika@example.com\r\n");
    expect(mime).toContain("Subject: Deck\r\n");
    expect(mime).toContain('Content-Type: text/plain; charset="UTF-8"');
  });

  it("Gmail headers cannot be injected and non-ASCII subjects are encoded", () => {
    const bad = gmailDraftsCreate.input.schema.validate({ to: ["a@example.com"], subject: "hi\r\nBcc: evil@example.com", body: "x" });
    expect(bad.ok).toBe(false);
    expect(gmailDraftsCreate.input.schema.validate({ to: ["not-an-email"], subject: "x", body: "y" }).ok).toBe(false);
    const msg = rfc822({ to: ["jun@example.com"], subject: "契約書", body: "今夜確認します。" });
    expect(msg).toContain("Subject: =?UTF-8?B?");
    expect(msg).not.toContain("契約書");
  });

  it("request hooks build the vendor call without any credential; the fetch transport injects it (development: from the env map)", async () => {
    const seen: { url: string; auth: string | null; ua: string | null }[] = [];
    const deps = makeDeps();
    deps.transport = createFetchTransport({
      env: { GITHUB_TOKEN: "GITHUB-TOKEN-SENTINEL" },
      connectors: { mode: "dev_shared" },
      fetch: async (url, init) => {
        const h = new Headers(init?.headers);
        seen.push({ url: String(url), auth: h.get("authorization"), ua: h.get("user-agent") });
        return new Response("[]", { status: 200 });
      },
    });
    const out = await run(deps, { input: { tool_id: "github.issues.list", input: { owner: "octo-org", repo: "api", labels: ["bug", "p1"] }, idempotency_key: "gh1" }, bearer: bearer() });
    expect(out.status).toBe(200);
    expect(seen[0]?.url).toBe("https://api.github.com/repos/octo-org/api/issues?state=open&per_page=30&labels=bug%2Cp1");
    expect(seen[0]?.auth).toBe("Bearer GITHUB-TOKEN-SENTINEL");
    expect(seen[0]?.ua).toBe("shogunagent-router");
    const built = githubIssuesList.input.toRequest?.({ input: { owner: "o", repo: "r" }, request: { method: "GET", path: "/" } });
    expect(JSON.stringify(built)).not.toMatch(/authorization|token/i);
    expect(JSON.stringify(out.body)).not.toContain("SENTINEL");

    const missing = makeDeps();
    missing.transport = createFetchTransport({ env: {}, connectors: { mode: "dev_shared" }, fetch: async () => new Response("{}") });
    const m = await run(missing, { input: { tool_id: "calendar.events.list", input: { time_min: "2026-10-27T00:00:00Z" }, idempotency_key: "cal" }, bearer: bearer() });
    expect(m.status).toBe(503);
    if ("error" in m.body) expect(m.body.error.next_action).toBe("try_alternative");
  });

  it("calendar ids are path-encoded and invitations follow notify_attendees; Linear sends a parameterised mutation", () => {
    const list = calendarEventsList.input.toRequest?.({ input: { calendar_id: "team@group.calendar.google.com", time_min: "2026-10-27T00:00:00Z", query: "deck" }, request: { method: "GET", path: "/" } });
    expect(list?.path).toBe("/calendar/v3/calendars/team%40group.calendar.google.com/events");
    expect(list?.query).toMatchObject({ singleEvents: "true", orderBy: "startTime", q: "deck" });
    const create = calendarEventsCreate.input.toRequest?.({ input: { title: "R", start: "2026-10-27T15:00:00+09:00", end: "2026-10-27T15:30:00+09:00", notify_attendees: false, attendees: ["a@example.com"] }, request: { method: "POST", path: "/" } });
    expect(create?.query).toEqual({ sendUpdates: "none" });
    expect(create?.body).toMatchObject({ summary: "R", attendees: [{ email: "a@example.com" }] });
    const lin = linearIssuesCreate.input.toRequest?.({ input: { team_id: "9cfb482a-81e3-4154-b5b9-2c805e70a02d", title: "Bug \" } mutation {" }, request: { method: "POST", path: "/graphql" } });
    const body = lin?.body as { query: string; variables: { input: { title: string } } };
    expect(body.query).not.toContain("Bug");
    expect(body.variables.input.title).toBe('Bug " } mutation {');
  });

  it("discover ranks the new connectors on their own tasks, by the same published score", () => {
    const catalog = fullCatalog();
    const top = (task: string) => discover(catalog, noopHealthStore, { task, limit: 3 }).result.candidates.map((c) => c.tool_id);
    expect(top("what meetings does the user have tomorrow on the calendar")[0]).toBe("calendar.events.list");
    expect(top("file a bug as a github issue in the repository")[0]).toBe("github.issues.create");
    expect(top("list open issues in the github repository")[0]).toBe("github.issues.list");
    expect(top("post the update in the slack channel")[0]).toBe("slack.messages.post");
    expect(top("draft a reply email for the user to review")[0]).toBe("gmail.drafts.create");
    expect(top("create a linear ticket for this bug")[0]).toBe("linear.issues.create");
    // E45: the Gmail reads, ranked by the same formula and labeled first-party.
    expect(top("find the email from mika about the contract in gmail")[0]).toBe("gmail.messages.search");
    expect(top("read the email message and its body text")[0]).toBe("gmail.messages.get");
    const search = discover(catalog, noopHealthStore, { task: "search my gmail for unread email", limit: 1 }).result.candidates[0];
    expect(search?.first_party).toBe(true);
    expect(search?.scoreBreakdown.price).toBeLessThan(1);
  });

  it("E45: Gmail reads run directly at L1 with the caller's own token and never fetch an attachment", async () => {
    const seen: { url: string; auth: string | null }[] = [];
    const deps = makeDeps();
    deps.transport = createFetchTransport({
      env: { GOOGLE_OAUTH_ACCESS_TOKEN: "GOOGLE-TOKEN-SENTINEL" },
      connectors: { mode: "dev_shared" },
      fetch: async (url, init) => {
        seen.push({ url: String(url), auth: new Headers(init?.headers).get("authorization") });
        const body = String(url).includes("/messages/")
          ? { id: "m1", threadId: "t1", labelIds: ["INBOX"], payload: { headers: [{ name: "Subject", value: "Deck" }], parts: [{ mimeType: "text/plain", body: { data: "SGVsbG8" } }, { mimeType: "image/png", filename: "x.png", body: { attachmentId: "a1" } }] } }
          : { messages: [{ id: "m1", threadId: "t1" }], resultSizeEstimate: 1 };
        return new Response(JSON.stringify(body), { status: 200, headers: { "content-type": "application/json" } });
      },
    });
    const found = await run(deps, { input: { tool_id: "gmail.messages.search", input: { query: "from:mika@example.com" }, idempotency_key: "gm1" }, bearer: bearer() });
    expect(found.status).toBe(200);
    expect(seen[0]?.url).toBe("https://gmail.googleapis.com/gmail/v1/users/me/messages?q=from%3Amika%40example.com&maxResults=20");
    expect(seen[0]?.auth).toBe("Bearer GOOGLE-TOKEN-SENTINEL");
    const read = await run(deps, { input: { tool_id: "gmail.messages.get", input: { message_id: "m1" }, idempotency_key: "gm2" }, bearer: bearer() });
    expect(read.status).toBe(200);
    expect(seen[1]?.url).toBe("https://gmail.googleapis.com/gmail/v1/users/me/messages/m1?format=full");
    expect(JSON.stringify(read.body)).toContain('"body_text":"Hello"');
    expect(JSON.stringify(read.body)).toContain('"attachment_count":1');
    expect(seen).toHaveLength(2);
    expect(seen.every((s) => !s.url.includes("attachments"))).toBe(true);
    expect(JSON.stringify(found.body) + JSON.stringify(read.body)).not.toContain("SENTINEL");
    // An id that is not Gmail's shape never reaches the vendor.
    const bad = await run(deps, { input: { tool_id: "gmail.messages.get", input: { message_id: "../drafts" }, idempotency_key: "gm3" }, bearer: bearer() });
    expect(bad.status).toBe(400);
    expect(seen).toHaveLength(2);
  });
});
