//! Per-client rate limit on Memory API calls that start a model call (2026-10-20 cost audit).
//!
//! A local caller with a `full` connection or token could queue model work without bound:
//! `translate.text`, `translate.reply_draft`, `memory.search` with `mode: "ai"`, `skills.run` and
//! `forms.plan` each start a provider call billed to the company key or the user's own key. Every
//! face (MCP, REST and so the CLI) asks [`LlmRateLimiter::check`] before such a call, after
//! authentication, plan and scope, keyed by the caller's identity ([`ClientKey`]): the app's
//! connection for a `--client` session, the token's id for a token. Other tools are not limited
//! here.
//!
//! The window is shared across processes through a small file beside the memory DB
//! ([`STORE_FILE`], timestamps and client keys only, never a token or any content), so spawning
//! many `shogun-mcp` processes for one connection does not multiply the allowance. When the file
//! cannot be locked, read or written, the limiter still counts in memory for this process: a
//! broken file never lifts the limit, it only stops sharing it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::memory_api::Tool;

/// Model-starting calls one client may make per [`WINDOW_MS`]. Generous for an agent that
/// translates or searches interactively (one call every two seconds, sustained), far below what
/// a runaway loop would queue.
pub const LLM_CALLS_PER_WINDOW: usize = 30;
/// The sliding window, ms.
pub const WINDOW_MS: u64 = 60_000;
/// The shared window file, next to the memory DB.
pub const STORE_FILE: &str = "llm_rate.json";
/// The machine code every face answers a limited call with.
pub const RATE_LIMITED: &str = "rate_limited";
/// The sentence that goes with [`RATE_LIMITED`].
pub const RATE_LIMITED_MESSAGE: &str = "Too many AI requests from this app in the last minute. \
Wait a moment and try again.";
/// Most clients the shared file keeps a window for; the least recently active are dropped.
const MAX_CLIENTS: usize = 64;
/// A window file larger than this is not ours: it is ignored and rewritten.
const MAX_STORE_BYTES: u64 = 256 * 1024;

/// Whether a call to `tool` starts a model call. `search_mode` is `memory.search`'s `mode`
/// argument (`"ai"` widens and reranks on a model; anything else is a local keyword search).
pub fn triggers_llm(tool: Tool, search_mode: Option<&str>) -> bool {
    match tool {
        Tool::TranslateText | Tool::TranslateReplyDraft | Tool::SkillsRun | Tool::FormsPlan => true,
        Tool::MemorySearch => search_mode == Some("ai"),
        _ => false,
    }
}

/// Who a limited call is counted against. Never a token or a verifier: a token is named by its
/// issued id, a connection by its connection id.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClientKey(String);

impl ClientKey {
    /// An app connection (`shogun-mcp --client <id>`), by its connection id.
    pub fn connection(client: &str) -> Self {
        let id = crate::memory_api_settings::grant_id(client).unwrap_or(client);
        Self(format!("client:{id}"))
    }

    /// An issued token, by its id ([`crate::memory_api::TokenListing::id`]).
    pub fn token(id: &str) -> Self {
        Self(format!("token:{id}"))
    }

