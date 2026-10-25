//! The fixed Keychain accounts SHOGUN writes, sorted into the ones "Delete everything" removes
//! and the ones it keeps on purpose (issue #780). Plain data, compiled on every platform; the
//! macOS Keychain code (`keychain_store`) re-exports both lists.

/// Every fixed Keychain account that holds a secret the user supplied or a connection produced
/// (issue #780). "Delete everything" removes each one; the per-service `<source>-tokenset` OAuth
/// sets are removed through the token store as well. A test lists every account constant in the
/// tree and fails until a new one is placed here or in [`KEPT_ACCOUNTS`].
pub const USER_SECRET_ACCOUNTS: &[&str] = &[
    // BYOK chat/draft provider keys (one per provider).
    "anthropic-byok",
    "openrouter-byok",
    "openai-byok",
    "gemini-byok",
    // Meeting transcription and dictation clean-up keys (the second is the pre-rename account).
    "deepgram-asr",
    "jev-forms-byok",
    "voice-edit-groq-byok",
    "groq-byok",
    // The user's own Google OAuth client for Calendar / Drive.
    "google-oauth-client-id",
    "google-oauth-client-secret",
    "slack-oauth-client-id",
    "slack-oauth-client-secret",
    "notion-oauth-client",
    "granola-oauth-client",
    "github-app-client-id",
    "linear-app-client-id",
    // Mail's third-party key, and the verifiers of every Memory API token issued to other apps.
    "composio-api-key",
    // Development-only credential for the operator-managed loopback Gmail backend.
    "gmail-local-backend-token",
    "memory-api-tokens",
    // The Batch relay bearer a principal's hourly Dream Cycle presents (`token_store`, E5-T5).
    "batch-relay-bearer",
    // Legacy: the background-service key a development build could hold.
    "select-kk-batch",
];

/// Accounts "Delete everything" keeps on purpose: the database key (the emptied database must
/// still open), the licence key and its pending claim (the plan the user paid for is not their
/// captured data), and the Keychain mirrors of the trial clock and the time high-water mark
/// (audit 2026-10-10 H2, 2026-10-20 M7: deleting everything must not restart the 7-day trial or
/// let a clock set back revive a plan). The account session and a sign-out still waiting
/// to reach the server (#148 P2) are kept for the same reason as the licence key: "Delete
/// everything" deletes what this Mac captured, not the plan or the account (signing out is its
/// own action, Settings › Account).
pub const KEPT_ACCOUNTS: &[&str] = &[
    "memory-db-key",
    "license-key",
    "license-claim",
    "trial-started-at",
    // Audit 2026-10-20 M7: the time high-water mark, for the same reason as the trial stamp.
    "time-high-water-mark",
    // Audit 2026-10-20 M6: which settings files have been sealed (ids only).
    "settings-sealed",
    "account-session",
    "account-signout-pending",
    // The local Google display profile (name, photo link; no image bytes) follows the session:
    // kept while it is, cleared by sign-out (`clear_local_profile`).
    "account-local-profile",
];
