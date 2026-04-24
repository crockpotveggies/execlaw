//! Hook registry (§4.2).
//!
//! When a plugin is enabled, its manifest hook declarations register
//! into live lookup maps keyed by the appropriate primary key:
//!
//! - `tools_by_name` — every declared `[[tools]]` entry, keyed by tool name
//! - `ui_panels_by_mount` — `[[ui_panels]]` keyed by mount path
//! - `transports_by_id` — the plugin's `[transport]` keyed by transport id
//! - `identity_providers` — all enabled `[identity_provider]` plugins
//! - `event_subscriptions_by_kind` — which plugins listen for which events
//! - `alert_sources_by_prefix` — which plugins own alert fingerprint prefixes
//!
//! The registry is **additive per plugin, atomic per enable**: enabling
//! registers every hook the manifest declares; disabling removes them
//! all at once.

use execlaw_plugin_sdk::PluginManifest;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, RwLock};

/// A tool handler resolved to its owning plugin.
#[derive(Debug, Clone)]
pub struct RegisteredTool {
    pub plugin_id: String,
    pub tool_name: String,
    pub latency: String,
    pub required_capabilities: Vec<String>,
    pub schema_path: Option<String>,
}

/// A UI-panel mount (admin-UI sub-route).
#[derive(Debug, Clone)]
pub struct RegisteredUiPanel {
    pub plugin_id: String,
    pub mount: String,
    pub entry: String,
}

/// A transport connection (Signal, email, etc.).
#[derive(Debug, Clone)]
pub struct RegisteredTransport {
    pub plugin_id: String,
    pub transport_id: String,
    pub supports_attachments: bool,
    pub supports_groups: bool,
}

/// An identity provider (matches transport identifiers → principals).
#[derive(Debug, Clone)]
pub struct RegisteredIdentityProvider {
    pub plugin_id: String,
    pub resolves: Vec<String>,
    pub trust_hint_default: String,
}

/// Event subscription keyed by event kind (e.g. `conversation.message_inbound`).
#[derive(Debug, Clone)]
pub struct RegisteredEventSubscription {
    pub plugin_id: String,
    pub kind: String,
    pub handler: String,
}

/// Alert-source namespace a plugin owns.
#[derive(Debug, Clone)]
pub struct RegisteredAlertSource {
    pub plugin_id: String,
    pub fingerprint_prefix: String,
}

/// The live hook registry. Cheap to clone (Arc inside); `RwLock`
/// guards mutation on plugin enable/disable.
#[derive(Debug, Default, Clone)]
pub struct HookRegistry {
    inner: Arc<RwLock<HookRegistryInner>>,
}

#[derive(Debug, Default)]
struct HookRegistryInner {
    tools_by_name: BTreeMap<String, RegisteredTool>,
    ui_panels_by_mount: BTreeMap<String, RegisteredUiPanel>,
    transports_by_id: BTreeMap<String, RegisteredTransport>,
    identity_providers: BTreeMap<String, RegisteredIdentityProvider>,
    event_subs: HashMap<String, Vec<RegisteredEventSubscription>>,
    alert_sources: Vec<RegisteredAlertSource>,
    enabled_plugins: BTreeSet<String>,
}

