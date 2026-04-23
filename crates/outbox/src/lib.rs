//! execlaw-outbox
//!
//! Effect relay — drains `state_outbox`, dispatches effects to registered
//! consumers (the `Dispatcher` trait), honors framework-minted idempotency
//! keys and exponential backoff (§2.4, §2.15 of MIGRATION_PLAN.md).
//!
//! The LLM never calls external APIs directly. Instead, a turn commits
//! an outbox row inside the SQLite transaction that commits the turn; the
//! drain loop runs in a separate tokio task and delivers with at-least-
//! once semantics. Consumer-side inbox dedup (see `OutboxStore::
//! inbox_record_if_new` in `execlaw-core`) makes this effectively exactly-
//! once at the sink.
//!
//! **No cloud SDKs.** Dispatchers are our own Rust code; transport plugins
//! are registered via the hook framework (§4.2), not imported crates.

#![forbid(unsafe_code)]

use async_trait::async_trait;
use execlaw_core::db::Database;
use execlaw_core::events::{EventKind, EventLog, EventRecord, PendingEvent, Snapshot};
use execlaw_core::ids::{ConversationId, EventSeq};
use execlaw_core::outbox::{OutboxRow, OutboxStore};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Backoff + retry budget (Phase 0 primitives kept, expanded)
// ---------------------------------------------------------------------------

/// How long to wait before retrying a failed effect.
///
/// Baseline: 1s, doubled per attempt, capped at 10 minutes. After
/// `max_attempts`, the caller should move the row to `dead_letter` and
/// fire an Error alert.
pub fn exp_backoff(attempt: u32) -> Duration {
    const BASE_MS: u64 = 1_000;
    const CAP_MS: u64 = 10 * 60 * 1_000;
    let ms = BASE_MS.saturating_mul(1u64.checked_shl(attempt.min(15)).unwrap_or(1));
    Duration::from_millis(ms.min(CAP_MS))
}

#[derive(Debug, Clone, Copy)]
pub struct RetryBudget {
    pub max_attempts: u32,
}

impl Default for RetryBudget {
    fn default() -> Self {
        // §2.15: "Per-effect retry budget: 5 attempts with exponential
        // backoff to a ceiling, then move to a dead_letter table."
        Self { max_attempts: 5 }
    }
}

impl RetryBudget {
    pub fn should_dead_letter(&self, attempts: u32) -> bool {
        attempts >= self.max_attempts
    }
}

// ---------------------------------------------------------------------------
// Dispatcher trait — one per effect kind
// ---------------------------------------------------------------------------

/// A registered handler for one effect kind (e.g., `"transport.send"`,
/// `"schedule.wakeup"`). Implementations receive an outbox row and either
/// deliver it successfully (returns Ok) or report a reason it should retry
/// (returns Err — the drain loop applies the retry budget).
#[async_trait]
pub trait Dispatcher: Send + Sync {
    /// The effect_kind this dispatcher handles.
    fn effect_kind(&self) -> &'static str;

    /// Deliver the effect. Errors trigger the retry budget logic.
    async fn dispatch(&self, row: &OutboxRow) -> Result<(), String>;
}

// ---------------------------------------------------------------------------
// Wakeup dispatcher — the canonical built-in
// ---------------------------------------------------------------------------

/// Payload for a `schedule.wakeup` effect. The wakeup dispatcher appends
/// a `Wakeup` event to the conversation's log, which the worker treats as
/// a resume event (§2.10 of the plan — "scheduled wakeups are just resume
/// events").
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WakeupPayload {
    pub note: String,
}

/// Appends a `Wakeup` event to the conversation's log when fired.
pub struct WakeupDispatcher {
    db: Database,
}

impl WakeupDispatcher {
    pub fn new(db: Database) -> Self {
        Self { db }
    }
}

