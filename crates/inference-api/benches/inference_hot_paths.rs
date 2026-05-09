//! Microbenchmarks for inference-api hot paths (§0 axiom #14).
//!
//! Streaming decode is on the per-token path for every streamed chat
//! response — budget ≤ 5µs per chunk-decode, else it starts eating
//! into the voice-LLM latency budget.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use execlaw_inference_api::ChatStreamChunk;

const CHUNK_WITH_CONTENT: &str = r#"{
    "id": "chatcmpl-bench",
    "model": "QuantTrio/Qwen3.5-27B-AWQ",
    "choices": [
        {"index": 0, "delta": {"content": "Hello there"}}
    ]
}"#;

const CHUNK_WITH_TOOL_CALL: &str = r#"{
    "id": "chatcmpl-bench",
    "model": "QuantTrio/Qwen3.5-27B-AWQ",
    "choices": [
        {
            "index": 0,
            "delta": {
                "tool_calls": [
                    {"index": 0, "id": "tc_1", "type": "function",
                     "function": {"name": "read_memory", "arguments": "{\"key\":\"x\"}"}}
                ]
            }
        }
    ]
}"#;

fn bench_chunk_decode(c: &mut Criterion) {
    let mut group = c.benchmark_group("stream_chunk_decode");
    group.bench_function("content_delta", |b| {
        b.iter(|| serde_json::from_str::<ChatStreamChunk>(black_box(CHUNK_WITH_CONTENT)).unwrap())
    });
    group.bench_function("tool_call_delta", |b| {
        b.iter(|| serde_json::from_str::<ChatStreamChunk>(black_box(CHUNK_WITH_TOOL_CALL)).unwrap())
    });
    group.finish();
}

criterion_group!(benches, bench_chunk_decode);
criterion_main!(benches);
