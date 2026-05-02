//! Per-plugin in-memory response cache.
//!
//! Backs `http_get_cached(url, query, bearer, ttl_secs)`. Keyed by
//! the SHA-256 of (url + canonical query + bearer-hash) so a token
//! rotation invalidates the entry naturally without an explicit
//! invalidate call.
//!
//! Cache is **per-plugin** (each `ScriptPlugin` owns its own
//! [`HttpCache`]). Sized by entry count rather than bytes —
//! plugins doing wholesale Google Contacts list calls cache
//! ~one entry, not thousands.

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const MAX_ENTRIES: usize = 64;

#[derive(Clone)]
struct Entry {
    body: serde_json::Value,
    expires_at: Instant,
}

#[derive(Default)]
pub(crate) struct HttpCache {
    inner: Mutex<HashMap<String, Entry>>,
}

impl HttpCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the cached body if present and not expired.
    pub fn get(&self, key: &str) -> Option<serde_json::Value> {
        let mut g = self.inner.lock().unwrap();
        if let Some(e) = g.get(key) {
            if Instant::now() < e.expires_at {
                return Some(e.body.clone());
            }
            g.remove(key);
        }
        None
    }

    /// Insert; evicts the oldest entry if we'd exceed MAX_ENTRIES.
    /// "Oldest" by expiry — close-to-expiry entries die first so
    /// fresh entries dominate.
    pub fn put(&self, key: String, body: serde_json::Value, ttl: Duration) {
        let mut g = self.inner.lock().unwrap();
        if g.len() >= MAX_ENTRIES {
            // Evict the entry with the soonest expiry.
            if let Some(victim) = g
                .iter()
                .min_by_key(|(_, e)| e.expires_at)
                .map(|(k, _)| k.clone())
            {
                g.remove(&victim);
            }
        }
        g.insert(
            key,
            Entry {
                body,
                expires_at: Instant::now() + ttl,
            },
        );
    }
}

/// Hash (url, query, bearer) into a short opaque key. Using
/// SHA-256 means a bearer rotation produces a fresh key (no
/// stale-data after a refresh).
pub(crate) fn cache_key(url: &str, query_repr: &str, bearer: &str) -> String {
    let mut h = Sha256::new();
    h.update(url.as_bytes());
    h.update(b"\x00");
    h.update(query_repr.as_bytes());
    h.update(b"\x00");
    h.update(bearer.as_bytes());
    let bytes = h.finalize();
    // First 16 bytes are plenty for cache-key uniqueness.
    bytes[..16].iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_then_get_returns_value_within_ttl() {
        let c = HttpCache::new();
        c.put(
            "k".into(),
            serde_json::json!({"hello": "world"}),
            Duration::from_secs(60),
        );
        let v = c.get("k").unwrap();
        assert_eq!(v["hello"], "world");
    }

    #[test]
    fn get_returns_none_after_ttl() {
        let c = HttpCache::new();
        c.put("k".into(), serde_json::json!(1), Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(5));
        assert!(c.get("k").is_none());
    }

    #[test]
    fn cache_key_changes_when_bearer_rotates() {
        let a = cache_key("https://x", "q=1", "tok-A");
        let b = cache_key("https://x", "q=1", "tok-B");
        assert_ne!(a, b);
    }

    #[test]
    fn cache_key_stable_for_same_inputs() {
        let a = cache_key("https://x", "q=1", "tok");
        let b = cache_key("https://x", "q=1", "tok");
        assert_eq!(a, b);
    }

    #[test]
    fn put_at_capacity_evicts_soonest_expiry() {
        let c = HttpCache::new();
        // Fill to capacity with 1-hour TTL.
        for i in 0..MAX_ENTRIES {
            c.put(
                format!("k{i}"),
                serde_json::json!(i),
                Duration::from_secs(3600),
            );
        }
        // Insert one with a SHORTER TTL — its `expires_at` is
        // sooner than every existing entry's. The eviction at
        // insert-time picks one of the equal-TTL entries (which
        // one is non-deterministic — HashMap iteration), then
        // victim lands.
        c.put("victim".into(), serde_json::json!("v"), Duration::from_secs(10));
        // Insert one more — pushes over capacity. The min-expiry
        // is now `victim`, which gets evicted in favour of `fresh`.
        c.put("fresh".into(), serde_json::json!("f"), Duration::from_secs(3600));
        // Contract: victim is gone (it had the soonest expiry when
        // `fresh` arrived). `fresh` is present. The MAX_ENTRIES
        // long-TTL entries minus one (the original eviction at
        // victim-insert time) survive — exact identity is HashMap-
        // iteration-order-dependent so we don't pin which.
        assert!(c.get("victim").is_none());
        assert!(c.get("fresh").is_some());
        // Total survivors of the original batch = MAX_ENTRIES - 1.
        let survivors = (0..MAX_ENTRIES)
            .filter(|i| c.get(&format!("k{i}")).is_some())
            .count();
        assert_eq!(survivors, MAX_ENTRIES - 1);
    }
}
