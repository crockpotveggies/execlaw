//! google-contacts — Google People-API-backed identity provider.
//!
//! Subprocess plugin (§2 in MIGRATION_PLAN). The host stitches the
//! current OAuth access_token into every incoming RPC under
//! `params._oauth.controller`; this plugin reads that, calls the
//! People API, and returns a normalised match shape that matches
//! the existing `identity-local-address-book` reference.
//!
//! Two RPC methods:
//!
//! ```text
//! identity.resolve {transport, handle, _oauth: {controller: "ya29..."}}
//!   -> {match: IdentityMatch | null}
//!
//! tool.call {tool: "contacts.list", args: {limit?: u32}, _oauth: {...}}
//!   -> {contacts: [...]}
//! ```
//!
//! The plugin caches the People API response for [`CACHE_TTL_SECS`]
//! (default 5 min) so a flurry of identity.resolves during cold-
//! contact processing doesn't burn quota. Cache invalidates on TTL
//! or on access_token change (the host rotates them every ~hour).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const PEOPLE_API_URL: &str =
    "https://people.googleapis.com/v1/people/me/connections?personFields=names,emailAddresses,phoneNumbers&pageSize=1000";
const CACHE_TTL_SECS: u64 = 300;

#[derive(Debug, Deserialize)]
struct RpcRequest {
    id: u64,
    method: String,
    #[serde(default)]
    params: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct RpcOk {
    id: u64,
    result: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct RpcErr {
    id: u64,
    error: RpcErrBody,
}

#[derive(Debug, Serialize)]
struct RpcErrBody {
    code: i32,
    message: String,
}

/// Match shape mirrors `identity-local-address-book` so the host's
/// dispatcher can rank across providers without per-provider
/// branching.
#[derive(Debug, Clone, Serialize)]
struct IdentityMatch {
    stable_principal_id: String,
    confidence: f64,
    trust_hint: String,
    display_name: Option<String>,
    tags: Vec<String>,
    resolved_by: String,
}

#[derive(Debug, Clone, Serialize)]
struct ContactSummary {
    /// Stable principal id for cross-call dedup. Today: hash of the
    /// People API resource_name. Future: keyed off the resource_name
    /// directly once the host accepts non-uuid principals.
    principal_id: String,
    display_name: Option<String>,
    emails: Vec<String>,
    phones: Vec<String>,
}

// ---------------------------------------------------------------------------
// People API decode shapes (subset).

#[derive(Debug, Deserialize)]
struct PeopleResponse {
    #[serde(default)]
    connections: Vec<PeoplePerson>,
}

#[derive(Debug, Deserialize)]
struct PeoplePerson {
    #[serde(rename = "resourceName")]
    resource_name: Option<String>,
    #[serde(default)]
    names: Vec<NameField>,
    #[serde(rename = "emailAddresses", default)]
    email_addresses: Vec<ValueField>,
    #[serde(rename = "phoneNumbers", default)]
    phone_numbers: Vec<ValueField>,
}

#[derive(Debug, Deserialize)]
struct NameField {
    #[serde(rename = "displayName")]
    display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ValueField {
    value: Option<String>,
}

// ---------------------------------------------------------------------------
// In-process cache.

struct ContactCache {
    /// Token the cached response was fetched with — invalidated
    /// when the host rotates the access_token.
    access_token: String,
    fetched_at: u64,
    contacts: Vec<ContactSummary>,
    /// (transport, normalised_handle) → contact index for O(1)
    /// identity.resolve lookups.
    by_handle: HashMap<(String, String), usize>,
}

impl ContactCache {
    fn is_fresh(&self, access_token: &str, now: u64) -> bool {
        self.access_token == access_token && now.saturating_sub(self.fetched_at) < CACHE_TTL_SECS
    }
}

struct PluginState {
    http: reqwest::Client,
    cache: Mutex<Option<ContactCache>>,
}

impl PluginState {
    fn new() -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .user_agent("execlaw/google-contacts/0.1")
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            cache: Mutex::new(None),
        }
    }

    /// Fetch + cache the operator's contacts. Returns Err with a
    /// short string the dispatcher can surface to the operator.
    async fn ensure_contacts(&self, access_token: &str) -> Result<(), String> {
        let now = unix_now();
        {
            let guard = self.cache.lock().unwrap();
            if let Some(cache) = guard.as_ref() {
                if cache.is_fresh(access_token, now) {
                    return Ok(());
                }
            }
        }
        let resp = self
            .http
            .get(PEOPLE_API_URL)
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(|e| format!("People API request failed: {e}"))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| format!("People API body read: {e}"))?;
        if !status.is_success() {
            return Err(format!(
                "People API returned {}: {}",
                status.as_u16(),
                truncate(&body, 200),
            ));
        }
        let parsed: PeopleResponse = serde_json::from_str(&body)
            .map_err(|e| format!("People API decode: {e}"))?;
        let mut contacts = Vec::with_capacity(parsed.connections.len());
        let mut by_handle: HashMap<(String, String), usize> = HashMap::new();
        for person in parsed.connections {
            let display_name = person
                .names
                .iter()
                .find_map(|n| n.display_name.clone())
                .filter(|s| !s.trim().is_empty());
            let emails: Vec<String> = person
                .email_addresses
                .iter()
                .filter_map(|v| v.value.clone())
                .filter(|s| !s.trim().is_empty())
                .collect();
            let phones: Vec<String> = person
                .phone_numbers
                .iter()
                .filter_map(|v| v.value.clone())
                .filter(|s| !s.trim().is_empty())
                .collect();
            // Skip records that have nothing to match against.
            if emails.is_empty() && phones.is_empty() {
                continue;
            }
            let principal_id = principal_id_for(&person.resource_name);
            let summary = ContactSummary {
                principal_id,
                display_name,
                emails: emails.clone(),
                phones: phones.clone(),
            };
            let idx = contacts.len();
            for e in &emails {
                by_handle.insert(("email".into(), normalise_email(e)), idx);
            }
            for p in &phones {
                by_handle.insert(("phone".into(), normalise_phone(p)), idx);
            }
            contacts.push(summary);
        }
        let mut guard = self.cache.lock().unwrap();
        *guard = Some(ContactCache {
            access_token: access_token.to_owned(),
            fetched_at: now,
            contacts,
            by_handle,
        });
        Ok(())
    }
}