#[async_trait]
impl Dispatcher for WakeupDispatcher {
    fn effect_kind(&self) -> &'static str {
        "schedule.wakeup"
    }

    async fn dispatch(&self, row: &OutboxRow) -> Result<(), String> {
        let log = EventLog::new(&self.db);
        let last_seq = log
            .last_seq(&row.conversation_id)
            .map_err(|e| format!("last_seq: {e}"))?;
        let next_seq = last_seq.next();

        // Forward the original payload (already MessagePack-encoded) — but
        // also decode just enough to validate it's a proper WakeupPayload.
        let _decoded: WakeupPayload =
            rmp_serde::from_slice(&row.payload).map_err(|e| format!("decode wakeup: {e}"))?;

        let ev = EventRecord {
            conversation_id: row.conversation_id.clone(),
            seq: next_seq,
            kind: EventKind::Wakeup,
            payload: row.payload.clone(),
            committed_at: chrono::Utc::now().timestamp(),
            actor: Some("system".into()),
        };
        log.append(&ev).map_err(|e| format!("append wakeup: {e}"))?;
        Ok(())
    }
}

// Keep the Snapshot/PendingEvent imports used by Phase 1+ consumers.
#[allow(dead_code)]
fn _keep_imports_alive(_s: Snapshot, _e: PendingEvent, _c: ConversationId, _ev: EventSeq) {}

// ---------------------------------------------------------------------------
// Registry + drain loop
// ---------------------------------------------------------------------------

/// Keyed by `effect_kind`.
#[derive(Default)]
pub struct DispatcherRegistry {
    inner: HashMap<&'static str, Arc<dyn Dispatcher>>,
}

