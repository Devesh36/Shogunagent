/**
 * Idempotency for `run` (E8-T5; CLAUDE.md "every write path takes an idempotency key"). A repeated
 * key from the same actor returns the stored response without re-running; the same key with a
 * different tool or input is a conflict.
 *
 * The stored response IS a result body, so its production home is the principal's own store
 * (invariant 2: bodies never persist outside it), reached through the state layer the same way
 * the run log is. This in-memory store is bounded and TTL-expired, for development and tests;
 * `apps/api` must not back it with a hosted database.
 */
import type { Json } from "../connectors/types.js";

export interface StoredRun {
  tool_id: string;
  input_hash: string;
  response: Json;
  stored_at: number;
}

export interface IdempotencyStore {
  get(actor: string, key: string): Promise<StoredRun | undefined>;
  put(actor: string, key: string, run: StoredRun): Promise<void>;
}

export class InMemoryIdempotencyStore implements IdempotencyStore {
  readonly #entries = new Map<string, StoredRun>();
  readonly #ttlMs: number;
  readonly #max: number;
  readonly #now: () => number;

  constructor(opts: { ttlMs?: number; max?: number; now?: () => number } = {}) {
    this.#ttlMs = opts.ttlMs ?? 24 * 60 * 60 * 1000;
    this.#max = opts.max ?? 10_000;
    this.#now = opts.now ?? Date.now;
  }

  async get(actor: string, key: string): Promise<StoredRun | undefined> {
    const k = `${actor}\u0000${key}`;
    const e = this.#entries.get(k);
    if (!e) return undefined;
    if (this.#now() - e.stored_at > this.#ttlMs) {
      this.#entries.delete(k);
      return undefined;
    }
    return e;
  }

  async put(actor: string, key: string, run: StoredRun): Promise<void> {
    if (this.#entries.size >= this.#max) {
      const oldest = this.#entries.keys().next().value;
      if (oldest !== undefined) this.#entries.delete(oldest);
    }
    this.#entries.set(`${actor}\u0000${key}`, run);
  }
}
