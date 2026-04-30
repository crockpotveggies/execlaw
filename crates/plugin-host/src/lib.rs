//! execlaw-plugin-host
//!
//! Plugin discovery, lifecycle (install / enable / disable / uninstall),
//! isolation tiers (in-process / WASM / subprocess / container) and the
//! hook-registry that routes tool calls, UI panel mounts, event
//! subscriptions etc. to installed plugins.
//!
//! Phase 0 ships a skeleton; Phase 2 fills in the runtime loader per §4.4.

#![forbid(unsafe_code)]

pub mod builtin_registrar;
pub mod hook_registry;
pub mod host;
pub mod subprocess;

pub use builtin_registrar::{
    register_builtins, register_core_builtins, RegisterBuiltinsError,
};
pub use hook_registry::{
    HookRegistry, RegisteredAlertSource, RegisteredAny, RegisteredBuiltin,
    RegisteredEventSubscription, RegisteredIdentityProvider, RegisteredTool,
    RegisteredTransport, RegisteredUiPanel,
};
pub use host::{BuiltinTools, PluginHost, PluginHostError, PluginRow};
pub use subprocess::{RpcError, RpcRequest, RpcResponse, SubprocessPlugin, SubprocessSpec};

use execlaw_plugin_sdk::PluginManifest;
use std::collections::BTreeMap;

/// A tiny in-memory registry we can grow into the §4.2 hook registry.
#[derive(Debug, Default)]
pub struct PluginRegistry {
    installed: BTreeMap<String, PluginManifest>,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn install(&mut self, manifest: PluginManifest) {
        self.installed.insert(manifest.plugin.id.clone(), manifest);
    }

    pub fn uninstall(&mut self, id: &str) -> Option<PluginManifest> {
        self.installed.remove(id)
    }

    pub fn get(&self, id: &str) -> Option<&PluginManifest> {
        self.installed.get(id)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &PluginManifest)> {
        self.installed.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_plugin_sdk::PluginManifest;

    fn tiny(id: &str) -> PluginManifest {
        PluginManifest::parse(&format!(
            "[plugin]\nid = \"{id}\"\nname = \"N\"\nversion = \"0.1.0\"\n"
        ))
        .unwrap()
    }

    #[test]
    fn install_and_lookup_plugins() {
        let mut r = PluginRegistry::new();
        r.install(tiny("a"));
        r.install(tiny("b"));
        assert_eq!(r.iter().count(), 2);
        assert!(r.get("a").is_some());
        let removed = r.uninstall("a").unwrap();
        assert_eq!(removed.plugin.id, "a");
        assert_eq!(r.iter().count(), 1);
    }
}
