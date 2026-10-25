import { serve } from "@hono/node-server";
import { Server } from "node:http";
import { createLocalGmailApp, FetchGmailProvider, GMAIL_PORT } from "./gmail-local.js";
import { FileGmailStore, keychainSecret } from "./gmail-local-store.js";

try {
  if (process.platform !== "darwin") throw new Error("macos_keychain_required");
  const store = new FileGmailStore();
  const configuration = store.load();
  const bearer = keychainSecret("com.selectkk.shogun", "gmail-local-backend-token");
  const projectKey = keychainSecret("com.selectkk.shogun.gmail-backend", "composio-api-key");
  const toolVersion = configuration.toolVersion ?? "";
  const app = createLocalGmailApp({ bearer, store, provider: new FetchGmailProvider(projectKey), toolVersion });
  const server = serve({ fetch: app.fetch, hostname: "127.0.0.1", port: GMAIL_PORT }, () => {
    console.info(`Gmail test backend ready at http://127.0.0.1:${GMAIL_PORT} (authenticated, read-only)`);
  });
  if (!(server instanceof Server)) throw new Error("http_server_required");
  server.requestTimeout = 15_000;
  server.headersTimeout = 10_000;
  server.timeout = 20_000;
  server.on("error", () => { console.error("gmail_backend_listen_failed_check_port_4319"); process.exit(1); });
  for (const signal of ["SIGINT", "SIGTERM"] as const) {
    process.once(signal, () => {
      server.close(() => process.exit(0));
      setTimeout(() => process.exit(0), 2000).unref();
    });
  }
} catch {
  console.error("Gmail backend refused startup. Run gmail:setup, unlock Keychain, and run gmail:check-config. See docs/gmail-local-backend.md.");
  process.exit(1);
}
