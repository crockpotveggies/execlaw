//! Channel-keyed registry — channel → (plugin_id, icon) lookup.
//!
//! Phase B simplification: pre-fix this registry returned an
//! `Arc<dyn TransportApi>` per channel through a per-plugin
//! `HostTransportFactory`. The factory's only job was to mint
//! a `RhaiBackedTransport` (480 lines of method-mapping shim)
//! that delegated each `TransportApi` call to a plugin tool name.
//! Auto-bridge sites (text-reply bridge, attachment fan-out,
//! research-PDF dispatch) consumed the trait.
//!
//! Now the registry just answers two questions:
//!
//!   1. What plugin handles channel X? (`lookup_first_supported_binding`)
//!   2. What sidebar icon does channel X want? (`icon_for`)
//!
//! Auto-bridge sites take the answer + call
//! `plugin_host.call_tool("<channel>.send_message", ...)`
//! directly. No adapter layer; the plugin's own tool body owns
//! the wire-format transformation (e.g. signal-cli's
//! `group.<base64>` recipient encoding lives in
//! `plugins/signal/main.rhai::wire_recipient`).
//!
//! ### Cardinality
//!
//! Today exactly one channel is registered (`signal`). Adding a
//! second is one [`HostTransportRegistry::register`] call at boot
//! — no edits to the generic bridge sites.

use execlaw_core::transport_bindings::TransportBinding;
use std::collections::BTreeMap;

/// Per-channel boot-time metadata. Cheap to clone.
#[derive(Debug, Clone)]
pub struct ChannelInfo {
    /// Plugin id that owns this channel — auto-bridge dispatches
    /// `<channel>.send_message` (and friends) into this plugin's
    /// tool surface.
    pub plugin_id: String,
    /// Bootstrap-icons name (sans `bi-` prefix) the SPA renders
    /// next to bridged thread titles in the sidebar. Sourced from
    /// the plugin manifest's `[transport].icon` field at boot.
    pub icon: String,
}

/// One result from [`HostTransportRegistry::lookup_first_supported_binding`].
/// The auto-bridge sites pass these fields straight into a
/// `plugin_host.call_tool` invocation.
#[derive(Debug, Clone)]
pub struct ResolvedBinding {
    pub plugin_id: String,
    pub channel: String,
    /// Foreign id from the binding row (`+15551234567` for DMs,
    /// the raw `internal_id` base64 form for Signal groups).
    /// Auto-bridge passes this verbatim as the tool's `to` arg —
    /// the plugin's tool body handles any wire-format transform
    /// (e.g. `group.<base64>` for Signal groups).
    pub foreign_id: String,
    pub is_group: bool,
}

/// The registry. Cheap to clone. Plant on `AppState` once at boot;
/// auto-bridge call sites read through it on every dispatch.
#[derive(Clone, Default)]
pub struct HostTransportRegistry {
    by_channel: BTreeMap<String, ChannelInfo>,
}

