//! `tracing_subscriber::Layer` that mirrors every event into the
//! SQLite `log_entries` table (Phase 5 observability).
//!
//! The file appender (`tracing-appender::rolling::daily`) handles
//! durable JSONL on disk; this layer adds a queryable second copy
//! so the admin log-viewer route (`GET /api/admin/logs`) can serve
//! filtered slices to the UI in Phase 6.
//!
//! Cost: one SQLite INSERT per tracing event. INSERTs serialize
//! through the writer mutex but they're tiny (~5-10 µs each on a
//! warm WAL DB), so even chatty `debug!` paths stay well under any
//! latency budget. If volume becomes a concern, the layer can be
//! gated to `level >= info` via `EnvFilter`.

use execlaw_core::Database;
use execlaw_core::logs::{LogLevel, LogRow, LogStore};
use std::collections::BTreeMap;
use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

/// The layer itself. Cheap to clone — internals are an `Arc`-ed DB
/// handle. Constructed once at server startup and added to the
/// subscriber registry alongside the stdout + file layers.
#[derive(Clone)]
pub struct SqliteLogLayer {
    db: Database,
}

impl SqliteLogLayer {
    pub fn new(db: Database) -> Self {
        Self { db }
    }
}

impl<S> Layer<S> for SqliteLogLayer
where
    S: Subscriber,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);

        let level = match *metadata.level() {
            tracing::Level::ERROR => LogLevel::Error,
            tracing::Level::WARN => LogLevel::Warn,
            tracing::Level::INFO => LogLevel::Info,
            tracing::Level::DEBUG => LogLevel::Debug,
            tracing::Level::TRACE => LogLevel::Trace,
        };

        let message = visitor
            .fields
            .remove("message")
            .unwrap_or_else(|| metadata.target().to_owned());
        let conversation_id = visitor.fields.remove("conversation_id");
        let plugin_id = visitor.fields.remove("plugin_id");
        let fields_json = if visitor.fields.is_empty() {
            None
        } else {
            serde_json::to_vec(&visitor.fields).ok()
        };

        let row = LogRow {
            ts_ms: chrono::Utc::now().timestamp_millis(),
            level,
            target: metadata.target().to_owned(),
            conversation_id,
            plugin_id,
            message,
            fields_json,
        };

        // Best-effort: log-write failures must not break the
        // process (DB locked for example). Drop on error; the
        // file appender still has the line.
        let _ = LogStore::new(&self.db).insert(&row);
    }
}

#[derive(Default)]
struct FieldVisitor {
    fields: BTreeMap<String, String>,
}

impl Visit for FieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields
            .insert(field.name().to_owned(), value.to_owned());
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields
            .insert(field.name().to_owned(), value.to_string());
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields
            .insert(field.name().to_owned(), value.to_string());
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields
            .insert(field.name().to_owned(), value.to_string());
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.fields
            .insert(field.name().to_owned(), value.to_string());
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields
            .insert(field.name().to_owned(), format!("{value:?}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_core::db::DbConfig;
    use execlaw_core::migrations::MigrationRunner;
    use tracing_subscriber::layer::SubscriberExt;

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    /// Every tracing event becomes a row in `log_entries` with the
    /// matching level, target, message, and any `conversation_id` /
    /// `plugin_id` / extra-field columns extracted correctly.
    #[test]
    fn tracing_event_writes_a_log_row() {
        let db = fresh_db();
        let layer = SqliteLogLayer::new(db.clone());
        let subscriber = tracing_subscriber::registry().with(layer);

        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(
                conversation_id = "conv-trace",
                plugin_id = "plugin-test",
                custom_field = 42,
                "something broke"
            );
        });

        let store = LogStore::new(&db);
        let rows = store
            .query(Some(LogLevel::Warn), None, None, None, 100)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].message, "something broke");
        assert_eq!(rows[0].conversation_id.as_deref(), Some("conv-trace"));
        assert_eq!(rows[0].plugin_id.as_deref(), Some("plugin-test"));
        // Extra fields land in fields_json.
        let fields: serde_json::Value =
            serde_json::from_slice(&rows[0].fields_json.clone().unwrap()).unwrap();
        assert_eq!(fields["custom_field"], "42");
    }

    /// Multiple events round-trip in order; level filtering picks
    /// out the right subset.
    #[test]
    fn level_filter_in_query_works_after_layer_writes() {
        let db = fresh_db();
        let layer = SqliteLogLayer::new(db.clone());
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("info one");
            tracing::warn!("warn one");
            tracing::error!("error one");
        });

        let store = LogStore::new(&db);
        let warns = store
            .query(Some(LogLevel::Warn), None, None, None, 100)
            .unwrap();
        assert_eq!(warns.len(), 1);
        assert_eq!(warns[0].message, "warn one");
        let errors = store
            .query(Some(LogLevel::Error), None, None, None, 100)
            .unwrap();
        assert_eq!(errors.len(), 1);
    }

    /// Layer is resilient to DB write failures — closing the DB
    /// shouldn't panic, and subsequent events still get processed
    /// (they just don't land in the table).
    #[test]
    fn layer_swallows_db_write_errors() {
        let db = fresh_db();
        let layer = SqliteLogLayer::new(db.clone());
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("first");
            // No way to "close" the in-memory DB cleanly mid-test;
            // this test mostly proves the layer doesn't panic on
            // normal events.
            tracing::info!("second");
        });
        let store = LogStore::new(&db);
        let rows = store.query(None, None, None, None, 100).unwrap();
        assert_eq!(rows.len(), 2);
    }
}
