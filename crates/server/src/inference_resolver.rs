//! Inference-client resolution for the runner (Phase 12.E).
//!
//! Pre-Phase-12, `state.inference` was a single `InferenceClient`
//! built from `EXECLAW_INFERENCE_URL` at process start — operators
//! who wanted to swap backends had to restart execlaw. With managed
//! backends spawning their own containers + writing endpoints back
//! into `config_backends` (Phase 12.C), we need the runner to pick
//! up those URLs on every turn instead of the boot-time URL.
//!
//! `InferenceResolver` is the indirection that closes that loop.
//! Per turn, the runner calls `resolve(purpose)`:
//!
//!   1. Read the `config_backends` row for `purpose`.
//!   2. If it has a non-empty `endpoint`, build a fresh
//!      `InferenceClient` for that URL.
//!   3. If the row has no endpoint OR no row exists, fall through
//!      to the boot-time `bootstrap` client. (External rows that
//!      operators left unconfigured still work via the bootstrap.)
//!   4. If neither path produces a URL, return `None` and the
//!      caller falls back to the stub turn.
//!
//! No caching: `InferenceClient` is a thin wrapper around a base
//! URL + a `reqwest::Client`. Construction is sub-millisecond, and
//! avoiding a cache means the resolver is naturally hot-reloadable
//! — a Backends save updates the row, the *next* turn picks up the
//! new URL, no lock contention or invalidation dance.
//!
//! Scope of v1: every caller asks for `BackendPurpose::Standard`.
//! Per-purpose routing for Small / Voice* lands when the runner
//! grows modality-aware backend selection.

use execlaw_core::Database;
use execlaw_core::backends::{BackendMode, BackendPurpose, BackendStore};
use execlaw_inference_api::InferenceClient;
use std::sync::Arc;

/// Fallback `model` id when the operator's backend row has no
/// `--model=…` arg AND there's no bootstrap-specified model. The
/// chat-completions request MUST send some non-empty `model`
/// string; this is a "best guess" placeholder. The right answer
/// is for the operator to set the model via Settings → Backends.
///
/// Pre-2026-05-13 this default was hardcoded into
/// `ServerConfig::default()` and read on every turn from
/// `state.config.model_id`. That created a second source of truth
/// that drifted from `config_backends.model_spec_json.args` — the
/// supervisor would spawn vLLM with model X, the chat path would
/// send model Y, vLLM 404'd. Single source of truth now: this
/// constant is ONLY reached if the operator's DB row is in an
/// unusable state.
pub const DEFAULT_FALLBACK_MODEL: &str = "QuantTrio/Qwen3.6-27B-AWQ";

/// Resolved inference target for one turn.
///
/// All four fields come from the same DB row read, so they
/// can't drift. Always carries a non-empty `model_id` (falls back
/// to [`DEFAULT_FALLBACK_MODEL`] when the row has no `--model=…`).
#[derive(Debug, Clone)]
pub struct ResolvedInference {
    pub client: Arc<InferenceClient>,
    pub model_id: String,
    pub endpoint: String,
    /// Whether the operator toggled "enable thinking" / reasoning
    /// on this backend row. Pre-2026-05-13 every chat call site did
    /// its OWN `BackendStore::get(...).ok().flatten().map(|r|
    /// r.reasoning_enabled).unwrap_or(false)` — a redundant second
    /// DB read that silently masked errors (`.ok()`) AND opened a
    /// drift window where the resolver got the row from `t0` and
    /// reasoning was read at `t1` after a config save. Now carried
    /// on the resolved struct so it's bound to the same row as the
    /// endpoint + model id.
    pub reasoning_enabled: bool,
    /// `"db"` when the resolution came from a backend row;
    /// `"bootstrap"` when it came from the boot-time URL.
    /// Surfaced for the turn-timing trace so the operator can
    /// confirm which path won.
    pub source: &'static str,
}

#[derive(Debug, Clone)]
pub struct InferenceResolver {
    /// Boot-time fallback. Constructed from `--inference-url`
    /// when set; `None` when the operator hasn't configured a
    /// global URL and managed backends are the only source.
    pub bootstrap: Option<Arc<InferenceClient>>,
    /// Optional model id paired with the bootstrap client. When
    /// the bootstrap fires, we use this string instead of
    /// `DEFAULT_FALLBACK_MODEL` so an operator who set
    /// `--inference-url --inference-model` gets exactly what they
    /// asked for.
    pub bootstrap_model: Option<String>,
}

