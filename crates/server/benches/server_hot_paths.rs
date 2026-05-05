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
            let outcome =
                rt.block_on(registry.observe_frame(black_box(&header), black_box(&payload)));
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
        let stt: SttFactory = Arc::new(|| Box::new(MockStt::new(Vec::new(), String::new())));
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
    let db =
        execlaw_core::Database::open(&execlaw_core::db::DbConfig::in_memory_unencrypted()).unwrap();
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
    let encoded = serde_json::to_string(&token_delta).unwrap();
    c.bench_function("runner_frame_decode_token_delta", |b| {
        b.iter(|| serde_json::from_str::<RunnerToServer>(black_box(&encoded)).unwrap())
    });
    c.bench_function("runner_frame_encode_cancel", |b| {
        b.iter(|| serde_json::to_string(black_box(&cancel)).unwrap())
    });
}

// ---------------------------------------------------------------------------
// 2026-05-03 — deep-research web-tooling hot paths.
//
// Two new benches lock in the perf budget on the gather phase's
// per-page CPU work after the rev-5 swap from regex+truncate to
// scraper+dom_smoothie. The pre-existing implementations were
// chosen partly for their constant-time character; the new ones
// are richer but still need to fit inside a per-URL slice of the
// gather worker's wall budget.
//
// Budgets:
//   * research/parse_ddg_html — < 5 ms per result page (gather
//     calls it once per sub-query; with parallel_workers=3 a
//     50ms regression here costs the whole job 150 ms+).
//   * research/extract_readable_text — < 50 ms per typical
//     article page (gather runs this from spawn_blocking, but
//     a regression past ~100 ms starts compounding across
//     parallel workers + the per-job page cap).
// ---------------------------------------------------------------------------

fn bench_research_parse_ddg_html(c: &mut Criterion) {
    use execlaw_server::tool_apis_search::parse_ddg_html;
    // Use the live-capture fixture so the bench reflects real DDG
    // output, not a synthetic toy. ~29 KB, ~10 result blocks —
    // representative of a typical gather sub-query response.
    let html = include_str!("../src/ddg_live_fixture.html");
    c.bench_function("research/parse_ddg_html_live_capture", |b| {
        b.iter(|| {
            let results = parse_ddg_html(black_box(html), black_box(10));
            black_box(results);
        })
    });
}

fn bench_research_extract_readable_text(c: &mut Criterion) {
    use execlaw_server::research::readability_extract::extract_readable_text;
    // Synthetic article: nav + sidebar + article body + footer.
    // Roughly the shape of a typical garden / news / blog page.
    // We deliberately don't pull a multi-MB capture — the bench
    // measures the steady-state per-article cost, not the input-
    // cap defense (which has its own test).
    let html = r##"<!DOCTYPE html><html><head><title>Bench Article</title></head><body>
<nav><a>Home</a> | <a>About</a> | <a>Subscribe</a></nav>
<aside><h3>Popular</h3><ul><li>Item 1</li><li>Item 2</li></ul></aside>
<article>
  <h1>Long-Form Article For Benchmark</h1>
  <p>This is a paragraph of moderate length to give Readability some content to score.
     It contains enough text to clear the degenerate-article threshold and exercise
     the scoring + cleaning passes.</p>
  <p>A second paragraph keeps the score above the noise floor and lets the cleaning
     pass do meaningful work walking the DOM.</p>
  <p>A third paragraph for good measure — three paragraphs is roughly the lower bound
     where Readability stops bailing out as degenerate.</p>
</article>
<footer><p>Footer chrome to be stripped.</p></footer>
</body></html>"##;
    c.bench_function("research/extract_readable_text_typical_article", |b| {
        b.iter(|| {
            let outcome = extract_readable_text(
                black_box(html),
                black_box(Some("https://example.com/x")),
                black_box(4_000),
            );
            black_box(outcome);
        })
    });
}