fn extract_oauth_token(params: &serde_json::Value, account: &str) -> Option<String> {
    params
        .get("_oauth")
        .and_then(|v| v.get(account))
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned())
}

fn normalise_email(e: &str) -> String {
    e.trim().to_lowercase()
}

/// Strip everything that isn't a digit. Matches the People API's
/// loose phone-number formatting against the strict identifier
/// shapes execlaw stores (E.164 or digit-only).
fn normalise_phone(p: &str) -> String {
    p.chars().filter(|c| c.is_ascii_digit()).collect()
}

fn principal_id_for(resource_name: &Option<String>) -> String {
    match resource_name {
        Some(s) if !s.is_empty() => format!("pri_google_{}", short_hash(s)),
        _ => format!("pri_google_anon_{}", unix_now()),
    }
}

fn short_hash(s: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    format!("{:x}", h.finish())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_owned()
    } else {
        format!("{}…", &s[..max])
    }
}

// ---------------------------------------------------------------------------
// RPC handlers.

async fn handle(
    state: &PluginState,
    req: &RpcRequest,
) -> Result<serde_json::Value, (i32, String)> {
    match req.method.as_str() {
        "identity.resolve" => identity_resolve(state, &req.params).await,
        "tool.call" => tool_call(state, &req.params).await,
        "shutdown" => std::process::exit(0),
        other => Err((-32601, format!("unknown method '{other}'"))),
    }
}