impl InferenceResolver {
    pub fn new(bootstrap: Option<Arc<InferenceClient>>) -> Self {
        Self {
            bootstrap,
            bootstrap_model: None,
        }
    }

    pub fn with_bootstrap_model(mut self, model: Option<String>) -> Self {
        self.bootstrap_model = model;
        self
    }

    /// Pick the `(InferenceClient, model_id)` pair for the next
    /// turn. Bound together intentionally — pre-2026-05-13 these
    /// came from separate sources (`config_backends.endpoint` vs
    /// `state.config.model_id`) and drifted out of sync the moment
    /// an operator switched models without restarting; the
    /// chat-completions request would 404 with
    /// `The model X does not exist` because vLLM was loaded with
    /// model Y. One source of truth, one read, both fields atomic.
    ///
    /// Decision tree:
    ///   * Row has `endpoint` AND `--model=…` arg → use both.
    ///   * Row has `endpoint` but no model arg → use endpoint +
    ///     bootstrap_model (or `DEFAULT_FALLBACK_MODEL`).
    ///   * Row has no endpoint, `mode = managed` → `None` (the
    ///     supervisor hasn't come up yet; we don't silently route
    ///     to bootstrap because that's a different URL than the
    ///     operator chose).
    ///   * Row absent OR external+no-endpoint → bootstrap fallback.
    ///
    /// **Loud failure semantics**: a DB read error is logged at
    /// WARN to the `inference_resolver` target with the underlying
    /// `DbError` formatted in. Pre-2026-05-13 we silently
    /// swallowed via `.ok().flatten()`, which masked the
    /// "BLOB column got TEXT-overwritten by raw SQL" failure mode
    /// for hours of operator-time. Errors now surface
    /// unambiguously even if the return value is still `None`.
    pub fn resolve(&self, db: &Database, purpose: BackendPurpose) -> Option<ResolvedInference> {
        let store = BackendStore::new(db);
        let row = match store.get(purpose) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    target: "inference_resolver",
                    purpose = ?purpose,
                    error = %e,
                    "config_backends read failed — falling through to bootstrap. \
                     Likely causes: BLOB column corrupted by a raw SQL UPDATE \
                     (use Settings → Backends instead), schema migration miss, \
                     or DB lock contention."
                );
                return self.bootstrap_resolved();
            }
        };
        match row {
            Some(r) => {
                let endpoint = r
                    .endpoint
                    .clone()
                    .filter(|s| !s.trim().is_empty());
                let row_model = extract_model_arg(&r.model_spec_json);
                let reasoning_enabled = r.reasoning_enabled;
                match endpoint {
                    Some(url) => Some(ResolvedInference {
                        client: Arc::new(InferenceClient::new(url.clone())),
                        model_id: row_model
                            .or_else(|| self.bootstrap_model.clone())
                            .unwrap_or_else(|| DEFAULT_FALLBACK_MODEL.to_owned()),
                        endpoint: url,
                        reasoning_enabled,
                        source: "db",
                    }),
                    None => {
                        if r.mode == BackendMode::Managed {
                            tracing::debug!(
                                target: "inference_resolver",
                                purpose = ?purpose,
                                "managed backend row has no endpoint yet — supervisor not ready; \
                                 NOT falling through to bootstrap so the operator sees the stub"
                            );
                            None
                        } else {
                            self.bootstrap_resolved()
                        }
                    }
                }
            }
            None => self.bootstrap_resolved(),
        }
    }

    fn bootstrap_resolved(&self) -> Option<ResolvedInference> {
        let client = self.bootstrap.clone()?;
        Some(ResolvedInference {
            endpoint: client.base_url.clone(),
            client,
            model_id: self
                .bootstrap_model
                .clone()
                .unwrap_or_else(|| DEFAULT_FALLBACK_MODEL.to_owned()),
            // Bootstrap has no row to read this from; default OFF.
            // Operators who want reasoning on must configure the
            // Standard backend row via Settings → Backends.
            reasoning_enabled: false,
            source: "bootstrap",
        })
    }
}

