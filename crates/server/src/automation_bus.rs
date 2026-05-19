//! Server-side dispatch + poller for the automation event bus (M1).
//!
//! Layers on top of [`execlaw_core::automation_bus`] (which owns the
//! durable [`state_bus_events`] table and the sync `BusEventStore`)
//! by adding the in-process queue + workers described in the v3.2
//! design doc:
//!
//!   * `tokio::sync::mpsc` channel (bounded) carrying event IDs
//!     produced by external ingress (webhooks, sockets, plugins).
//!   * Dispatcher task that pops IDs, loads the row, claims it, and
//!     hands it to an [`EventHandler`].
//!   * Worker pool ([`tokio::sync::Semaphore`]) bounding concurrent
//!     in-flight handlers — backpressure surface #2.
//!   * Internal poller task that, every [`INTERNAL_POLL_INTERVAL`],
//!     scans `state_bus_events` for rows with `internal=1` and no
//!     `dispatched_at`, and pushes their IDs onto the main mpsc.
//!     This lane is for in-process producers (automation side
//!     effects, plugin emits) so they can never deadlock through
//!     the channel.
//!   * Crash-recovery scan at startup: drains any pending rows
//!     (both ingress + internal lanes) before the live dispatcher
//!     starts taking from the channel. Survives ungraceful exits
//!     between the INSERT and `mark_dispatched`.
//!
//! Delivery is at-least-once. The handler MUST be idempotent — both
//! because of the crash-recovery scan and because a race between the
//! recovery scan and a live mpsc delivery could (rarely) double-fire
//! the handler for the same row. In M1 the handler is a no-op stub
//! ([`noop_handler`]) so the substrate can be wired end-to-end before
//! the matcher (M2+) is built.

use execlaw_core::Database;
use execlaw_core::automation_bus::{
    BusEventError, BusEventRow, BusEventStore, Event, PublishOutcome,
};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Notify, Semaphore, mpsc};
use tracing::{debug, info, warn};

/// Default mpsc channel capacity. 256 is enough headroom for normal
/// bursts; producers backpressure (await on `send`) when full.
pub const DEFAULT_CHANNEL_CAPACITY: usize = 256;

/// Default dispatcher worker pool size. M1 has no automation matcher
/// yet so this is effectively the cap on concurrent dispatches; once
/// M2 wires matching + automation runs, this is the cap on concurrent
/// runs.
pub const DEFAULT_WORKER_CONCURRENCY: usize = 16;

/// Internal poller tick. 100ms is the latency floor for in-process
/// producers — they write durably and the poller picks them up on
/// the next tick. Trades fast internal-event response against
/// frequency of idle wakeups.
pub const INTERNAL_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Per-tick batch size for the poller and the crash-recovery sweep.
/// Big enough that a busy poller doesn't fall behind; small enough
/// that one tick can't hog SQLite's write lock.
pub const POLL_BATCH_SIZE: i64 = 256;

type BoxFut = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Handler invoked for each event the dispatcher receives.
///
/// In M1 the only handler is [`noop_handler`] — the matcher arrives
/// in M2. Kept as a function-type so M2's swap is a one-line change
/// at the `cmd_serve` wiring point.
pub type EventHandler = Arc<dyn Fn(BusEventRow) -> BoxFut + Send + Sync>;

/// Clone-able handle for `AppState`. Publishing is async because
/// the underlying mpsc send awaits on a full channel (backpressure
/// surface #1 in the design doc).
#[derive(Clone)]
pub struct AutomationBus {
    inner: Arc<AutomationBusInner>,
}

struct AutomationBusInner {
    db: Database,
    tx: mpsc::Sender<String>,
}

/// Handles to the spawned dispatcher + poller. Caller can `.join()`
/// after notifying `stop` to drain cleanly.
pub struct BusTasks {
    pub dispatcher: tokio::task::JoinHandle<()>,
    pub poller: tokio::task::JoinHandle<()>,
}

impl BusTasks {
    pub async fn join(self) {
        let _ = self.dispatcher.await;
        let _ = self.poller.await;
    }
}

