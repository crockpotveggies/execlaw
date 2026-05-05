//! Channel-keyed registry for host-side `TransportApi`
//! implementations.
//!
//! Why this exists: prior to 2026-05-05 (rev `e42ce4b`), the
//! "bridge a message back through the originating transport"
//! call sites in `chats.rs`, `attachment_api.rs`, and
//! `research/runner.rs` each:
//!
//!   * matched on the literal `SIGNAL_CHANNEL` string,
//!   * constructed `SignalCliTransport` directly, and
//!   * imported `crate::signal_transport` from a module that has
//!     no business knowing about Signal specifically.
//!
//! Adding a second transport (WhatsApp, Telegram, email-bridged-
//! agent, ...) would have meant editing every one of those sites —
//! a Liskov-shaped leak the operator's "are we violating
//! encapsulation" question correctly flagged.
//!
//! The registry inverts the dependency: each transport plugin
//! ships a [`HostTransportFactory`] at boot, the registry stores
//! them keyed on `channel`, and the bridge call sites just walk
//! the conversation's bindings and ask the registry "got a
//! transport for `binding.channel`?". No channel-string match
//! outside the per-channel modules.
//!
//! ### Cardinality
//!
//! Today exactly one factory is registered (`signal-cli`). Adding
//! a second is one [`HostTransportRegistry::register`] call at
//! boot — no edits to the generic bridge sites.
//!
//! ### What stays per-channel
//!
//! The host-side **implementation** of each transport (sidecar
//! protocol, attachment encoding, WS frame parsing) lives in its
//! own module — for Signal that's `signal_transport.rs`,
//! `signal_inbound.rs`, `signal_admin.rs`. The plugin manifest
//! declares the tools as `host_implemented = true` because Rhai
//! can't reach `Capability::Transport`. That layering is correct
//! and unchanged. What this module fixes is the *call sites*: they
//! no longer name Signal.

use async_trait::async_trait;
use execlaw_core::Database;
use execlaw_core::ids::ConversationId;
use execlaw_core::tool::TransportApi;
use execlaw_core::transport_bindings::TransportBinding;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Per-channel constructor. Each transport plugin ships one of
/// these at boot. Captures whatever long-lived dependencies the
/// transport needs (signal-cli's RPC resolver, a future SMTP
/// client config, a Telegram bot-token registry) so the per-call
/// `build` is just a dependency-injection step on the binding's
/// hot data.
#[async_trait]
pub trait HostTransportFactory: Send + Sync {
    /// Channel name this factory handles. Same string the binding
    /// store uses (`signal`, `email`, `whatsapp`, ...). Lookup is
    /// case-sensitive — the upstream binding-insert path
    /// normalises before write, so we don't here.
    fn channel(&self) -> &str;

    /// Build a [`TransportApi`] scoped to one conversation +
    /// recipient. Failures are rare (resolver refused, transport
    /// disabled mid-call) — the caller treats `None` as "no
    /// transport available right now," skips the bridge, and
    /// continues. The caller already committed any web-side
    /// surfaces (card pair / model_turn) so a missed bridge
    /// degrades gracefully rather than failing the user-visible
    /// flow.
    fn build(
        &self,
        db: &Database,
        conversation_id: &ConversationId,
        foreign_id: &str,
    ) -> Option<Arc<dyn TransportApi>>;
}

/// The registry. Cheap to clone (every entry is `Arc`). Plant on
/// `AppState` once at boot; bridge sites read through it on every
/// dispatch.
#[derive(Clone, Default)]
pub struct HostTransportRegistry {
    by_channel: BTreeMap<String, Arc<dyn HostTransportFactory>>,
}

