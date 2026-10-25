/** Aggregate counters only. Never read provider request logs or transcript bodies. */
export interface SpeechUsage { seconds: number; unsupported: boolean }
export interface SpeechUsageReader { read(tag: string, now: Date): Promise<{ day: SpeechUsage; month: SpeechUsage }> }
export class FetchSpeechUsage implements SpeechUsageReader {
  #key: string;
  constructor(key: string, private readonly request: typeof fetch = (input, init) => fetch(input, init)) { this.#key = key; }
  private async json(path: string): Promise<unknown> {
    try {
      const response = await this.request(`https://api.deepgram.com/v1/${path}`, {
        headers: { authorization: `Token ${this.#key}` }, redirect: "manual", signal: AbortSignal.timeout(10_000),
      });
      if (!response.ok) { await response.body?.cancel(); throw new Error(); }
      const reader = response.body?.getReader(); if (!reader) throw new Error();
      let size = 0; let text = ""; const decoder = new TextDecoder();
      try { for (;;) { const chunk = await reader.read(); if (chunk.done) break; size += chunk.value.length; if (size > 262144) throw new Error(); text += decoder.decode(chunk.value, { stream: true }); } }
      finally { await reader.cancel(); reader.releaseLock(); }
      return JSON.parse(text + decoder.decode());
    } catch { throw new Error("speech usage unavailable"); }
  }
  async read(tag: string, now: Date) {
    const projects = await this.json("projects") as { projects?: { project_id?: unknown }[] };
    // A dedicated project is required; never guess which project should be billed.
    if (!Array.isArray(projects.projects) || projects.projects.length !== 1) throw new Error("speech usage unavailable");
    const id = projects.projects[0]?.project_id;
    if (typeof id !== "string" || !/^[a-zA-Z0-9_-]{1,100}$/.test(id)) throw new Error("speech usage unavailable");
    const day = now.toISOString().slice(0, 10);
    const end = new Date(now.getTime() + 86400000).toISOString().slice(0, 10);
    const read = async (start: string, userTag?: string) => {
      const query = new URLSearchParams({ start, end }); if (userTag) query.set("tag", userTag);
      return parseSpeechUsage(await this.json(`projects/${id}/usage/breakdown?${query}`));
    };
    return { day: await read(day), month: await read(`${day.slice(0, 7)}-01`, tag) };
  }
}
export function parseSpeechUsage(raw: unknown): SpeechUsage {
  if (!raw || typeof raw !== "object" || !("results" in raw) || !Array.isArray(raw.results)) throw new Error("speech usage unavailable");
  let seconds = 0; let unsupported = false;
  for (const value of raw.results) {
    if (!value || typeof value !== "object") throw new Error("speech usage unavailable");
    const row = value as Record<string, unknown>;
    const hours = row.total_hours;
    if (typeof hours !== "number" || !Number.isFinite(hours) || hours < 0) throw new Error("speech usage unavailable");
    seconds += Math.ceil(hours * 3600);
    for (const name of ["agent_hours", "tokens_in", "tokens_out", "tts_characters"]) {
      const n = row[name];
      if (typeof n !== "number" || !Number.isFinite(n) || n < 0) throw new Error("speech usage unavailable");
      if (n > 0) unsupported = true;
    }
  }
  if (!Number.isSafeInteger(seconds)) throw new Error("speech usage unavailable");
  return { seconds, unsupported };
}