/// Pull the `--model=…` argument out of a backend row's
/// `model_spec_json`. Tolerates both `["--model=X"]` and
/// `["--model", "X"]` shapes (different operator workflows write
/// them differently). Returns `None` when no model arg is present
/// so the caller can fall back to the bootstrap or the default.
fn extract_model_arg(spec: &serde_json::Value) -> Option<String> {
    let args = spec.get("args")?.as_array()?;
    // First pass: `--model=X` form.
    for a in args {
        if let Some(s) = a.as_str() {
            if let Some(rest) = s.strip_prefix("--model=") {
                let trimmed = rest.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_owned());
                }
            }
        }
    }
    // Second pass: `--model` followed by a value.
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        if a.as_str() == Some("--model") {
            if let Some(v) = iter.next().and_then(|v| v.as_str()) {
                let trimmed = v.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_owned());
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_core::MigrationRunner;
    use execlaw_core::backends::{BackendStore, BackendUpsert};
    use execlaw_core::db::DbConfig;

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    fn upsert_row(
        store: &BackendStore<'_>,
        purpose: BackendPurpose,
        mode: BackendMode,
        endpoint: Option<&str>,
    ) {
        upsert_row_with_spec(store, purpose, mode, endpoint, serde_json::json!({}));
    }

    fn upsert_row_with_spec(
        store: &BackendStore<'_>,
        purpose: BackendPurpose,
        mode: BackendMode,
        endpoint: Option<&str>,
        model_spec_json: serde_json::Value,
    ) {
        store
            .upsert(
                &BackendUpsert {
                    purpose,
                    inference_backend: "service-vllm".into(),
                    model_spec_json,
                    gpu_id: None,
                    endpoint: endpoint.map(String::from),
                    notes: None,
                    reasoning_enabled: false,
                    mode,
                },
                100,
            )
            .unwrap();
    }

    #[test]
    fn no_row_no_bootstrap_returns_none() {
        let db = fresh_db();
        let resolver = InferenceResolver::new(None);
        assert!(resolver.resolve(&db, BackendPurpose::Standard).is_none());
    }

    #[test]
    fn no_row_with_bootstrap_returns_bootstrap() {
        let db = fresh_db();
        let bootstrap = Arc::new(InferenceClient::new("http://boot:8000/v1"));
        let resolver = InferenceResolver::new(Some(bootstrap.clone()));
        let got = resolver.resolve(&db, BackendPurpose::Standard).unwrap();
        assert_eq!(got.endpoint, "http://boot:8000/v1");
        assert_eq!(got.source, "bootstrap");
        // No DB row, no bootstrap_model → default fallback.
        assert_eq!(got.model_id, DEFAULT_FALLBACK_MODEL);
    }

    #[test]
    fn external_row_with_endpoint_returns_row_url() {
        let db = fresh_db();
        let store = BackendStore::new(&db);
        upsert_row(
            &store,
            BackendPurpose::Standard,
            BackendMode::External,
            Some("http://192.168.1.50:8000/v1"),
        );
        let bootstrap = Arc::new(InferenceClient::new("http://boot:8000/v1"));
        let resolver = InferenceResolver::new(Some(bootstrap));
        let got = resolver.resolve(&db, BackendPurpose::Standard).unwrap();
        assert_eq!(got.endpoint, "http://192.168.1.50:8000/v1");
        assert_eq!(got.source, "db");
    }

    #[test]
    fn external_row_without_endpoint_falls_through_to_bootstrap() {
        // External rows that the operator typed an inference_backend
        // for but never set a URL on still work — the bootstrap
        // catches them. Pre-Phase-12 behaviour.
        let db = fresh_db();
        let store = BackendStore::new(&db);
        upsert_row(
            &store,
            BackendPurpose::Standard,
            BackendMode::External,
            None,
        );
        let bootstrap = Arc::new(InferenceClient::new("http://boot:8000/v1"));
        let resolver = InferenceResolver::new(Some(bootstrap));
        let got = resolver.resolve(&db, BackendPurpose::Standard).unwrap();
        assert_eq!(got.endpoint, "http://boot:8000/v1");
        assert_eq!(got.source, "bootstrap");
    }

    #[test]
    fn managed_row_without_endpoint_returns_none_even_with_bootstrap() {
        // Managed rows whose supervisor hasn't come up yet must NOT
        // silently fall through to a different URL — the operator
        // explicitly chose managed mode. Surfacing None drops to
        // the stub turn so the issue is visible.
        let db = fresh_db();
        let store = BackendStore::new(&db);
        upsert_row(&store, BackendPurpose::Standard, BackendMode::Managed, None);
        let bootstrap = Arc::new(InferenceClient::new("http://boot:8000/v1"));
        let resolver = InferenceResolver::new(Some(bootstrap));
        assert!(resolver.resolve(&db, BackendPurpose::Standard).is_none());
    }

    #[test]
    fn managed_row_with_endpoint_and_model_arg_returns_both_atomically() {
        // Steady-state happy path AND the regression case from the
        // 2026-05-13 hang: supervisor wrote http://127.0.0.1:8101/v1
        // back AND `--model=QuantTrio/Qwen3.6-27B-AWQ` is in the
        // args. Pre-rework these came from separate state
        // (config_backends.endpoint + state.config.model_id) and
        // drifted out of sync; the chat path sent model=Qwen3.5
        // while vLLM was loaded with Qwen3.6 and 404'd. Now they
        // come from the same row read.
        let db = fresh_db();
        let store = BackendStore::new(&db);
        upsert_row_with_spec(
            &store,
            BackendPurpose::Standard,
            BackendMode::Managed,
            Some("http://127.0.0.1:8101/v1"),
            serde_json::json!({
                "image": "vllm/vllm-openai:v0.20.2",
                "args": ["--model=QuantTrio/Qwen3.6-27B-AWQ"],
            }),
        );
        let resolver = InferenceResolver::new(None);
        let got = resolver.resolve(&db, BackendPurpose::Standard).unwrap();
        assert_eq!(got.endpoint, "http://127.0.0.1:8101/v1");
        assert_eq!(got.model_id, "QuantTrio/Qwen3.6-27B-AWQ");
        assert_eq!(got.source, "db");
    }

    #[test]
    fn extract_model_arg_handles_both_arg_shapes() {
        // Operators / wizard / supervisor write args in two forms:
        //   ["--model=X"]            (single token)
        //   ["--model", "X"]         (separate tokens)
        // Both must resolve to the same model id.
        assert_eq!(
            super::extract_model_arg(&serde_json::json!({
                "args": ["--model=Qwen3.6-27B"]
            })),
            Some("Qwen3.6-27B".to_owned())
        );
        assert_eq!(
            super::extract_model_arg(&serde_json::json!({
                "args": ["--model", "Qwen3.6-27B"]
            })),
            Some("Qwen3.6-27B".to_owned())
        );
        // Missing model arg → None (caller falls back).
        assert_eq!(
            super::extract_model_arg(&serde_json::json!({
                "args": ["--enable-prefix-caching"]
            })),
            None
        );
        // Empty args / no args → None.
        assert_eq!(
            super::extract_model_arg(&serde_json::json!({ "args": [] })),
            None
        );
        assert_eq!(
            super::extract_model_arg(&serde_json::json!({})),
            None
        );
        // `--model=` with empty value → None, not Some("").
        assert_eq!(
            super::extract_model_arg(&serde_json::json!({
                "args": ["--model="]
            })),
            None
        );
    }

    #[test]
    fn reasoning_enabled_round_trips_from_row() {
        // Pre-2026-05-13 four separate chat sites each re-read this
        // bool via `.ok().flatten().map(|r| r.reasoning_enabled)`,
        // opening a drift window (resolver row at t0, reasoning row
        // at t1) AND silently masking BLOB-decode errors. The field
        // now rides on `ResolvedInference` from the same row read.
        let db = fresh_db();
        let store = BackendStore::new(&db);
        store
            .upsert(
                &BackendUpsert {
                    purpose: BackendPurpose::Standard,
                    inference_backend: "service-vllm".into(),
                    model_spec_json: serde_json::json!({ "args": ["--model=Q"] }),
                    gpu_id: None,
                    endpoint: Some("http://127.0.0.1:8101/v1".into()),
                    notes: None,
                    reasoning_enabled: true,
                    mode: BackendMode::External,
                },
                100,
            )
            .unwrap();
        let resolver = InferenceResolver::new(None);
        let got = resolver.resolve(&db, BackendPurpose::Standard).unwrap();
        assert!(got.reasoning_enabled, "row reasoning_enabled = true must reach the resolved struct");

        // And toggle off: same flow, false comes through.
        store
            .upsert(
                &BackendUpsert {
                    purpose: BackendPurpose::Standard,
                    inference_backend: "service-vllm".into(),
                    model_spec_json: serde_json::json!({ "args": ["--model=Q"] }),
                    gpu_id: None,
                    endpoint: Some("http://127.0.0.1:8101/v1".into()),
                    notes: None,
                    reasoning_enabled: false,
                    mode: BackendMode::External,
                },
                101,
            )
            .unwrap();
        let got = resolver.resolve(&db, BackendPurpose::Standard).unwrap();
        assert!(!got.reasoning_enabled, "row reasoning_enabled = false must reach the resolved struct");
    }

    #[test]
    fn bootstrap_resolution_defaults_reasoning_off() {
        // The bootstrap path has no row to read; reasoning defaults
        // OFF. Operators who want it on must configure the Standard
        // backend row.
        let db = fresh_db();
        let bootstrap = Arc::new(InferenceClient::new("http://boot:8000/v1"));
        let resolver = InferenceResolver::new(Some(bootstrap));
        let got = resolver.resolve(&db, BackendPurpose::Standard).unwrap();
        assert_eq!(got.source, "bootstrap");
        assert!(!got.reasoning_enabled);
    }

    #[test]
    fn empty_string_endpoint_treated_as_unset() {
        // Some upsert paths can leave endpoint = "" instead of
        // null. Treat both equivalently so a stale empty string
        // doesn't synthesize an InferenceClient with a useless URL.
        let db = fresh_db();
        let store = BackendStore::new(&db);
        upsert_row(
            &store,
            BackendPurpose::Standard,
            BackendMode::External,
            Some(""),
        );
        let bootstrap = Arc::new(InferenceClient::new("http://boot:8000/v1"));
        let resolver = InferenceResolver::new(Some(bootstrap));
        let got = resolver.resolve(&db, BackendPurpose::Standard).unwrap();
        assert_eq!(got.endpoint, "http://boot:8000/v1");
        assert_eq!(got.source, "bootstrap");
    }

    #[test]
    fn resolve_picks_up_supervisor_endpoint_after_set_endpoint_writeback() {
        // The whole point of Phase 12.E: a fresh row with no
        // endpoint resolves to None (managed) → supervisor calls
        // BackendStore::set_endpoint → next resolve picks up the
        // new URL without any cache invalidation.
        let db = fresh_db();
        let store = BackendStore::new(&db);
        upsert_row(&store, BackendPurpose::Standard, BackendMode::Managed, None);
        let resolver = InferenceResolver::new(None);
        assert!(resolver.resolve(&db, BackendPurpose::Standard).is_none());

        store
            .set_endpoint(
                BackendPurpose::Standard,
                Some("http://127.0.0.1:8101/v1"),
                200,
            )
            .unwrap();
        let got = resolver.resolve(&db, BackendPurpose::Standard).unwrap();
        assert_eq!(got.endpoint, "http://127.0.0.1:8101/v1");
        assert_eq!(got.source, "db");
    }

    #[test]
    fn bootstrap_model_override_wins_when_row_has_no_model_arg() {
        // Operator who passes --inference-url AND --inference-model
        // at boot wants those values used even when no DB row exists.
        let db = fresh_db();
        let bootstrap = Arc::new(InferenceClient::new("http://boot:8000/v1"));
        let resolver = InferenceResolver::new(Some(bootstrap))
            .with_bootstrap_model(Some("operator/Custom-7B".to_owned()));
        let got = resolver.resolve(&db, BackendPurpose::Standard).unwrap();
        assert_eq!(got.model_id, "operator/Custom-7B");
        assert_eq!(got.source, "bootstrap");
    }
}
