//! Microbenchmarks for execlaw-server hot paths (§0 axiom #14).
//!
//! Covers JWT access-token issue/verify and the runner WS frame codec.
//! Capability-token benches were retired with the dead `capability`
//! module (2026-04-28 prune); the in-process tool dispatcher gates on
//! the policy `capability_set` directly and never minted a JWT bearer.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use execlaw_server::auth::JwtSigner;

fn bench_jwt_access(c: &mut Criterion) {
    let signer = JwtSigner::generate("execlaw-bench".into());
    c.bench_function("issue_access_token", |b| {
        b.iter(|| {
            signer
                .issue_access_token(black_box("pri_ctrl"), black_box("sess-1"), black_box(60))
                .unwrap()
        })
    });
    let tok = signer.issue_access_token("pri_ctrl", "sess-1", 60).unwrap();
    c.bench_function("verify_access_token", |b| {
        b.iter(|| signer.verify_access_token(black_box(&tok)).unwrap())
    });
}

// ---------------------------------------------------------------------------
// Phase 13.C audit closure — voice hot paths.
//
// Budgets (§2.13 + 13.C audit):
//   * decode_pcm16_le / encode_pcm16_le — sub-millisecond per chunk;
//     they run for every inbound + outbound voice frame at ~250ms cadence.
//   * pcm_to_wav — sub-millisecond per utterance; called once on flush.
//   * voice_session::observe_frame — sub-millisecond per inbound frame
//     (the lock + jitter-buffer reorder runs on the WS hot path).
//   * voice_runtime::ingest_chunks — ≤ 5ms per chunk batch (the lock
//     acquire + STT push + dedup runs every released chunk).
//
// We bench the pure functions where possible so a regression shows
// up cleanly without runtime + tokio overhead.
// ---------------------------------------------------------------------------

fn bench_voice_pcm_codecs(c: &mut Criterion) {
    use execlaw_server::voice_clients::WhisperClient;

    // Typical ~1s voice utterance at 16 kHz mono. The encoded path
    // runs per outbound TTS chunk too; the per-chunk size we use
    // there (OUTBOUND_CHUNK_SAMPLES = 2400) is smaller, but the
    // larger buffer is the worst case.
    let samples_1s: Vec<i16> = (0..16_000_i16).collect();
    let bytes_1s: Vec<u8> = {
        let mut out = Vec::with_capacity(samples_1s.len() * 2);
        for s in &samples_1s {
            out.extend_from_slice(&s.to_le_bytes());
        }
        out
    };

    c.bench_function("voice/pcm_to_wav_1s_16k", |b| {
        b.iter(|| {
            // Pure function — exposed via test-only module path on
            // WhisperClient. We re-implement here to avoid coupling
            // to a pub(crate) helper; the encoding is identical.
            let n_samples = samples_1s.len() as u32;
            let data_size = n_samples * 2;
            let mut out = Vec::with_capacity(44 + data_size as usize);
            out.extend_from_slice(b"RIFF");
            out.extend_from_slice(&(36 + data_size).to_le_bytes());
            out.extend_from_slice(b"WAVE");
            out.extend_from_slice(b"fmt ");
            out.extend_from_slice(&16u32.to_le_bytes());
            out.extend_from_slice(&1u16.to_le_bytes());
            out.extend_from_slice(&1u16.to_le_bytes());
            out.extend_from_slice(&16_000u32.to_le_bytes());
            out.extend_from_slice(&32_000u32.to_le_bytes());
            out.extend_from_slice(&2u16.to_le_bytes());
            out.extend_from_slice(&16u16.to_le_bytes());
            out.extend_from_slice(b"data");
            out.extend_from_slice(&data_size.to_le_bytes());
            for s in black_box(&samples_1s) {
                out.extend_from_slice(&s.to_le_bytes());
            }
            black_box(out);
        })
    });

    // The runtime's decode/encode use module-private functions; we
    // bench equivalent inline implementations (the prod functions
    // are 5 lines each — drift risk is low).
    c.bench_function("voice/decode_pcm16_le_1s_16k", |b| {
        b.iter(|| {
            let bytes = black_box(&bytes_1s);
            let mut out = Vec::with_capacity(bytes.len() / 2);
            let mut i = 0;
            while i + 1 < bytes.len() {
                out.push(i16::from_le_bytes([bytes[i], bytes[i + 1]]));
                i += 2;
            }
            black_box(out);
        })
    });

    c.bench_function("voice/encode_pcm16_le_chunk", |b| {
        // 2400 samples = OUTBOUND_CHUNK_SAMPLES (one ~100ms TTS chunk).
        let chunk: Vec<i16> = (0..2400_i16).collect();
        b.iter(|| {
            let chunk = black_box(&chunk);
            let mut out = Vec::with_capacity(chunk.len() * 2);
            for s in chunk {
                out.extend_from_slice(&s.to_le_bytes());
            }
            black_box(out);
        })
    });

    // Reference the whisper client to keep the import live + force
    // a compile error if the client signature drifts (no-op call).
    let _client = WhisperClient::new("http://0.0.0.0:1");
    black_box(_client);
}