impl HostTransportRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a factory. Idempotent on the same channel name —
    /// later registrations overwrite earlier ones (intentional:
    /// lets a test fixture replace the Signal factory with a mock
    /// without restructuring the boot path).
    pub fn register(&mut self, factory: Arc<dyn HostTransportFactory>) {
        self.by_channel.insert(factory.channel().to_owned(), factory);
    }

    /// Walk `bindings` in order and return a transport for the
    /// first binding whose channel has a registered factory. The
    /// per-binding `foreign_id` flows into [`HostTransportFactory::build`]
    /// so transports that key on recipient (signal-cli's
    /// per-message addressing) get it without the bridge site
    /// having to thread it.
    ///
    /// Returns `(transport, foreign_id)` so the caller can dispatch
    /// straight to `transport.send(channel, foreign_id, text)`.
    /// `None` when none of the conversation's bindings have a
    /// registered transport (web-only conversation, transport
    /// plugin not installed).
    pub fn build_for_first_supported_binding(
        &self,
        db: &Database,
        conversation_id: &ConversationId,
        bindings: &[TransportBinding],
    ) -> Option<(Arc<dyn TransportApi>, String, String)> {
        for binding in bindings {
            let factory = match self.by_channel.get(&binding.channel) {
                Some(f) => f,
                None => continue,
            };
            if let Some(transport) =
                factory.build(db, conversation_id, &binding.foreign_id)
            {
                return Some((transport, binding.channel.clone(), binding.foreign_id.clone()));
            }
        }
        None
    }

    /// Number of registered channels. Used by tests + an optional
    /// boot-log line so the operator can see "1 transport registered:
    /// signal" without grepping.
    pub fn len(&self) -> usize {
        self.by_channel.len()
    }

    /// True when no factories are registered. Equivalent to
    /// `len() == 0`.
    pub fn is_empty(&self) -> bool {
        self.by_channel.is_empty()
    }

    /// Iterator over registered channel names. Useful for boot
    /// logging + the `/api/admin/me/transports` endpoint that
    /// drives the SPA's "My identities" dropdown.
    pub fn channels(&self) -> impl Iterator<Item = &str> {
        self.by_channel.keys().map(|s| s.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_core::tool::ApiError;
    use execlaw_core::transport_bindings::TransportBinding;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Mock factory that records every `build` call so a test can
    /// pin "the registry actually called us with the right channel
    /// + recipient."
    struct CountingFactory {
        channel: String,
        builds: Arc<AtomicU32>,
        last_recipient: Arc<std::sync::Mutex<Option<String>>>,
    }

    impl CountingFactory {
        fn new(channel: &str) -> (Self, Arc<AtomicU32>, Arc<std::sync::Mutex<Option<String>>>) {
            let builds = Arc::new(AtomicU32::new(0));
            let last = Arc::new(std::sync::Mutex::new(None));
            (
                Self {
                    channel: channel.to_owned(),
                    builds: builds.clone(),
                    last_recipient: last.clone(),
                },
                builds,
                last,
            )
        }
    }

    /// Stub transport that errors for every call. The registry
    /// tests don't dispatch — they just verify the build path —
    /// so a non-functional `TransportApi` is fine here.
    struct StubTransport;

    #[async_trait]
    impl TransportApi for StubTransport {
        async fn resolve_recipient(
            &self,
            _channel: &str,
            _free_form: &str,
        ) -> Result<String, ApiError> {
            Err(ApiError::Validation("stub".into()))
        }
        async fn send(
            &self,
            _channel: &str,
            _recipient: &str,
            _text: &str,
        ) -> Result<String, ApiError> {
            Err(ApiError::Validation("stub".into()))
        }
        async fn current_chat_id(&self, _channel: &str) -> Result<Option<String>, ApiError> {
            Ok(None)
        }
    }

    #[async_trait]
    impl HostTransportFactory for CountingFactory {
        fn channel(&self) -> &str {
            &self.channel
        }
        fn build(
            &self,
            _db: &Database,
            _cid: &ConversationId,
            foreign_id: &str,
        ) -> Option<Arc<dyn TransportApi>> {
            self.builds.fetch_add(1, Ordering::Relaxed);
            *self.last_recipient.lock().unwrap() = Some(foreign_id.to_owned());
            Some(Arc::new(StubTransport))
        }
    }

    fn fresh_db() -> Database {
        let db = execlaw_core::Database::open(&execlaw_core::DbConfig::in_memory_unencrypted())
            .unwrap();
        execlaw_core::MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    fn b(channel: &str, foreign: &str) -> TransportBinding {
        TransportBinding {
            channel: channel.to_owned(),
            foreign_id: foreign.to_owned(),
            principal_group_id: "pg-x".to_owned(),
            is_group: false,
            created_at: 0,
            last_seen_at: None,
        }
    }

    #[test]
    fn empty_registry_returns_none() {
        let r = HostTransportRegistry::new();
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
        let db = fresh_db();
        let cid = ConversationId::from("c-1");
        let got = r.build_for_first_supported_binding(&db, &cid, &[b("signal", "+15551234567")]);
        assert!(got.is_none());
    }

    #[test]
    fn registry_dispatches_to_matching_channel() {
        let mut r = HostTransportRegistry::new();
        let (sf, sb, sl) = CountingFactory::new("signal");
        let (ef, eb, _el) = CountingFactory::new("email");
        r.register(Arc::new(sf));
        r.register(Arc::new(ef));
        assert_eq!(r.len(), 2);
        let db = fresh_db();
        let cid = ConversationId::from("c-1");
        let got = r.build_for_first_supported_binding(
            &db,
            &cid,
            &[b("signal", "+15551234567")],
        );
        assert!(got.is_some());
        let (_t, ch, fid) = got.unwrap();
        assert_eq!(ch, "signal");
        assert_eq!(fid, "+15551234567");
        assert_eq!(sb.load(Ordering::Relaxed), 1, "signal factory must have been called");
        assert_eq!(eb.load(Ordering::Relaxed), 0, "email factory must NOT have been called");
        assert_eq!(sl.lock().unwrap().as_deref(), Some("+15551234567"));
    }

    #[test]
    fn registry_skips_unregistered_channels_and_finds_first_match() {
        // Walk order: skip channels with no factory, return the
        // first that has one. Simulates a future "this conversation
        // is bridged on whatsapp + signal; whatsapp plugin not
        // installed, so signal wins" case.
        let mut r = HostTransportRegistry::new();
        let (sf, sb, _) = CountingFactory::new("signal");
        r.register(Arc::new(sf));
        let db = fresh_db();
        let cid = ConversationId::from("c-1");
        let got = r.build_for_first_supported_binding(
            &db,
            &cid,
            &[b("whatsapp", "+15550000001"), b("signal", "+15550000002")],
        );
        assert!(got.is_some());
        let (_t, ch, fid) = got.unwrap();
        assert_eq!(ch, "signal");
        assert_eq!(fid, "+15550000002");
        assert_eq!(sb.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn channels_iterator_lists_every_registered_channel() {
        let mut r = HostTransportRegistry::new();
        let (sf, _, _) = CountingFactory::new("signal");
        let (ef, _, _) = CountingFactory::new("email");
        r.register(Arc::new(sf));
        r.register(Arc::new(ef));
        let mut names: Vec<&str> = r.channels().collect();
        names.sort();
        assert_eq!(names, vec!["email", "signal"]);
    }

    #[test]
    fn re_registering_same_channel_overwrites() {
        let mut r = HostTransportRegistry::new();
        let (a, ab, _) = CountingFactory::new("signal");
        let (b_factory, bb, _) = CountingFactory::new("signal");
        r.register(Arc::new(a));
        r.register(Arc::new(b_factory));
        assert_eq!(r.len(), 1, "second register must REPLACE, not stack");
        let db = fresh_db();
        let cid = ConversationId::from("c-1");
        r.build_for_first_supported_binding(&db, &cid, &[b("signal", "+1")]);
        assert_eq!(ab.load(Ordering::Relaxed), 0);
        assert_eq!(bb.load(Ordering::Relaxed), 1);
    }
}
