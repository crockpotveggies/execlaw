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
use execlaw_core::conversation::{
    ConversationKind, ConversationRow, ConversationStore, Modality, Phase,
};
use execlaw_core::ephemeral_sweeper::sweep_once;
use execlaw_core::events::EventRecord as CoreEventRecord;
use execlaw_core::outbox::{OutboxRow, OutboxStatus, OutboxStore};
use execlaw_core::transport_conversations::{ConversationResolver, ResolveInput};

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

// ---------------------------------------------------------------------------
// PrincipalStore — identity resolution path runs on every chat request
// (§2.14). Budget ≤ 100 µs p99 for `get`; the per-turn cost is what
// gates cold-contact detection latency.
// ---------------------------------------------------------------------------

fn bench_principal_store(c: &mut Criterion) {
    use execlaw_core::ids::PluginId;
    use execlaw_core::principal::{
        Identifier, Principal, PrincipalStore, TrustLevel as CoreTrustLevel,
    };

    let db = fresh_db();
    let store = PrincipalStore::new(&db);
    // Seed ~100 principals so lookups are realistic (not empty-table fast).
    for i in 0..100 {
        let id_str = format!("pri-{i}");
        store
            .upsert(&Principal {
                id: execlaw_core::ids::PrincipalId::from(id_str.clone()),
                identifiers: vec![Identifier {
                    transport: "web".into(),
                    handle: format!("web:{id_str}"),
                }],
                trust_level: CoreTrustLevel::KnownTrusted {
                    resolvers: vec![PluginId::from("identity-local")],
                    approved_by: execlaw_core::ids::PrincipalId::from("controller"),
                    approved_at: 1,
                },
                resolved_by: vec![],
                metadata: serde_json::json!({}),
                first_seen: i,
                last_seen: Some(i),
                controller_notes: None,
            })
            .unwrap();
    }

    let hit_id = execlaw_core::ids::PrincipalId::from("pri-42");
    let miss_id = execlaw_core::ids::PrincipalId::from("does-not-exist");

    let mut group = c.benchmark_group("principal_store");
    group.bench_function("get_hit", |b| {
        b.iter(|| store.get(black_box(&hit_id)).unwrap())
    });
    group.bench_function("get_miss", |b| {
        b.iter(|| store.get(black_box(&miss_id)).unwrap())
    });
    let ident = Identifier {
        transport: "web".into(),
        handle: "web:pri-50".into(),
    };
    group.bench_function("find_by_identifier_hit", |b| {
        b.iter(|| store.find_by_identifier(black_box(&ident)).unwrap())
    });
    group.finish();
}

// ---------------------------------------------------------------------------
// ConversationResolver — every inbound non-UI message hits this; budget
// ≤ 50µs per call (single-row index lookup + at most one UPDATE).
// ---------------------------------------------------------------------------

fn fresh_conv_row(id: &str) -> ConversationRow {
    ConversationRow {
        conversation_id: execlaw_core::ids::ConversationId::from(id),
        kind: ConversationKind::ControllerDM,
        last_seq: EventSeq(0),
        phase: Phase::Idle,
        controller_id: None,
        trust_class: "Controller".into(),
        snapshot_blob: None,
        snapshot_seq: None,
        lease_owner: None,
        lease_expires: None,
        modality: Modality::Text,
        display_name: None,
        is_pinned: false,
        is_ephemeral: false,
        ephemeral_expires_at: None,
    }
}

