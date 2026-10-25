/**
 * An opaque credential wrapper (CLAUDE.md invariant 7). The agent's bearer enters the router as a
 * `Secret`; only src/metering.ts and src/wallet.ts (to identify the actor as a hash) and
 * src/transport.ts (to forward it to the first-party state face, and to apps/api's wallet in
 * `X-Caller-Authorization`, E32) call `expose()`. It serialises as a redaction marker so it cannot
 * reach a log line, a run record or an idempotency entry by accident.
 */
export class Secret {
  readonly #value: string;

  constructor(value: string) {
    this.#value = value;
  }

  expose(): string {
    return this.#value;
  }

  toJSON(): string {
    return "[redacted]";
  }

  toString(): string {
    return "[redacted]";
  }

  [Symbol.for("nodejs.util.inspect.custom")](): string {
    return "Secret([redacted])";
  }
}
