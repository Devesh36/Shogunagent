/** The slice of `cloudflare:test` (@cloudflare/vitest-pool-workers) the Workers-runtime suite
 * uses, typed locally so the package's `@cloudflare/workers-types` globals are not pulled into a
 * tsconfig that already carries `@types/node`. */
declare module "cloudflare:test" {
  export const fetchMock: {
    activate(): void;
    deactivate(): void;
    disableNetConnect(): void;
    assertNoPendingInterceptors(): void;
    get(origin: string): {
      intercept(options: { path: string; method: string }): { reply(status: number, body: string): unknown };
    };
  };
  export function createExecutionContext(): {
    waitUntil(promise: Promise<unknown>): void;
    passThroughOnException(): void;
    props: unknown;
  };
  export function waitOnExecutionContext(ctx: { waitUntil(promise: Promise<unknown>): void }): Promise<void>;
}
