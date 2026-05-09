//! Model family identification.
//!
//! Mapped from the operator-configured `model_id` string (e.g.
//! `"QuantTrio/Qwen3.5-27B-AWQ"`, `"deepseek-ai/DeepSeek-R1"`,
//! `"meta-llama/Llama-3.3-70B-Instruct"`). The detection is
//! conservative — unknown ids fall through to
//! [`ModelFamily::OpenAiGeneric`] which applies only the safest
//! transformations (strip code fences, no kwargs, passthrough
//! everything else).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ModelFamily {
    /// Qwen3 / Qwen3.5. `enable_thinking` kwarg honored; `<think>`
    /// blocks frequent unless suppressed; "Thinking Process:"
    /// preamble is a fallback shape some templates emit.
    Qwen3,
    /// DeepSeek R1 (reasoning-tuned). Always emits reasoning;
    /// vLLM also surfaces a separate `reasoning_content` field on
    /// some configs. Strip `<think>` defensively.
    DeepSeekR1,
    /// DeepSeek V3 / V2.5 (non-reasoning chat). Generally clean
    /// OpenAI-shaped; occasional code fences.
    DeepSeekV3,
    /// Meta Llama 3.x / 4. Doesn't honor `enable_thinking`; tool
    /// calls sometimes emit inline `<|python_tag|>` markers (v2
    /// gap; for now we only normalize the text body).
    Llama3,
    /// Mistral / Mixtral. Generally clean; fence stripping covers
    /// older versions. Recent versions speak OpenAI tool-call
    /// format natively.
    Mistral,
    /// Google Gemma 2/3. Rejects the `system` role; we merge any
    /// system message into the first user message.
    Gemma,
    /// Catch-all for unknown OpenAI-compatible backends. Safest
    /// transformations only: strip ```` ``` ```` fences,
    /// passthrough kwargs.
    OpenAiGeneric,
}

impl ModelFamily {
    /// Detect the family from a model_id string. Case-insensitive
    /// substring match against well-known tokens. Conservative —
    /// returns `OpenAiGeneric` when nothing matches.
    pub fn detect(model_id: &str) -> Self {
        let s = model_id.to_ascii_lowercase();
        // Order matters: check more-specific finetunes BEFORE the
        // base architecture. `DeepSeek-R1-Distill-Qwen-7B` is a
        // DeepSeek finetune (emits `<think>` like R1) even though
        // its base is Qwen — so deepseek/llama/mistral wins over
        // qwen when both substrings are present.
        if s.contains("deepseek") && (s.contains("r1") || s.contains("reason")) {
            Self::DeepSeekR1
        } else if s.contains("deepseek") {
            Self::DeepSeekV3
        } else if s.contains("qwen") {
            Self::Qwen3
        } else if s.contains("llama") {
            Self::Llama3
        } else if s.contains("mistral") || s.contains("mixtral") || s.contains("magistral") {
            Self::Mistral
        } else if s.contains("gemma") {
            Self::Gemma
        } else {
            Self::OpenAiGeneric
        }
    }

    /// Stable identifier for logs and telemetry.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Qwen3 => "qwen3",
            Self::DeepSeekR1 => "deepseek-r1",
            Self::DeepSeekV3 => "deepseek-v3",
            Self::Llama3 => "llama3",
            Self::Mistral => "mistral",
            Self::Gemma => "gemma",
            Self::OpenAiGeneric => "openai-generic",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_qwen_family_from_repo_paths() {
        assert_eq!(
            ModelFamily::detect("QuantTrio/Qwen3.5-27B-AWQ"),
            ModelFamily::Qwen3
        );
        assert_eq!(ModelFamily::detect("Qwen/Qwen3-32B"), ModelFamily::Qwen3);
        assert_eq!(ModelFamily::detect("qwen2.5-7b"), ModelFamily::Qwen3);
    }

    #[test]
    fn detects_deepseek_r1_vs_v3() {
        assert_eq!(
            ModelFamily::detect("deepseek-ai/DeepSeek-R1-Distill-Qwen-7B"),
            ModelFamily::DeepSeekR1
        );
        assert_eq!(
            ModelFamily::detect("deepseek-ai/deepseek-v3-0324"),
            ModelFamily::DeepSeekV3
        );
        // Reasoning-tuned variants regardless of "R1" string.
        assert_eq!(
            ModelFamily::detect("DeepSeek-Reasoner-Lite"),
            ModelFamily::DeepSeekR1
        );
    }

    #[test]
    fn detects_llama_mistral_gemma() {
        assert_eq!(
            ModelFamily::detect("meta-llama/Llama-3.3-70B-Instruct"),
            ModelFamily::Llama3
        );
        assert_eq!(
            ModelFamily::detect("mistralai/Mixtral-8x22B-Instruct-v0.1"),
            ModelFamily::Mistral
        );
        assert_eq!(
            ModelFamily::detect("mistralai/Magistral-Small-2509"),
            ModelFamily::Mistral
        );
        assert_eq!(
            ModelFamily::detect("google/gemma-2-9b-it"),
            ModelFamily::Gemma
        );
    }

    #[test]
    fn unknown_model_falls_through_to_generic() {
        assert_eq!(
            ModelFamily::detect("some-vendor/Mystery-13B"),
            ModelFamily::OpenAiGeneric
        );
        assert_eq!(ModelFamily::detect(""), ModelFamily::OpenAiGeneric);
    }

    #[test]
    fn case_insensitive_detection() {
        assert_eq!(ModelFamily::detect("LLAMA-3-70B"), ModelFamily::Llama3);
        assert_eq!(ModelFamily::detect("QWEN3"), ModelFamily::Qwen3);
    }
}
