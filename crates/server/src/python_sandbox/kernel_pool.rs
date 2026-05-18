//! Per-conversation kernel pool.
//!
//! Owns the `conversation_id -> kernel_id` map plus the lifecycle
//! policy around it: lazy spawn on first `ensure_for`, soft reset via
//! gateway restart, hard drop on conversation delete, and idle
//! eviction on a periodic tick.
//!
//! The pool is `Clone` (cheap — `Arc` inside) so axum handlers and
//! the background eviction task can hold it concurrently.
//!
//! Concurrency notes:
//!   - One `tokio::sync::Mutex` guards the `HashMap`. Lock is held
//!     only across map mutations, NEVER across HTTP/WS calls to the
//!     gateway. That keeps the map non-poisonable and lets parallel
//!     conversations make progress.
//!   - There is a small race window in `ensure_for` where two
//!     concurrent callers for the same conversation BOTH miss the
//!     cache and both create a kernel server-side. The second
//!     caller deletes the loser; net state: one kernel per
//!     conversation. The window is short (one HTTP create) and the
//!     cleanup is idempotent — the gateway's `DELETE /api/kernels/<id>`
//!     returns 404 if already gone.
//!   - Idle eviction removes the map entry BEFORE calling
//!     `delete_kernel` so a racing `ensure_for` can't return a
//!     kernel id we're about to invalidate.

use crate::python_sandbox::client::{GatewayClient, GatewayError, KernelId};
use execlaw_core::ids::ConversationId;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Default idle eviction window. Matches the gateway's
/// `MappingKernelManager.cull_idle_timeout` (15 min) so both sides
/// agree on what "alive" means and we don't get drift where the
/// gateway culls but the pool still thinks the kernel is fine.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Kernel spec name. The `python-sandbox:fast` image ships only the
/// stock `python3` spec.
const DEFAULT_KERNEL_NAME: &str = "python3";

#[derive(Debug, Clone)]
struct KernelEntry {
    kernel_id: KernelId,
    last_used: Instant,
}

#[derive(Clone)]
pub struct KernelPool {
    inner: Arc<KernelPoolInner>,
}

struct KernelPoolInner {
    client: GatewayClient,
    state: Mutex<HashMap<ConversationId, KernelEntry>>,
    idle_timeout: Duration,
}

impl KernelPool {
    pub fn new(client: GatewayClient) -> Self {
        Self::with_idle_timeout(client, DEFAULT_IDLE_TIMEOUT)
    }

    pub fn with_idle_timeout(client: GatewayClient, idle_timeout: Duration) -> Self {
        Self {
            inner: Arc::new(KernelPoolInner {
                client,
                state: Mutex::new(HashMap::new()),
                idle_timeout,
            }),
        }
    }

    /// Get the conversation's kernel id, spawning one if needed.
    /// Bumps `last_used` so an active conversation never gets
    /// evicted underneath itself.
    pub async fn ensure_for(
        &self,
        convo: &ConversationId,
    ) -> Result<KernelId, GatewayError> {
        // Fast path: cached.
        {
            let mut state = self.inner.state.lock().await;
            if let Some(entry) = state.get_mut(convo) {
                entry.last_used = Instant::now();
                return Ok(entry.kernel_id.clone());
            }
        }
        // Slow path: spawn. Lock is released here so other
        // conversations aren't blocked by our HTTP create_kernel.
        let info = self.inner.client.create_kernel(DEFAULT_KERNEL_NAME).await?;
        let mut state = self.inner.state.lock().await;
        if let Some(existing) = state.get_mut(convo) {
            // A concurrent caller raced us. They win; we delete our
            // newly-minted kernel server-side.
            existing.last_used = Instant::now();
            let winner = existing.kernel_id.clone();
            let loser = info.id;
            drop(state);
            tracing::debug!(
                %convo, %loser, %winner,
                "ensure_for race resolved — deleting redundant kernel"
            );
            let _ = self.inner.client.delete_kernel(&loser).await;
            return Ok(winner);
        }
        let kid = info.id.clone();
        state.insert(
            convo.clone(),
            KernelEntry {
                kernel_id: kid.clone(),
                last_used: Instant::now(),
            },
        );
        drop(state);
        tracing::info!(%convo, %kid, "python_sandbox spawned kernel for conversation");
        Ok(kid)
    }