fn bench_conversation_resolver(c: &mut Criterion) {
    let mut group = c.benchmark_group("conversation_resolver");

    // Controller short-circuit: pure stack work, no DB writes. The
    // hottest path on a controller-dominant deployment.
    group.bench_function("resolve_controller_short_circuit", |b| {
        let db = fresh_db();
        let resolver = ConversationResolver::new(&db);
        b.iter(|| {
            let outcome = resolver
                .resolve_or_mint(&ResolveInput {
                    plugin_id: black_box("transport-signal"),
                    transport_handle: black_box("signal:+15551234"),
                    principal_id: black_box("controller-1"),
                    is_controller: true,
                    idle_timeout_ms: 60_000,
                    now: black_box(1_000_000),
                })
                .unwrap();
            black_box(outcome);
        });
    });

    // Within-window continue: the steady-state hot path for an active
    // outsider. One SELECT + one UPDATE in a transaction.
    group.bench_function("resolve_continue_within_idle", |b| {
        let db = fresh_db();
        let resolver = ConversationResolver::new(&db);
        // Seed a current row.
        resolver
            .resolve_or_mint(&ResolveInput {
                plugin_id: "p",
                transport_handle: "h",
                principal_id: "x",
                is_controller: false,
                idle_timeout_ms: 60_000,
                now: 1_000,
            })
            .unwrap();

        let mut now = 1_010i64;
        b.iter(|| {
            now += 1;
            let outcome = resolver
                .resolve_or_mint(&ResolveInput {
                    plugin_id: "p",
                    transport_handle: "h",
                    principal_id: "x",
                    is_controller: false,
                    idle_timeout_ms: 60_000,
                    now: black_box(now),
                })
                .unwrap();
            black_box(outcome);
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// EphemeralSweeper — runs ~every 5 min, hot when many incognito threads
// expired in a window. Budget the per-conversation cost so a backlog of
// 1,000 expired threads sweeps in <1s.
// ---------------------------------------------------------------------------

fn bench_ephemeral_sweeper(c: &mut Criterion) {
    let mut group = c.benchmark_group("ephemeral_sweeper");
    group.sample_size(20); // each sample reseeds — keep runtime sane

    for n in [10usize, 100usize].iter() {
        group.bench_with_input(BenchmarkId::new("sweep_n_threads", n), n, |b, &n| {
            b.iter_with_setup(
                || {
                    let db = fresh_db();
                    let convs = ConversationStore::new(&db);
                    for i in 0..n {
                        let id = format!("c{i}");
                        let cid = execlaw_core::ids::ConversationId::from(id.as_str());
                        convs.upsert(&fresh_conv_row(&id)).unwrap();
                        convs.mark_ephemeral(&cid, Some(50)).unwrap();
                        // 3 events per thread — representative of a brief incognito chat.
                        for s in 1..=3i64 {
                            let ev = CoreEventRecord::new(
                                cid.clone(),
                                EventSeq(s),
                                EventKind::UserMsg,
                                &serde_json::json!({"i": s}),
                                None,
                            )
                            .unwrap();
                            execlaw_core::events::EventLog::new(&db).append(&ev).unwrap();
                        }
                    }
                    db
                },
                |db| {
                    let report = sweep_once(black_box(&db), black_box(100)).unwrap();
                    black_box(report);
                },
            );
        });
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// Conversation metadata mutators — backing the PATCH /api/chats/:id route
// and the set_thread_name agent tool. Each is one UPDATE; budget ≤ 200µs
// each so the SPA can rapid-fire rename / pin / toggle without lag.
// ---------------------------------------------------------------------------

fn bench_conversation_metadata(c: &mut Criterion) {
    let mut group = c.benchmark_group("conversation_metadata");

    group.bench_function("set_display_name", |b| {
        let db = fresh_db();
        let store = ConversationStore::new(&db);
        store.upsert(&fresh_conv_row("c-bench")).unwrap();
        let cid = execlaw_core::ids::ConversationId::from("c-bench");
        b.iter(|| {
            store
                .set_display_name(black_box(&cid), black_box(Some("Q4 plans")))
                .unwrap();
        });
    });

    group.bench_function("set_pinned", |b| {
        let db = fresh_db();
        let store = ConversationStore::new(&db);
        store.upsert(&fresh_conv_row("c-bench")).unwrap();
        let cid = execlaw_core::ids::ConversationId::from("c-bench");
        let mut flag = false;
        b.iter(|| {
            flag = !flag;
            store.set_pinned(black_box(&cid), black_box(flag)).unwrap();
        });
    });

    group.bench_function("mark_ephemeral_then_clear", |b| {
        let db = fresh_db();
        let store = ConversationStore::new(&db);
        store.upsert(&fresh_conv_row("c-bench")).unwrap();
        let cid = execlaw_core::ids::ConversationId::from("c-bench");
        let mut on = false;
        b.iter(|| {
            on = !on;
            let expires = if on { Some(black_box(9_999i64)) } else { None };
            store.mark_ephemeral(black_box(&cid), expires).unwrap();
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Sidebar thread-list query — runs on SPA mount + every state.changed
// WS event. Budget: ≤ 5ms for 1k threads so the SPA never blocks on it.
// ---------------------------------------------------------------------------

fn bench_list_thread_summaries(c: &mut Criterion) {
    let mut group = c.benchmark_group("list_thread_summaries");
    group.sample_size(20);
    for n in [10usize, 100usize, 1000usize].iter() {
        group.bench_with_input(
            BenchmarkId::new("threads", n),
            n,
            |b, &n| {
                let db = fresh_db();
                let store = ConversationStore::new(&db);
                for i in 0..n {
                    let id = format!("conv-{i}");
                    let mut row = fresh_conv_row(&id);
                    row.last_seq = EventSeq(i as i64);
                    store.upsert(&row).unwrap();
                    if i % 50 == 0 {
                        store
                            .set_pinned(
                                &execlaw_core::ids::ConversationId::from(
                                    id.as_str(),
                                ),
                                true,
                            )
                            .unwrap();
                    }
                }
                b.iter(|| {
                    let summaries = store.list_thread_summaries().unwrap();
                    black_box(summaries);
                });
            },
        );
    }
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
    bench_principal_store,
    bench_conversation_resolver,
    bench_ephemeral_sweeper,
    bench_conversation_metadata,
    bench_list_thread_summaries,
);
criterion_main!(benches);