impl DispatcherRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, disp: Arc<dyn Dispatcher>) {
        self.inner.insert(disp.effect_kind(), disp);
    }

    pub fn get(&self, effect_kind: &str) -> Option<Arc<dyn Dispatcher>> {
        self.inner.get(effect_kind).cloned()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct DrainConfig {
    pub batch_size: i64,
    pub poll_interval: Duration,
    pub retry_budget: RetryBudget,
}

impl Default for DrainConfig {
    fn default() -> Self {
        Self {
            batch_size: 32,
            poll_interval: Duration::from_millis(500),
            retry_budget: RetryBudget::default(),
        }
    }
}

/// Run one pass of the drain loop: fetch ready rows, claim each, dispatch,
/// mark delivered or record failure. Returns the number of rows processed.
///
/// Safe to call repeatedly; `claim` guarantees no two invocations dispatch
/// the same row.
pub async fn drain_once(
    db: &Database,
    registry: &DispatcherRegistry,
    cfg: &DrainConfig,
) -> Result<usize, String> {
    let store = OutboxStore::new(db);
    let now_ts = chrono::Utc::now().timestamp();
    let ready = store
        .ready_pending(now_ts, cfg.batch_size)
        .map_err(|e| format!("ready_pending: {e}"))?;

    let mut processed = 0usize;
    for row in ready {
        let id = match row.id {
            Some(i) => i,
            None => continue,
        };

        // Try to claim — if another drain instance got here first, skip.
        let owned = store.claim(id).map_err(|e| format!("claim: {e}"))?;
        if !owned {
            continue;
        }

        let dispatcher = match registry.get(&row.effect_kind) {
            Some(d) => d,
            None => {
                warn!(
                    id = id,
                    effect_kind = %row.effect_kind,
                    "no dispatcher registered for effect_kind; moving to dead_letter"
                );
                let _ = store.record_failure(
                    id,
                    &format!("no dispatcher for '{}'", row.effect_kind),
                    1, // force dead-letter immediately
                    0,
                );
                continue;
            }
        };

        match dispatcher.dispatch(&row).await {
            Ok(()) => {
                store
                    .mark_delivered(id)
                    .map_err(|e| format!("mark_delivered: {e}"))?;
                debug!(id = id, effect_kind = %row.effect_kind, "delivered");
                processed += 1;
            }
            Err(e) => {
                let attempt = (row.attempts + 1) as u32;
                let backoff_secs = exp_backoff(attempt).as_secs() as i64;
                let retrying = store
                    .record_failure(id, &e, cfg.retry_budget.max_attempts, backoff_secs)
                    .map_err(|err| format!("record_failure: {err}"))?;
                if retrying {
                    debug!(
                        id = id,
                        attempt = attempt,
                        error = %e,
                        "retry scheduled"
                    );
                } else {
                    warn!(
                        id = id,
                        attempt = attempt,
                        error = %e,
                        "moved to dead_letter"
                    );
                }
                processed += 1;
            }
        }
    }
    Ok(processed)
}

/// Long-running drain loop. Terminates when the `shutdown` receiver fires.
///
/// Typical usage: spawn this as a tokio task at server startup and drop
/// the handle on shutdown.
pub async fn run_drain_loop(
    db: Database,
    registry: Arc<DispatcherRegistry>,
    cfg: DrainConfig,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    info!(?cfg, "outbox drain loop starting");
    loop {
        if *shutdown.borrow() {
            break;
        }
        match drain_once(&db, &registry, &cfg).await {
            Ok(n) if n > 0 => debug!(processed = n, "drain pass"),
            Ok(_) => {}
            Err(e) => warn!(error = %e, "drain pass failed"),
        }

        tokio::select! {
            _ = tokio::time::sleep(cfg.poll_interval) => {}
            _ = shutdown.changed() => break,
        }
    }
    info!("outbox drain loop stopped");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_core::db::{Database, DbConfig};
    use execlaw_core::ids::{IdempotencyKey, TurnSeq};
    use execlaw_core::migrations::MigrationRunner;
    use execlaw_core::outbox::{OutboxRow, OutboxStatus, OutboxStore};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    #[test]
    fn backoff_grows_and_caps() {
        let a0 = exp_backoff(0);
        let a1 = exp_backoff(1);
        let a2 = exp_backoff(2);
        let big = exp_backoff(20);
        assert!(a1 > a0);
        assert!(a2 > a1);
        assert_eq!(big, Duration::from_millis(10 * 60 * 1_000));
    }

    #[test]
    fn default_budget_is_five() {
        let b = RetryBudget::default();
        assert_eq!(b.max_attempts, 5);
        assert!(!b.should_dead_letter(4));
        assert!(b.should_dead_letter(5));
    }

    struct RecordingDispatcher {
        kind: &'static str,
        calls: Arc<AtomicUsize>,
        fail_first_n: usize,
    }

    #[async_trait]
    impl Dispatcher for RecordingDispatcher {
        fn effect_kind(&self) -> &'static str {
            self.kind
        }

        async fn dispatch(&self, _row: &OutboxRow) -> Result<(), String> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if n <= self.fail_first_n {
                Err(format!("synthetic failure #{n}"))
            } else {
                Ok(())
            }
        }
    }

    fn enqueue_row(store: &OutboxStore<'_>, effect_kind: &str, ord: u32) -> i64 {
        let cid = ConversationId::from("conv-1");
        let key = IdempotencyKey::mint(&cid, TurnSeq(1), ord);
        let payload = rmp_serde::to_vec_named(&WakeupPayload {
            note: format!("test-{ord}"),
        })
        .unwrap();
        store
            .enqueue(&OutboxRow {
                id: None,
                idempotency_key: key,
                conversation_id: cid,
                effect_kind: effect_kind.into(),
                payload,
                status: OutboxStatus::Pending,
                attempts: 0,
                next_attempt_at: None,
                last_error: None,
                enqueued_seq: EventSeq(1),
            })
            .unwrap()
    }

    #[tokio::test]
    async fn drain_delivers_pending_row() {
        let db = fresh_db();
        let store = OutboxStore::new(&db);
        let _id = enqueue_row(&store, "test.effect", 0);

        let calls = Arc::new(AtomicUsize::new(0));
        let mut reg = DispatcherRegistry::new();
        reg.register(Arc::new(RecordingDispatcher {
            kind: "test.effect",
            calls: calls.clone(),
            fail_first_n: 0,
        }));

        let n = drain_once(&db, &reg, &DrainConfig::default())
            .await
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn drain_retries_transient_failures() {
        let db = fresh_db();
        let store = OutboxStore::new(&db);
        enqueue_row(&store, "test.effect", 1);

        let calls = Arc::new(AtomicUsize::new(0));
        let mut reg = DispatcherRegistry::new();
        reg.register(Arc::new(RecordingDispatcher {
            kind: "test.effect",
            calls: calls.clone(),
            fail_first_n: 100, // always fail
        }));

        // First pass: dispatcher fails → row goes back to pending with backoff.
        let _ = drain_once(&db, &reg, &DrainConfig::default())
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Because backoff is 1s+, the row is NOT ready again immediately —
        // drain_once should see zero ready rows on the next call.
        let n2 = drain_once(&db, &reg, &DrainConfig::default())
            .await
            .unwrap();
        assert_eq!(n2, 0, "should wait for backoff before retrying");
    }

    #[tokio::test]
    async fn drain_dead_letters_after_retry_budget() {
        let db = fresh_db();
        let store = OutboxStore::new(&db);
        enqueue_row(&store, "test.effect", 2);

        let calls = Arc::new(AtomicUsize::new(0));
        let mut reg = DispatcherRegistry::new();
        reg.register(Arc::new(RecordingDispatcher {
            kind: "test.effect",
            calls: calls.clone(),
            fail_first_n: 100,
        }));

        // Budget = 2 so second failure dead-letters.
        let cfg = DrainConfig {
            retry_budget: RetryBudget { max_attempts: 2 },
            ..Default::default()
        };

        let _ = drain_once(&db, &reg, &cfg).await.unwrap();

        // Force the row back to pending NOW (skip backoff for the test).
        db.with_conn(|c| {
            c.execute(
                "UPDATE state_outbox SET next_attempt_at = 0 WHERE status = 'pending'",
                [],
            )?;
            Ok(())
        })
        .unwrap();

        let _ = drain_once(&db, &reg, &cfg).await.unwrap();

        // After two failures, should be dead-lettered.
        assert_eq!(store.dead_letter_count().unwrap(), 1);
    }

    #[tokio::test]
    async fn drain_moves_unknown_effect_to_dead_letter() {
        let db = fresh_db();
        let store = OutboxStore::new(&db);
        enqueue_row(&store, "effect.nobody.handles", 3);

        let reg = DispatcherRegistry::new();
        let _ = drain_once(&db, &reg, &DrainConfig::default())
            .await
            .unwrap();

        assert_eq!(store.dead_letter_count().unwrap(), 1);
    }

    #[tokio::test]
    async fn wakeup_dispatcher_appends_wakeup_event() {
        let db = fresh_db();
        let store = OutboxStore::new(&db);
        enqueue_row(&store, "schedule.wakeup", 7);

        let mut reg = DispatcherRegistry::new();
        reg.register(Arc::new(WakeupDispatcher::new(db.clone())));

        let _ = drain_once(&db, &reg, &DrainConfig::default())
            .await
            .unwrap();

        // Verify a Wakeup event landed in the log.
        let log = EventLog::new(&db);
        let events = log
            .replay_since(&ConversationId::from("conv-1"), EventSeq(0))
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EventKind::Wakeup);
    }

    #[tokio::test]
    async fn claim_prevents_double_dispatch() {
        let db = fresh_db();
        let store = OutboxStore::new(&db);
        let id = enqueue_row(&store, "test.effect", 9);

        // First claim succeeds.
        assert!(store.claim(id).unwrap());
        // Second claim on the same row returns false.
        assert!(!store.claim(id).unwrap());
    }
}
