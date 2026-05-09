//! Dispatch-time resolver for the active web-search provider(s).
//!
//! Reads the `config_search_providers` table at every call site
//! that needs a `WebSearchApi`, constructs the right adapter from
//! each enabled row's per-kind config_json, and returns either
//!
//!   * the lone enabled adapter (when only one is enabled), OR
//!   * a [`crate::search_rotating::RotatingWebSearchApi`] wrapper
//!     covering every enabled provider (when 2+ are enabled).
//!
//! The rotating wrapper round-robins per call, parks providers
//! on rate-limit / quota errors for 60s, and enforces a global
//! 250ms pacing gate across all web_search dispatches. State
//! (cursor, cooldowns) lives in a process-global so resolver
//! constructions can stay per-call without losing rotation context.
//!
//! Falls back to DuckDuckGo (always available, no config) when
//! the store lookup fails or no row is enabled — better to attempt
//! a search than break the caller.
//!
//! Why per-call rather than cached: provider config can change
//! mid-process (operator updates the SearxNG URL, swaps to Brave,
//! etc.) and a cached resolver would serve stale config until the
//! next restart.

use crate::search_rotating::RotatingWebSearchApi;
use crate::tool_apis_search::DuckDuckGoSearchApi;
use crate::tool_apis_search_brave::BraveSearchApi;
use crate::tool_apis_search_exa::ExaSearchApi;
use crate::tool_apis_search_perplexity::PerplexitySearchApi;
use crate::tool_apis_search_searchapi::SearchApiSearchApi;
use crate::tool_apis_search_searxng::SearxNGSearchApi;
use crate::tool_apis_search_serpapi::SerpApiSearchApi;
use crate::tool_apis_search_tavily::TavilySearchApi;
use crate::tool_apis_search_websurfx::WebsurfxSearchApi;
use execlaw_core::Database;
use execlaw_core::search_providers::{SearchProviderKind, SearchProviderRow, SearchProviderStore};
use execlaw_core::tool::WebSearchApi;
use serde_json::Value;
use std::sync::Arc;

/// Construct the active web-search dispatcher for `db`. Returns
/// the rotating wrapper when 2+ providers are enabled; a single
/// adapter otherwise. Always returns something so the caller can
/// dispatch unconditionally — on a store-lookup failure it falls
/// back to DuckDuckGo (the seed default, no config required).
pub fn resolve_active_provider(db: &Database) -> Arc<dyn WebSearchApi> {
    let store = SearchProviderStore::new(db);
    let rows = match store.list_all() {
        Ok(rs) => rs,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "search_resolver: store list_all failed; falling back to DuckDuckGo"
            );
            return Arc::new(DuckDuckGoSearchApi::new());
        }
    };

    // Sort: is_default first, then alphabetical for stability so
    // the operator's "default" pick is the rotation's seed
    // position. Rotation cursor cycles from there.
    let mut enabled: Vec<SearchProviderRow> = rows.into_iter().filter(|r| r.enabled).collect();
    enabled.sort_by(|a, b| {
        b.is_default
            .cmp(&a.is_default) // default first (true > false)
            .then_with(|| a.kind.as_str().cmp(b.kind.as_str()))
    });

    if enabled.is_empty() {
        tracing::warn!(
            "search_resolver: no enabled providers in config_search_providers; \
             falling back to DuckDuckGo"
        );
        return Arc::new(DuckDuckGoSearchApi::new());
    }

    if enabled.len() == 1 {
        // Single-provider passthrough — no rotation overhead +
        // skips the global pacing gate (the per-adapter gate is
        // sufficient for one provider).
        return construct_from_row(&enabled[0]);
    }

    let providers: Vec<(SearchProviderKind, Arc<dyn WebSearchApi>)> = enabled
        .iter()
        .map(|r| (r.kind, construct_from_row(r)))
        .collect();
    tracing::debug!(
        target: "search_resolver",
        providers = ?providers.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
        "rotating web_search across enabled providers",
    );
    Arc::new(RotatingWebSearchApi::new(providers))
}