async fn identity_resolve(
    state: &PluginState,
    params: &serde_json::Value,
) -> Result<serde_json::Value, (i32, String)> {
    let transport = params
        .get("transport")
        .and_then(|v| v.as_str())
        .ok_or((-32602, "missing 'transport' param".to_owned()))?;
    let handle = params
        .get("handle")
        .and_then(|v| v.as_str())
        .ok_or((-32602, "missing 'handle' param".to_owned()))?;
    // Without an access_token we can't query — return null match
    // (NOT an error). The dispatcher cycles through the next
    // identity provider; the plugin Settings page is where the
    // operator sees the not-connected state.
    let Some(access_token) = extract_oauth_token(params, "controller") else {
        return Ok(serde_json::json!({"match": null}));
    };
    if transport != "email" && transport != "phone" {
        // Plugin's manifest declares only these transports.
        return Ok(serde_json::json!({"match": null}));
    }
    if let Err(e) = state.ensure_contacts(&access_token).await {
        // Soft-fail on People API errors. The host's dispatcher
        // logs + skips us; cold-contact flow falls through.
        eprintln!("google-contacts: ensure_contacts: {e}");
        return Ok(serde_json::json!({"match": null}));
    }
    let normalised = if transport == "email" {
        normalise_email(handle)
    } else {
        normalise_phone(handle)
    };
    let cache = state.cache.lock().unwrap();
    let Some(cache) = cache.as_ref() else {
        return Ok(serde_json::json!({"match": null}));
    };
    let Some(idx) = cache.by_handle.get(&(transport.to_owned(), normalised)) else {
        return Ok(serde_json::json!({"match": null}));
    };
    let contact = &cache.contacts[*idx];
    Ok(serde_json::json!({"match": IdentityMatch {
        stable_principal_id: contact.principal_id.clone(),
        confidence: 0.95,
        trust_hint: "Contact".into(),
        display_name: contact.display_name.clone(),
        tags: vec!["google_contacts".into()],
        resolved_by: "google-contacts".into(),
    }}))
}

async fn tool_call(
    state: &PluginState,
    params: &serde_json::Value,
) -> Result<serde_json::Value, (i32, String)> {
    let tool = params
        .get("tool")
        .and_then(|v| v.as_str())
        .ok_or((-32602, "missing 'tool' param".to_owned()))?;
    match tool {
        "contacts.list" => {
            let access_token = extract_oauth_token(params, "controller")
                .ok_or((-32001, "Google account not connected".to_owned()))?;
            state
                .ensure_contacts(&access_token)
                .await
                .map_err(|e| (-32002, e))?;
            let limit = params
                .get("args")
                .and_then(|a| a.get("limit"))
                .and_then(|v| v.as_u64())
                .unwrap_or(200) as usize;
            let cache = state.cache.lock().unwrap();
            let cache = cache.as_ref().expect("cache populated by ensure_contacts");
            let contacts: Vec<&ContactSummary> = cache.contacts.iter().take(limit).collect();
            Ok(serde_json::json!({
                "contacts": contacts,
                "total": cache.contacts.len(),
                "limit": limit,
            }))
        }
        other => Err((-32601, format!("unknown tool '{other}'"))),
    }
}

