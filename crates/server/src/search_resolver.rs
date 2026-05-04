//! Dispatch-time resolver for the active web-search provider.
//!
//! Reads the `config_search_providers` table at every call site
//! that needs a `WebSearchApi`, constructs the right adapter from
//! the row's per-kind config_json, and returns it boxed. Falling
//! back to DDG with empty config when no provider is active or
//! the active row's config is malformed — better to attempt a
//! search than to return None and break the caller.
//!
//! Why per-call rather than cached: provider config can change
//! mid-process (operator updates the SearxNG URL, swaps to Brave,
//! etc.) and a cached provider would serve stale config until the
//! next restart. The lookup is a single SQL query against an
//! indexed table — micro-optimisation of zero practical value.

use crate::tool_apis_search::DuckDuckGoSearchApi;
use crate::tool_apis_search_brave::BraveSearchApi;
use crate::tool_apis_search_exa::ExaSearchApi;
use crate::tool_apis_search_searxng::SearxNGSearchApi;
use crate::tool_apis_search_tavily::TavilySearchApi;
use execlaw_core::Database;
use execlaw_core::search_providers::{
    SearchProviderKind, SearchProviderRow, SearchProviderStore,
};
use execlaw_core::tool::WebSearchApi;
use serde_json::Value;
use std::sync::Arc;

/// Construct the active provider for `db`. Always returns
/// something so the caller can dispatch unconditionally; on any
/// failure path it falls back to DuckDuckGo (the seed default,
/// always works without config).
pub fn resolve_active_provider(db: &Database) -> Arc<dyn WebSearchApi> {
    let row = match SearchProviderStore::new(db).active() {
        Ok(Some(r)) => r,
        Ok(None) => {
            tracing::warn!(
                "search_resolver: no active provider in config_search_providers; \
                 falling back to DuckDuckGo"
            );
            return Arc::new(DuckDuckGoSearchApi::new());
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "search_resolver: store lookup failed; falling back to DuckDuckGo"
            );
            return Arc::new(DuckDuckGoSearchApi::new());
        }
    };
    construct_from_row(&row)
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

    #[test]
    fn resolves_to_searxng_when_promoted_with_base_url() {
        let db = fresh_db();
        let store = SearchProviderStore::new(&db);
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
    fn resolves_to_brave_when_promoted_with_api_key() {
        let db = fresh_db();
        let store = SearchProviderStore::new(&db);
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
    fn resolves_to_exa_when_promoted_with_api_key() {
        let db = fresh_db();
        let store = SearchProviderStore::new(&db);
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
    fn resolves_to_tavily_when_promoted_with_api_key() {
        let db = fresh_db();
        let store = SearchProviderStore::new(&db);
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
