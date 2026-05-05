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

#[derive(Debug, Clone)]
pub struct InferenceResolver {
    /// Boot-time fallback. Constructed from
    /// `state.config.inference_base_url` when set; `None` when the
    /// operator hasn't configured a global URL and managed
    /// backends are the only source.
    pub bootstrap: Option<Arc<InferenceClient>>,
}

impl InferenceResolver {
    pub fn new(bootstrap: Option<Arc<InferenceClient>>) -> Self {
        Self { bootstrap }
    }

    /// Pick the `InferenceClient` for the next turn on `purpose`.
    ///
    /// Decision tree:
    ///   * Row has `endpoint` set (any mode) → fresh client for that URL.
    ///   * Row exists but `endpoint` is `None`, `mode = managed` →
    ///     return `None`. The operator declared a managed backend
    ///     that hasn't come up yet; falling back to the bootstrap
    ///     would silently route to a different URL than the
    ///     operator chose. Better to surface "no inference" via
    ///     the stub-turn fallback so the operator notices the
    ///     supervisor hasn't written the endpoint yet.
    ///   * Row absent OR row.mode = external + no endpoint →
    ///     bootstrap fallback. External-mode rows that the operator
    ///     never finished configuring fall through to the global
    ///     URL, which is the pre-Phase-12 behaviour.
    pub fn resolve(&self, db: &Database, purpose: BackendPurpose) -> Option<Arc<InferenceClient>> {
        let store = BackendStore::new(db);
        let row = store.get(purpose).ok().flatten();
        match row {
            Some(r) => match r.endpoint {
                Some(url) if !url.trim().is_empty() => Some(Arc::new(InferenceClient::new(url))),
                _ => {
                    // No endpoint. Managed-mode rows surface as None
                    // so the operator sees "supervisor hasn't come
                    // up" rather than a silent bootstrap fallback.
                    if r.mode == BackendMode::Managed {
                        None
                    } else {
                        self.bootstrap.clone()
                    }
                }
            },
            None => self.bootstrap.clone(),
        }
    }
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
        store
            .upsert(
                &BackendUpsert {
                    purpose,
                    inference_backend: "service-vllm".into(),
                    model_spec_json: serde_json::json!({}),
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
        assert_eq!(got.base_url, "http://boot:8000/v1");
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
        assert_eq!(got.base_url, "http://192.168.1.50:8000/v1");
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
        assert_eq!(got.base_url, "http://boot:8000/v1");
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
    fn managed_row_with_endpoint_returns_supervisor_url() {
        // Steady-state happy path: supervisor wrote
        // http://127.0.0.1:8101/v1 back, resolver picks it up.
        // The `/v1` suffix is part of the supervisor → store
        // contract — without it the inference-api client appends
        // `/chat/completions` to a bare base URL and vLLM 404s
        // because the OpenAI routes are mounted under /v1.
        let db = fresh_db();
        let store = BackendStore::new(&db);
        upsert_row(
            &store,
            BackendPurpose::Standard,
            BackendMode::Managed,
            Some("http://127.0.0.1:8101/v1"),
        );
        let resolver = InferenceResolver::new(None);
        let got = resolver.resolve(&db, BackendPurpose::Standard).unwrap();
        assert_eq!(got.base_url, "http://127.0.0.1:8101/v1");
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
        assert_eq!(got.base_url, "http://boot:8000/v1");
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
        assert_eq!(got.base_url, "http://127.0.0.1:8101/v1");
    }
}
