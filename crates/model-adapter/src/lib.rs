//! Per-model-family request preparation + response normalization.
//!
//! Sits between every `ChatRequest` build site and the inference
//! client so a model swap doesn't require code changes at every
//! call site. Today's families:
//!
//!   * Qwen3 / Qwen3.5 — `enable_thinking` kwarg, `<think>` strip,
//!     "Thinking Process:" preamble strip
//!   * DeepSeek R1 — `<think>` strip (kwarg not honored)
//!   * DeepSeek V3 — fence strip
//!   * Llama 3 / 4 — fence strip; tool-call `<|python_tag|>` shape
//!     is a documented v2 gap
//!   * Mistral / Mixtral / Magistral — fence strip
//!   * Gemma 2/3 — system → user merge (Gemma rejects system role)
//!   * OpenAiGeneric — defensive fallback (strip fences + `<think>`
//!     if present, otherwise passthrough)
//!
//! ## Usage
//!
//! ```ignore
//! use execlaw_model_adapter::{adapter_for, ModelFamily, OutputHint};
//!
//! let adapter = adapter_for(ModelFamily::detect(&model_id));
//! let adapted = adapter
//!     .chat(&inference_client, request, OutputHint::StructuredJson)
//!     .await?;
//! let json_text = adapted.content;
//! ```
//!
//! ## What's NOT covered (v1)
//!
//! - **Tool-call format normalization.** Llama's `<|python_tag|>`
//!   inline calls and DeepSeek R1's Python-style calls would not
//!   populate `AdaptedResponse::tool_calls`. The current chat
//!   dispatcher trusts OpenAI-shape `tool_calls[]`. Fixing this is
//!   v2 work — needs a per-family text scanner.
//! - **Per-family stop sequences.** Some models (Gemma, certain
//!   Llama tunes) need explicit stops to avoid runaway generation.
//!   Add to each `prepare_request` as we encounter the issue.
//! - **Streaming response normalization.** The streaming chat path
//!   uses `ThinkBlockFilter` which already handles `<think>` strip
//!   across chunks. The adapter handles non-streaming responses
//!   only.

pub mod adapter;
pub mod extract;
pub mod families;
pub mod family;

pub use adapter::{AdaptedResponse, ModelAdapter, OutputHint};
pub use families::{
    adapter_for, DeepSeekR1Adapter, DeepSeekV3Adapter, GemmaAdapter, Llama3Adapter,
    MistralAdapter, OpenAiGenericAdapter, Qwen3Adapter,
};
pub use family::ModelFamily;
