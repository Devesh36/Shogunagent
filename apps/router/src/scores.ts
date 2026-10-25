/** Score components shared by `discover` (src/discover.ts) and the personal factor (src/personal.ts). */

export function priceScore(quoteMicroUsd: number): number {
  return 1 / (1 + Math.log(1 + Math.max(0, quoteMicroUsd) / 1000));
}

export function latencyScore(p95Ms: number): number {
  return 1 / (1 + Math.max(0, p95Ms) / 1000);
}