fn bench_voice_observe_frame(c: &mut Criterion) {
    use execlaw_server::EventBus;
    use execlaw_server::voice_frame::VoiceFrameHeader;
    use execlaw_server::voice_session::VoiceSessionRegistry;

    let payload: Vec<u8> = vec![0u8; 2_000]; // ~10ms of pcm16 @ 16kHz
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    c.bench_function("voice/observe_frame_in_order", |b| {
        let bus = EventBus::new();
        let registry = VoiceSessionRegistry::new(bus);
        let mut seq: u32 = 0;
        b.iter(|| {
            let header = VoiceFrameHeader {
                session: "bench".into(),
                seq,
                codec: "pcm16le".into(),
                sample_rate: 16_000,
                channels: 1,
                ts_ms: None,
            };
            let outcome = rt.block_on(registry.observe_frame(
                black_box(&header),
                black_box(&payload),
            ));
            black_box(outcome);
            seq = seq.wrapping_add(1);
        })
    });
}

fn bench_voice_ingest_chunks(c: &mut Criterion) {
    use execlaw_server::EventBus;
    use execlaw_server::voice_runtime::{SttFactory, TtsFactory, VoiceRuntime};
    use execlaw_server::voice_session::OrderedAudioChunk;
    use execlaw_voice_pipeline::traits::{MockStt, MockTts, TtsClient};
    use std::sync::Arc;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    c.bench_function("voice/ingest_chunks_pcm16_320samples", |b| {
        let bus = EventBus::new();
        let stt: SttFactory =
            Arc::new(|| Box::new(MockStt::new(Vec::new(), String::new())));
        let tts: TtsFactory =
            Arc::new(|| (Box::new(MockTts::default()) as Box<dyn TtsClient>, None));
        let runtime = VoiceRuntime::new(bus, stt, tts);
        let mut seq: u32 = 0;
        // ~20ms of pcm16 @ 16kHz = 320 samples = 640 bytes.
        let payload: Vec<u8> = vec![0u8; 640];
        b.iter(|| {
            let chunk = OrderedAudioChunk {
                session: "bench".into(),
                seq,
                codec: "pcm16le".into(),
                sample_rate: 16_000,
                channels: 1,
                payload: payload.clone(),
            };
            rt.block_on(runtime.ingest_chunks(black_box(std::slice::from_ref(&chunk))));
            seq = seq.wrapping_add(1);
        })
    });
}

