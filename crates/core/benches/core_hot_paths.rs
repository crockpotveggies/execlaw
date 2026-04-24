//! Microbenchmarks for execlaw-core hot paths (§0 axiom #14).
//!
//! Every load-bearing primitive on the turn path has a bench here with an
//! explicit latency expectation. Run with:
//!
//! ```text
//! cargo bench -p execlaw-core
//! ```
//!
//! The first run establishes a baseline; subsequent runs compare against it.
//! A regression >10% on any of these blocks a merge.

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use execlaw_core::db::{Database, DbConfig};
use execlaw_core::event_hmac::{canonical_bytes, sign_event, verify_event};
use execlaw_core::events::{EventKind, EventLog, EventRecord, PendingEvent, ToolResultPayload, ToolUsePayload};
use execlaw_core::ids::{ConversationId, EventSeq, IdempotencyKey, TurnSeq};
use execlaw_core::migrations::MigrationRunner;
use execlaw_core::outbox::{OutboxRow, OutboxStatus, OutboxStore};

fn fresh_db() -> Database {
    let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
    MigrationRunner::new(&db).apply_all().unwrap();
    db
}

// ---------------------------------------------------------------------------
// HMAC sign + verify — runs on every state_events row; budget ≤ 10µs p99.
// ---------------------------------------------------------------------------

fn bench_hmac(c: &mut Criterion) {
    let key = b"execlaw-event-log-hmac-key------";
    let canon = canonical_bytes(
        "conv-abc123",
        42,
        "tool_use",
        1_714_000_000,
        Some("agent"),
        &vec![0xABu8; 256], // representative MessagePack payload size
    );
    let tag = sign_event(key, &canon);

    let mut group = c.benchmark_group("event_hmac");
    group.throughput(Throughput::Bytes(canon.len() as u64));

    group.bench_function("canonical_bytes", |b| {
        b.iter(|| {
            canonical_bytes(
                black_box("conv-abc123"),
                black_box(42),
                black_box("tool_use"),
                black_box(1_714_000_000),
                black_box(Some("agent")),
                black_box(&vec![0xABu8; 256]),
            )
        })
    });
    group.bench_function("sign_event", |b| {
        b.iter(|| sign_event(black_box(key), black_box(&canon)))
    });
    group.bench_function("verify_event", |b| {
        b.iter(|| verify_event(black_box(key), black_box(&canon), black_box(&tag)))
    });
    group.finish();
}

// ---------------------------------------------------------------------------
// Idempotency key minting — called on every outbox enqueue.
// ---------------------------------------------------------------------------

fn bench_idempotency_key(c: &mut Criterion) {
    let cid = ConversationId::from("conv-abc123");
    c.bench_function("idempotency_key_mint", |b| {
        b.iter(|| {
            IdempotencyKey::mint(
                black_box(&cid),
                black_box(TurnSeq(47)),
                black_box(3),
            )
        })
    });
}

// ---------------------------------------------------------------------------
// EventRecord::new + decode_payload — MessagePack serde roundtrip.
// ---------------------------------------------------------------------------

fn bench_event_record_encode_decode(c: &mut Criterion) {
    let cid = ConversationId::from("conv-bench");
    let payload = ToolUsePayload {
        ordinal: 0,
        tool_name: "list_events".into(),
        args_json: serde_json::json!({"start": "2026-01-01", "end": "2026-12-31"}),
    };

    c.bench_function("event_record_new_tool_use", |b| {
        b.iter(|| {
            EventRecord::new(
                black_box(cid.clone()),
                black_box(EventSeq(1)),
                black_box(EventKind::ToolUse),
                black_box(&payload),
                black_box(Some("agent".to_owned())),
            )
            .unwrap()
        })
    });

    let ev = EventRecord::new(
        cid.clone(),
        EventSeq(1),
        EventKind::ToolUse,
        &payload,
        Some("agent".into()),
    )
    .unwrap();
    c.bench_function("event_record_decode_tool_use", |b| {
        b.iter(|| {
            let _p: ToolUsePayload = black_box(&ev).decode_payload().unwrap();
        })
    });
}