impl HookRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Enable a plugin: register every hook declared by its manifest.
    ///
    /// Returns `Err` with a conflict description if a tool name / ui
    /// panel mount / transport id is already owned by another plugin.
    /// The registry is left untouched on error (all-or-nothing).
    pub fn enable(&self, manifest: &PluginManifest) -> Result<(), String> {
        let mut w = self.inner.write().unwrap();
        let plugin_id = &manifest.plugin.id;

        if w.enabled_plugins.contains(plugin_id) {
            return Err(format!("plugin '{plugin_id}' is already enabled"));
        }

        // Validate conflicts first, then insert.
        for t in &manifest.tools {
            if let Some(existing) = w.tools_by_name.get(&t.name) {
                return Err(format!(
                    "tool '{}' is already registered by plugin '{}'",
                    t.name, existing.plugin_id
                ));
            }
        }
        for p in &manifest.ui_panels {
            if let Some(existing) = w.ui_panels_by_mount.get(&p.mount) {
                return Err(format!(
                    "ui_panel mount '{}' is already registered by plugin '{}'",
                    p.mount, existing.plugin_id
                ));
            }
        }
        if let Some(t) = &manifest.transport
            && let Some(existing) = w.transports_by_id.get(&t.transport_id)
        {
            return Err(format!(
                "transport id '{}' is already registered by plugin '{}'",
                t.transport_id, existing.plugin_id
            ));
        }

        // Insert.
        for t in &manifest.tools {
            let latency = match t.latency {
                execlaw_plugin_sdk::manifest::ToolLatency::Low => "low",
                execlaw_plugin_sdk::manifest::ToolLatency::Medium => "medium",
                execlaw_plugin_sdk::manifest::ToolLatency::High => "high",
            };
            w.tools_by_name.insert(
                t.name.clone(),
                RegisteredTool {
                    plugin_id: plugin_id.clone(),
                    tool_name: t.name.clone(),
                    latency: latency.to_owned(),
                    required_capabilities: t.required_capabilities.clone(),
                    schema_path: t.schema.clone(),
                },
            );
        }
        for p in &manifest.ui_panels {
            w.ui_panels_by_mount.insert(
                p.mount.clone(),
                RegisteredUiPanel {
                    plugin_id: plugin_id.clone(),
                    mount: p.mount.clone(),
                    entry: p.entry.clone(),
                },
            );
        }
        if let Some(t) = &manifest.transport {
            w.transports_by_id.insert(
                t.transport_id.clone(),
                RegisteredTransport {
                    plugin_id: plugin_id.clone(),
                    transport_id: t.transport_id.clone(),
                    supports_attachments: t.supports_attachments,
                    supports_groups: t.supports_groups,
                },
            );
        }
        if let Some(ip) = &manifest.identity_provider {
            w.identity_providers.insert(
                plugin_id.clone(),
                RegisteredIdentityProvider {
                    plugin_id: plugin_id.clone(),
                    resolves: ip.resolves.clone(),
                    trust_hint_default: ip.trust_hint_default.clone(),
                },
            );
        }
        for sub in &manifest.event_subscriptions {
            w.event_subs
                .entry(sub.on.clone())
                .or_default()
                .push(RegisteredEventSubscription {
                    plugin_id: plugin_id.clone(),
                    kind: sub.on.clone(),
                    handler: sub.handler.clone().unwrap_or_default(),
                });
        }
        for src in &manifest.alert_sources {
            w.alert_sources.push(RegisteredAlertSource {
                plugin_id: plugin_id.clone(),
                fingerprint_prefix: src.fingerprint_prefix.clone(),
            });
        }
        w.enabled_plugins.insert(plugin_id.clone());
        Ok(())
    }

    /// Disable a plugin: remove every hook it owns.
    pub fn disable(&self, plugin_id: &str) {
        let mut w = self.inner.write().unwrap();
        w.tools_by_name.retain(|_, v| v.plugin_id != plugin_id);
        w.ui_panels_by_mount.retain(|_, v| v.plugin_id != plugin_id);
        w.transports_by_id.retain(|_, v| v.plugin_id != plugin_id);
        w.identity_providers.remove(plugin_id);
        for subs in w.event_subs.values_mut() {
            subs.retain(|s| s.plugin_id != plugin_id);
        }
        w.event_subs.retain(|_, v| !v.is_empty());
        w.alert_sources.retain(|s| s.plugin_id != plugin_id);
        w.enabled_plugins.remove(plugin_id);
    }

    pub fn is_enabled(&self, plugin_id: &str) -> bool {
        self.inner
            .read()
            .unwrap()
            .enabled_plugins
            .contains(plugin_id)
    }

    pub fn tool(&self, name: &str) -> Option<RegisteredTool> {
        self.inner.read().unwrap().tools_by_name.get(name).cloned()
    }

    pub fn all_tools(&self) -> Vec<RegisteredTool> {
        self.inner
            .read()
            .unwrap()
            .tools_by_name
            .values()
            .cloned()
            .collect()
    }

    pub fn transport(&self, id: &str) -> Option<RegisteredTransport> {
        self.inner.read().unwrap().transports_by_id.get(id).cloned()
    }

    pub fn identity_providers(&self) -> Vec<RegisteredIdentityProvider> {
        self.inner
            .read()
            .unwrap()
            .identity_providers
            .values()
            .cloned()
            .collect()
    }

    pub fn ui_panels(&self) -> Vec<RegisteredUiPanel> {
        self.inner
            .read()
            .unwrap()
            .ui_panels_by_mount
            .values()
            .cloned()
            .collect()
    }

    pub fn subscribers_for(&self, event_kind: &str) -> Vec<RegisteredEventSubscription> {
        self.inner
            .read()
            .unwrap()
            .event_subs
            .get(event_kind)
            .cloned()
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_with_tools(id: &str, tool_names: &[&str]) -> PluginManifest {
        let mut t = format!("[plugin]\nid = \"{id}\"\nname = \"{id}\"\nversion = \"1.0.0\"\n");
        for name in tool_names {
            t.push_str(&format!(
                "\n[[tools]]\nname = \"{name}\"\nschema = \"schemas/{name}.json\"\nlatency = \"low\"\nrequired_capabilities = []\n"
            ));
        }
        PluginManifest::parse(&t).unwrap()
    }

    #[test]
    fn enable_registers_tools() {
        let reg = HookRegistry::new();
        reg.enable(&manifest_with_tools("p1", &["a", "b"])).unwrap();
        assert!(reg.tool("a").is_some());
        assert!(reg.tool("b").is_some());
        assert_eq!(reg.all_tools().len(), 2);
        assert!(reg.is_enabled("p1"));
    }

    #[test]
    fn duplicate_tool_name_rejected() {
        let reg = HookRegistry::new();
        reg.enable(&manifest_with_tools("p1", &["shared"])).unwrap();
        let err = reg
            .enable(&manifest_with_tools("p2", &["shared"]))
            .unwrap_err();
        assert!(err.contains("already registered by plugin 'p1'"));
        // p2 should not be registered at all.
        assert!(!reg.is_enabled("p2"));
    }

    #[test]
    fn disable_removes_all_owned_hooks() {
        let reg = HookRegistry::new();
        reg.enable(&manifest_with_tools("p1", &["x", "y"])).unwrap();
        reg.disable("p1");
        assert!(reg.tool("x").is_none());
        assert!(reg.tool("y").is_none());
        assert!(!reg.is_enabled("p1"));
    }

    #[test]
    fn already_enabled_plugin_errors_on_second_enable() {
        let reg = HookRegistry::new();
        reg.enable(&manifest_with_tools("p1", &["a"])).unwrap();
        let err = reg.enable(&manifest_with_tools("p1", &["a"])).unwrap_err();
        assert!(err.contains("already enabled"));
    }

    #[test]
    fn enable_then_disable_then_reenable_works() {
        let reg = HookRegistry::new();
        let m = manifest_with_tools("p1", &["a"]);
        reg.enable(&m).unwrap();
        reg.disable("p1");
        reg.enable(&m).unwrap();
        assert!(reg.tool("a").is_some());
    }

    /// All-or-nothing atomicity: if ONE tool from a manifest conflicts
    /// with an existing registration, NONE of its siblings may land in
    /// the registry. A leaked unique tool here would be a trust-class
    /// bypass — a plugin whose install failed could still have tools
    /// callable by the agent.
    #[test]
    fn partial_conflict_leaves_registry_clean() {
        let reg = HookRegistry::new();
        reg.enable(&manifest_with_tools("p1", &["shared"])).unwrap();

        // p2 wants to register both "shared" (conflict) AND "unique"
        // (fine). The whole enable must fail, and "unique" must not
        // leak into the registry.
        let err = reg
            .enable(&manifest_with_tools("p2", &["unique", "shared"]))
            .unwrap_err();
        assert!(err.contains("shared"));
        assert!(
            reg.tool("unique").is_none(),
            "partial-install left a leaked tool in the registry"
        );
        assert!(!reg.is_enabled("p2"));
    }

    /// Transport IDs are singleton across plugins — two plugins cannot
    /// both claim `transport_id = "signal"`.
    #[test]
    fn transport_id_conflict_rejected() {
        let m1 = r#"[plugin]
id = "p1"
name = "p1"
version = "1.0.0"

[transport]
transport_id = "signal"
supports_attachments = true
supports_groups = false
"#;
        let m2 = r#"[plugin]
id = "p2"
name = "p2"
version = "1.0.0"

[transport]
transport_id = "signal"
supports_attachments = false
supports_groups = false
"#;
        let reg = HookRegistry::new();
        reg.enable(&PluginManifest::parse(m1).unwrap()).unwrap();
        let err = reg
            .enable(&PluginManifest::parse(m2).unwrap())
            .unwrap_err();
        assert!(err.contains("transport id 'signal'"));
        assert!(
            !reg.is_enabled("p2"),
            "p2 must not be marked enabled when its transport clashed"
        );
        // p1's transport is still registered correctly.
        assert_eq!(reg.transport("signal").unwrap().plugin_id, "p1");
    }

    /// UI-panel mount paths are also singleton — two plugins cannot
    /// both claim `/plugins/whatever`.
    #[test]
    fn ui_panel_mount_conflict_rejected() {
        let m1 = r#"[plugin]
id = "p1"
name = "p1"
version = "1.0.0"

[[ui_panels]]
mount = "/plugins/thing"
entry = "index.js"
"#;
        let m2 = r#"[plugin]
id = "p2"
name = "p2"
version = "1.0.0"

[[ui_panels]]
mount = "/plugins/thing"
entry = "other.js"
"#;
        let reg = HookRegistry::new();
        reg.enable(&PluginManifest::parse(m1).unwrap()).unwrap();
        let err = reg
            .enable(&PluginManifest::parse(m2).unwrap())
            .unwrap_err();
        assert!(err.contains("ui_panel mount"));
        assert_eq!(reg.ui_panels().len(), 1);
    }

    /// Event subscriptions from multiple plugins coexist, and
    /// `subscribers_for` returns them all.
    #[test]
    fn multiple_plugins_can_subscribe_to_same_event_kind() {
        let m1 = r#"[plugin]
id = "p1"
name = "p1"
version = "1.0.0"

[[event_subscriptions]]
on = "conversation.message_inbound"
handler = "handle_p1"
"#;
        let m2 = r#"[plugin]
id = "p2"
name = "p2"
version = "1.0.0"

[[event_subscriptions]]
on = "conversation.message_inbound"
handler = "handle_p2"
"#;
        let reg = HookRegistry::new();
        reg.enable(&PluginManifest::parse(m1).unwrap()).unwrap();
        reg.enable(&PluginManifest::parse(m2).unwrap()).unwrap();
        let subs = reg.subscribers_for("conversation.message_inbound");
        assert_eq!(subs.len(), 2);
        let plugin_ids: Vec<&str> = subs.iter().map(|s| s.plugin_id.as_str()).collect();
        assert!(plugin_ids.contains(&"p1"));
        assert!(plugin_ids.contains(&"p2"));
    }

    /// Disabling one of two plugins that share an event kind must leave
    /// the other's subscription intact.
    #[test]
    fn disable_preserves_other_plugins_subscriptions() {
        let m1 = r#"[plugin]
id = "p1"
name = "p1"
version = "1.0.0"

[[event_subscriptions]]
on = "x"
handler = "h"
"#;
        let m2 = r#"[plugin]
id = "p2"
name = "p2"
version = "1.0.0"

[[event_subscriptions]]
on = "x"
handler = "h"
"#;
        let reg = HookRegistry::new();
        reg.enable(&PluginManifest::parse(m1).unwrap()).unwrap();
        reg.enable(&PluginManifest::parse(m2).unwrap()).unwrap();
        reg.disable("p1");
        let subs = reg.subscribers_for("x");
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].plugin_id, "p2");
    }

    /// Identity providers are keyed by plugin — enable+disable is clean.
    #[test]
    fn identity_provider_registration_and_removal() {
        let m = r#"[plugin]
id = "idp-google"
name = "idp"
version = "1.0.0"

[identity_provider]
resolves = ["email", "phone"]
trust_hint_default = "Contact"
"#;
        let reg = HookRegistry::new();
        reg.enable(&PluginManifest::parse(m).unwrap()).unwrap();
        assert_eq!(reg.identity_providers().len(), 1);
        assert_eq!(reg.identity_providers()[0].resolves.len(), 2);
        reg.disable("idp-google");
        assert!(reg.identity_providers().is_empty());
    }
}