// ---------------------------------------------------------------------------
// Phase 16 — runner supervisor hot paths.
//
// Budgets:
//   * principal_set_hash — ≤ 5µs for ≤16 principals. Called once per
//     turn to resolve the group_id; the SHA-256 is the only crypto
//     on this path.
//   * runner registry get — ≤ 1µs. Called once per turn (sometimes
//     more, e.g. cancel_turn). DashMap reads are usually 200ns range.
//   * frame encode/decode for ServerToRunner / RunnerToServer —
//     ≤ 50µs per frame for typical token deltas. WS pumps thousands
//     of these per turn for streaming chats; serde_json overhead
//     dominates and we want a clean baseline.
// ---------------------------------------------------------------------------

fn bench_runner_principal_group_hash(c: &mut Criterion) {
    use execlaw_core::ids::PrincipalId;
    use execlaw_core::principal_groups::principal_set_hash;
    let small: Vec<PrincipalId> = vec![PrincipalId::from("controller")];
    let medium: Vec<PrincipalId> = (0..8)
        .map(|i| PrincipalId::from(format!("principal-{i:02}")))
        .collect();
    let large: Vec<PrincipalId> = (0..32)
        .map(|i| PrincipalId::from(format!("principal-{i:02}")))
        .collect();
    c.bench_function("principal_set_hash/1", |b| {
        b.iter(|| principal_set_hash(black_box(&small)))
    });
    c.bench_function("principal_set_hash/8", |b| {
        b.iter(|| principal_set_hash(black_box(&medium)))
    });
    c.bench_function("principal_set_hash/32", |b| {
        b.iter(|| principal_set_hash(black_box(&large)))
    });
}

fn bench_runner_supervisor_lookup(c: &mut Criterion) {
    use execlaw_server::events::EventBus;
    use execlaw_server::runner_supervisor::RunnerSupervisor;
    let db = execlaw_core::Database::open(
        &execlaw_core::db::DbConfig::in_memory_unencrypted(),
    )
    .unwrap();
    execlaw_core::MigrationRunner::new(&db).apply_all().unwrap();
    let sup = RunnerSupervisor::new(db, EventBus::new());
    // Seed 64 supervisor entries via the public auth path so the
    // DashMap shard count reflects realistic working sets.
    for i in 0..64 {
        let key = format!("g-{i:03}");
        let (sec, _) = sup.register_pending_spawn(&key);
        let _ = sup.accept_registration(&key, &sec, i == 0);
    }
    c.bench_function("runner_supervisor_get_hit", |b| {
        b.iter(|| sup.get(black_box("g-032")).map(|h| h.group_id))
    });
    c.bench_function("runner_supervisor_get_miss", |b| {
        b.iter(|| sup.get(black_box("g-not-here")).map(|h| h.group_id))
    });
}

fn bench_runner_frame_codec(c: &mut Criterion) {
    use execlaw_runner_protocol::{RunnerToServer, ServerToRunner};
    let token_delta = RunnerToServer::TokenDelta {
        turn_id: "turn-1234".into(),
        conversation_id: "conv-abc".into(),
        text: "Hello, this is a streaming token delta.".into(),
    };
    let cancel = ServerToRunner::CancelTurn {
        turn_id: "turn-1234".into(),
    };
    c.bench_function("runner_frame_encode_token_delta", |b| {
        b.iter(|| serde_json::to_string(black_box(&token_delta)).unwrap())
    });
    let encoded =
        serde_json::to_string(&token_delta).unwrap();
    c.bench_function("runner_frame_decode_token_delta", |b| {
        b.iter(|| {
            serde_json::from_str::<RunnerToServer>(black_box(&encoded)).unwrap()
        })
    });
    c.bench_function("runner_frame_encode_cancel", |b| {
        b.iter(|| serde_json::to_string(black_box(&cancel)).unwrap())
    });
}

criterion_group!(
    benches,
    bench_jwt_access,
    bench_voice_pcm_codecs,
    bench_voice_observe_frame,
    bench_voice_ingest_chunks,
    bench_runner_principal_group_hash,
    bench_runner_supervisor_lookup,
    bench_runner_frame_codec,
);
criterion_main!(benches);