    /// Look up the conversation's kernel without spawning. Returns
    /// `None` if no kernel has been created (or it was evicted).
    pub async fn current_kernel(&self, convo: &ConversationId) -> Option<KernelId> {
        self.inner
            .state
            .lock()
            .await
            .get(convo)
            .map(|e| e.kernel_id.clone())
    }

    /// Soft reset: restart the kernel subprocess server-side, keeping
    /// the same `kernel_id` so our map entry stays valid. Wires to
    /// `python.reset`. No-op if no kernel exists.
    pub async fn reset(&self, convo: &ConversationId) -> Result<(), GatewayError> {
        let kid = self.current_kernel(convo).await;
        if let Some(kid) = kid {
            self.inner.client.restart_kernel(&kid).await?;
            // Restart implies fresh activity — keep this convo's
            // entry alive against the eviction clock.
            let mut state = self.inner.state.lock().await;
            if let Some(entry) = state.get_mut(convo) {
                entry.last_used = Instant::now();
            }
            tracing::info!(%convo, %kid, "python_sandbox reset kernel");
        }
        Ok(())
    }

    /// Interrupt the kernel's currently-executing cell. Wires to
    /// `python.interrupt` AND to the SPA's stop button while a cell
    /// is running. No-op if no kernel exists or the kernel is idle
    /// (the gateway returns 204 either way).
    pub async fn interrupt(&self, convo: &ConversationId) -> Result<(), GatewayError> {
        let kid = self.current_kernel(convo).await;
        if let Some(kid) = kid {
            self.inner.client.interrupt_kernel(&kid).await?;
        }
        Ok(())
    }

    /// Hard drop — delete the kernel server-side AND remove from the
    /// map. Called when the conversation itself is being archived/
    /// deleted, freeing the slot under the gateway's `max_kernels`
    /// ceiling.
    pub async fn drop_kernel(&self, convo: &ConversationId) -> Result<(), GatewayError> {
        // Remove from map FIRST so any racing ensure_for doesn't
        // return a kernel id we're about to invalidate.
        let kid = self
            .inner
            .state
            .lock()
            .await
            .remove(convo)
            .map(|e| e.kernel_id);
        if let Some(kid) = kid {
            tracing::info!(%convo, %kid, "python_sandbox dropping kernel for conversation");
            self.inner.client.delete_kernel(&kid).await?;
        }
        Ok(())
    }

    /// One pass of idle eviction. Returns count evicted. Designed to
    /// be invoked from a periodic background worker; see
    /// [`KernelPool::spawn_eviction_loop`].
    pub async fn evict_idle(&self) -> usize {
        let cutoff = Instant::now()
            .checked_sub(self.inner.idle_timeout)
            .unwrap_or(Instant::now());
        let stale: Vec<(ConversationId, KernelId)> = {
            let state = self.inner.state.lock().await;
            state
                .iter()
                .filter(|(_, e)| e.last_used < cutoff)
                .map(|(c, e)| (c.clone(), e.kernel_id.clone()))
                .collect()
        };
        let count = stale.len();
        for (convo, kid) in stale {
            // Remove from map first (see drop_kernel rationale).
            self.inner.state.lock().await.remove(&convo);
            tracing::info!(%convo, %kid, "python_sandbox evicting idle kernel");
            if let Err(e) = self.inner.client.delete_kernel(&kid).await {
                tracing::warn!(?e, %kid, "delete during eviction failed (continuing)");
            }
        }
        count
    }