// ---------------------------------------------------------------------------
// commit_turn — the atomic write path, including tool_use/tool_result pairing.
// ---------------------------------------------------------------------------

fn bench_commit_turn(c: &mut Criterion) {
    let mut group = c.benchmark_group("commit_turn");
    for n in [1usize, 4, 10] {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter_batched(
                || {
                    let db = fresh_db();
                    let cid = ConversationId::from(format!("conv-{n}"));
                    let mut pending: Vec<PendingEvent> = Vec::with_capacity(n);
                    for i in 0..n {
                        let ord = i as u32;
                        pending.push(
                            PendingEvent::encode(
                                EventKind::ToolUse,
                                &ToolUsePayload {
                                    ordinal: ord,
                                    tool_name: "ping".into(),
                                    args_json: serde_json::json!({}),
                                },
                                Some("agent".into()),
                            )
                            .unwrap(),
                        );
                        pending.push(
                            PendingEvent::encode(
                                EventKind::ToolResult,
                                &ToolResultPayload {
                                    ordinal: ord,
                                    outcome: Ok(serde_json::json!({"pong": true})),
                                },
                                Some("system".into()),
                            )
                            .unwrap(),
                        );
                    }
                    (db, cid, pending)
                },
                |(db, cid, pending)| {
                    let log = EventLog::new(&db);
                    log.commit_turn(&cid, EventSeq(0), pending).unwrap()
                },
                criterion::BatchSize::SmallInput,
            )
        });
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// replay_since over an already-populated conversation.
// ---------------------------------------------------------------------------

fn bench_replay_since(c: &mut Criterion) {
    let db = fresh_db();
    let cid = ConversationId::from("conv-replay");
    let log = EventLog::new(&db);
    // Pre-seed 500 events.
    for i in 1..=500i64 {
        let ev = EventRecord::new(
            cid.clone(),
            EventSeq(i),
            EventKind::UserMsg,
            &serde_json::json!({"i": i}),
            None,
        )
        .unwrap();
        log.append(&ev).unwrap();
    }
    c.bench_function("replay_since_0_of_500", |b| {
        b.iter(|| log.replay_since(black_box(&cid), black_box(EventSeq(0))).unwrap())
    });
    c.bench_function("replay_since_450_of_500", |b| {
        b.iter(|| log.replay_since(black_box(&cid), black_box(EventSeq(450))).unwrap())
    });
}

// ---------------------------------------------------------------------------
// HMAC-signed vs keyless EventLog — measure the cost of the tamper-evidence
// axiom (§7.8) added in Phase 1.
// ---------------------------------------------------------------------------

fn bench_event_log_append(c: &mut Criterion) {
    let mut group = c.benchmark_group("event_log_append");

    // Keyless baseline.
    group.bench_function("keyless", |b| {
        b.iter_batched(
            || {
                let db = fresh_db();
                let cid = ConversationId::from("conv-a");
                (db, cid, 1i64)
            },
            |(db, cid, seq)| {
                let log = EventLog::new(&db);
                let ev = EventRecord::new(
                    cid.clone(),
                    EventSeq(seq),
                    EventKind::UserMsg,
                    &serde_json::json!({"text": "hello"}),
                    Some("controller".into()),
                )
                .unwrap();
                log.append(&ev).unwrap();
            },
            criterion::BatchSize::SmallInput,
        )
    });

    // HMAC-keyed — production path. Measures the added cost of signing.
    group.bench_function("hmac_keyed", |b| {
        let key = b"execlaw-bench-hmac-key-32-bytes!".to_vec();
        b.iter_batched(
            || {
                let db = fresh_db();
                let cid = ConversationId::from("conv-b");
                (db, cid, 1i64, key.clone())
            },
            |(db, cid, seq, key)| {
                let log = EventLog::new(&db).with_hmac_key(key);
                let ev = EventRecord::new(
                    cid.clone(),
                    EventSeq(seq),
                    EventKind::UserMsg,
                    &serde_json::json!({"text": "hello"}),
                    Some("controller".into()),
                )
                .unwrap();
                log.append(&ev).unwrap();
            },
            criterion::BatchSize::SmallInput,
        )
    });

    group.finish();
}