// ---------------------------------------------------------------------------
// Phase 3 — Signal outbound dispatch.
//
// Budget: sub-1ms per `signal.send_message` invocation against an
// in-process axum mock standing in for the supervised signal-cli
// sidecar. Empirical baseline on Windows / reqwest+axum loopback is
// ~480µs, dominated by the HTTP round-trip and JSON ser/de — the
// pure dispatch logic (arg parse, capability lookup, binding query,
// outcome construction) lives in the ~50µs noise floor below that.
// Real signal-cli end-to-end is 50–500ms (Signal's servers), so any
// regression that pushes us past 1ms means we're holding up the
// agent's tool result for no good reason.
//
// What's NOT measured: the network round-trip to a real signal-cli
// container (signal-cli's own crypto + network is ~50–500 ms;
// dominated by Signal's servers, not us). The bench pins our
// CONTRIBUTION to the latency: arg parsing, capability lookup,
// binding-store query, JSON serialise, reqwest send, response
// parse. A regression here means we slowed our own dispatch path,
// not signal-cli.
// ---------------------------------------------------------------------------

fn bench_signal_send_outbound(c: &mut Criterion) {
    use execlaw_core::Database;
    use execlaw_core::db::DbConfig;
    use execlaw_core::ids::ConversationId;
    use execlaw_core::migrations::MigrationRunner;
    use execlaw_core::tool::{Clock, SystemClock, ToolCtx, ToolImpl};
    use execlaw_core::transport_bindings::TransportBindingStore;
    use execlaw_server::signal_tools::SignalSendMessageTool;
    use execlaw_server::signal_transport::{
        SIGNAL_CHANNEL, SignalCliTransport, StaticEndpointResolver,
    };
    use std::sync::Arc;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    // In-process axum mock standing in for signal-cli-rest-api.
    let mock_url = rt.block_on(async {
        use axum::Router;
        use axum::routing::post;
        let app: Router = Router::new().route(
            "/v2/send",
            post(|| async { axum::Json(serde_json::json!({ "timestamp": 1700000000_i64 })) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    });

    // Seed an in-memory DB with one binding so resolve_recipient
    // hits the canonical path. Bench measures the FULL outbound
    // dispatch — args → resolve → send — not just the HTTP call.
    let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
    MigrationRunner::new(&db).apply_all().unwrap();
    TransportBindingStore::new(&db)
        .insert_binding(SIGNAL_CHANNEL, "+15559998888", "pg-bench", false, 0)
        .unwrap();

    let resolver: Arc<dyn execlaw_server::signal_transport::RpcEndpointResolver> =
        Arc::new(StaticEndpointResolver(mock_url));
    let tool = SignalSendMessageTool::new();

    c.bench_function("signal/send_message_outbound", |b| {
        // Plain `block_on` per-iter — `Bencher::to_async` requires
        // criterion's `async_tokio` feature which isn't enabled in
        // the workspace. The cost of `block_on`-spinning on the
        // outer thread is in the noise next to the per-iter HTTP
        // round-trip; correctness pinned by the unit-test pass.
        b.iter(|| {
            rt.block_on(async {
                let transport = Arc::new(SignalCliTransport::new(
                    resolver.clone(),
                    db.clone(),
                    Some("+15551234567".into()),
                    None,
                ));
                let mut ctx = ToolCtx::empty(
                    ConversationId::from_string("conv-bench"),
                    "Controller",
                    Arc::new(SystemClock) as Arc<dyn Clock>,
                );
                ctx.transport = Some(transport);
                let outcome = tool
                    .invoke(
                        ctx,
                        serde_json::json!({ "to": "+15559998888", "text": "hi" }),
                    )
                    .await;
                black_box(outcome);
            });
        });
    });
}

// ---------------------------------------------------------------------------
// Phase 4 — Signal inbound decode.
//
// Budget: sub-50µs per WS frame. The consumer is loop-driven on the
// WS read path; every inbound message — including the receipts and
// typing indicators we drop at decode time — pays the decode cost.
// A regression here means the consumer's read loop slows for every
// contact's keystroke, which over a busy day accumulates.
//
// Doesn't measure the route step (that's `route_inbound_message` and
// dominated by SQLite which has its own bench in core). Pure JSON
// shape detection.
// ---------------------------------------------------------------------------

fn bench_signal_inbound_decode(c: &mut Criterion) {
    let text_frame = serde_json::json!({
        "account": "+15551234567",
        "envelope": {
            "source": "+15559998888",
            "sourceNumber": "+15559998888",
            "sourceName": "Alice",
            "timestamp": 1700000000000_i64,
            "dataMessage": {
                "timestamp": 1700000000000_i64,
                "message": "hi from a friend, what's the weather like today?",
                "groupInfo": null,
            }
        }
    })
    .to_string();
    let typing_frame = serde_json::json!({
        "account": "+15551234567",
        "envelope": {
            "source": "+15559998888",
            "timestamp": 1700000000000_i64,
            "typingMessage": {
                "action": "STARTED",
                "timestamp": 1700000000000_i64,
            }
        }
    })
    .to_string();

    c.bench_function("signal/decode_text_frame", |b| {
        b.iter(|| {
            let out = execlaw_server::signal_inbound::decode_frame(
                black_box(&text_frame),
                Some("+15551234567"),
            );
            black_box(out);
        });
    });

    c.bench_function("signal/decode_typing_drop", |b| {
        // Drop path: every typing indicator hits this. Should be
        // strictly faster than the text path (no String allocation
        // for the body).
        b.iter(|| {
            let out = execlaw_server::signal_inbound::decode_frame(
                black_box(&typing_frame),
                Some("+15551234567"),
            );
            black_box(out);
        });
    });
}

// ---------------------------------------------------------------------------
// Phase 5 — Signal group-name resolution.
//
// Budget: sub-200µs per `signal.add_group_members` / `signal.leave_group`
// invocation EXCLUDING the sidecar HTTP round-trip. Both tools call
// `transport.list_groups()` to map `groupName` → `group_id`; the
// matcher walks every group and exact-name compares. With 100s of
// groups in a busy operator's account this is an O(n) scan that runs
// before every group mutation.
//
// The bench measures the matcher cost only — `list_groups` is mocked
// to return a static vec so we isolate the in-memory filter cost from
// the HTTP path. A regression here means renaming a contact-list
// entry slows a destructive op the operator already feels nervous
// about (leaving a group on the agent's behalf).
// ---------------------------------------------------------------------------

fn bench_signal_group_name_resolution(c: &mut Criterion) {
    use async_trait::async_trait;
    use execlaw_core::tool::{ApiError, TransportApi, TransportGroupSummary};

    /// Static fixture: pretend the controller is in 200 groups,
    /// only one of which matches the lookup name. Models a busy
    /// operator's group list.
    struct StaticGroupList(Vec<TransportGroupSummary>);

    #[async_trait]
    impl TransportApi for StaticGroupList {
        async fn resolve_recipient(&self, _: &str, _: &str) -> Result<String, ApiError> {
            unreachable!()
        }
        async fn send(&self, _: &str, _: &str, _: &str) -> Result<String, ApiError> {
            unreachable!()
        }
        async fn current_chat_id(&self, _: &str) -> Result<Option<String>, ApiError> {
            Ok(None)
        }
        async fn list_groups(&self, _: &str) -> Result<Vec<TransportGroupSummary>, ApiError> {
            Ok(self.0.clone())
        }
    }

    let groups: Vec<TransportGroupSummary> = (0..200)
        .map(|i| TransportGroupSummary {
            id: format!("group-base64-{i:04x}"),
            name: Some(format!("Group {i}")),
            member_count: 5,
        })
        .collect();
    let transport: Box<dyn TransportApi> = Box::new(StaticGroupList(groups));

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    c.bench_function("signal/group_name_resolution_200_groups", |b| {
        b.iter(|| {
            // resolve_group_id is private to signal_tools, but the
            // hot-path inlines list_groups + filter — replicate it
            // here so the bench measures the same instructions
            // without depending on a pub helper.
            rt.block_on(async {
                let groups = transport.list_groups("signal").await.unwrap();
                let target = black_box("Group 137");
                let matches: Vec<_> = groups
                    .into_iter()
                    .filter(|g| g.name.as_deref() == Some(target))
                    .collect();
                black_box(matches);
            });
        });
    });
}

// ---------------------------------------------------------------------------
// Phase 6 — Signal inbound decode WITH attachments.
//
// Budget: sub-100µs per frame even when the data message carries
// 10 attachments. The decode runs on every WS frame; an attachment-
// heavy group chat (member sharing several photos at once) must
// not spike the consumer's per-message cost.
// ---------------------------------------------------------------------------

fn bench_signal_inbound_decode_with_attachments(c: &mut Criterion) {
    let attachments: Vec<serde_json::Value> = (0..10)
        .map(|i| {
            serde_json::json!({
                "id": format!("att-base64-{i:04x}"),
                "contentType": "image/jpeg",
                "filename": format!("photo_{i}.jpg"),
                "size": 1_500_000,
            })
        })
        .collect();
    let frame = serde_json::json!({
        "account": "+15551234567",
        "envelope": {
            "source": "+15559998888",
            "sourceNumber": "+15559998888",
            "sourceName": "Alice",
            "timestamp": 1700000000000_i64,
            "dataMessage": {
                "timestamp": 1700000000000_i64,
                "message": "check out my vacation pics",
                "groupInfo": null,
                "attachments": attachments,
            }
        }
    })
    .to_string();

    c.bench_function("signal/decode_text_frame_with_10_attachments", |b| {
        b.iter(|| {
            let out = execlaw_server::signal_inbound::decode_frame(
                black_box(&frame),
                Some("+15551234567"),
            );
            black_box(out);
        });
    });
}

// ---------------------------------------------------------------------------
// Phase 7 — Outbound attachment base64 encode.
//
// Budget: sub-5ms per outbound send carrying a single 500KB
// attachment (a typical photo). The encode dominates the in-process
// CPU cost on `signal.send_message` with attachments; the
// HTTP round-trip dominates the wall-clock side.
//
// We bench the pure encode path so a regression — e.g., a future
// refactor that switches base64 engines or adds a copy — surfaces
// without any signal-cli mock noise.
// ---------------------------------------------------------------------------

fn bench_signal_outbound_attachment_encode(c: &mut Criterion) {
    use base64::Engine;
    // 500 KiB of pseudo-random bytes — typical jpeg payload size.
    // Random pattern resists base64-engine-specific shortcuts on
    // long all-zeros runs.
    let bytes: Vec<u8> = (0..512 * 1024)
        .map(|i| ((i * 2654435761_usize) % 256) as u8)
        .collect();
    let engine = base64::engine::general_purpose::STANDARD;
    c.bench_function("signal/outbound_attachment_encode_500kb", |b| {
        b.iter(|| {
            let encoded = engine.encode(black_box(&bytes));
            // Mirror the format!() the transport uses so the
            // bench captures both the encode + the data-URL
            // string concatenation.
            let url = format!("data:image/jpeg;base64,{encoded}");
            black_box(url);
        });
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
    bench_research_parse_ddg_html,
    bench_research_extract_readable_text,
    bench_signal_send_outbound,
    bench_signal_inbound_decode,
    bench_signal_group_name_resolution,
    bench_signal_inbound_decode_with_attachments,
    bench_signal_outbound_attachment_encode,
);
criterion_main!(benches);
