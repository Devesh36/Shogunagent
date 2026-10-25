//! 匿名プロダクト分析（PostHog）。DAU/MAU 用の最小イベント送信。
//!
//! 大半は net 非依存の純ロジックで、Linux CI でテストできる。実 HTTPS 送信は
//! `net` feature の [`reqwest_transport::ReqwestTransport`] が担う。
//! CLAUDE.md invariant: 外部 egress は shogun-core に集約（FR-TR-03）。

use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub mod dream_batch;
pub mod funnel;
pub mod mount;
#[cfg(feature = "net")]
pub mod reqwest_transport;

/// The PostHog ingestion host every SHOGUN process sends to (debug builds may override it).
pub const DEFAULT_HOST: &str = "https://us.i.posthog.com";

/// What a missing `opt_out` field in `analytics.json` (or a missing file) means.
///
/// ShogunAgent E13-T4 (CLAUDE.md "No telemetry by default. Opt-in only"): the server build sends
/// nothing unless the file says `opt_out: false`. The ShogunAI desktop app keeps its opt-out
/// convention (the Settings switch writes the file; a file without the field is "on").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsentDefault {
    /// Desktop (feature `desktop`, not a server): a file without `opt_out` counts as consent.
    OptOut,
    /// Server (feature `daemon-server`, the default build without `desktop`, or
    /// [`SERVER_ENV`]`=1`): only an explicit `opt_out: false` is consent.
    OptIn,
}

/// Set to `1` to force the server posture ([`ConsentDefault::OptIn`]) on any build.
pub const SERVER_ENV: &str = "SHOGUNAGENT_SERVER";

/// The posture of this build and process (pure: the three inputs are passed in).
///
/// Server wins over desktop: a binary built with both `desktop` and `daemon-server` is a server,
/// and the environment variable turns any build into one. Only a desktop build that is neither
/// keeps the opt-out default.
pub fn consent_default_for(
    server_feature: bool,
    desktop_feature: bool,
    server_env: Option<&str>,
) -> ConsentDefault {
    let env_says_server = server_env.map(str::trim) == Some("1");
    if server_feature || env_says_server || !desktop_feature {
        ConsentDefault::OptIn
    } else {
        ConsentDefault::OptOut
    }
}

/// The posture of this process: the cargo features it was built with and [`SERVER_ENV`].
pub fn consent_default() -> ConsentDefault {
    let env = std::env::var(SERVER_ENV).ok();
    consent_default_for(
        cfg!(feature = "daemon-server"),
        cfg!(feature = "desktop"),
        env.as_deref(),
    )
}

/// 1イベントのプロパティ集合。
pub type Props = serde_json::Map<String, Value>;

/// 送信待ちの1イベント。`props` は共通プロパティ込みの完成形。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub name: String,
    pub props: Props,
}

/// 送信先の固定設定（セッション内不変）。
#[derive(Debug, Clone)]
pub struct AnalyticsConfig {
    /// PostHog project write key（`phc_...`）。公開安全な書き込み専用キー。
    pub api_key: String,
    /// 匿名の distinct_id（永続マシンUUID）。
    pub distinct_id: String,
}

/// PostHog キーの解決（純関数）。優先順位: 実行時 env → ビルド時埋め込み → None（無効）。
///
/// - `runtime`: プロセス起動時の `SHOGUN_POSTHOG_KEY`（ローカル開発での上書き用）
/// - `built_in`: 呼び出し側クレートで `option_env!("SHOGUN_POSTHOG_KEY")` によりビルド時に
///   埋め込まれたキー（配布ビルドの既定値。リリース CI がビルド環境に設定する）
///
/// 空文字・空白のみは「未設定」として次の候補へフォールバックする。両方無ければ `None` で
/// 分析は no-op（開発・OSS ビルドで無害）。キーは PostHog の write-only project key であり
/// シークレットではないが、リポジトリにはコミットしない（CI secret から注入）。
pub fn resolve_api_key(runtime: Option<&str>, built_in: Option<&str>) -> Option<String> {
    [runtime, built_in]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|k| !k.is_empty())
        .map(str::to_string)
}

/// PostHog `/batch` に投げる JSON ボディを組み立てる純関数。
///
/// 形式: `{ "api_key": ..., "batch": [ { "event", "distinct_id", "properties" }, ... ] }`
/// v1 はタイムスタンプを付与せず PostHog サーバ受信時刻を採用する。
pub fn build_batch_payload(config: &AnalyticsConfig, events: &[Event]) -> String {
    let batch: Vec<Value> = events
        .iter()
        .map(|e| {
            json!({
                "event": e.name,
                "distinct_id": config.distinct_id,
                "properties": e.props,
            })
        })
        .collect();
    json!({ "api_key": config.api_key, "batch": batch }).to_string()
}

