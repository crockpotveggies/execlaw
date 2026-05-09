//! execlaw-identity-api
//!
//! Trait for identity-provider plugins (§2.14). Each installed identity
//! provider resolves an inbound transport identifier to a Principal + trust
//! hint; the control plane aggregates all matches to decide whether a
//! conversation is cold, auto-trusted, or blocked.

#![forbid(unsafe_code)]

use async_trait::async_trait;
use execlaw_core::ids::PluginId;
use execlaw_core::principal::{Identifier, TrustHint};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentityMatch {
    pub stable_principal_id: String,
    pub confidence: f32,
    pub trust_hint: TrustHint,
    pub display_name: Option<String>,
    pub tags: Vec<String>,
    pub resolved_by: PluginId,
}

#[async_trait]
pub trait IdentityProvider: Send + Sync {
    async fn resolve(&self, id: &Identifier) -> anyhow::Result<Option<IdentityMatch>>;
}
