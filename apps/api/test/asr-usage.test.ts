import { describe, expect, it, vi } from "vitest";
import { FetchSpeechUsage, parseSpeechUsage } from "../src/asr-usage.js";
const counter = { total_hours: 0.5, agent_hours: 0, tokens_in: 0, tokens_out: 0, tts_characters: 0 };
describe("aggregate speech usage", () => {
  it("counts stream hours and detects paid non-speech APIs", () => {
    expect(parseSpeechUsage({ results: [counter] })).toEqual({ seconds: 1800, unsupported: false });
    expect(parseSpeechUsage({ results: [{ ...counter, tokens_in: 1 }] }).unsupported).toBe(true);
  });
  it.each([{}, { results: [{}] }, { results: [{ ...counter, total_hours: -1 }] }, { results: [{ ...counter, total_hours: Infinity }] }])("fails closed on invalid counters", raw => {
    expect(() => parseSpeechUsage(raw)).toThrow("speech usage unavailable");
  });
  it("uses a dedicated project and aggregate-only endpoints", async () => {
    const request = vi.fn<typeof fetch>().mockResolvedValueOnce(Response.json({ projects: [{ project_id: 'project-1' }] }))
      .mockResolvedValueOnce(Response.json({ results: [counter] })).mockResolvedValueOnce(Response.json({ results: [] }));
    expect(await new FetchSpeechUsage('test-key', request).read('test-tag', new Date('2026-10-17T04:00:00Z'))).toEqual({ day: { seconds: 1800, unsupported: false }, month: { seconds: 0, unsupported: false } });
    expect(request.mock.calls.map(([url]) => String(url))).toEqual([
      'https://api.deepgram.com/v1/projects',
      'https://api.deepgram.com/v1/projects/project-1/usage/breakdown?start=2026-10-17&end=2026-10-18',
      'https://api.deepgram.com/v1/projects/project-1/usage/breakdown?start=2026-10-01&end=2026-10-18&tag=test-tag',
    ]);
  });
  it("refuses ambiguous projects and redirects without leaking errors", async () => {
    const request = vi.fn<typeof fetch>().mockResolvedValue(Response.json({ projects: [{ project_id: 'a' }, { project_id: 'b' }] }));
    await expect(new FetchSpeechUsage('test-key', request).read('tag', new Date())).rejects.toThrow('speech usage unavailable');
    expect(request).toHaveBeenCalledTimes(1);
    const redirect = vi.fn<typeof fetch>().mockResolvedValue(new Response('secret body', { status: 302 }));
    await expect(new FetchSpeechUsage('test-key', redirect).read('tag', new Date())).rejects.toThrow('speech usage unavailable');
  });
});
