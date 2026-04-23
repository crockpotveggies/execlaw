# execlaw-inference-api

The single internal contract for LLM access: an OpenAI-compatible `/v1/chat/completions`
client. No cloud-vendor SDKs in any form. Endpoints always resolve to local
inference servers (vLLM, OpenArc, llama.cpp server, Ollama) or operator-opted-in
inference-bridge plugins running locally.

Default model: `QuantTrio/Qwen3.5-27B-AWQ` (2026-04-23 locked decision).