impl AutomationBus {
    /// Spawn the bus + dispatcher + poller with default tuning.
    pub fn spawn(db: Database, handler: EventHandler, stop: Arc<Notify>) -> (Self, BusTasks) {
        Self::spawn_with_config(
            db,
            handler,
            DEFAULT_CHANNEL_CAPACITY,
            DEFAULT_WORKER_CONCURRENCY,
            INTERNAL_POLL_INTERVAL,
            stop,
        )
    }

    /// Test/tuning constructor — exposes the otherwise-default knobs.
    pub fn spawn_with_config(
        db: Database,
        handler: EventHandler,
        channel_capacity: usize,
        worker_concurrency: usize,
        poll_interval: Duration,
        stop: Arc<Notify>,
    ) -> (Self, BusTasks) {
        let (tx, rx) = mpsc::channel::<String>(channel_capacity);
        let bus = Self {
            inner: Arc::new(AutomationBusInner {
                db: db.clone(),
                tx: tx.clone(),
            }),
        };
        let workers = Arc::new(Semaphore::new(worker_concurrency));
        let dispatcher = tokio::spawn(dispatcher_loop(
            db.clone(),
            rx,
            workers.clone(),
            handler.clone(),
            stop.clone(),
        ));
        let poller = tokio::spawn(internal_poller_loop(db, tx, poll_interval, stop));
        (bus, BusTasks { dispatcher, poller })
    }

    /// Publish an external event.
    ///
    /// 1. Persists to SQLite via [`BusEventStore::publish`] with
    ///    `internal=false`. PK collisions return [`PublishOutcome::Duplicate`]
    ///    (treated as success — see core docs).
    /// 2. On `Inserted`, sends the event id on the bounded mpsc.
    ///    The `await` here is the backpressure point — caller blocks
    ///    when the dispatcher is behind.
    /// 3. On `Duplicate`, the channel is NOT touched (the original
    ///    insertion's send already happened, or will be picked up
    ///    by crash recovery).
    ///
    /// If the channel is closed (dispatcher dropped), the row is
    /// already durable and the next-boot crash-recovery scan will
    /// pick it up. We log and continue — failure to enqueue is not
    /// a publish failure.
    pub async fn publish(&self, evt: Event) -> Result<PublishOutcome, BusEventError> {
        let store = BusEventStore::new(&self.inner.db);
        let outcome = store.publish(&evt, false)?;
        if matches!(outcome, PublishOutcome::Inserted) {
            if let Err(e) = self.inner.tx.send(evt.id.clone()).await {
                warn!(
                    event_id = %evt.id,
                    error = %e,
                    "automation bus dispatch channel closed; relying on crash-recovery scan",
                );
            }
        }
        Ok(outcome)
    }

    /// Publish an internal event. Persists durable-only with
    /// `internal=1` and does NOT touch the channel — the poller
    /// picks it up on the next tick.
    ///
    /// Use this from in-process producers (automation side effects,
    /// plugin emits) to avoid producer-consumer deadlock through
    /// the channel.
    pub async fn publish_internal(
        &self,
        evt: Event,
    ) -> Result<PublishOutcome, BusEventError> {
        let store = BusEventStore::new(&self.inner.db);
        store.publish(&evt, true)
    }

    /// Synchronous, channel-free publish path — exposed for the
    /// retention sweeper's tests and the rare caller that doesn't
    /// have an async context. Internal events are durable-only and
    /// the poller picks them up; external events skip the channel
    /// and rely on the crash-recovery scan to land in the dispatcher.
    pub fn publish_sync(
        &self,
        evt: &Event,
        internal: bool,
    ) -> Result<PublishOutcome, BusEventError> {
        BusEventStore::new(&self.inner.db).publish(evt, internal)
    }

    /// Construct a non-dispatching bus. `publish` still writes durable
    /// rows but the mpsc channel is closed up front — sends fall back
    /// to the "channel closed; rely on crash-recovery" log line.
    ///
    /// Used by `routes::test_app_state` so sync `#[test]` fixtures can
    /// build an `AppState` without needing a tokio runtime to host
    /// the dispatcher + poller tasks. Tests that need to verify
    /// dispatch behavior should use [`AutomationBus::spawn`] inside a
    /// `#[tokio::test]` directly (see this module's tests for the
    /// pattern).
    pub fn stub(db: Database) -> Self {
        let (tx, rx) = mpsc::channel::<String>(1);
        // Drop the receiver immediately so sends return Err and the
        // "no dispatcher" code path in `publish` activates. The
        // closure is intentional — every test publish is durable
        // even without a live dispatcher.
        drop(rx);
        Self {
            inner: Arc::new(AutomationBusInner { db, tx }),
        }
    }
}

