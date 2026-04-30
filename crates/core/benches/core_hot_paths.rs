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
use execlaw_core::backends::{BackendMode, BackendPurpose, BackendStore, BackendUpsert};
use execlaw_core::conversation::{
    ConversationKind, ConversationRow, ConversationStore, Modality, Phase,
};
use execlaw_core::db::{Database, DbConfig};
use execlaw_core::ephemeral_sweeper::sweep_once;
use execlaw_core::event_hmac::{canonical_bytes, sign_event, verify_event};
use execlaw_core::events::EventRecord as CoreEventRecord;
use execlaw_core::events::{
    EventKind, EventLog, EventRecord, PendingEvent, ToolResultPayload, ToolUsePayload,
};
use execlaw_core::ids::ResearchJobId;
use execlaw_core::ids::{ConversationId, EventSeq, IdempotencyKey, TurnSeq};
use execlaw_core::migrations::MigrationRunner;
use execlaw_core::outbox::{OutboxRow, OutboxStatus, OutboxStore};
use execlaw_core::refresh_tokens::RefreshTokenStore;
use execlaw_core::research::{
    PhaseGates, PlanStep, ResearchConfigStore, ResearchConfigUpdate, ResearchJobStore,
    ResearchNote, ResearchPlan, ResearchSource, SubQueryState,
};
use execlaw_core::tool_access::{ToolAccessSeed, ToolAccessStore, ToolSource};
use execlaw_core::transport_conversations::{ConversationResolver, ResolveInput};
use execlaw_core::webauthn::{WebauthnCredentialRow, WebauthnStore};

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
        b.iter(|| IdempotencyKey::mint(black_box(&cid), black_box(TurnSeq(47)), black_box(3)))
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
        b.iter(|| {
            log.replay_since(black_box(&cid), black_box(EventSeq(0)))
                .unwrap()
        })
    });
    c.bench_function("replay_since_450_of_500", |b| {
        b.iter(|| {
            log.replay_since(black_box(&cid), black_box(EventSeq(450)))
                .unwrap()
        })
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
            store
                .ready_pending(black_box(1_000_000_000), black_box(32))
                .unwrap()
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

    // list_all powers GET /api/admin/principals. Bench at scale so the
    // Settings → Principals page stays snappy when the population
    // grows. Reuses the 100-row population from above.
    group.bench_function("list_all_100", |b| {
        b.iter(|| {
            let all = store.list_all().unwrap();
            black_box(all);
        })
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
        last_activity_at: 0,
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
                            execlaw_core::events::EventLog::new(&db)
                                .append(&ev)
                                .unwrap();
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
        group.bench_with_input(BenchmarkId::new("threads", n), n, |b, &n| {
            let db = fresh_db();
            let store = ConversationStore::new(&db);
            for i in 0..n {
                let id = format!("conv-{i}");
                let mut row = fresh_conv_row(&id);
                row.last_seq = EventSeq(i as i64);
                store.upsert(&row).unwrap();
                if i % 50 == 0 {
                    store
                        .set_pinned(&execlaw_core::ids::ConversationId::from(id.as_str()), true)
                        .unwrap();
                }
            }
            b.iter(|| {
                let summaries = store.list_thread_summaries().unwrap();
                black_box(summaries);
            });
        });
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// Deployment registry — Settings → Deployments page calls list every
// time it mounts. Budget ≤ 1ms even with 64 deployments (the realistic
// upper bound: one row per (purpose × backend × variant)).
// ---------------------------------------------------------------------------

fn bench_backend_store(c: &mut Criterion) {
    // Phase 8.5 — `config_backends` has at most 5 rows (one per
    // purpose), so we don't sweep across sizes. We bench
    // list_all (the SPA's hot read) at the actual upper bound and
    // upsert (the operator's edit, off the dispatch path).
    let mut group = c.benchmark_group("backend_store");

    group.bench_function("list_all/full", |b| {
        let db = fresh_db();
        let store = BackendStore::new(&db);
        for p in BackendPurpose::all() {
            store
                .upsert(
                    &BackendUpsert {
                        purpose: *p,
                        inference_backend: "service-vllm".into(),
                        model_spec_json: serde_json::json!({"model": "Qwen3.5-27B-AWQ"}),
                        gpu_id: None,
                        endpoint: Some("http://127.0.0.1:8000/v1".into()),
                        notes: None,
                        reasoning_enabled: false,
                        mode: BackendMode::External,
                    },
                    0,
                )
                .unwrap();
        }
        b.iter(|| {
            let rows = store.list_all().unwrap();
            black_box(rows);
        });
    });

    group.bench_function("upsert", |b| {
        let db = fresh_db();
        let store = BackendStore::new(&db);
        b.iter(|| {
            let row = store
                .upsert(
                    &BackendUpsert {
                        purpose: BackendPurpose::Standard,
                        inference_backend: "service-vllm".into(),
                        model_spec_json: serde_json::json!({"m": "x"}),
                        gpu_id: None,
                        endpoint: None,
                        notes: None,
                        reasoning_enabled: false,
                        mode: BackendMode::External,
                    },
                    0,
                )
                .unwrap();
            black_box(row);
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// WebauthnStore — count_for_user is hit on EVERY login (gates the
// second-factor branch), so it has to stay below ~50µs. list_for_user
// is hit only when the second factor activates and is allowed to be
// linear in cred count up to MAX_CREDENTIALS_PER_USER.
// ---------------------------------------------------------------------------

fn bench_webauthn_store(c: &mut Criterion) {
    let mut group = c.benchmark_group("webauthn_store");

    fn seed_user_and_creds(db: &Database, n: usize) {
        // Insert a user row so the FK on state_webauthn_credentials
        // is satisfied — match the shape the production user-row
        // insert uses.
        db.with_conn(|c| {
            c.execute(
                "INSERT INTO users \
                 (user_id, username, display_name, email, password_hash, role, \
                  created_at, last_login_at) \
                 VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6, NULL)",
                rusqlite::params!["u1", "alice", "Alice", "argon2-hash", "controller", 0i64],
            )?;
            Ok(())
        })
        .unwrap();
        let store = WebauthnStore::new(db);
        for i in 0..n {
            store
                .insert(&WebauthnCredentialRow {
                    credential_id: format!("cred-{i}"),
                    user_id: "u1".into(),
                    label: format!("key-{i}"),
                    passkey_json: r#"{"opaque":"blob"}"#.into(),
                    counter: 0,
                    created_at: i as i64,
                    last_used_at: None,
                })
                .unwrap();
        }
    }

    // Login-path hot spot: count_for_user runs on every /api/login,
    // even when the user has no webauthn registered. Must stay tiny.
    group.bench_function("count_for_user/0", |b| {
        let db = fresh_db();
        seed_user_and_creds(&db, 0);
        let store = WebauthnStore::new(&db);
        b.iter(|| {
            let n = store.count_for_user(black_box("u1")).unwrap();
            black_box(n);
        });
    });
    group.bench_function("count_for_user/3", |b| {
        let db = fresh_db();
        seed_user_and_creds(&db, 3);
        let store = WebauthnStore::new(&db);
        b.iter(|| {
            let n = store.count_for_user(black_box("u1")).unwrap();
            black_box(n);
        });
    });

    // Authentication-ceremony path: list_for_user assembles the
    // candidate list passed to start_passkey_authentication.
    for n in [1usize, 5usize, 10usize].iter() {
        group.bench_with_input(BenchmarkId::new("list_for_user", n), n, |b, &n| {
            let db = fresh_db();
            seed_user_and_creds(&db, n);
            let store = WebauthnStore::new(&db);
            b.iter(|| {
                let rows = store.list_for_user(black_box("u1")).unwrap();
                black_box(rows);
            });
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// RefreshTokenStore — every /api/login + /api/token/refresh writes a
// row, every /api/token/refresh + /api/logout consumes one. The
// consume path is on the silent-retry hot path, so it's the most
// latency-sensitive of the three. revoke_all_for_user only fires on
// "sign out everywhere", but it's interesting to know it's not
// accidentally O(n²).
// ---------------------------------------------------------------------------

fn bench_refresh_token_store(c: &mut Criterion) {
    let mut group = c.benchmark_group("refresh_token_store");

    group.bench_function("issue", |b| {
        let db = fresh_db();
        let store = RefreshTokenStore::new(&db);
        b.iter(|| {
            let tok = store.issue(black_box("u"), black_box("s"), 3600).unwrap();
            black_box(tok);
        });
    });

    group.bench_function("consume_hit", |b| {
        let db = fresh_db();
        let store = RefreshTokenStore::new(&db);
        // Pre-issue tokens so each consume has a row to delete.
        // Issue one per iteration via a queue to avoid measuring the
        // issue cost.
        b.iter_batched(
            || store.issue("u", "s", 3600).unwrap(),
            |tok| {
                let row = store.consume(black_box(&tok)).unwrap();
                black_box(row);
            },
            criterion::BatchSize::SmallInput,
        );
    });

    group.bench_function("consume_miss", |b| {
        // Hot path when an attacker reuses a consumed token: the
        // row is gone and we should fail-fast at the DELETE.
        let db = fresh_db();
        let store = RefreshTokenStore::new(&db);
        b.iter(|| {
            let row = store.consume(black_box("definitely-not-a-token")).unwrap();
            black_box(row);
        });
    });

    for n in [1usize, 16usize, 64usize].iter() {
        group.bench_with_input(BenchmarkId::new("revoke_all_for_user", n), n, |b, &n| {
            b.iter_batched(
                || {
                    let db = fresh_db();
                    let store = RefreshTokenStore::new(&db);
                    for _ in 0..n {
                        store.issue("u", "s", 3600).unwrap();
                    }
                    db
                },
                |db| {
                    let store = RefreshTokenStore::new(&db);
                    let removed = store.revoke_all_for_user(black_box("u")).unwrap();
                    black_box(removed);
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// ToolAccessStore — runs on EVERY tool dispatch via the chained
// dispatch's pre-gate. Has to be tiny because the runner can issue
// dozens of tool calls per turn.
// ---------------------------------------------------------------------------

fn bench_tool_access_store(c: &mut Criterion) {
    let mut group = c.benchmark_group("tool_access_store");

    group.bench_function("get_hit", |b| {
        let db = fresh_db();
        let store = ToolAccessStore::new(&db);
        store
            .upsert_seen(
                &ToolAccessSeed {
                    tool_name: "read_memory".into(),
                    source: ToolSource::Builtin,
                    source_id: None,
                    description: None,
                    input_schema: None,
                    default_allowed_classes: vec!["Controller".into(), "KnownTrusted".into()],
                },
                100,
            )
            .unwrap();
        b.iter(|| {
            let row = store.get(black_box("read_memory")).unwrap();
            black_box(row);
        });
    });

    group.bench_function("get_miss", |b| {
        let db = fresh_db();
        let store = ToolAccessStore::new(&db);
        b.iter(|| {
            let row = store.get(black_box("never_seen_tool")).unwrap();
            black_box(row);
        });
    });

    for n in [4usize, 32usize, 128usize].iter() {
        group.bench_with_input(BenchmarkId::new("list_all", n), n, |b, &n| {
            let db = fresh_db();
            let store = ToolAccessStore::new(&db);
            for i in 0..n {
                store
                    .upsert_seen(
                        &ToolAccessSeed {
                            tool_name: format!("tool-{i}"),
                            source: ToolSource::Plugin,
                            source_id: Some(format!("plugin-{}", i % 4)),
                            description: None,
                            input_schema: None,
                            default_allowed_classes: vec!["Controller".into()],
                        },
                        100,
                    )
                    .unwrap();
            }
            b.iter(|| {
                let rows = store.list_all().unwrap();
                black_box(rows);
            });
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// research JobStore + ConfigStore + plan codec hot paths.
//
// Budgets (single in-memory SQLite, debug build):
//   * insert_pending           ≤ 200µs   (single INSERT + read-back)
//   * claim_next_pending       ≤ 300µs   (txn: SELECT + UPDATE + read-back)
//   * set_planned              ≤ 200µs   (single UPDATE + rmp encode)
//   * list_for_conversation/64 ≤ 400µs   (scan within one conv, decode N rows)
//   * active_count_for_conv/64 ≤ 100µs   (count over indexed conv)
//   * config_get               ≤ 100µs   (singleton-row SELECT)
//   * to_summary (plan decode) ≤ 50µs    (rmp_serde::from_slice on small blob)
//
// Anything materially over budget is a perf regression and gets fixed
// before merge per axiom #14.
// ---------------------------------------------------------------------------

fn fixture_plan() -> ResearchPlan {
    ResearchPlan {
        thesis: "compare runtime quality between two open-weights models on \
                 fast-path single-turn benchmarks across 8 sub-queries"
            .into(),
        steps: (0..8)
            .map(|i| PlanStep {
                query: format!("sub-query {i}"),
                rationale: Some(format!("rationale for sub-query {i}")),
            })
            .collect(),
    }
}

fn bench_research_job_store(c: &mut Criterion) {
    let mut group = c.benchmark_group("research_job_store");

    group.bench_function("insert_pending", |b| {
        b.iter_batched(
            || {
                let db = fresh_db();
                let cid = ConversationId::from("conv-research-bench");
                (db, cid)
            },
            |(db, cid)| {
                let store = ResearchJobStore::new(&db);
                let id = ResearchJobId::new();
                let row = store
                    .insert_pending(
                        black_box(&id),
                        black_box(&cid),
                        black_box("what's new in Kokoro 2026?"),
                        black_box("Controller"),
                        None,
                        100,
                    )
                    .unwrap();
                black_box(row);
            },
            criterion::BatchSize::SmallInput,
        )
    });

    group.bench_function("claim_next_pending", |b| {
        b.iter_batched(
            || {
                let db = fresh_db();
                let cid = ConversationId::from("conv-research-bench");
                let store = ResearchJobStore::new(&db);
                store
                    .insert_pending(
                        &ResearchJobId::new(),
                        &cid,
                        "claim-bench",
                        "Controller",
                        None,
                        100,
                    )
                    .unwrap();
                db
            },
            |db| {
                let store = ResearchJobStore::new(&db);
                let claimed = store.claim_next_pending(black_box("card-x"), 200).unwrap();
                black_box(claimed);
            },
            criterion::BatchSize::SmallInput,
        )
    });

    // claim against an empty Pending queue — supervisor's no-op tick
    // hits this path hundreds of times per hour. Should be cheap.
    group.bench_function("claim_next_pending_empty_queue", |b| {
        b.iter_batched(
            fresh_db,
            |db| {
                let store = ResearchJobStore::new(&db);
                let claimed = store.claim_next_pending(black_box("card-x"), 200).unwrap();
                black_box(claimed);
            },
            criterion::BatchSize::SmallInput,
        )
    });

    group.bench_function("set_planned", |b| {
        b.iter_batched(
            || {
                let db = fresh_db();
                let cid = ConversationId::from("conv");
                let store = ResearchJobStore::new(&db);
                let id = ResearchJobId::new();
                store
                    .insert_pending(&id, &cid, "q", "Controller", None, 100)
                    .unwrap();
                store.claim_next_pending("card-1", 150).unwrap();
                (db, id, fixture_plan())
            },
            |(db, id, plan)| {
                ResearchJobStore::new(&db)
                    .set_planned(black_box(&id), black_box(&plan), 200)
                    .unwrap();
            },
            criterion::BatchSize::SmallInput,
        )
    });

    for n in [4usize, 16, 64].iter() {
        group.bench_with_input(BenchmarkId::new("list_for_conversation", n), n, |b, &n| {
            let db = fresh_db();
            let cid = ConversationId::from("conv-research-list");
            let store = ResearchJobStore::new(&db);
            for i in 0..n {
                let id = ResearchJobId::new();
                store
                    .insert_pending(
                        &id,
                        &cid,
                        &format!("query {i}"),
                        "Controller",
                        None,
                        100 + i as i64,
                    )
                    .unwrap();
                // Half of them have plans landed so the per-row
                // decode cost is exercised.
                if i % 2 == 0 {
                    store
                        .claim_next_pending(&format!("card-{i}"), 200 + i as i64)
                        .unwrap();
                    store
                        .set_planned(&id, &fixture_plan(), 250 + i as i64)
                        .unwrap();
                }
            }
            b.iter(|| {
                let rows = store.list_for_conversation(black_box(&cid)).unwrap();
                black_box(rows.iter().map(|r| r.to_summary()).collect::<Vec<_>>());
            });
        });
    }

    for n in [4usize, 16, 64].iter() {
        group.bench_with_input(
            BenchmarkId::new("active_count_for_conversation", n),
            n,
            |b, &n| {
                let db = fresh_db();
                let cid = ConversationId::from("conv-research-count");
                let store = ResearchJobStore::new(&db);
                for i in 0..n {
                    store
                        .insert_pending(
                            &ResearchJobId::new(),
                            &cid,
                            &format!("query {i}"),
                            "Controller",
                            None,
                            100 + i as i64,
                        )
                        .unwrap();
                }
                b.iter(|| {
                    let n = store
                        .active_count_for_conversation(black_box(&cid))
                        .unwrap();
                    black_box(n);
                });
            },
        );
    }

    group.finish();
}

fn bench_research_config_store(c: &mut Criterion) {
    let mut group = c.benchmark_group("research_config_store");

    group.bench_function("get_seeded_defaults", |b| {
        let db = fresh_db();
        let store = ResearchConfigStore::new(&db);
        b.iter(|| {
            let cfg = store.get().unwrap();
            black_box(cfg);
        });
    });

    group.bench_function("update_single_field", |b| {
        b.iter_batched(
            fresh_db,
            |db| {
                let store = ResearchConfigStore::new(&db);
                let saved = store
                    .update(
                        &ResearchConfigUpdate {
                            phase_gates: Some(PhaseGates::None),
                            ..Default::default()
                        },
                        500,
                    )
                    .unwrap();
                black_box(saved);
            },
            criterion::BatchSize::SmallInput,
        )
    });

    group.finish();
}

fn fixture_notes(n: usize) -> Vec<ResearchNote> {
    (0..n)
        .map(|i| ResearchNote {
            index: i as u32,
            sub_query: format!("sub-query {i}"),
            state: if i % 3 == 0 {
                SubQueryState::Done
            } else if i % 3 == 1 {
                SubQueryState::Running
            } else {
                SubQueryState::Failed
            },
            excerpt: format!(
                "Extracted facts for sub-query {i}: lorem ipsum dolor sit amet, \
                 consectetur adipiscing elit. Sed do eiusmod tempor incididunt \
                 ut labore et dolore magna aliqua. Ut enim ad minim veniam.",
            ),
            sources: vec![ResearchSource {
                url: format!("https://example.com/source-{i}"),
                title: Some(format!("Source {i}")),
                fetched_ok: true,
                error: None,
            }],
            tokens_used: Some(123),
            error: None,
        })
        .collect()
}

// C4 — gather-phase JobStore hot paths.
//
// Budgets:
//   * mark_gathering           ≤ 200µs (single UPDATE w/ status guard)
//   * mark_synthesizing        ≤ 200µs (same)
//   * set_notes/8              ≤ 300µs (encode + UPDATE)
//   * set_notes/64             ≤ 600µs (per-row decode cost grows)
//
// The set_notes path runs once per worker completion in the gather
// phase — i.e. up to `parallel_workers` times per phase. The 64-step
// case is the worst plausible plan we'd ever ship.
fn bench_research_gather_paths(c: &mut Criterion) {
    let mut group = c.benchmark_group("research_gather");

    group.bench_function("mark_gathering", |b| {
        b.iter_batched(
            || {
                let db = fresh_db();
                let cid = ConversationId::from("conv-gather-bench");
                let store = ResearchJobStore::new(&db);
                let id = ResearchJobId::new();
                store
                    .insert_pending(&id, &cid, "q", "Controller", None, 100)
                    .unwrap();
                store.claim_next_pending("card-1", 110).unwrap();
                store.set_planned(&id, &fixture_plan(), 120).unwrap();
                (db, id)
            },
            |(db, id)| {
                let n = ResearchJobStore::new(&db)
                    .mark_gathering(black_box(&id), 200)
                    .unwrap();
                black_box(n);
            },
            criterion::BatchSize::SmallInput,
        )
    });

    group.bench_function("mark_synthesizing", |b| {
        b.iter_batched(
            || {
                let db = fresh_db();
                let cid = ConversationId::from("conv-gather-bench");
                let store = ResearchJobStore::new(&db);
                let id = ResearchJobId::new();
                store
                    .insert_pending(&id, &cid, "q", "Controller", None, 100)
                    .unwrap();
                store.claim_next_pending("card-1", 110).unwrap();
                store.set_planned(&id, &fixture_plan(), 120).unwrap();
                store.mark_gathering(&id, 130).unwrap();
                (db, id)
            },
            |(db, id)| {
                let n = ResearchJobStore::new(&db)
                    .mark_synthesizing(black_box(&id), 200)
                    .unwrap();
                black_box(n);
            },
            criterion::BatchSize::SmallInput,
        )
    });

    for n in [4usize, 8, 64].iter() {
        group.bench_with_input(BenchmarkId::new("set_notes", n), n, |b, &n| {
            b.iter_batched(
                || {
                    let db = fresh_db();
                    let cid = ConversationId::from("conv");
                    let store = ResearchJobStore::new(&db);
                    let id = ResearchJobId::new();
                    store
                        .insert_pending(&id, &cid, "q", "Controller", None, 100)
                        .unwrap();
                    let notes = fixture_notes(n);
                    (db, id, notes)
                },
                |(db, id, notes)| {
                    ResearchJobStore::new(&db)
                        .set_notes(black_box(&id), black_box(&notes), 200)
                        .unwrap();
                },
                criterion::BatchSize::SmallInput,
            )
        });
    }

    group.finish();
}

fn bench_research_plan_codec(c: &mut Criterion) {
    let mut group = c.benchmark_group("research_plan_codec");
    let plan = fixture_plan();
    let encoded = rmp_serde::to_vec(&plan).unwrap();
    group.throughput(Throughput::Bytes(encoded.len() as u64));

    group.bench_function("encode", |b| {
        b.iter(|| {
            let bytes = rmp_serde::to_vec(black_box(&plan)).unwrap();
            black_box(bytes);
        })
    });

    group.bench_function("decode", |b| {
        b.iter(|| {
            let plan: ResearchPlan = rmp_serde::from_slice(black_box(&encoded)).unwrap();
            black_box(plan);
        })
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
    bench_principal_store,
    bench_conversation_resolver,
    bench_ephemeral_sweeper,
    bench_conversation_metadata,
    bench_list_thread_summaries,
    bench_backend_store,
    bench_webauthn_store,
    bench_refresh_token_store,
    bench_tool_access_store,
    bench_research_job_store,
    bench_research_config_store,
    bench_research_plan_codec,
    bench_research_gather_paths,
);
criterion_main!(benches);
