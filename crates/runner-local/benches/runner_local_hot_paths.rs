//! Microbenchmarks for execlaw-runner-local (§0 axiom #14).
//!
//! Run with `cargo bench -p execlaw-runner-local`.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use execlaw_core::conversation::{
    ConversationKind, ConversationRow, ConversationStore, Modality, Phase,
};
use execlaw_core::db::{Database, DbConfig};
use execlaw_core::ids::{ConversationId, EventSeq};
use execlaw_core::migrations::MigrationRunner;
use execlaw_runner_local::thread_tool::{ThreadToolDispatcher, thread_tool_definitions};

fn fresh_db_with_conv(id: &str) -> (Database, ConversationId) {
    let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
    MigrationRunner::new(&db).apply_all().unwrap();
    let cid = ConversationId::from(id);
    ConversationStore::new(&db)
        .upsert(&ConversationRow {
            conversation_id: cid.clone(),
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
        })
        .unwrap();
    (db, cid)
}

fn bench_thread_tool(c: &mut Criterion) {
    let mut group = c.benchmark_group("thread_tool");

    // Tool-definition list — built once per turn at most. Should be
    // negligible but worth a regression guard.
    group.bench_function("definitions", |b| {
        b.iter(|| black_box(thread_tool_definitions()));
    });

    // Successful set_thread_name dispatch — UPDATE behind a single
    // statement; budget ≤ 200µs.
    group.bench_function("dispatch_set_thread_name_ok", |b| {
        let (db, cid) = fresh_db_with_conv("c-bench");
        let dispatcher =
            ThreadToolDispatcher::new(ConversationStore::new(&db), cid.clone());
        let args = serde_json::json!({"name": "Q4 plans recap"});
        b.iter(|| {
            let r = dispatcher
                .dispatch(black_box("set_thread_name"), black_box(&args))
                .unwrap();
            black_box(r);
        });
    });

    // Validation-rejection path: pure stack work, no DB. Documents that
    // a misbehaving model burning tokens by spamming bad inputs costs
    // us nothing.
    group.bench_function("dispatch_set_thread_name_too_long", |b| {
        let (db, cid) = fresh_db_with_conv("c-bench-long");
        let dispatcher =
            ThreadToolDispatcher::new(ConversationStore::new(&db), cid.clone());
        let args = serde_json::json!({"name": "x".repeat(256)});
        b.iter(|| {
            let _ = dispatcher
                .dispatch(black_box("set_thread_name"), black_box(&args))
                .unwrap_err();
        });
    });

    group.finish();
}

criterion_group!(benches, bench_thread_tool);
criterion_main!(benches);
