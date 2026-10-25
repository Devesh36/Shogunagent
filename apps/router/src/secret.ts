/**
 * An opaque credential wrapper (CLAUDE.md invariant 7). The agent's bearer enters the router as a
 * `Secret`; only src/metering.ts (to identify the actor) and src/transport.ts (to forward it to
 * the first-party state face) call `expose()`. It serialises as a redaction marker so it cannot
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
