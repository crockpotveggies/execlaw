//! Criterion benches for the model adapter.
//!
//! Budgets locked 2026-05-03 (axiom #14):
//!
//! | Path                                  | Budget (median) | Rationale                                  |
//! |---------------------------------------|-----------------|--------------------------------------------|
//! | family_detect                         |   < 1 µs        | Runs once per LLM call (string scan).      |
//! | qwen3_prepare_request                 |   < 5 µs        | Runs every call; mostly Option<Value> set. |
//! | qwen3_process_response_clean          |   < 50 µs       | Most responses; one regex search miss.     |
//! | qwen3_process_response_with_think     |   < 100 µs      | One regex hit + slice.                     |
//! | qwen3_process_response_with_preamble  |   < 50 µs       | String search + slice.                     |
//! | gemma_merge_system_into_user          |   < 10 µs       | Per call when family is Gemma.             |

use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};
use execlaw_inference_api::{ChatMessage, ChatRequest, ChatResponse, Choice, ModelId, Role};
use execlaw_model_adapter::{
    adapter::{AdaptedResponse, ModelAdapter, OutputHint},
    families::{GemmaAdapter, Qwen3Adapter},
    family::ModelFamily,
};

fn req() -> ChatRequest {
    ChatRequest {
        model: ModelId("test".into()),
        messages: vec![ChatMessage::system("sys"), ChatMessage::user("hi")],
        tools: None,
        stream: false,
        temperature: None,
        max_tokens: None,
        chat_template_kwargs: None,
    }
}

fn resp(text: &str) -> ChatResponse {
    ChatResponse {
        id: "1".into(),
        model: "test".into(),
        choices: vec![Choice {
            index: 0,
            message: ChatMessage {
                role: Role::Assistant,
                content: Some(text.into()),
                tool_call_id: None,
                name: None,
                tool_calls: vec![],
            },
            finish_reason: Some("stop".into()),
        }],
        usage: None,
    }
}

fn bench_family_detect(c: &mut Criterion) {
    let inputs = [
        "QuantTrio/Qwen3.5-27B-AWQ",
        "deepseek-ai/DeepSeek-R1",
        "meta-llama/Llama-3.3-70B-Instruct",
        "mistralai/Mixtral-8x22B-Instruct-v0.1",
        "google/gemma-2-9b-it",
        "some-vendor/Mystery-Model",
    ];
    c.bench_function("family_detect_mixed", |b| {
        b.iter(|| {
            for s in &inputs {
                black_box(ModelFamily::detect(black_box(s)));
            }
        })
    });
}

fn bench_qwen3_prepare(c: &mut Criterion) {
    let adapter = Qwen3Adapter;
    c.bench_function("qwen3_prepare_request", |b| {
        b.iter(|| {
            let r = adapter.prepare_request(black_box(req()), OutputHint::StructuredJson);
            black_box(r);
        })
    });
}

fn bench_qwen3_process(c: &mut Criterion) {
    let adapter = Qwen3Adapter;
    let mut group = c.benchmark_group("qwen3_process_response");

    let clean = resp(
        "{\"thesis\": \"a clean structured reply with no preamble\", \"steps\": [{\"query\": \"x\"}]}",
    );
    let with_think = resp(
        "<think>I should think about this carefully and weigh the options. Step 1: identify the goal. Step 2: outline the approach. Step 3: write the JSON.</think>{\"thesis\":\"t\",\"steps\":[{\"query\":\"x\"}]}",
    );
    let with_preamble = resp(
        "Thinking Process:\n1. Analyze the request.\n2. Format as JSON.\n\n{\"thesis\":\"t\",\"steps\":[{\"query\":\"x\"}]}",
    );

    group.throughput(Throughput::Bytes(
        clean.choices[0].message.content.as_ref().unwrap().len() as u64,
    ));
    group.bench_function("clean", |b| {
        b.iter(|| {
            let r: AdaptedResponse =
                adapter.process_response(black_box(clean.clone()), OutputHint::StructuredJson);
            black_box(r);
        })
    });
    group.bench_function("with_think", |b| {
        b.iter(|| {
            let r: AdaptedResponse =
                adapter.process_response(black_box(with_think.clone()), OutputHint::StructuredJson);
            black_box(r);
        })
    });
    group.bench_function("with_preamble", |b| {
        b.iter(|| {
            let r: AdaptedResponse = adapter
                .process_response(black_box(with_preamble.clone()), OutputHint::StructuredJson);
            black_box(r);
        })
    });
    group.finish();
}

fn bench_gemma_merge(c: &mut Criterion) {
    let adapter = GemmaAdapter;
    c.bench_function("gemma_merge_system_into_user", |b| {
        b.iter(|| {
            let r = adapter.prepare_request(black_box(req()), OutputHint::Conversation);
            black_box(r);
        })
    });
}

criterion_group!(
    benches,
    bench_family_detect,
    bench_qwen3_prepare,
    bench_qwen3_process,
    bench_gemma_merge
);
criterion_main!(benches);
