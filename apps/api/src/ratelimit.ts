/**
 * Per-licence request rate limiting (docs/batch-relay-design.md §2.2, audit P1).
 *
 * The daily chunk cap bounds how much a licence can spend in a day; this bounds how fast. They
 * are different guards: 50 parallel submissions can respect the cap and still knock the relay
 * (and the Anthropic account's own rate limits) over, and a leaked token's first move is a loop.
 *
 * `TokenBucketLimiter` is in-memory and only correct for a single relay process. With the shared
 * store configured (runbook §9-4) `SqlRateLimiter` in `pg-store.ts` holds the same buckets in
 * Postgres so every instance draws from one budget. The clock
 * is a parameter, so the whole thing is deterministic under test.
 */

export interface RateLimiter {
  /** Consume one slot for `key` at `nowMs`. `false` means the caller must back off. May be async
   * for a shared-store limiter (issue #29 FR-SVC-04); the app always awaits it. */
  take(key: string, nowMs: number): boolean | Promise<boolean>;
  /** Whole seconds until `key` has a slot again — the `Retry-After` value. Only meaningful
   * immediately after [`take`] returned `false`; a client told to back off with no interval
   * retries as fast as it can, which is the loop the bucket exists to stop. */
  retryAfterSecs(key: string, nowMs: number): number | Promise<number>;
}

/** Never limits. The default when a deployment has its own edge rate limiting in front. */
export class NoRateLimit implements RateLimiter {
  take(): boolean {
    return true;
  }

  retryAfterSecs(): number {
    return 0;
  }
}

interface Bucket {
  /** Tokens left, fractional between refills. */
  tokens: number;
  /** When `tokens` was last brought up to date. Doubles as the idle-sweep timestamp. */
  at: number;
}

/** Buckets idle for this long are dropped on the next sweep — the map must not grow with every
 * licence that ever called. */
const IDLE_EVICT_MS = 10 * 60_000;
/** Sweep at most this often, and only when the map is worth sweeping. */
const SWEEP_EVERY_MS = 60_000;
const SWEEP_MIN_SIZE = 256;

export class TokenBucketLimiter implements RateLimiter {
  private readonly buckets = new Map<string, Bucket>();
  private lastSweep = 0;

  /** `burst` is the bucket size: how many requests may arrive back-to-back. */
  constructor(
    private readonly burst: number,
    private readonly perMinute: number,
  ) {}

  take(key: string, nowMs: number): boolean {
    this.sweep(nowMs);
    const b = this.buckets.get(key);
    if (!b) {
      this.buckets.set(key, { tokens: this.burst - 1, at: nowMs });
      return true;
    }
    const refill = ((nowMs - b.at) / 60_000) * this.perMinute;
    b.tokens = Math.min(this.burst, b.tokens + Math.max(0, refill));
    b.at = nowMs;
    if (b.tokens < 1) return false;
    b.tokens -= 1;
    return true;
  }

  retryAfterSecs(key: string, nowMs: number): number {
    const b = this.buckets.get(key);
    if (!b || this.perMinute <= 0) return 1;
    // `take` already refilled the bucket up to `nowMs`, so `tokens` is current.
    const refill = ((nowMs - b.at) / 60_000) * this.perMinute;
    const tokens = Math.min(this.burst, b.tokens + Math.max(0, refill));
    if (tokens >= 1) return 0;
    // At least a second: a sub-second Retry-After rounds to 0 and reads as "retry now".
    return Math.max(1, Math.ceil(((1 - tokens) / this.perMinute) * 60));
  }

  private sweep(nowMs: number): void {
    if (this.buckets.size < SWEEP_MIN_SIZE || nowMs - this.lastSweep < SWEEP_EVERY_MS) return;
    this.lastSweep = nowMs;
    for (const [key, b] of this.buckets) {
      if (nowMs - b.at > IDLE_EVICT_MS) this.buckets.delete(key);
    }
  }
}

/**
 * A fixed-window limit: at most `limit` takes per key in each `windowMs` window (aligned to the
 * epoch, so an hourly window is a UTC clock hour). Used where a token bucket's idle eviction
 * would undo the limit: a 30-per-hour bucket evicted after ten idle minutes would start full
 * again. The bound is `limit` per window, so up to twice that across one window boundary.
 */
export class WindowLimiter implements RateLimiter {
  private readonly windows = new Map<string, { window: number; used: number }>();

  constructor(
    private readonly limit: number,
    private readonly windowMs: number,
  ) {}

  take(key: string, nowMs: number): boolean {
    const window = Math.floor(nowMs / this.windowMs);
    // One entry per key; a stale window is simply overwritten, so the map never outgrows the keys.
    const entry = this.windows.get(key);
    const used = entry && entry.window === window ? entry.used : 0;
    if (used >= this.limit) return false;
    this.windows.set(key, { window, used: used + 1 });
    return true;
  }

  retryAfterSecs(_key: string, nowMs: number): number {
    const end = (Math.floor(nowMs / this.windowMs) + 1) * this.windowMs;
    return Math.max(1, Math.ceil((end - nowMs) / 1000));
  }
}

/** Gives back one in-flight slot. Safe to call more than once. */
export type InFlightRelease = () => Promise<void>;

/** A per-key concurrency limit. `acquire` answers a release function, or `undefined` when the key
 * already has its maximum in flight. */
export interface InFlightLimiter {
  acquire(key: string, nowMs: number): Promise<InFlightRelease | undefined>;
}

/** Never limits. */
export class NoInFlightLimit implements InFlightLimiter {
  acquire(): Promise<InFlightRelease> {
    return Promise.resolve(() => Promise.resolve());
  }
}

/** In-process concurrency limit (the Node entry and tests). On Workers each request builds its own
 * app, so production uses the shared-store `SqlInFlightLimiter` (pg-store.ts) instead. */
export class InMemoryInFlight implements InFlightLimiter {
  private readonly held = new Map<string, number>();

  constructor(private readonly max: number) {}

  acquire(key: string): Promise<InFlightRelease | undefined> {
    const n = this.held.get(key) ?? 0;
    if (n >= this.max) return Promise.resolve(undefined);
    this.held.set(key, n + 1);
    let released = false;
    return Promise.resolve(() => {
      if (!released) {
        released = true;
        const left = (this.held.get(key) ?? 1) - 1;
        if (left <= 0) this.held.delete(key);
        else this.held.set(key, left);
      }
      return Promise.resolve();
    });
  }
}