    /// Spawn the background eviction worker. Returns a handle the
    /// caller can `.abort()` on shutdown. Loop interval is half the
    /// idle timeout so a kernel that's exactly at the deadline gets
    /// culled within `idle_timeout * 1.5` of its last use.
    pub fn spawn_eviction_loop(&self) -> tokio::task::JoinHandle<()> {
        let pool = self.clone();
        let interval = pool.inner.idle_timeout / 2;
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            // Skip the immediate tick so we don't evict on startup.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let n = pool.evict_idle().await;
                if n > 0 {
                    tracing::debug!(evicted = n, "python_sandbox idle eviction pass");
                }
            }
        })
    }

    pub fn idle_timeout(&self) -> Duration {
        self.inner.idle_timeout
    }

    pub fn client(&self) -> &GatewayClient {
        &self.inner.client
    }

    /// How many conversations currently have a kernel registered.
    /// Test/observability helper.
    pub async fn len(&self) -> usize {
        self.inner.state.lock().await.len()
    }
}

// ===================================================================
// Tests
// ===================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn convo(id: &str) -> ConversationId {
        ConversationId::from(id.to_string())
    }

    /// Live integration test — gated on `EXECLAW_TEST_GATEWAY_URL`.
    /// Exercises the full pool surface against a real gateway:
    /// ensure_for caches, reset preserves the id, interrupt is
    /// idempotent on idle, drop_kernel removes the entry, evict_idle
    /// culls stale entries.
    #[tokio::test]
    async fn live_pool_lifecycle() {
        let Ok(url) = std::env::var("EXECLAW_TEST_GATEWAY_URL") else {
            eprintln!(
                "skipping: set EXECLAW_TEST_GATEWAY_URL=http://127.0.0.1:18888 \
                 with the image running to exercise this"
            );
            return;
        };
        let client = GatewayClient::new(url).expect("client builds");
        // Short idle for the eviction subtest. 500 ms gives enough
        // headroom that a sibling live test creating kernels
        // concurrently doesn't push us past the cutoff before
        // we've finished setup. Earlier 50 ms was tight enough that
        // the parallel `live_execute_*` tests' load made the
        // ensure_for call here cross the deadline mid-flight.
        let pool = KernelPool::with_idle_timeout(client, Duration::from_millis(500));

        let a = convo("convo-a");
        let b = convo("convo-b");

        // ensure_for caches: same kernel id across calls.
        let k1 = pool.ensure_for(&a).await.expect("spawn for a");
        let k2 = pool.ensure_for(&a).await.expect("cached for a");
        assert_eq!(k1, k2, "ensure_for must cache per conversation");
        assert_eq!(pool.len().await, 1);

        // Different conversation => different kernel id.
        let kb = pool.ensure_for(&b).await.expect("spawn for b");
        assert_ne!(k1, kb, "different convos must get different kernel ids");
        assert_eq!(pool.len().await, 2);

        // current_kernel returns the cached id without spawning.
        assert_eq!(pool.current_kernel(&a).await.as_ref(), Some(&k1));

        // reset() keeps the kernel_id stable (restart preserves id).
        pool.reset(&a).await.expect("reset a");
        assert_eq!(
            pool.current_kernel(&a).await.as_ref(),
            Some(&k1),
            "reset must NOT change the kernel id"
        );

        // interrupt() is a no-op against an idle kernel — must not error.
        pool.interrupt(&a).await.expect("interrupt idle");

        // drop_kernel removes from map AND deletes server-side.
        pool.drop_kernel(&a).await.expect("drop a");
        assert!(pool.current_kernel(&a).await.is_none());
        assert_eq!(pool.len().await, 1);

        // evict_idle: b's last_used is fresh (500 ms timeout, we
        // just did the operations); but if we sleep past the
        // timeout it becomes stale.
        tokio::time::sleep(Duration::from_millis(700)).await;
        let n = pool.evict_idle().await;
        assert_eq!(n, 1, "evict_idle must cull b after sleep");
        assert_eq!(pool.len().await, 0);

        // Idempotent eviction — calling again on empty map is a no-op.
        assert_eq!(pool.evict_idle().await, 0);
    }
}
