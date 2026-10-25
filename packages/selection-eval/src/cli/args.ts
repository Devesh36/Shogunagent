/** `--key value` / `--key=value` / `--flag` into a map. Unknown keys are kept so CLIs can validate. */
export function parseArgs(argv: string[]): Map<string, string | true> {
  const out = new Map<string, string | true>();
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (!a || !a.startsWith("--")) continue;
    const eq = a.indexOf("=");
    if (eq !== -1) {
      out.set(a.slice(2, eq), a.slice(eq + 1));
      continue;
    }
    const next = argv[i + 1];
    if (next !== undefined && !next.startsWith("--")) {
      out.set(a.slice(2), next);
      i++;
    } else out.set(a.slice(2), true);
  }
  return out;
}

export function str(args: Map<string, string | true>, key: string): string | undefined {
  const v = args.get(key);
  return typeof v === "string" ? v : undefined;
}

export function flag(args: Map<string, string | true>, key: string): boolean {
  return args.has(key);
}