async fn dispatcher_loop(
    db: Database,
    mut rx: mpsc::Receiver<String>,
    workers: Arc<Semaphore>,
    handler: EventHandler,
    stop: Arc<Notify>,
) {
    // Crash recovery: drain any rows persisted by a previous process
    // before the live mpsc starts pumping. Pulls both lanes (external
    // + internal) so the first poller tick has less work to do.
    let store = BusEventStore::new(&db);
    match store.fetch_pending(false, POLL_BATCH_SIZE * 4) {
        Ok(ids) if !ids.is_empty() => {
            info!(
                count = ids.len(),
                "automation bus: replaying pending events from crash-recovery scan",
            );
            for id in ids {
                dispatch_one(&db, &handler, &workers, id).await;
            }
        }
        Ok(_) => {}
        Err(e) => warn!(error = %e, "automation bus crash-recovery scan failed"),
    }

    info!("automation bus dispatcher running");
    loop {
        tokio::select! {
            _ = stop.notified() => {
                info!("automation bus dispatcher: stop received, draining");
                rx.close();
                while let Some(id) = rx.recv().await {
                    dispatch_one(&db, &handler, &workers, id).await;
                }
                info!("automation bus dispatcher: drained, exiting");
                return;
            }
            maybe_id = rx.recv() => {
                match maybe_id {
                    Some(id) => dispatch_one(&db, &handler, &workers, id).await,
                    None => {
                        info!("automation bus dispatcher: channel closed, exiting");
                        return;
                    }
                }
            }
        }
    }
}

/// Quick checks first — no permit needed for skips. Only acquire a
/// worker slot for events that actually need handler-bound work.
///
/// Race guard: we **claim** the row (atomic `mark_dispatched`) BEFORE
/// running the handler. If another path (crash-recovery scan vs.
/// live mpsc, poller vs. recovery) already claimed it, we skip.
/// This guarantees the handler fires at most once per event id
/// despite the at-least-once delivery from the recovery scan +
/// channel + poller paths overlapping.
///
/// Known limitation (acceptable for M1, fix in M2): if the process is
/// killed after `mark_dispatched` succeeds but BEFORE the spawned
/// handler task completes, the row stays marked-as-dispatched and
/// the handler effectively never ran. The next-boot crash-recovery
/// scan won't pick it up (it filters on `dispatched_at IS NULL`).
/// With the M1 no-op handler this is invisible; M2's automation
/// matcher will either (a) unclaim on shutdown or (b) track a
/// separate `completed_at` so retention only sweeps fully-run rows.
async fn dispatch_one(
    db: &Database,
    handler: &EventHandler,
    workers: &Arc<Semaphore>,
    id: String,
) {
    let row = match BusEventStore::new(db).get(&id) {
        Ok(Some(r)) => r,
        Ok(None) => {
            warn!(
                event_id = %id,
                "automation bus: dispatch for unknown event_id (retention sweep race?)",
            );
            return;
        }
        Err(e) => {
            warn!(event_id = %id, error = %e, "automation bus: failed to load event row");
            return;
        }
    };
    if row.dispatched_at.is_some() {
        debug!(event_id = %id, "automation bus: event already dispatched, skipping");
        return;
    }
    // Atomically claim. If we lose the race, another dispatcher path
    // owns this event — bail without running the handler. This is
    // the at-most-once-handler-call invariant.
    let now = chrono::Utc::now().timestamp();
    let claimed = match BusEventStore::new(db).mark_dispatched(&row.id, now) {
        Ok(b) => b,
        Err(e) => {
            warn!(event_id = %id, error = %e, "automation bus: mark_dispatched failed; will retry on next recovery scan");
            return;
        }
    };
    if !claimed {
        debug!(event_id = %id, "automation bus: event already claimed by another dispatcher path, skipping");
        return;
    }
    // Real work — gate on the worker semaphore (backpressure #2).
    let permit = match workers.clone().acquire_owned().await {
        Ok(p) => p,
        Err(_) => {
            warn!("automation bus dispatcher: worker semaphore closed; dropping event");
            return;
        }
    };
    let handler = handler.clone();
    tokio::spawn(async move {
        let _permit = permit;
        (handler)(row).await;
    });
}