    /// A caller that is neither (the desktop's own in-process faces and tests).
    pub fn local() -> Self {
        Self("local".to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Admit one call at `now_ms` into `window` (timestamps of earlier admitted calls), or say how
/// many ms until the oldest one leaves the window. Pure; prunes `window` as a side effect.
pub fn admit(window: &mut Vec<u64>, now_ms: u64, limit: usize, window_ms: u64) -> Result<(), u64> {
    let start = now_ms.saturating_sub(window_ms);
    // A timestamp from the future (a clock moved back) still counts until it ages out of a
    // window measured from itself, so turning the clock back does not reset the allowance.
    window.retain(|&at| at > start || at > now_ms);
    if window.len() >= limit {
        let oldest = window.iter().copied().min().unwrap_or(now_ms);
        let retry = oldest.saturating_add(window_ms).saturating_sub(now_ms);
        return Err(retry.clamp(1, window_ms.max(1)));
    }
    window.push(now_ms);
    Ok(())
}

/// The per-client limiter every face holds. See the module docs.
#[derive(Debug)]
pub struct LlmRateLimiter {
    path: Option<PathBuf>,
    limit: usize,
    window_ms: u64,
    memory: Mutex<HashMap<String, Vec<u64>>>,
}

impl Default for LlmRateLimiter {
    fn default() -> Self {
        Self::in_memory()
    }
}

impl LlmRateLimiter {
    /// A limiter for this process only (the desktop's in-process faces, tests).
    pub fn in_memory() -> Self {
        Self {
            path: None,
            limit: LLM_CALLS_PER_WINDOW,
            window_ms: WINDOW_MS,
            memory: Mutex::new(HashMap::new()),
        }
    }

    /// A limiter whose window is shared through `path` with every other process using it.
    pub fn shared(path: PathBuf) -> Self {
        Self { path: Some(path), ..Self::in_memory() }
    }

    /// Override the limit and window (tests).
    #[must_use]
    pub fn with_limit(mut self, limit: usize, window_ms: u64) -> Self {
        self.limit = limit.max(1);
        self.window_ms = window_ms.max(1);
        self
    }

    /// Count one model-starting call by `client` at `now_ms`: `Ok` to go ahead, `Err(ms)` to
    /// refuse it with the wait until the next call would be admitted.
    pub fn check(&self, client: &ClientKey, now_ms: u64) -> Result<(), u64> {
        // The in-process window always counts, so a shared file that cannot be used never lifts
        // the limit for this process.
        let local = {
            let mut memory = match self.memory.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            let window = memory.entry(client.as_str().to_string()).or_default();
            admit(window, now_ms, self.limit, self.window_ms)
        };
        let Some(path) = &self.path else {
            return local;
        };
        match self.check_shared(path, client, now_ms) {
            Some(shared) => {
                // A refusal from either window refuses the call. A local refusal already did not
                // record the call; a shared refusal leaves the local count one high, which only
                // errs towards refusing.
                local.and(shared)
            }
            None => local,
        }
    }

    /// The shared window's verdict, or `None` when the file could not be used.
    fn check_shared(&self, path: &Path, client: &ClientKey, now_ms: u64) -> Option<Result<(), u64>> {
        let _lock = crate::settings_file::lock(path, "rate window busy").ok()?;
        let mut windows = load(path);
        let verdict = {
            let window = windows.entry(client.as_str().to_string()).or_default();
            admit(window, now_ms, self.limit, self.window_ms)
        };
        let start = now_ms.saturating_sub(self.window_ms);
        windows.retain(|_, stamps| stamps.iter().any(|&at| at > start || at > now_ms));
        if windows.len() > MAX_CLIENTS {
            let mut by_recent: Vec<(String, u64)> = windows
                .iter()
                .map(|(key, stamps)| (key.clone(), stamps.iter().copied().max().unwrap_or(0)))
                .collect();
            by_recent.sort_by_key(|entry| std::cmp::Reverse(entry.1));
            let keep: std::collections::HashSet<String> =
                by_recent.into_iter().take(MAX_CLIENTS).map(|(key, _)| key).collect();
            windows.retain(|key, _| keep.contains(key));
        }
        save(path, &windows).ok()?;
        Some(verdict)
    }
}

fn load(path: &Path) -> HashMap<String, Vec<u64>> {
    let small = std::fs::metadata(path).is_ok_and(|meta| meta.len() <= MAX_STORE_BYTES);
    if !small {
        return HashMap::new();
    }
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn save(path: &Path, windows: &HashMap<String, Vec<u64>>) -> Result<(), String> {
    let text = serde_json::to_string(windows).map_err(|_| "rate window encode".to_string())?;
    // Not durable: losing the last minute's window to a power cut only resets a rate limit.
    crate::settings_file::write_atomic(path, text.as_bytes(), false)
}

/// Where the shared window lives for a face whose memory DB is `db_path`.
pub fn resolve_store_path(db_path: &str) -> PathBuf {
    Path::new(db_path)
        .parent()
        .map(|dir| dir.join(STORE_FILE))
        .unwrap_or_else(|| PathBuf::from(STORE_FILE))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("shogun-rate-{tag}-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn only_model_starting_calls_are_limited() {
        assert!(triggers_llm(Tool::TranslateText, None));
        assert!(triggers_llm(Tool::TranslateReplyDraft, None));
        assert!(triggers_llm(Tool::SkillsRun, None));
        assert!(triggers_llm(Tool::FormsPlan, None));
        assert!(triggers_llm(Tool::MemorySearch, Some("ai")));
        assert!(!triggers_llm(Tool::MemorySearch, Some("keyword")));
        assert!(!triggers_llm(Tool::MemorySearch, None));
        assert!(!triggers_llm(Tool::MemoryAppendNote, None));
        assert!(!triggers_llm(Tool::SkillsRunStatus, None));
    }

    #[test]
    fn a_window_admits_the_limit_then_refuses_until_the_oldest_ages_out() {
        let mut window = Vec::new();
        for i in 0..3 {
            assert_eq!(admit(&mut window, 1_000 + i, 3, 60_000), Ok(()));
        }
        assert_eq!(admit(&mut window, 2_000, 3, 60_000), Err(59_000));
        assert_eq!(window.len(), 3, "a refusal is not recorded");
        assert_eq!(admit(&mut window, 61_000, 3, 60_000), Ok(()), "the first aged out");
        assert_eq!(admit(&mut window, 61_000, 3, 60_000), Err(1));
    }

    #[test]
    fn turning_the_clock_back_does_not_reset_the_allowance() {
        let mut window = Vec::new();
        for _ in 0..3 {
            admit(&mut window, 500_000, 3, 60_000).unwrap();
        }
        assert!(admit(&mut window, 100_000, 3, 60_000).is_err());
    }

    #[test]
    fn each_client_has_its_own_window_and_thirty_a_minute_is_the_default() {
        let limiter = LlmRateLimiter::in_memory();
        let cursor = ClientKey::connection("cursor");
        let other = ClientKey::token("t1");
        for i in 0..LLM_CALLS_PER_WINDOW as u64 {
            assert_eq!(limiter.check(&cursor, 10_000 + i), Ok(()));
        }
        assert!(limiter.check(&cursor, 20_000).is_err());
        assert_eq!(limiter.check(&other, 20_000), Ok(()), "another client is unaffected");
        assert_eq!(limiter.check(&cursor, 10_000 + WINDOW_MS + 30), Ok(()));
    }

    #[test]
    fn apps_sharing_a_connection_share_its_window() {
        assert_eq!(ClientKey::connection("chatgpt-desktop"), ClientKey::connection("codex-cli"));
        assert_ne!(ClientKey::connection("cursor"), ClientKey::token("cursor"));
    }

    #[test]
    fn the_shared_window_spans_processes_and_holds_no_secret() {
        let dir = TempDir::new("shared");
        let path = dir.path().join(STORE_FILE);
        let a = LlmRateLimiter::shared(path.clone()).with_limit(2, 60_000);
        let b = LlmRateLimiter::shared(path.clone()).with_limit(2, 60_000);
        let key = ClientKey::connection("cursor");
        assert_eq!(a.check(&key, 1_000), Ok(()));
        assert_eq!(b.check(&key, 1_001), Ok(()));
        assert!(b.check(&key, 1_002).is_err(), "the second process sees the first one's call");
        assert!(a.check(&key, 1_003).is_err());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("client:cursor"), "{text}");
    }

    #[test]
    fn a_broken_shared_file_still_limits_in_memory() {
        let dir = TempDir::new("broken");
        // A directory where the file should be: it can never be read or replaced.
        let path = dir.path().join(STORE_FILE);
        std::fs::create_dir_all(&path).unwrap();
        let limiter = LlmRateLimiter::shared(path).with_limit(1, 60_000);
        let key = ClientKey::local();
        assert_eq!(limiter.check(&key, 1), Ok(()));
        assert!(limiter.check(&key, 2).is_err());
    }

    #[test]
    fn garbage_in_the_shared_file_is_replaced() {
        let dir = TempDir::new("garbage");
        let path = dir.path().join(STORE_FILE);
        std::fs::write(&path, "not json").unwrap();
        let limiter = LlmRateLimiter::shared(path.clone()).with_limit(1, 60_000);
        assert_eq!(limiter.check(&ClientKey::local(), 1), Ok(()));
        assert!(std::fs::read_to_string(&path).unwrap().starts_with('{'));
    }
}
