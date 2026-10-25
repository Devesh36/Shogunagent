import { FetchGmailProvider, verifyGmailConfiguration } from "../src/gmail-local.js";
import { FileGmailStore, keychainSecret } from "../src/gmail-local-store.js";

// Safe operator diagnostic: authenticated metadata reads only; no mailbox read or Connect.
try {
  const store = new FileGmailStore();
  const state = store.load();
  const api = new FetchGmailProvider(keychainSecret("com.selectkk.shogun.gmail-backend", "composio-api-key"));
  const version = await verifyGmailConfiguration(api, state);
  store.save({ ...state, toolVersion: version });
  console.info(`Gmail configuration verified. Read tools pinned to ${version}. No mailbox data read. Run gmail:dev next.`);
} catch {
  console.error("Gmail configuration check failed. Check the project key permissions, Gmail ac_ ID, OAuth2 configuration and Keychain access. No provider error body was logged.");
  process.exit(1);
}