fn bench_event_log_replay_keyed(c: &mut Criterion) {
    let key = b"execlaw-bench-hmac-key-32-bytes!".to_vec();
    let db = fresh_db();
    let cid = ConversationId::from("conv-replay-hmac");
    let log = EventLog::new(&db).with_hmac_key(key.clone());
    for i in 1..=500i64 {
        let ev = EventRecord::new(
            cid.clone(),
            EventSeq(i),
            EventKind::UserMsg,
            &serde_json::json!({"i": i}),
            None,
        )
        .unwrap();
        log.append(&ev).unwrap();
    }
    let mut group = c.benchmark_group("event_log_replay_500");
    group.bench_function("hmac_verified", |b| {
        b.iter(|| {
            EventLog::new(&db)
                .with_hmac_key(key.clone())
                .replay_since(black_box(&cid), black_box(EventSeq(0)))
                .unwrap()
        })
    });
    // Keyless replay — baseline without verify cost.
    group.bench_function("keyless", |b| {
        b.iter(|| {
            EventLog::new(&db)
                .replay_since(black_box(&cid), black_box(EventSeq(0)))
                .unwrap()
        })
    });
    group.finish();
}

// ---------------------------------------------------------------------------
// Outbox: claim + ready_pending + record_failure.
// ---------------------------------------------------------------------------

fn bench_outbox(c: &mut Criterion) {
    let mut group = c.benchmark_group("outbox");

    group.bench_function("ready_pending_empty", |b| {
        let db = fresh_db();
        b.iter(|| {
            let store = OutboxStore::new(&db);
            store.ready_pending(black_box(1_000_000_000), black_box(32)).unwrap()
        })
    });

    group.bench_function("enqueue", |b| {
        let db = fresh_db();
        let cid = ConversationId::from("conv-enq");
        let mut ord = 0u32;
        b.iter(|| {
            let store = OutboxStore::new(&db);
            let key = IdempotencyKey::mint(&cid, TurnSeq(1), ord);
            ord += 1;
            store
                .enqueue(&OutboxRow {
                    id: None,
                    idempotency_key: key,
                    conversation_id: cid.clone(),
                    effect_kind: "transport.send".into(),
                    payload: b"payload".to_vec(),
                    status: OutboxStatus::Pending,
                    attempts: 0,
                    next_attempt_at: None,
                    last_error: None,
                    enqueued_seq: EventSeq(1),
                })
                .unwrap()
        })
    });

    group.bench_function("claim", |b| {
        b.iter_batched(
            || {
                let db = fresh_db();
                let store = OutboxStore::new(&db);
                let cid = ConversationId::from("conv-claim");
                let id = store
                    .enqueue(&OutboxRow {
                        id: None,
                        idempotency_key: IdempotencyKey::mint(&cid, TurnSeq(1), 0),
                        conversation_id: cid,
                        effect_kind: "e".into(),
                        payload: vec![],
                        status: OutboxStatus::Pending,
                        attempts: 0,
                        next_attempt_at: None,
                        last_error: None,
                        enqueued_seq: EventSeq(1),
                    })
                    .unwrap();
                (db, id)
            },
            |(db, id)| {
                let store = OutboxStore::new(&db);
                store.claim(id).unwrap()
            },
            criterion::BatchSize::SmallInput,
        )
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_hmac,
    bench_idempotency_key,
    bench_event_record_encode_decode,
    bench_commit_turn,
    bench_replay_since,
    bench_event_log_append,
    bench_event_log_replay_keyed,
    bench_outbox,
);
criterion_main!(benches);