/// Pure builder: row → boxed adapter. Public for tests + future
/// admin flows that want to construct an adapter from a row
/// without going through the store (e.g. the test-search endpoint).
pub fn construct_from_row(row: &SearchProviderRow) -> Arc<dyn WebSearchApi> {
    let cfg: Value = serde_json::from_str(&row.config_json).unwrap_or(Value::Null);
    match row.kind {
        SearchProviderKind::DuckDuckGo => Arc::new(DuckDuckGoSearchApi::new()),
        SearchProviderKind::SearxNG => {
            let base_url = cfg
                .get("base_url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            Arc::new(SearxNGSearchApi::new(base_url))
        }
        SearchProviderKind::Brave => {
            let api_key = cfg
                .get("api_key")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            Arc::new(BraveSearchApi::new(api_key))
        }
        SearchProviderKind::Exa => {
            let api_key = cfg
                .get("api_key")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            Arc::new(ExaSearchApi::new(api_key))
        }
        SearchProviderKind::Tavily => {
            let api_key = cfg
                .get("api_key")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            Arc::new(TavilySearchApi::new(api_key))
        }
        SearchProviderKind::SerpApi => {
            let api_key = cfg
                .get("api_key")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            Arc::new(SerpApiSearchApi::new(api_key))
        }
        SearchProviderKind::Perplexity => {
            let api_key = cfg
                .get("api_key")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            Arc::new(PerplexitySearchApi::new(api_key))
        }
        SearchProviderKind::SearchApi => {
            let api_key = cfg
                .get("api_key")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            Arc::new(SearchApiSearchApi::new(api_key))
        }
        SearchProviderKind::Websurfx => {
            let base_url = cfg
                .get("base_url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            Arc::new(WebsurfxSearchApi::new(base_url))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_core::DbConfig;
    use execlaw_core::MigrationRunner;

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    #[test]
    fn resolves_to_duckduckgo_on_fresh_db_seed() {
        let db = fresh_db();
        let provider = resolve_active_provider(&db);
        assert_eq!(provider.provider_id(), "duckduckgo");
    }

    /// Disable the seed DDG row so a single-other-provider test
    /// gets the single-passthrough path (vs the rotating wrapper).
    fn disable_ddg(store: &SearchProviderStore) {
        let mut row = store.get(SearchProviderKind::DuckDuckGo).unwrap().unwrap();
        row.enabled = false;
        row.is_default = false;
        store.upsert(&row).unwrap();
    }

    #[test]
    fn single_enabled_provider_returns_passthrough_searxng() {
        let db = fresh_db();
        let store = SearchProviderStore::new(&db);
        disable_ddg(&store);
        store
            .upsert(&SearchProviderRow {
                kind: SearchProviderKind::SearxNG,
                enabled: true,
                is_default: true,
                config_json: r#"{"base_url":"https://searx.example.com"}"#.into(),
                created_at: 100,
                updated_at: 100,
            })
            .unwrap();
        let provider = resolve_active_provider(&db);
        assert_eq!(provider.provider_id(), "searxng");
    }

    #[test]
    fn single_enabled_provider_returns_passthrough_brave() {
        let db = fresh_db();
        let store = SearchProviderStore::new(&db);
        disable_ddg(&store);
        store
            .upsert(&SearchProviderRow {
                kind: SearchProviderKind::Brave,
                enabled: true,
                is_default: true,
                config_json: r#"{"api_key":"sk-test"}"#.into(),
                created_at: 100,
                updated_at: 100,
            })
            .unwrap();
        let provider = resolve_active_provider(&db);
        assert_eq!(provider.provider_id(), "brave");
    }

    #[test]
    fn single_enabled_provider_returns_passthrough_exa() {
        let db = fresh_db();
        let store = SearchProviderStore::new(&db);
        disable_ddg(&store);
        store
            .upsert(&SearchProviderRow {
                kind: SearchProviderKind::Exa,
                enabled: true,
                is_default: true,
                config_json: r#"{"api_key":"exa-test"}"#.into(),
                created_at: 100,
                updated_at: 100,
            })
            .unwrap();
        let provider = resolve_active_provider(&db);
        assert_eq!(provider.provider_id(), "exa");
    }

    #[test]
    fn single_enabled_provider_returns_passthrough_tavily() {
        let db = fresh_db();
        let store = SearchProviderStore::new(&db);
        disable_ddg(&store);
        store
            .upsert(&SearchProviderRow {
                kind: SearchProviderKind::Tavily,
                enabled: true,
                is_default: true,
                config_json: r#"{"api_key":"tvly-test"}"#.into(),
                created_at: 100,
                updated_at: 100,
            })
            .unwrap();
        let provider = resolve_active_provider(&db);
        assert_eq!(provider.provider_id(), "tavily");
    }

    #[test]
    fn two_enabled_providers_return_rotating_wrapper() {
        let db = fresh_db();
        let store = SearchProviderStore::new(&db);
        // DDG remains enabled (the seed default) — add Brave as a
        // second enabled provider. resolve should wrap them.
        store
            .upsert(&SearchProviderRow {
                kind: SearchProviderKind::Brave,
                enabled: true,
                is_default: false,
                config_json: r#"{"api_key":"sk-test"}"#.into(),
                created_at: 100,
                updated_at: 100,
            })
            .unwrap();
        let provider = resolve_active_provider(&db);
        assert_eq!(
            provider.provider_id(),
            "rotating",
            "2+ enabled providers must return the rotating wrapper",
        );
    }

    #[test]
    fn disabled_providers_excluded_from_rotation() {
        // Brave is enabled but disabled — only DDG remains enabled,
        // so resolve takes the single-passthrough path.
        let db = fresh_db();
        let store = SearchProviderStore::new(&db);
        store
            .upsert(&SearchProviderRow {
                kind: SearchProviderKind::Brave,
                enabled: false,
                is_default: false,
                config_json: r#"{"api_key":"sk-test"}"#.into(),
                created_at: 100,
                updated_at: 100,
            })
            .unwrap();
        let provider = resolve_active_provider(&db);
        assert_eq!(provider.provider_id(), "duckduckgo");
    }

    #[test]
    fn construct_handles_missing_config_field_without_panicking() {
        // Operator hand-edited the DB and left config_json="{}"
        // on a SearxNG row. The constructor must build an adapter
        // (with empty base_url) — the adapter itself surfaces a
        // friendly Validation error on the search call. Better
        // than panicking at construct time.
        let row = SearchProviderRow {
            kind: SearchProviderKind::SearxNG,
            enabled: true,
            is_default: true,
            config_json: "{}".into(),
            created_at: 0,
            updated_at: 0,
        };
        let provider = construct_from_row(&row);
        assert_eq!(provider.provider_id(), "searxng");
    }

    #[test]
    fn construct_handles_invalid_json_by_falling_back_to_empty_config() {
        let row = SearchProviderRow {
            kind: SearchProviderKind::Brave,
            enabled: true,
            is_default: true,
            config_json: "not-valid-json".into(),
            created_at: 0,
            updated_at: 0,
        };
        let provider = construct_from_row(&row);
        // The constructor doesn't panic; the adapter will surface
        // an empty-key validation error on the actual search call.
        assert_eq!(provider.provider_id(), "brave");
    }
}