async fn internal_poller_loop(
    db: Database,
    tx: mpsc::Sender<String>,
    poll_interval: Duration,
    stop: Arc<Notify>,
) {
    let mut tick = tokio::time::interval(poll_interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Burn the immediate-fire tick that interval emits on construction.
    tick.tick().await;
    info!(
        poll_ms = poll_interval.as_millis() as u64,
        "automation bus internal poller running",
    );
    loop {
        tokio::select! {
            _ = stop.notified() => {
                info!("automation bus internal poller: stop received, exiting");
                return;
            }
            _ = tick.tick() => {
                let store = BusEventStore::new(&db);
                match store.fetch_pending(true, POLL_BATCH_SIZE) {
                    Ok(ids) if ids.is_empty() => continue,
                    Ok(ids) => {
                        for id in ids {
                            if tx.send(id.clone()).await.is_err() {
                                warn!(event_id = %id, "automation bus poller: dispatch channel closed");
                                return;
                            }
                        }
                    }
                    Err(e) => warn!(error = %e, "automation bus poller: fetch_pending failed"),
                }
            }
        }
    }
}

/// No-op handler for M1 wiring. M2 replaces this in `cmd_serve` with
/// the automation matcher + run-spawn path.
pub fn noop_handler() -> EventHandler {
    Arc::new(|row: BusEventRow| {
        Box::pin(async move {
            debug!(
                event_id = %row.id,
                kind = %row.kind.as_str(),
                source = %row.source,
                "automation bus: event dispatched (no automations registered yet)",
            );
        })
    })
}

/// Test-only handler that counts deliveries via an `Arc<AtomicUsize>`
/// and records the delivered row IDs in order. Exposed under
/// `pub(crate)` so the integration tests in this crate can reach it
/// without rebuilding the harness.
#[cfg(test)]
pub(crate) fn counting_handler(
    counter: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
) -> EventHandler {
    Arc::new(move |row: BusEventRow| {
        let counter = counter.clone();
        let seen = seen.clone();
        Box::pin(async move {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            seen.lock().unwrap().push(row.id);
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_core::Database;
    use execlaw_core::automation_bus::{BusEventKind, BusEventStore, Event};
    use execlaw_core::db::DbConfig;
    use execlaw_core::migrations::MigrationRunner;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    fn evt(id: &str, ts: i64) -> Event {
        Event {
            id: id.into(),
            kind: BusEventKind::WebhookReceived,
            source: "test".into(),
            received_at: ts,
            payload: serde_json::json!({}),
        }
    }

    /// Drain helper: wait up to `timeout` for `counter` to reach
    /// `expected`, polling every 5ms. Returns whether we got there.
    async fn wait_for_count(
        counter: &Arc<AtomicUsize>,
        expected: usize,
        timeout: Duration,
    ) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed() < timeout {
            if counter.load(Ordering::SeqCst) >= expected {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        counter.load(Ordering::SeqCst) >= expected
    }

    #[tokio::test]
    async fn publish_delivers_through_dispatcher() {
        let db = fresh_db();
        let counter = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(Notify::new());
        let (bus, tasks) = AutomationBus::spawn(
            db.clone(),
            counting_handler(counter.clone(), seen.clone()),
            stop.clone(),
        );

        bus.publish(evt("a", 100)).await.unwrap();
        bus.publish(evt("b", 101)).await.unwrap();

        assert!(
            wait_for_count(&counter, 2, Duration::from_secs(2)).await,
            "dispatcher should deliver both events",
        );
        // Order on the channel is FIFO from publish order.
        assert_eq!(*seen.lock().unwrap(), vec!["a".to_string(), "b".to_string()]);

        // Both events should be marked dispatched.
        let store = BusEventStore::new(&db);
        assert!(store.get("a").unwrap().unwrap().dispatched_at.is_some());
        assert!(store.get("b").unwrap().unwrap().dispatched_at.is_some());

        stop.notify_waiters();
        tasks.join().await;
    }

    #[tokio::test]
    async fn duplicate_publish_does_not_double_dispatch() {
        let db = fresh_db();
        let counter = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(Notify::new());
        let (bus, tasks) = AutomationBus::spawn(
            db.clone(),
            counting_handler(counter.clone(), seen.clone()),
            stop.clone(),
        );

        let first = bus.publish(evt("dup", 100)).await.unwrap();
        let second = bus.publish(evt("dup", 100)).await.unwrap();
        assert_eq!(first, PublishOutcome::Inserted);
        assert_eq!(second, PublishOutcome::Duplicate);

        // Let any spurious dispatches settle. We expect exactly one
        // handler call (the second publish does NOT enqueue).
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        stop.notify_waiters();
        tasks.join().await;
    }

    #[tokio::test]
    async fn internal_publish_is_picked_up_by_poller() {
        let db = fresh_db();
        let counter = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(Notify::new());
        // Tighten the poll interval so the test isn't slow.
        let (bus, tasks) = AutomationBus::spawn_with_config(
            db.clone(),
            counting_handler(counter.clone(), seen.clone()),
            DEFAULT_CHANNEL_CAPACITY,
            DEFAULT_WORKER_CONCURRENCY,
            Duration::from_millis(20),
            stop.clone(),
        );

        bus.publish_internal(evt("int", 100)).await.unwrap();

        assert!(
            wait_for_count(&counter, 1, Duration::from_secs(2)).await,
            "poller should pick up internal event",
        );
        assert_eq!(*seen.lock().unwrap(), vec!["int".to_string()]);

        stop.notify_waiters();
        tasks.join().await;
    }

    #[tokio::test]
    async fn crash_recovery_drains_pending_rows_at_boot() {
        let db = fresh_db();
        // Seed pending rows BEFORE spawning the bus, simulating a
        // previous process that wrote them and died without marking
        // dispatched.
        let store = BusEventStore::new(&db);
        store.publish(&evt("survivor-1", 100), false).unwrap();
        store.publish(&evt("survivor-2", 101), true).unwrap();

        let counter = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(Notify::new());
        let (_bus, tasks) = AutomationBus::spawn(
            db.clone(),
            counting_handler(counter.clone(), seen.clone()),
            stop.clone(),
        );

        assert!(
            wait_for_count(&counter, 2, Duration::from_secs(2)).await,
            "crash recovery should replay both pending events",
        );
        let seen = seen.lock().unwrap();
        assert!(seen.contains(&"survivor-1".to_string()));
        assert!(seen.contains(&"survivor-2".to_string()));

        stop.notify_waiters();
        tasks.join().await;
    }

    #[tokio::test]
    async fn bounded_channel_applies_backpressure() {
        // 1-deep channel; concurrency=1 worker. A handler that holds
        // for 100ms blocks the dispatcher, so the second `publish`
        // must await until the first drains.
        let db = fresh_db();
        let counter = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(Notify::new());

        // Slow handler — sleeps 100ms before completing.
        let slow_handler: EventHandler = {
            let counter = counter.clone();
            Arc::new(move |_row: BusEventRow| {
                let counter = counter.clone();
                Box::pin(async move {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    counter.fetch_add(1, Ordering::SeqCst);
                })
            })
        };

        let (bus, tasks) = AutomationBus::spawn_with_config(
            db.clone(),
            slow_handler,
            1, // channel capacity
            1, // worker concurrency
            Duration::from_secs(60), // poller off-path
            stop.clone(),
        );

        let start = std::time::Instant::now();
        // First publish: fits in channel slot 0, dispatcher takes it.
        bus.publish(evt("e1", 1)).await.unwrap();
        // Second publish: channel has room because the dispatcher
        // pulled the first id; the second await should be near-
        // instant if backpressure works correctly.
        bus.publish(evt("e2", 2)).await.unwrap();
        // Third publish: by now the dispatcher is busy on e2 (slow
        // handler holding the worker permit), so channel is full
        // (slot 0 = e2 freshly delivered, plus the in-flight e1 has
        // freed slot 0). Actually with a 1-deep channel + 1 worker,
        // the third publish blocks until at least one handler completes.
        bus.publish(evt("e3", 3)).await.unwrap();
        let elapsed = start.elapsed();

        // We don't assert a precise wall-clock value because CI is
        // jittery, but we do require that delivering three events
        // through a 1-deep / 1-worker pipeline with a 100ms handler
        // took at least ONE handler-duration's worth of time. (Best
        // case: e1 starts immediately, e2 enters slot when e1 starts
        // running, e3 awaits until e1 finishes → ~100ms minimum.)
        assert!(
            elapsed >= Duration::from_millis(80),
            "three publishes through 1-deep+1-worker should backpressure to >= ~100ms, got {:?}",
            elapsed,
        );

        // Wait for all three handlers to finish.
        assert!(wait_for_count(&counter, 3, Duration::from_secs(3)).await);

        stop.notify_waiters();
        tasks.join().await;
    }

    #[tokio::test]
    async fn stop_drains_remaining_channel_items() {
        let db = fresh_db();
        let counter = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(Notify::new());
        let (bus, tasks) = AutomationBus::spawn(
            db.clone(),
            counting_handler(counter.clone(), seen.clone()),
            stop.clone(),
        );

        // Publish a small burst, then immediately signal stop. All
        // queued items must still be handled before the dispatcher
        // exits.
        for i in 0..5 {
            bus.publish(evt(&format!("d{i}"), i)).await.unwrap();
        }
        // Tiny pause to make sure publish has fully landed in the
        // channel before stop fires.
        tokio::time::sleep(Duration::from_millis(10)).await;
        stop.notify_waiters();
        tasks.join().await;

        assert_eq!(counter.load(Ordering::SeqCst), 5);
    }

    #[tokio::test]
    async fn already_dispatched_events_are_excluded_from_recovery_scan() {
        // Pre-mark events as dispatched BEFORE spawning the bus. The
        // crash-recovery scan filters on `dispatched_at IS NULL`, so
        // these rows must not fire the handler at boot.
        let db = fresh_db();
        let store = BusEventStore::new(&db);
        store.publish(&evt("pre-marked-1", 100), false).unwrap();
        store.publish(&evt("pre-marked-2", 101), false).unwrap();
        assert!(store.mark_dispatched("pre-marked-1", 200).unwrap());
        assert!(store.mark_dispatched("pre-marked-2", 201).unwrap());

        let counter = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(Notify::new());
        let (_bus, tasks) = AutomationBus::spawn(
            db.clone(),
            counting_handler(counter.clone(), seen.clone()),
            stop.clone(),
        );

        // Give the dispatcher time to complete its recovery scan.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "pre-dispatched rows must not fire the handler at boot",
        );

        stop.notify_waiters();
        tasks.join().await;
    }

    #[tokio::test]
    async fn recovery_race_with_live_publish_fires_handler_at_most_once() {
        // Crash-recovery scan runs at boot. If the same id also rides
        // the live mpsc (e.g., publish happens during the recovery
        // window because the scan-then-loop transition isn't atomic),
        // the claim-before-handle invariant must keep the handler
        // call count at exactly 1.
        let db = fresh_db();
        let store = BusEventStore::new(&db);
        // Seed a pending row so recovery scan tries to dispatch it.
        store.publish(&evt("race", 100), false).unwrap();

        let counter = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(Notify::new());
        let (bus, tasks) = AutomationBus::spawn(
            db.clone(),
            counting_handler(counter.clone(), seen.clone()),
            stop.clone(),
        );
        // Also push the same id via the live publish path. The id
        // is the same, so the INSERT is rejected as Duplicate and
        // the channel is NOT touched — meaning the recovery scan is
        // the only path that should fire the handler. But if a
        // dispatcher ever DID re-enqueue a duplicate id (M2 retry
        // path, future bug, etc.), the claim guard keeps the count
        // at 1. We assert the invariant explicitly here.
        let dup = bus.publish(evt("race", 100)).await.unwrap();
        assert_eq!(dup, PublishOutcome::Duplicate);

        // Wait for the recovery handler to land + any spurious
        // duplicate to fail-quietly.
        assert!(wait_for_count(&counter, 1, Duration::from_secs(2)).await);
        // Give 100ms for any race to expose itself.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "at-most-once handler invariant violated",
        );

        stop.notify_waiters();
        tasks.join().await;
    }
}