impl HostTransportRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a channel. Idempotent on the same channel name —
    /// later registrations overwrite earlier ones (intentional;
    /// lets a test fixture replace the canonical mapping with a
    /// mock without restructuring the boot path).
    pub fn register(&mut self, channel: impl Into<String>, info: ChannelInfo) {
        self.by_channel.insert(channel.into(), info);
    }

    /// Walk `bindings` in order; return the first whose channel is
    /// registered. Auto-bridge sites use the result to dispatch
    /// `plugin_host.call_tool("<channel>.send_message", {to,
    /// text, ...})`.
    ///
    /// `None` when none of the conversation's bindings have a
    /// registered handler (web-only conversation, or the channel's
    /// plugin isn't installed).
    pub fn lookup_first_supported_binding(
        &self,
        bindings: &[TransportBinding],
    ) -> Option<ResolvedBinding> {
        for binding in bindings {
            if let Some(info) = self.by_channel.get(&binding.channel) {
                return Some(ResolvedBinding {
                    plugin_id: info.plugin_id.clone(),
                    channel: binding.channel.clone(),
                    foreign_id: binding.foreign_id.clone(),
                    is_group: binding.is_group,
                });
            }
        }
        None
    }

    /// Bootstrap-icons name registered for `channel`, or `None`
    /// when the channel has no entry. Used by the SPA's sidebar
    /// per-row marker via the `/api/chats` thread-list endpoint.
    pub fn icon_for(&self, channel: &str) -> Option<&str> {
        self.by_channel.get(channel).map(|i| i.icon.as_str())
    }

    pub fn len(&self) -> usize {
        self.by_channel.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_channel.is_empty()
    }

    /// Iterator over registered channel names. Used by boot
    /// logging + future plugin-introspection paths.
    pub fn channels(&self) -> impl Iterator<Item = &str> {
        self.by_channel.keys().map(|s| s.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(channel: &str, foreign: &str) -> TransportBinding {
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
        assert!(
            r.lookup_first_supported_binding(&[binding("signal", "+15551234567")])
                .is_none()
        );
        assert!(r.icon_for("signal").is_none());
    }

    #[test]
    fn registered_channel_round_trips() {
        let mut r = HostTransportRegistry::new();
        r.register(
            "signal",
            ChannelInfo {
                plugin_id: "signal".into(),
                icon: "chat-quote".into(),
            },
        );
        assert_eq!(r.icon_for("signal"), Some("chat-quote"));
        let resolved = r
            .lookup_first_supported_binding(&[binding("signal", "+15551234567")])
            .expect("signal binding must resolve");
        assert_eq!(resolved.plugin_id, "signal");
        assert_eq!(resolved.channel, "signal");
        assert_eq!(resolved.foreign_id, "+15551234567");
        assert!(!resolved.is_group);
    }

    #[test]
    fn lookup_skips_unregistered_channels_and_finds_first_match() {
        let mut r = HostTransportRegistry::new();
        r.register(
            "signal",
            ChannelInfo {
                plugin_id: "signal".into(),
                icon: "chat-quote".into(),
            },
        );
        let resolved = r
            .lookup_first_supported_binding(&[
                binding("whatsapp", "+15550000001"),
                binding("signal", "+15550000002"),
            ])
            .unwrap();
        assert_eq!(resolved.channel, "signal");
        assert_eq!(resolved.foreign_id, "+15550000002");
    }

    #[test]
    fn group_binding_propagates_is_group_flag() {
        let mut r = HostTransportRegistry::new();
        r.register(
            "signal",
            ChannelInfo {
                plugin_id: "signal".into(),
                icon: "chat-quote".into(),
            },
        );
        let mut group = binding("signal", "BASE64_INTERNAL_ID");
        group.is_group = true;
        let resolved = r.lookup_first_supported_binding(&[group]).unwrap();
        assert_eq!(resolved.foreign_id, "BASE64_INTERNAL_ID");
        assert!(
            resolved.is_group,
            "group bindings must propagate is_group through to the auto-bridge caller"
        );
    }

    #[test]
    fn channels_iterator_lists_every_registered_channel() {
        let mut r = HostTransportRegistry::new();
        r.register(
            "signal",
            ChannelInfo {
                plugin_id: "signal".into(),
                icon: "chat-quote".into(),
            },
        );
        r.register(
            "whatsapp",
            ChannelInfo {
                plugin_id: "whatsapp".into(),
                icon: "phone".into(),
            },
        );
        let mut names: Vec<&str> = r.channels().collect();
        names.sort();
        assert_eq!(names, vec!["signal", "whatsapp"]);
    }

    #[test]
    fn re_registering_same_channel_overwrites() {
        let mut r = HostTransportRegistry::new();
        r.register(
            "signal",
            ChannelInfo {
                plugin_id: "signal-a".into(),
                icon: "icon-a".into(),
            },
        );
        r.register(
            "signal",
            ChannelInfo {
                plugin_id: "signal-b".into(),
                icon: "icon-b".into(),
            },
        );
        assert_eq!(r.len(), 1);
        let resolved = r
            .lookup_first_supported_binding(&[binding("signal", "+1")])
            .unwrap();
        assert_eq!(resolved.plugin_id, "signal-b");
        assert_eq!(r.icon_for("signal"), Some("icon-b"));
    }
}
