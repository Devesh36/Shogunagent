/**
 * The router's boot report (E48): one fixed word per wiring decision, logged once per isolate by
 * src/worker.ts as `kind=catalog outcome=<word>`. Words only: no value, no URL, no count.
 *
 *   wallet_in_memory_dev | wallet_wired_with_outbox | wallet_outbox_missing
 *   idempotency_postgres | idempotency_in_memory
 *   connector_credentials_per_caller | connector_credentials_dev_shared | connector_credentials_none
 *   dev_shared_credentials_ignored   (the development flag is set on a production deployment)
 */
import type { RouterEnv } from "./env.js";
import { connectorSourceFor } from "./transport.js";

export function wiringWords(env: RouterEnv): string[] {
  const words: string[] = [];
  const db = Boolean(env.HYPERDRIVE?.connectionString);
  if (!env.ROUTER_WALLET_URL) words.push("wallet_in_memory_dev");
  else words.push(db ? "wallet_wired_with_outbox" : "wallet_outbox_missing");
  words.push(db ? "idempotency_postgres" : "idempotency_in_memory");
  const { source, ignoredFlag } = connectorSourceFor(env as unknown as Record<string, string | undefined>);
  words.push(source.mode === "per_caller" ? "connector_credentials_per_caller" : source.mode === "dev_shared" ? "connector_credentials_dev_shared" : "connector_credentials_none");
  if (ignoredFlag) words.push("dev_shared_credentials_ignored");
  return words;
}