/// バッチ送信の抽象。本番は reqwest、テストは Fake。
pub trait Transport: Send + 'static {
    /// 組み立て済み JSON ボディを送る。失敗は `Err(())`（本文は秘匿、内容は返さない）。
    /// Unit err は意図的：エラー内容を呼び出し元に漏らさない設計。
    #[allow(clippy::result_unit_err)]
    fn send_batch(&self, body: String) -> Result<(), ()>;
}

/// ワーカーの調律値。
const MAX_BATCH: usize = 20;
const FLUSH_INTERVAL: Duration = Duration::from_secs(3);
const MAX_RETRIES: u32 = 2;
const CONTROL_ACK_TIMEOUT: Duration = Duration::from_secs(5);

enum WorkerMessage {
    Event(Event),
    SetOptOut {
        value: bool,
        acknowledged: Sender<()>,
    },
}

/// 送信ワーカーのループ。`rx` が閉じたら残りをフラッシュして終了。
/// opt_out が true の間は受信イベントを破棄する。
fn run_worker<T: Transport>(
    rx: Receiver<WorkerMessage>,
    config: AnalyticsConfig,
    transport: T,
    opt_out: Arc<AtomicBool>,
) {
    let mut buf: Vec<Event> = Vec::new();
    let mut worker_opted_out = opt_out.load(Ordering::Acquire);
    loop {
        match rx.recv_timeout(FLUSH_INTERVAL) {
            Ok(WorkerMessage::Event(ev)) => {
                if !worker_opted_out && !opt_out.load(Ordering::Acquire) {
                    buf.push(ev);
                    if buf.len() >= MAX_BATCH {
                        flush_if_allowed(&config, &transport, &mut buf, worker_opted_out, &opt_out);
                    }
                }
            }
            Ok(WorkerMessage::SetOptOut {
                value,
                acknowledged,
            }) => {
                worker_opted_out = value;
                if value {
                    buf.clear();
                }
                let _ = acknowledged.send(());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if !buf.is_empty() {
                    flush_if_allowed(&config, &transport, &mut buf, worker_opted_out, &opt_out);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if !buf.is_empty() {
                    flush_if_allowed(&config, &transport, &mut buf, worker_opted_out, &opt_out);
                }
                return;
            }
        }
    }
}

fn flush_if_allowed<T: Transport>(
    config: &AnalyticsConfig,
    transport: &T,
    buf: &mut Vec<Event>,
    worker_opted_out: bool,
    opt_out: &AtomicBool,
) {
    if worker_opted_out || opt_out.load(Ordering::Acquire) {
        buf.clear();
    } else {
        flush(config, transport, buf);
    }
}

fn flush<T: Transport>(config: &AnalyticsConfig, transport: &T, buf: &mut Vec<Event>) {
    let body = build_batch_payload(config, buf);
    for _ in 0..=MAX_RETRIES {
        if transport.send_batch(body.clone()).is_ok() {
            break;
        }
    }
    // 成否に関わらずバッファは破棄（無限肥大を防ぐ — リングバッファ相当）。
    buf.clear();
}

/// 呼び出し元が使うハンドル。Clone 可・全 clone が同じチャネルと opt_out を共有。
#[derive(Clone)]
pub struct AnalyticsHandle {
    tx: Sender<WorkerMessage>,
    opt_out: Arc<AtomicBool>,
    control: Arc<Mutex<()>>,
    /// 全イベントに合流させる共通プロパティ（os/app_version/plan 等）。
    base: Props,
}

impl AnalyticsHandle {
    /// transport を注入してワーカースレッドを起こし、ハンドルを返す。
    /// `opt_out` は Handle とワーカーで共有する（トグルが即時反映される）。
    pub fn spawn_shared<T: Transport>(
        config: AnalyticsConfig,
        base: Props,
        opt_out: Arc<AtomicBool>,
        transport: T,
    ) -> Self {
        let (tx, rx) = mpsc::channel::<WorkerMessage>();
        let worker_opt_out = opt_out.clone();
        let _ = std::thread::Builder::new()
            .name("shogun-analytics".into())
            .spawn(move || run_worker(rx, config, transport, worker_opt_out));
        Self {
            tx,
            opt_out,
            control: Arc::new(Mutex::new(())),
            base,
        }
    }

    /// イベントを非ブロッキングで送る。opt_out 中は即破棄。共通プロパティを合流させる。
    pub fn capture(&self, event: &str, extra: Props) {
        if self.opt_out.load(Ordering::Acquire) {
            return;
        }
        let mut props = self.base.clone();
        for (k, v) in extra {
            props.insert(k, v);
        }
        let _ = self.tx.send(WorkerMessage::Event(Event {
            name: event.to_string(),
            props,
        }));
    }

    /// Linearize a privacy toggle through the worker and acknowledge only after its old buffer is
    /// cleared. A failed transition remains opted out on the capture side.
    pub fn set_opt_out(&self, value: bool) -> Result<(), &'static str> {
        if value {
            self.opt_out.store(true, Ordering::Release);
        }
        let _guard = self.control.lock().map_err(|_| {
            self.opt_out.store(true, Ordering::Release);
            "analytics control lock unavailable"
        })?;
        let (acknowledged, acknowledgement) = mpsc::channel();
        self.tx
            .send(WorkerMessage::SetOptOut {
                value,
                acknowledged,
            })
            .map_err(|_| {
                self.opt_out.store(true, Ordering::Release);
                "analytics worker unavailable"
            })?;
        acknowledgement
            .recv_timeout(CONTROL_ACK_TIMEOUT)
            .map_err(|_| {
                self.opt_out.store(true, Ordering::Release);
                "analytics worker did not acknowledge privacy setting"
            })?;
        self.opt_out.store(value, Ordering::Release);
        Ok(())
    }

    /// v1 は口だけ用意（アカウント基盤到来時に匿名→アカウントIDのマージへ差し替え）。
    /// 現状は `$identify` イベントを1件送るだけ。
    pub fn identify(&self, account_id: &str) {
        let mut p = Props::new();
        p.insert("account_id".into(), Value::String(account_id.to_string()));
        self.capture("$identify", p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> AnalyticsConfig {
        AnalyticsConfig { api_key: "phc_test".into(), distinct_id: "abc-123".into() }
    }

    #[test]
    fn payload_has_api_key_and_one_batch_entry_per_event() {
        let mut props = Props::new();
        props.insert("cold_start".into(), Value::Bool(true));
        let events = vec![Event { name: "app_opened".into(), props }];

        let body = build_batch_payload(&cfg(), &events);
        let parsed: Value = serde_json::from_str(&body).unwrap();

        assert_eq!(parsed["api_key"], "phc_test");
        assert_eq!(parsed["batch"].as_array().unwrap().len(), 1);
        assert_eq!(parsed["batch"][0]["event"], "app_opened");
        assert_eq!(parsed["batch"][0]["distinct_id"], "abc-123");
        assert_eq!(parsed["batch"][0]["properties"]["cold_start"], true);
    }

    #[test]
    fn empty_events_produce_empty_batch() {
        let body = build_batch_payload(&cfg(), &[]);
        let parsed: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["batch"].as_array().unwrap().len(), 0);
    }

    // resolve_api_key: 実行時 env → ビルド時埋め込み → None

    #[test]
    fn runtime_key_wins_over_built_in() {
        assert_eq!(
            resolve_api_key(Some("phc_runtime"), Some("phc_builtin")),
            Some("phc_runtime".to_string())
        );
    }

    #[test]
    fn built_in_key_is_the_distributed_default() {
        assert_eq!(resolve_api_key(None, Some("phc_builtin")), Some("phc_builtin".to_string()));
    }

    #[test]
    fn empty_or_whitespace_runtime_falls_back_to_built_in() {
        assert_eq!(resolve_api_key(Some(""), Some("phc_builtin")), Some("phc_builtin".to_string()));
        assert_eq!(
            resolve_api_key(Some("  \t"), Some("phc_builtin")),
            Some("phc_builtin".to_string())
        );
    }

    #[test]
    fn no_key_anywhere_disables_analytics() {
        assert_eq!(resolve_api_key(None, None), None);
        assert_eq!(resolve_api_key(Some(""), Some("")), None);
    }

    #[test]
    fn resolved_key_is_trimmed() {
        assert_eq!(resolve_api_key(Some(" phc_x "), None), Some("phc_x".to_string()));
    }

    // consent_default_for (E13-T4): the server posture is opt-in; only a pure desktop build keeps
    // the opt-out default.

    #[test]
    fn a_server_build_is_opt_in_whatever_else_is_on() {
        assert_eq!(consent_default_for(true, false, None), ConsentDefault::OptIn);
        assert_eq!(consent_default_for(true, true, None), ConsentDefault::OptIn);
        assert_eq!(consent_default_for(true, true, Some("0")), ConsentDefault::OptIn);
    }

    #[test]
    fn the_default_build_without_desktop_is_a_server() {
        assert_eq!(consent_default_for(false, false, None), ConsentDefault::OptIn);
    }

    #[test]
    fn the_environment_turns_a_desktop_build_into_a_server() {
        assert_eq!(consent_default_for(false, true, Some("1")), ConsentDefault::OptIn);
        assert_eq!(consent_default_for(false, true, Some(" 1 ")), ConsentDefault::OptIn);
    }

    #[test]
    fn only_a_plain_desktop_build_keeps_the_opt_out_default() {
        assert_eq!(consent_default_for(false, true, None), ConsentDefault::OptOut);
        assert_eq!(consent_default_for(false, true, Some("")), ConsentDefault::OptOut);
        assert_eq!(consent_default_for(false, true, Some("0")), ConsentDefault::OptOut);
        assert_eq!(consent_default_for(false, true, Some("yes")), ConsentDefault::OptOut);
    }

    #[test]
    fn this_build_reports_the_posture_its_features_imply() {
        // Without `desktop` (the ShogunAgent default) or with `daemon-server`, nothing is sent
        // unless the file opts in. The env override is not set by the test runner.
        let expected = if cfg!(feature = "daemon-server") || !cfg!(feature = "desktop") {
            ConsentDefault::OptIn
        } else {
            ConsentDefault::OptOut
        };
        if std::env::var_os(SERVER_ENV).is_none() {
            assert_eq!(consent_default(), expected);
        }
    }

    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// 送られたボディを記録する Fake。
    #[derive(Clone, Default)]
    struct FakeTransport {
        sent: Arc<Mutex<Vec<String>>>,
    }
    impl Transport for FakeTransport {
        fn send_batch(&self, body: String) -> Result<(), ()> {
            self.sent.lock().unwrap().push(body);
            Ok(())
        }
    }

    #[test]
    fn capture_merges_base_props_and_flushes_on_shutdown() {
        let mut base = Props::new();
        base.insert("app_version".into(), "0.0.0".into());
        let opt_out = Arc::new(AtomicBool::new(false));
        let fake = FakeTransport::default();
        let sent = fake.sent.clone();

        let handle = AnalyticsHandle::spawn_shared(cfg(), base, opt_out.clone(), fake);
        let mut p = Props::new();
        p.insert("cold_start".into(), serde_json::Value::Bool(true));
        handle.capture("app_opened", p);
        drop(handle); // チャネルを閉じてワーカーを終了 → 最終フラッシュ

        for _ in 0..50 {
            if !sent.lock().unwrap().is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let bodies = sent.lock().unwrap();
        assert_eq!(bodies.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&bodies[0]).unwrap();
        assert_eq!(parsed["batch"][0]["event"], "app_opened");
        assert_eq!(parsed["batch"][0]["properties"]["app_version"], "0.0.0");
        assert_eq!(parsed["batch"][0]["properties"]["cold_start"], true);
    }

    #[test]
    fn opt_out_drops_events() {
        let opt_out = Arc::new(AtomicBool::new(true));
        let fake = FakeTransport::default();
        let sent = fake.sent.clone();
        let handle = AnalyticsHandle::spawn_shared(cfg(), Props::new(), opt_out.clone(), fake);
        handle.capture("app_opened", Props::new());
        drop(handle);
        std::thread::sleep(Duration::from_millis(120));
        assert!(sent.lock().unwrap().is_empty());
    }

    #[test]
    fn queued_event_is_cleared_before_opt_out_acknowledgement() {
        let opt_out = Arc::new(AtomicBool::new(false));
        let fake = FakeTransport::default();
        let sent = fake.sent.clone();
        let handle = AnalyticsHandle::spawn_shared(cfg(), Props::new(), opt_out, fake);

        handle.capture("app_opened", Props::new());
        assert!(handle.set_opt_out(true).is_ok());
        drop(handle);

        assert!(sent.lock().unwrap().is_empty());
    }

    #[test]
    fn opt_out_at_flush_boundary_clears_without_sending() {
        let opt_out = AtomicBool::new(true);
        let fake = FakeTransport::default();
        let mut buf = vec![Event {
            name: "app_opened".into(),
            props: Props::new(),
        }];

        flush_if_allowed(&cfg(), &fake, &mut buf, false, &opt_out);

        assert!(buf.is_empty());
        assert!(fake.sent.lock().unwrap().is_empty());
    }
}