// ---------------------------------------------------------------------------
// Stdin loop.

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let state = PluginState::new();
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();
    while let Ok(Some(line)) = reader.next_line().await {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: RpcRequest = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("google-contacts: unparseable: {e}: {trimmed}");
                continue;
            }
        };
        let resp = match handle(&state, &req).await {
            Ok(result) => serde_json::to_string(&RpcOk {
                id: req.id,
                result,
            })
            .unwrap_or_default(),
            Err((code, message)) => serde_json::to_string(&RpcErr {
                id: req.id,
                error: RpcErrBody { code, message },
            })
            .unwrap_or_default(),
        };
        if stdout.write_all(resp.as_bytes()).await.is_err() {
            break;
        }
        if stdout.write_all(b"\n").await.is_err() {
            break;
        }
        if stdout.flush().await.is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_oauth_token_pulls_named_account() {
        let p = serde_json::json!({"_oauth": {"controller": "ya29.tok"}});
        assert_eq!(extract_oauth_token(&p, "controller").as_deref(), Some("ya29.tok"));
        assert!(extract_oauth_token(&p, "team").is_none());
    }

    #[test]
    fn extract_oauth_token_returns_none_when_oauth_missing() {
        let p = serde_json::json!({});
        assert!(extract_oauth_token(&p, "controller").is_none());
    }

    #[test]
    fn normalise_email_lowercases_and_trims() {
        assert_eq!(normalise_email(" Alice@Example.COM "), "alice@example.com");
    }

    #[test]
    fn normalise_phone_strips_formatting_to_digits_only() {
        assert_eq!(normalise_phone("+1 (555) 123-4567"), "15551234567");
        assert_eq!(normalise_phone("555.123.4567"), "5551234567");
    }

    #[test]
    fn principal_id_is_stable_across_calls_for_same_resource() {
        let r = Some("people/c12345".to_owned());
        assert_eq!(principal_id_for(&r), principal_id_for(&r));
        assert!(principal_id_for(&r).starts_with("pri_google_"));
    }

    #[tokio::test]
    async fn identity_resolve_returns_null_when_no_oauth_token() {
        let state = PluginState::new();
        let r = identity_resolve(
            &state,
            &serde_json::json!({"transport": "email", "handle": "x@x"}),
        )
        .await
        .unwrap();
        assert!(r["match"].is_null());
    }

    #[tokio::test]
    async fn identity_resolve_returns_null_for_unsupported_transport() {
        let state = PluginState::new();
        let r = identity_resolve(
            &state,
            &serde_json::json!({
                "transport": "web",
                "handle": "x",
                "_oauth": {"controller": "tok"},
            }),
        )
        .await
        .unwrap();
        assert!(r["match"].is_null());
    }

    #[tokio::test]
    async fn identity_resolve_finds_contact_in_seeded_cache() {
        // Seed the cache directly to skip the network round-trip.
        let state = PluginState::new();
        let mut by_handle = HashMap::new();
        by_handle.insert(("email".into(), "alice@example.com".into()), 0);
        by_handle.insert(("phone".into(), "15551234567".into()), 0);
        *state.cache.lock().unwrap() = Some(ContactCache {
            access_token: "tok".into(),
            fetched_at: unix_now(),
            contacts: vec![ContactSummary {
                principal_id: "pri_google_xyz".into(),
                display_name: Some("Alice".into()),
                emails: vec!["alice@example.com".into()],
                phones: vec!["+1 555 123 4567".into()],
            }],
            by_handle,
        });
        let r = identity_resolve(
            &state,
            &serde_json::json!({
                "transport": "email",
                "handle": "ALICE@example.com",
                "_oauth": {"controller": "tok"},
            }),
        )
        .await
        .unwrap();
        assert_eq!(r["match"]["stable_principal_id"], "pri_google_xyz");
        assert_eq!(r["match"]["display_name"], "Alice");
        assert_eq!(r["match"]["resolved_by"], "google-contacts");
        // Phone normalisation: caller passes E.164, plugin matches.
        let r = identity_resolve(
            &state,
            &serde_json::json!({
                "transport": "phone",
                "handle": "+1-555-123-4567",
                "_oauth": {"controller": "tok"},
            }),
        )
        .await
        .unwrap();
        assert_eq!(r["match"]["stable_principal_id"], "pri_google_xyz");
    }

    #[tokio::test]
    async fn tool_call_contacts_list_errors_when_no_oauth() {
        let state = PluginState::new();
        let err = tool_call(
            &state,
            &serde_json::json!({"tool": "contacts.list", "args": {}}),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, -32001);
        assert!(err.1.contains("not connected"));
    }
}
