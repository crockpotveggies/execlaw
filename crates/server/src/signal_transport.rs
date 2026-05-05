//! Signal-cli `TransportApi` impl (Phase 3, MIGRATION_PLAN §sidecar).
//!
//! Wraps the supervised `signal-cli` sidecar's HTTP RPC surface
//! (`bbernhard/signal-cli-rest-api`, /v2/send + /v1/contacts) so the
//! agent's `signal.send_message` and `signal.reply` builtins can
//! dispatch outbound traffic without rhai needing host-side capability
//! injection it can't reach.
//!
//! ## Endpoint resolution
//!
//! The transport doesn't hard-code the sidecar's URL — it dials a
//! [`RpcEndpointResolver`] at every request. In production that
//! resolver IS the [`SidecarSupervisor`], which mints stable
//! `127.0.0.1:<host_port>` URLs on the first spawn and keeps them
//! for the lifetime of the process. This indirection lets:
//!
//!   * **tests** point the transport at a one-shot axum server with
//!     `StaticEndpointResolver` instead of constructing a full
//!     supervisor + bollard mock,
//!   * **boot ordering** stay loose — the transport can be wired
//!     into `ChainedToolDispatch` before the supervisor's first
//!     reconcile lands; calls before then surface a clean
//!     `signal-cli sidecar not running yet` error.
//!
//! ## Recipient resolution
//!
//! `resolve_recipient` first consults [`TransportBindingStore`] for
//! `(channel, free_form)` — that's the canonical mapping the
//! first-contact intake (Phase 4) populates. Misses fall through to
//! the sidecar's `/v1/contacts/<self>` listing so the agent can
//! still address never-before-seen contacts by display name.
//!
//! ## Self-number sourcing
//!
//! signal-cli's REST surface keys every send by the controller's
//! registered phone number. For Phase 3 we read it from the
//! `EXECLAW_SIGNAL_CONTROLLER_NUMBER` env var; Phase 4+ moves it to
//! the encrypted vault when the operator-facing Signal account
//! settings page lands. The env-var path errors loudly when unset —
//! a tool failure with a clear "set EXECLAW_SIGNAL_CONTROLLER_NUMBER"
//! message rather than dispatching to an empty number that signal-cli
//! would silently 400 on.

use async_trait::async_trait;
use execlaw_core::Database;
use execlaw_core::tool::{ApiError, TransportApi};
use execlaw_core::transport_bindings::TransportBindingStore;
use std::sync::Arc;
use std::time::Duration;

use crate::sidecar_supervisor::SidecarSupervisor;

/// The transport's outbound channel id. Mirrored from the plugin
/// manifest's `[transport].transport_id`. Pinned here so a typo in
/// either spot turns into a compile error in tests rather than a
/// silent miss against `state_principal_groups.channel`.
pub const SIGNAL_CHANNEL: &str = "signal";

/// Sidecar service name as declared in `plugins/signal/plugin.toml`'s
/// `[[services]].name`. The supervisor's primary key.
pub const SIGNAL_SIDECAR_NAME: &str = "signal-cli";

/// Resolves a sidecar's loopback RPC base URL. Production wires
/// [`SidecarSupervisor`] here; tests pass a static URL to bypass
/// the supervisor entirely.
#[async_trait]
pub trait RpcEndpointResolver: Send + Sync {
    /// `Some("http://127.0.0.1:<port>")` when the sidecar has been
    /// spawned at least once (port reused on respawn — see the
    /// supervisor's port-reuse contract). `None` until the first
    /// successful spawn.
    async fn rpc_base_url(&self, sidecar_name: &str) -> Option<String>;
}

#[async_trait]
impl RpcEndpointResolver for SidecarSupervisor {
    async fn rpc_base_url(&self, sidecar_name: &str) -> Option<String> {
        self.host_port_for(sidecar_name)
            .await
            .map(|p| format!("http://127.0.0.1:{p}"))
    }
}

/// Test/dev fallback resolver — yields a fixed base URL regardless
/// of sidecar name. Production code never instantiates this.
pub struct StaticEndpointResolver(pub String);

#[async_trait]
impl RpcEndpointResolver for StaticEndpointResolver {
    async fn rpc_base_url(&self, _sidecar_name: &str) -> Option<String> {
        Some(self.0.clone())
    }
}

/// `TransportApi` impl backed by the supervised signal-cli sidecar.
///
/// Built once per **conversation turn** (so `current_chat_id` can
/// snapshot the inbound foreign id at construction without an extra
/// async lookup on every `signal.reply` call). Cheap to mint —
/// holds Arcs / a connection-pooled reqwest client.
#[derive(Clone)]
pub struct SignalCliTransport {
    resolver: Arc<dyn RpcEndpointResolver>,
    sidecar_name: String,
    client: reqwest::Client,
    /// Controller's registered Signal number, e.g. `+15551234567`.
    /// Wrapped in `Option` so the transport itself constructs
    /// without it — `send` returns a clear validation error when
    /// unset, rather than a confusing 400 from signal-cli.
    self_number: Option<String>,
    /// Inbound foreign id for the turn that built this transport.
    /// `None` when the turn was triggered by the controller / web
    /// chat / a routine — `signal.reply` errors loudly in that case
    /// (it's a precondition the agent should have respected).
    current_chat_id: Option<String>,
    /// Phase 7 — conversation id of the calling turn. Used by
    /// `send_with_attachments` to enforce trust scope on each
    /// attachment_id (the row's `conversation_id` MUST match this
    /// value, else a Signal-Group A turn could exfiltrate a Web-A
    /// attachment to a Signal-Group B contact). `None` from
    /// fixtures / non-conversation contexts; `send_with_attachments`
    /// rejects with Validation when unset.
    caller_conversation_id: Option<execlaw_core::ids::ConversationId>,
    /// Database handle for the binding store — `resolve_recipient`'s
    /// first lookup tier. Cheap clone (`rusqlite::Connection` lives
    /// behind a pool).
    db: Database,
}

impl SignalCliTransport {
    /// Build a transport pinned to the supervised sidecar. `db` is
    /// used by `resolve_recipient` against
    /// `state_transport_bindings`; `current_chat_id` is the per-turn
    /// inbound foreign id (read by the dispatcher from
    /// `state_transport_bindings.foreign_id` for the conversation,
    /// `None` for non-Signal-triggered turns).
    pub fn new(
        resolver: Arc<dyn RpcEndpointResolver>,
        db: Database,
        self_number: Option<String>,
        current_chat_id: Option<String>,
    ) -> Self {
        Self {
            resolver,
            sidecar_name: SIGNAL_SIDECAR_NAME.to_owned(),
            client: build_client(),
            self_number,
            current_chat_id,
            caller_conversation_id: None,
            db,
        }
    }

    /// Phase 7 — pin the calling turn's conversation id so
    /// `send_with_attachments` can enforce trust scope on each
    /// attachment row. Production wiring (`tool_dispatch.rs::
    /// build_ctx_for`) calls this with `ctx.conversation_id`; tests
    /// that don't exercise outbound attachments leave it unset.
    pub fn with_caller_conversation_id(mut self, cid: execlaw_core::ids::ConversationId) -> Self {
        self.caller_conversation_id = Some(cid);
        self
    }

    /// Override the sidecar service name — only useful in tests
    /// that register a fixture sidecar under a non-default name.
    /// Production callers always use `SIGNAL_SIDECAR_NAME`.
    pub fn with_sidecar_name(mut self, name: impl Into<String>) -> Self {
        self.sidecar_name = name.into();
        self
    }

    /// Override the reqwest client. Tests use this to install a
    /// short timeout against a slow mock; production reuses the
    /// default client built by [`build_client`].
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    /// Read the controller's Signal number from the
    /// `EXECLAW_SIGNAL_CONTROLLER_NUMBER` env var. Validates against
    /// the canonical Signal/E.164 phone-number shape — `+` followed
    /// by 1–15 digits — and rejects everything else. Without this
    /// validator, an env var containing embedded newlines, quotes,
    /// or other control bytes would land in the outbound JSON body
    /// at `post_send`. `serde_json` itself escapes the string before
    /// emission, but a malformed self-number would still produce a
    /// confusing 400 from signal-cli rather than a clean error
    /// here at the boundary. Returns `None` for any malformed input
    /// so the transport surfaces the unset-self-number error from
    /// `post_send` (which names the env var) rather than dispatching
    /// against a nonsense identity.
    pub fn read_self_number_from_env() -> Option<String> {
        let raw = std::env::var("EXECLAW_SIGNAL_CONTROLLER_NUMBER").ok()?;
        let trimmed = raw.trim();
        if !is_valid_e164(trimmed) {
            tracing::warn!(
                target: "signal_transport",
                "EXECLAW_SIGNAL_CONTROLLER_NUMBER is not a valid E.164 number; \
                 ignoring (expected: '+' followed by 1–15 digits)"
            );
            return None;
        }
        Some(trimmed.to_owned())
    }

    /// Resolve the controller's self-number, preferring the explicit
    /// override (`self_number`, populated from the env var when set)
    /// and falling back to whatever signal-cli has registered on
    /// disk (`/v1/accounts`). The fallback path is the common case
    /// post-pairing — the operator just scanned a QR and the
    /// daemon now reports the number; without this dynamic lookup
    /// every send would fail with "env var unset" forcing a server
    /// restart per pairing.
    async fn resolve_self_number(&self, base_url: &str) -> Result<String, ApiError> {
        if let Some(n) = self.self_number.as_deref() {
            return Ok(n.to_owned());
        }
        let url = format!("{base_url}/v1/accounts");
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| ApiError::Storage(format!("signal-cli /v1/accounts: {e}")))?;
        if !resp.status().is_success() {
            return Err(ApiError::Storage(format!(
                "signal-cli /v1/accounts returned HTTP {}",
                resp.status()
            )));
        }
        let arr: Vec<String> = resp
            .json()
            .await
            .map_err(|e| ApiError::Storage(format!("parse /v1/accounts: {e}")))?;
        arr.into_iter().next().ok_or_else(|| {
            ApiError::Validation(
                "no Signal account is paired yet — open Settings → Plugins → Signal \
                 and scan a QR code to link your phone before sending."
                    .into(),
            )
        })
    }

    /// POST `/v2/send` with the canonical signal-cli-rest-api body
    /// shape. Surfaced as a method (not inlined into `send`) so the
    /// adversarial tests can exercise both happy + sad paths
    /// without an extra layer of mock indirection.
    async fn post_send(
        &self,
        base_url: &str,
        recipient: &str,
        text: &str,
    ) -> Result<String, ApiError> {
        let number = self.resolve_self_number(base_url).await?;
        let body = serde_json::json!({
            "number": number,
            "recipients": [recipient],
            "message": text,
        });
        let url = format!("{base_url}/v2/send");
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| ApiError::Storage(format!("signal-cli RPC: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            // Truncate the body before surfacing it — signal-cli
            // can dump several KB of stacktrace into a 5xx, and the
            // chat surface renders the whole error string verbatim.
            // 240 chars is enough to convey the diagnosis without
            // bloating the tool result.
            return Err(ApiError::Storage(format!(
                "signal-cli /v2/send returned HTTP {status}: {}",
                truncate_for_log(&body_text, 240)
            )));
        }
        // signal-cli-rest-api returns `{ "timestamp": <i64> }`. The
        // timestamp doubles as the message id for read-receipt
        // correlation. Older versions returned a different shape;
        // accept either by trying the canonical key first and
        // falling back to the raw body string.
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| ApiError::Storage(format!("signal-cli /v2/send body: {e}")))?;
        Ok(body
            .get("timestamp")
            .map(|v| v.to_string())
            .unwrap_or_else(|| body.to_string()))
    }

    /// Resolve the supervised sidecar's base URL + the controller's
    /// self-number in one go. Every group-RPC method needs both;
    /// extracting the call collapses six lines of boilerplate per
    /// method body into one. Returns the same
    /// `signal-cli sidecar … not running yet` storage error from
    /// `send` when the supervisor hasn't published a port.
    async fn resolve_endpoint(&self) -> Result<(String, String), ApiError> {
        let base_url = self
            .resolver
            .rpc_base_url(&self.sidecar_name)
            .await
            .ok_or_else(|| {
                ApiError::Storage(format!(
                    "signal-cli sidecar '{}' not running yet — \
                     check Settings → Sidecars; the supervisor will \
                     respawn within {} restart attempts",
                    self.sidecar_name,
                    crate::sidecar_supervisor::MAX_RESTART_ATTEMPTS,
                ))
            })?;
        let number = self.resolve_self_number(&base_url).await?;
        Ok((base_url, number))
    }

    /// Register a freshly-created Signal group as a transport
    /// binding. Lets `signal.send_message` dispatch to the new
    /// group_id immediately after `signal.create_group` returns
    /// (without this, resolve_recipient would NotFound the new
    /// group until the first inbound message landed).
    ///
    /// Mints a `state_principal_groups` row with
    /// `native_group_id = group_id` and `principals = []` — the
    /// Phase 5 inbound consumer reconciles membership as senders
    /// post into the group. The binding's `is_group = true` so the
    /// inbound consumer's group-routing branch picks it up
    /// instead of the 1:1 path.
    async fn register_outbound_group_binding(
        &self,
        group_id: &str,
        _title: Option<&str>,
    ) -> Result<(), ApiError> {
        // Validator double-check: every caller into the outbound
        // path already runs `is_valid_signal_group_id`, but the
        // binding row's `foreign_id` is also a routing key on
        // every inbound message — defending here keeps a future
        // refactor that sources the id from elsewhere from
        // sneaking a bad value into the table.
        if !is_valid_signal_group_id(group_id) {
            return Err(ApiError::Validation(
                "group_id must be a non-empty base64-shaped Signal id".into(),
            ));
        }
        use execlaw_core::principal_groups::{GroupKey, PrincipalGroupStore};
        use execlaw_core::transport_bindings::TransportBindingStore;
        let now = chrono::Utc::now().timestamp();
        let pg_store = PrincipalGroupStore::new(&self.db);
        let pg = pg_store
            .resolve(
                &GroupKey {
                    channel: SIGNAL_CHANNEL,
                    native_group_id: Some(group_id),
                    principals: &[],
                    includes_controller: true,
                },
                now,
            )
            .map_err(|e| ApiError::Storage(format!("principal_group mint: {e}")))?;
        let store = TransportBindingStore::new(&self.db);
        let _ = store
            .insert_binding(SIGNAL_CHANNEL, group_id, &pg.group_id, true, now)
            .map_err(|e| ApiError::Storage(format!("binding insert: {e}")))?;
        Ok(())
    }

    /// Surface a non-success HTTP response as a `Storage` error
    /// with a truncated body. Pulled out so every group-RPC method
    /// shares the same error shape — the chat surface's humanised
    /// "tool failed" line stays consistent across send / list /
    /// create / add / leave.
    async fn err_for_http(
        endpoint: &str,
        status: reqwest::StatusCode,
        resp: reqwest::Response,
    ) -> ApiError {
        let body_text = resp.text().await.unwrap_or_default();
        ApiError::Storage(format!(
            "signal-cli {endpoint} returned HTTP {status}: {}",
            truncate_for_log(&body_text, 240)
        ))
    }
}

/// True for canonical E.164 (Signal-shaped) phone numbers: a `+`
/// followed by 1–15 ASCII digits with no other characters. The
/// strict character set is the load-bearing constraint — it
/// guarantees the self-number can't carry newlines, quotes,
/// backslashes, or non-ASCII bytes that would muddle log lines or
/// confuse signal-cli. Length 1–15 follows the E.164 spec.
fn is_valid_e164(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'+') {
        return false;
    }
    let digits = &bytes[1..];
    !digits.is_empty() && digits.len() <= 15 && digits.iter().all(|b| b.is_ascii_digit())
}

/// True for canonical Signal group ids: a URL-safe base64 blob
/// with no path-control characters. Defends every URL-path
/// interpolation site (`/v1/groups/{number}/{group_id}/...`)
/// against a malformed sidecar response that could carry `..`,
/// `?`, `#`, or a slash and reach an unintended endpoint.
///
/// **Alphabet is deliberately URL-safe-only** — `A-Za-z0-9-_=`.
/// We reject `+` and `/` even though they're valid base64 chars,
/// because allowing `/` would let `/etc/passwd` pass the
/// validator (Signal-cli emits URL-safe ids on the v1 endpoints
/// we use; the rare standard-base64 case can be re-encoded by
/// the sidecar, but a path-traversing id can't).
fn is_valid_signal_group_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 256
        && s.as_bytes()
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(*b, b'-' | b'_' | b'='))
}

/// Truncate a signal-cli error response body to a fixed maximum
/// for inclusion in our error message. Prevents accidentally
/// surfacing several KB of HTML / debug context into the agent's
/// chat surface. 240 chars matches a typical "tool error" toast
/// budget.
fn truncate_for_log(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push('…');
    out
}

/// Build the reqwest client used to dial the sidecar. Localhost
/// loopback so connect timeouts can be aggressive — if signal-cli
/// hasn't bound the port within 2 s, the agent's tool timeout
/// trips before the user-facing UI feels stuck.
fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(15))
        .pool_max_idle_per_host(2)
        .build()
        // Builder failures here mean rustls misconfig — fall back
        // to the default-default client rather than panic so a
        // boot-time issue doesn't take the whole process down.
        .unwrap_or_else(|_| reqwest::Client::new())
}

#[async_trait]
impl TransportApi for SignalCliTransport {
    async fn resolve_recipient(&self, channel: &str, free_form: &str) -> Result<String, ApiError> {
        if channel != SIGNAL_CHANNEL {
            return Err(ApiError::Validation(format!(
                "signal transport cannot resolve channel '{channel}'"
            )));
        }
        // First tier: the canonical binding the first-contact intake
        // populated. This is the dominant path in production — every
        // controller-known contact is in here.
        let store = TransportBindingStore::new(&self.db);
        match store.lookup_principal_group(channel, free_form) {
            Ok(Some(_)) => return Ok(free_form.to_owned()),
            Ok(None) => {}
            Err(e) => {
                return Err(ApiError::Storage(format!("transport binding lookup: {e}")));
            }
        }
        // Second tier: signal-cli's own contact list. The sidecar
        // remembers contacts from the registered account's address
        // book; this catches the "operator typed a display name we
        // haven't bound yet" case. Phase 3 leaves this as a stub
        // (returns NotFound) — a real implementation lands in
        // Phase 5 alongside group ops.
        Err(ApiError::NotFound(format!(
            "no signal binding for '{free_form}'; \
             populate it via first-contact intake (Phase 4) or \
             pass an exact `signal:user:<uuid>` foreign id"
        )))
    }

    async fn send(&self, channel: &str, recipient: &str, text: &str) -> Result<String, ApiError> {
        if channel != SIGNAL_CHANNEL {
            return Err(ApiError::Validation(format!(
                "signal transport cannot send on channel '{channel}'"
            )));
        }
        if recipient.is_empty() {
            return Err(ApiError::Validation("recipient must not be empty".into()));
        }
        if text.is_empty() {
            return Err(ApiError::Validation(
                "message text must not be empty".into(),
            ));
        }
        let base_url = self
            .resolver
            .rpc_base_url(&self.sidecar_name)
            .await
            .ok_or_else(|| {
                ApiError::Storage(format!(
                    "signal-cli sidecar '{}' not running yet — \
                     check Settings → Sidecars; the supervisor will \
                     respawn within {} restart attempts",
                    self.sidecar_name,
                    crate::sidecar_supervisor::MAX_RESTART_ATTEMPTS,
                ))
            })?;
        self.post_send(&base_url, recipient, text).await
    }

    async fn current_chat_id(&self, channel: &str) -> Result<Option<String>, ApiError> {
        if channel != SIGNAL_CHANNEL {
            return Ok(None);
        }
        Ok(self.current_chat_id.clone())
    }

    // ---- Phase 5 group ops ----------------------------------------

    async fn list_groups(
        &self,
        channel: &str,
    ) -> Result<Vec<execlaw_core::tool::TransportGroupSummary>, ApiError> {
        if channel != SIGNAL_CHANNEL {
            return Err(ApiError::Validation(format!(
                "signal transport cannot list groups on channel '{channel}'"
            )));
        }
        let (base_url, number) = self.resolve_endpoint().await?;
        let url = format!("{base_url}/v1/groups/{number}");
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| ApiError::Storage(format!("signal-cli RPC: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Self::err_for_http("/v1/groups", status, resp).await);
        }
        // signal-cli-rest-api returns an array of `{id, name,
        // members, ...}`. We strip down to the agent-facing shape
        // — `id`, `name` (string or empty), `member_count`. Any
        // malformed entry skips silently rather than failing the
        // whole list (a single mis-shaped group shouldn't make the
        // operator's "what groups am I in" question fail).
        let raw: Vec<serde_json::Value> = resp
            .json()
            .await
            .map_err(|e| ApiError::Storage(format!("signal-cli /v1/groups body: {e}")))?;
        let mut out = Vec::with_capacity(raw.len());
        for g in raw {
            let Some(id) = g.get("id").and_then(|v| v.as_str()) else {
                tracing::debug!(target: "signal_transport", "skipping group without id");
                continue;
            };
            // `name` is a string in modern signal-cli; older builds
            // emitted null. Treat empty string the same as null so
            // the agent doesn't render `""` titles.
            let name = g
                .get("name")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            // `members` is an array of E.164 strings or contact
            // descriptors; we just want the count for the summary.
            let member_count = g
                .get("members")
                .and_then(|v| v.as_array())
                .map(|a| a.len() as u32)
                .unwrap_or(0);
            out.push(execlaw_core::tool::TransportGroupSummary {
                id: id.to_owned(),
                name,
                member_count,
            });
        }
        Ok(out)
    }

    async fn create_group(
        &self,
        channel: &str,
        title: Option<&str>,
        members: &[String],
    ) -> Result<String, ApiError> {
        if channel != SIGNAL_CHANNEL {
            return Err(ApiError::Validation(format!(
                "signal transport cannot create groups on channel '{channel}'"
            )));
        }
        if members.is_empty() {
            return Err(ApiError::Validation(
                "create_group requires at least one member".into(),
            ));
        }
        let (base_url, number) = self.resolve_endpoint().await?;
        // signal-cli-rest-api requires `name` even though signal
        // itself supports nameless groups — pass an empty string
        // when the operator didn't supply a title; the bridge
        // accepts that.
        let body = serde_json::json!({
            "name": title.unwrap_or(""),
            "members": members,
        });
        let url = format!("{base_url}/v1/groups/{number}");
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| ApiError::Storage(format!("signal-cli RPC: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Self::err_for_http("/v1/groups (POST)", status, resp).await);
        }
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| ApiError::Storage(format!("signal-cli create_group body: {e}")))?;
        let group_id = body
            .get("id")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .ok_or_else(|| {
                ApiError::Storage(format!(
                    "signal-cli create_group response missing `id`: {}",
                    truncate_for_log(&body.to_string(), 240)
                ))
            })?;
        // Defence in depth: validate the id we got back fits the
        // expected base64 shape before we (or anyone downstream)
        // ever interpolates it into a URL path. A misbehaving
        // sidecar that returned `"id": "../etc/passwd"` would
        // otherwise corrupt every later add_group_members /
        // leave_group / send call.
        if !is_valid_signal_group_id(&group_id) {
            return Err(ApiError::Storage(format!(
                "signal-cli create_group returned a non-base64 id: {}",
                truncate_for_log(&group_id, 64)
            )));
        }
        // Register the new group as a transport binding so the agent
        // can immediately call `signal.send_message` against the
        // returned id without waiting for an inbound message to
        // populate the binding. Mints a principal_group keyed by
        // `native_group_id = group_id` (the signal-cli base64) and
        // an empty principal set — Phase 5 inbound routing
        // reconciles membership as messages land. Failure here is
        // logged but does NOT unwind the bridge-side group: the
        // group exists; the agent can re-bind via the next inbound
        // or a manual list_groups + admin tooling.
        if let Err(e) = self.register_outbound_group_binding(&group_id, title).await {
            tracing::warn!(
                target: "signal_transport",
                group_id = %group_id,
                error = %e,
                "create_group succeeded on the bridge but binding insert failed; \
                 next inbound from this group will heal the binding",
            );
        }
        Ok(group_id)
    }

    async fn add_group_members(
        &self,
        channel: &str,
        group_id: &str,
        members: &[String],
    ) -> Result<(), ApiError> {
        if channel != SIGNAL_CHANNEL {
            return Err(ApiError::Validation(format!(
                "signal transport cannot add members on channel '{channel}'"
            )));
        }
        if !is_valid_signal_group_id(group_id) {
            return Err(ApiError::Validation(
                "group_id must be a non-empty base64-shaped Signal id (no path-control chars)"
                    .into(),
            ));
        }
        if members.is_empty() {
            return Err(ApiError::Validation(
                "add_group_members requires at least one member".into(),
            ));
        }
        let (base_url, number) = self.resolve_endpoint().await?;
        let body = serde_json::json!({ "members": members });
        let url = format!("{base_url}/v1/groups/{number}/{group_id}/members");
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| ApiError::Storage(format!("signal-cli RPC: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Self::err_for_http("/v1/groups/.../members (POST)", status, resp).await);
        }
        Ok(())
    }

    async fn leave_group(&self, channel: &str, group_id: &str) -> Result<(), ApiError> {
        if channel != SIGNAL_CHANNEL {
            return Err(ApiError::Validation(format!(
                "signal transport cannot leave groups on channel '{channel}'"
            )));
        }
        if !is_valid_signal_group_id(group_id) {
            return Err(ApiError::Validation(
                "group_id must be a non-empty base64-shaped Signal id (no path-control chars)"
                    .into(),
            ));
        }
        let (base_url, number) = self.resolve_endpoint().await?;
        let url = format!("{base_url}/v1/groups/{number}/{group_id}/quit");
        let resp = self
            .client
            .post(&url)
            .send()
            .await
            .map_err(|e| ApiError::Storage(format!("signal-cli RPC: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Self::err_for_http("/v1/groups/.../quit", status, resp).await);
        }
        Ok(())
    }

    // ---- Phase 7 outbound attachments -----------------------------

    async fn send_with_attachments(
        &self,
        channel: &str,
        recipient: &str,
        text: &str,
        attachments: &[String],
    ) -> Result<String, ApiError> {
        if channel != SIGNAL_CHANNEL {
            return Err(ApiError::Validation(format!(
                "signal transport cannot send on channel '{channel}'"
            )));
        }
        if recipient.is_empty() {
            return Err(ApiError::Validation("recipient must not be empty".into()));
        }
        if attachments.is_empty() {
            // Empty list → fall through to plain text send. Avoids
            // surprising the agent that calls with an empty array
            // because their templating produced no ids.
            return self.send(channel, recipient, text).await;
        }
        // Phase 7 — sub-cap on outbound attachments. signal-cli's
        // multipart limit is in the multi-megabyte range; bounding
        // batches at 8 keeps a single tool call from accidentally
        // sending hundreds of files (and runs the base64 encode on
        // a bounded amount of memory).
        const MAX_OUTBOUND_ATTACHMENTS: usize = 8;
        if attachments.len() > MAX_OUTBOUND_ATTACHMENTS {
            return Err(ApiError::Validation(format!(
                "max {MAX_OUTBOUND_ATTACHMENTS} attachments per send; got {}",
                attachments.len()
            )));
        }
        let caller_cid = self.caller_conversation_id.as_ref().ok_or_else(|| {
            ApiError::Validation(
                "send_with_attachments requires a calling conversation context".into(),
            )
        })?;
        // Trust scope + base64 encode each attachment. Failures
        // here surface as the agent's first-line error so the
        // operator sees "tool failed: attachment X not in this
        // conversation" rather than a confusing 400 from
        // signal-cli.
        use base64::Engine;
        use execlaw_core::attachments::AttachmentStore;
        use execlaw_core::ids::AttachmentId;
        // Per-file + aggregate caps. Mirror inbound's
        // MAX_INBOUND_ATTACHMENT_BYTES so an attachment that came
        // in over Signal can't be larger than what Signal itself
        // accepts on the way back out.
        const MAX_OUTBOUND_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;
        // Aggregate cap: 8 × 25 MiB = 200 MiB would crater the
        // host. 50 MiB total cap keeps the worst case bounded
        // regardless of how the per-file caps were filled.
        const MAX_TOTAL_OUTBOUND_BYTES: u64 = 50 * 1024 * 1024;

        let store = AttachmentStore::new(&self.db);
        let mut data_urls = Vec::with_capacity(attachments.len());
        let mut total_bytes: u64 = 0;
        for raw_id in attachments {
            let aid = AttachmentId::from(raw_id.as_str());
            let row = store
                .get(&aid)
                .map_err(|e| ApiError::Storage(format!("attachment lookup: {e}")))?
                .ok_or_else(|| ApiError::NotFound(format!("no attachment '{raw_id}'")))?;
            // Trust scope: cross-conversation refusal. NotFound
            // (not NotAuthorized) so the agent can't probe the
            // attachment-id space — same defense as
            // ServerAttachmentApi::send.
            if row.conversation_id.as_str() != caller_cid.as_str() {
                return Err(ApiError::NotFound(format!("no attachment '{raw_id}'")));
            }
            // Audit fix: pre-flight on-disk size against both per-
            // file and aggregate caps BEFORE reading. A malicious
            // 25 MiB file shouldn't get loaded into memory just to
            // be rejected by the aggregate cap on the next
            // iteration — and a `metadata` syscall is cheap.
            let on_disk = std::fs::metadata(&row.path)
                .map_err(|e| ApiError::Storage(format!("attachment stat: {e}")))?
                .len();
            if on_disk > MAX_OUTBOUND_ATTACHMENT_BYTES {
                return Err(ApiError::Validation(format!(
                    "attachment '{raw_id}' is {on_disk} bytes; max is {MAX_OUTBOUND_ATTACHMENT_BYTES}"
                )));
            }
            let projected = total_bytes.saturating_add(on_disk);
            if projected > MAX_TOTAL_OUTBOUND_BYTES {
                return Err(ApiError::Validation(format!(
                    "outbound attachment total would be {projected} bytes; max is {MAX_TOTAL_OUTBOUND_BYTES}"
                )));
            }
            // Audit fix: read on a blocking thread so a 25 MiB
            // file on a slow disk doesn't stall the tokio runtime
            // for 100s of ms. Mirrors `ServerAttachmentApi::send`,
            // which uses `spawn_blocking` for the same reason.
            let path_for_task = row.path.clone();
            let bytes = tokio::task::spawn_blocking(move || std::fs::read(path_for_task))
                .await
                .map_err(|e| ApiError::Storage(format!("attachment read join: {e}")))?
                .map_err(|e| ApiError::Storage(format!("attachment read: {e}")))?;
            // Defence in depth — the metadata check ran above, but
            // the file could grow between stat and read.
            if (bytes.len() as u64) > MAX_OUTBOUND_ATTACHMENT_BYTES {
                return Err(ApiError::Validation(format!(
                    "attachment '{raw_id}' grew to {} bytes between stat and read",
                    bytes.len()
                )));
            }
            total_bytes = total_bytes.saturating_add(bytes.len() as u64);
            let mime = if row.mime_type.is_empty() {
                "application/octet-stream"
            } else {
                row.mime_type.as_str()
            };
            // signal-cli-rest-api accepts data-URL formatted base64
            // with the `data:<mime>;base64,` prefix. Any other shape
            // (raw base64, multipart) is rejected as 400.
            let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
            data_urls.push(format!("data:{mime};base64,{encoded}"));
        }

        let (base_url, number) = self.resolve_endpoint().await?;
        let body = serde_json::json!({
            "number": number,
            "recipients": [recipient],
            "message": text,
            "base64_attachments": data_urls,
        });
        let url = format!("{base_url}/v2/send");
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| ApiError::Storage(format!("signal-cli RPC: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Self::err_for_http("/v2/send (with attachments)", status, resp).await);
        }
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| ApiError::Storage(format!("signal-cli /v2/send body: {e}")))?;
        Ok(body
            .get("timestamp")
            .map(|v| v.to_string())
            .unwrap_or_else(|| body.to_string()))
    }

    // ---- Phase 6 attachment fetch ---------------------------------

    async fn fetch_attachment(
        &self,
        channel: &str,
        attachment_id: &str,
    ) -> Result<execlaw_core::tool::FetchedAttachment, ApiError> {
        if channel != SIGNAL_CHANNEL {
            return Err(ApiError::Validation(format!(
                "signal transport cannot fetch attachments on channel '{channel}'"
            )));
        }
        // signal-cli's attachment ids are filename-shaped (base64 +
        // sometimes a `.<ext>` suffix). The same URL-path defense
        // we apply to group ids applies here — a malformed sidecar
        // response could otherwise lead us at `/etc/passwd`.
        // Accept the union of {alphanumerics, base64 url-safe chars,
        // `.`}: dot is needed for the optional extension, slash and
        // path-traversal sequences are still rejected.
        if attachment_id.is_empty()
            || attachment_id.len() > 256
            || !attachment_id
                .as_bytes()
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || matches!(*b, b'-' | b'_' | b'=' | b'.'))
            || attachment_id.contains("..")
        {
            return Err(ApiError::Validation(format!(
                "attachment id has invalid shape: {attachment_id:?}"
            )));
        }
        let base_url = self
            .resolver
            .rpc_base_url(&self.sidecar_name)
            .await
            .ok_or_else(|| {
                ApiError::Storage(format!(
                    "signal-cli sidecar '{}' not running yet",
                    self.sidecar_name,
                ))
            })?;
        let url = format!("{base_url}/v1/attachments/{attachment_id}");
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| ApiError::Storage(format!("signal-cli RPC: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Self::err_for_http("/v1/attachments", status, resp).await);
        }
        // Capture the `Content-Type` BEFORE consuming the body —
        // `bytes()` moves `resp`. Drop any charset suffix so the
        // attachment row stores `image/jpeg`, not
        // `image/jpeg; charset=binary`.
        let mime_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.split(';').next().unwrap_or(s).trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "application/octet-stream".to_owned());
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| ApiError::Storage(format!("signal-cli attachment body: {e}")))?
            .to_vec();
        Ok(execlaw_core::tool::FetchedAttachment {
            bytes,
            mime_type,
            filename: None, // bridge doesn't surface a filename on this endpoint
        })
    }
}

/// Channel-keyed factory for the host-side transport registry.
/// The registry calls `build` on every transport-bridge dispatch
/// (chat text reply auto-bridge, attachment fan-out, research-PDF
/// dispatch). Captures the long-lived `Arc<dyn RpcEndpointResolver>`
/// at registration so per-call construction is just an Arc clone +
/// SignalCliTransport mint.
pub struct SignalCliTransportFactory {
    resolver: std::sync::Arc<dyn RpcEndpointResolver>,
}

impl SignalCliTransportFactory {
    pub fn new(resolver: std::sync::Arc<dyn RpcEndpointResolver>) -> Self {
        Self { resolver }
    }
}

#[async_trait::async_trait]
impl crate::transport_registry::HostTransportFactory for SignalCliTransportFactory {
    fn channel(&self) -> &str {
        SIGNAL_CHANNEL
    }
    fn build(
        &self,
        db: &execlaw_core::Database,
        conversation_id: &execlaw_core::ids::ConversationId,
        foreign_id: &str,
    ) -> Option<std::sync::Arc<dyn execlaw_core::tool::TransportApi>> {
        let transport = SignalCliTransport::new(
            self.resolver.clone(),
            db.clone(),
            // `read_self_number_from_env` stays the override path
            // for back-compat. `post_send` falls through to
            // `/v1/accounts` when this is None — the common case
            // post-pairing.
            SignalCliTransport::read_self_number_from_env(),
            Some(foreign_id.to_owned()),
        )
        .with_caller_conversation_id(conversation_id.clone());
        Some(std::sync::Arc::new(transport))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_core::db::DbConfig;
    use execlaw_core::migrations::MigrationRunner;
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    /// In-process HTTP listener used as a signal-cli mock. Captures
    /// every request in `recorded` so tests can assert on the body
    /// shape without depending on `wiremock` (not in tree). Drops
    /// the listener cleanly on test exit via the `_handle` JoinHandle.
    struct MockSidecar {
        addr: SocketAddr,
        recorded: Arc<Mutex<Vec<MockRequest>>>,
        _handle: tokio::task::JoinHandle<()>,
    }

    #[derive(Debug, Clone)]
    struct MockRequest {
        path: String,
        body: serde_json::Value,
    }

    /// Spin up a tiny axum server returning `response_body` with
    /// `response_status` for every request. Uses axum because it's
    /// already in the workspace; `wiremock` would be cleaner but
    /// isn't a dep and Phase 3 isn't a good place to add one.
    async fn spawn_mock(response_status: u16, response_body: serde_json::Value) -> MockSidecar {
        use axum::extract::State;
        use axum::http::StatusCode;
        use axum::response::IntoResponse;
        use axum::{Json, Router};

        #[derive(Clone)]
        struct MockState {
            recorded: Arc<Mutex<Vec<MockRequest>>>,
            response_body: serde_json::Value,
            response_status: u16,
        }

        async fn catch_all(
            State(s): State<MockState>,
            uri: axum::http::Uri,
            body: Option<Json<serde_json::Value>>,
        ) -> impl IntoResponse {
            let body_v = body.map(|Json(v)| v).unwrap_or(serde_json::Value::Null);
            s.recorded.lock().unwrap().push(MockRequest {
                path: uri.path().to_owned(),
                body: body_v,
            });
            let status = StatusCode::from_u16(s.response_status).unwrap();
            (status, Json(s.response_body.clone()))
        }

        let recorded: Arc<Mutex<Vec<MockRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let state = MockState {
            recorded: recorded.clone(),
            response_body,
            response_status,
        };
        let app: Router = Router::new().fallback(catch_all).with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        MockSidecar {
            addr,
            recorded,
            _handle: handle,
        }
    }

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    fn transport_with_resolver(
        resolver: Arc<dyn RpcEndpointResolver>,
        self_number: Option<String>,
        current_chat_id: Option<String>,
    ) -> SignalCliTransport {
        SignalCliTransport::new(resolver, fresh_db(), self_number, current_chat_id)
    }

    #[tokio::test]
    async fn send_posts_canonical_v2_send_body_and_returns_timestamp() {
        let mock = spawn_mock(201, serde_json::json!({ "timestamp": 1700000000_i64 })).await;
        let resolver = Arc::new(StaticEndpointResolver(format!("http://{}", mock.addr)));
        let t = transport_with_resolver(resolver, Some("+15551234567".into()), None);
        let id = t
            .send(SIGNAL_CHANNEL, "+15559998888", "hello")
            .await
            .expect("send must succeed");
        assert_eq!(id, "1700000000");
        let recorded = mock.recorded.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        let r = &recorded[0];
        assert_eq!(r.path, "/v2/send");
        assert_eq!(r.body["number"], "+15551234567");
        assert_eq!(r.body["recipients"], serde_json::json!(["+15559998888"]));
        assert_eq!(r.body["message"], "hello");
    }

    #[tokio::test]
    async fn send_returns_storage_err_when_sidecar_not_running() {
        struct VanishedResolver;
        #[async_trait]
        impl RpcEndpointResolver for VanishedResolver {
            async fn rpc_base_url(&self, _: &str) -> Option<String> {
                None
            }
        }
        let t = transport_with_resolver(
            Arc::new(VanishedResolver),
            Some("+15551234567".into()),
            None,
        );
        let err = t
            .send(SIGNAL_CHANNEL, "+15559998888", "hello")
            .await
            .unwrap_err();
        match err {
            ApiError::Storage(msg) => assert!(msg.contains("signal-cli sidecar"), "got: {msg}"),
            other => panic!("expected Storage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_returns_validation_err_when_no_account_paired() {
        // No env override AND no account on disk → /v1/accounts
        // returns []. The transport must surface a clear "scan a
        // QR" message rather than dispatching against an empty
        // self-number (which would 400 from signal-cli with a
        // confusing payload).
        let mock = spawn_mock(200, serde_json::json!([])).await;
        let resolver = Arc::new(StaticEndpointResolver(format!("http://{}", mock.addr)));
        let t = transport_with_resolver(resolver, None, None);
        let err = t
            .send(SIGNAL_CHANNEL, "+15559998888", "hello")
            .await
            .unwrap_err();
        match err {
            ApiError::Validation(msg) => {
                assert!(
                    msg.contains("no Signal account is paired"),
                    "got: {msg}"
                );
            }
            other => panic!("expected Validation, got {other:?}"),
        }
        // /v1/accounts hit, /v2/send NOT hit.
        let paths: Vec<String> = mock
            .recorded
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.path.clone())
            .collect();
        assert!(paths.iter().any(|p| p == "/v1/accounts"));
        assert!(!paths.iter().any(|p| p == "/v2/send"));
    }

    #[tokio::test]
    async fn send_falls_back_to_paired_account_when_env_override_unset() {
        // Real production path post-pairing: env var unset, but
        // /v1/accounts reports the operator's number after they
        // scanned a QR. The transport must dispatch using THAT
        // number instead of erroring out.
        use axum::{
            Json, Router,
            extract::Request,
            http::StatusCode,
            response::IntoResponse,
            routing::any,
        };
        use std::sync::Mutex;
        let recorded: Arc<Mutex<Vec<MockRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let captured = recorded.clone();
        let app: Router = Router::new().fallback(any(
            move |req: Request| {
                let captured = captured.clone();
                async move {
                    let path = req.uri().path().to_owned();
                    let body_bytes = axum::body::to_bytes(req.into_body(), 65_536)
                        .await
                        .unwrap_or_default();
                    let body_v: serde_json::Value = if body_bytes.is_empty() {
                        serde_json::Value::Null
                    } else {
                        serde_json::from_slice(&body_bytes).unwrap_or(serde_json::Value::Null)
                    };
                    captured.lock().unwrap().push(MockRequest {
                        path: path.clone(),
                        body: body_v,
                    });
                    if path == "/v1/accounts" {
                        return (
                            StatusCode::OK,
                            Json(serde_json::json!(["+12369995414"])),
                        )
                            .into_response();
                    }
                    if path == "/v2/send" {
                        return (
                            StatusCode::CREATED,
                            Json(serde_json::json!({ "timestamp": 42_i64 })),
                        )
                            .into_response();
                    }
                    (StatusCode::NOT_FOUND, Json(serde_json::Value::Null)).into_response()
                }
            },
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _h = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let resolver = Arc::new(StaticEndpointResolver(format!("http://{addr}")));
        let t = transport_with_resolver(resolver, None, None);
        let id = t
            .send(SIGNAL_CHANNEL, "+15559998888", "hi")
            .await
            .expect("send should succeed using paired account");
        assert_eq!(id, "42");
        // /v2/send body must carry the discovered self-number.
        let recorded = recorded.lock().unwrap();
        let send_call = recorded
            .iter()
            .find(|r| r.path == "/v2/send")
            .expect("must POST /v2/send");
        assert_eq!(send_call.body["number"], "+12369995414");
    }

    #[tokio::test]
    async fn send_surfaces_5xx_as_storage_error() {
        let mock = spawn_mock(500, serde_json::json!({ "error": "boom" })).await;
        let resolver = Arc::new(StaticEndpointResolver(format!("http://{}", mock.addr)));
        let t = transport_with_resolver(resolver, Some("+15551234567".into()), None);
        let err = t
            .send(SIGNAL_CHANNEL, "+15559998888", "hello")
            .await
            .unwrap_err();
        match err {
            ApiError::Storage(msg) => {
                assert!(msg.contains("HTTP 500"), "got: {msg}");
                assert!(msg.contains("boom"), "body should be surfaced, got: {msg}");
            }
            other => panic!("expected Storage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_rejects_empty_recipient_and_message() {
        let resolver = Arc::new(StaticEndpointResolver("http://nowhere".into()));
        let t = transport_with_resolver(resolver, Some("+15551234567".into()), None);
        for (recipient, text) in [("", "hi"), ("+15551112222", "")] {
            let err = t.send(SIGNAL_CHANNEL, recipient, text).await.unwrap_err();
            assert!(matches!(err, ApiError::Validation(_)), "got {err:?}");
        }
    }

    #[tokio::test]
    async fn send_rejects_unknown_channel() {
        let resolver = Arc::new(StaticEndpointResolver("http://nowhere".into()));
        let t = transport_with_resolver(resolver, Some("+15551234567".into()), None);
        let err = t.send("whatsapp", "+15559998888", "hi").await.unwrap_err();
        assert!(matches!(err, ApiError::Validation(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn current_chat_id_returns_constructor_value_for_signal_channel() {
        let resolver = Arc::new(StaticEndpointResolver("http://nowhere".into()));
        let t = transport_with_resolver(
            resolver,
            Some("+15551234567".into()),
            Some("+15553334444".into()),
        );
        assert_eq!(
            t.current_chat_id(SIGNAL_CHANNEL).await.unwrap(),
            Some("+15553334444".into())
        );
        // Other channels return None even when the field is set.
        assert_eq!(t.current_chat_id("whatsapp").await.unwrap(), None);
    }

    #[tokio::test]
    async fn current_chat_id_is_none_when_turn_not_signal_triggered() {
        let resolver = Arc::new(StaticEndpointResolver("http://nowhere".into()));
        let t = transport_with_resolver(resolver, Some("+15551234567".into()), None);
        assert_eq!(t.current_chat_id(SIGNAL_CHANNEL).await.unwrap(), None);
    }

    #[tokio::test]
    async fn resolve_recipient_returns_foreign_id_when_binding_exists() {
        let resolver = Arc::new(StaticEndpointResolver("http://nowhere".into()));
        let db = fresh_db();
        let store = TransportBindingStore::new(&db);
        store
            .insert_binding(SIGNAL_CHANNEL, "+15553334444", "pg-test", false, 1700000000)
            .unwrap();
        let t = SignalCliTransport::new(resolver, db, Some("+15551234567".into()), None);
        let resolved = t
            .resolve_recipient(SIGNAL_CHANNEL, "+15553334444")
            .await
            .unwrap();
        assert_eq!(resolved, "+15553334444");
    }

    #[tokio::test]
    async fn resolve_recipient_returns_not_found_for_unknown_handle() {
        let resolver = Arc::new(StaticEndpointResolver("http://nowhere".into()));
        let t = transport_with_resolver(resolver, Some("+15551234567".into()), None);
        let err = t
            .resolve_recipient(SIGNAL_CHANNEL, "alice")
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::NotFound(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn resolve_recipient_rejects_other_channels() {
        let resolver = Arc::new(StaticEndpointResolver("http://nowhere".into()));
        let t = transport_with_resolver(resolver, Some("+15551234567".into()), None);
        let err = t
            .resolve_recipient("whatsapp", "+15559998888")
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::Validation(_)), "got {err:?}");
    }

    // `read_self_number_from_env` is a thin wrapper over
    // `std::env::var` + trim/filter. Testing it requires
    // `env::set_var` which is `unsafe` in Rust 2024 and the server
    // crate forbids unsafe — defer that coverage to an
    // integration-test crate when the boot wiring lands. The
    // pure-validation logic is exercised via `is_valid_e164` below.

    #[test]
    fn is_valid_e164_accepts_canonical_numbers() {
        for ok in [
            "+1",
            "+15551234567",
            "+999999999999999", // exactly 15 digits — boundary
            "+44207946000",
        ] {
            assert!(is_valid_e164(ok), "{ok} should be accepted");
        }
    }

    #[test]
    fn is_valid_e164_rejects_malformed_inputs() {
        for bad in [
            "",                  // empty
            "+",                 // plus only
            "15551234567",       // missing leading +
            "+1234567890123456", // 16 digits — over the cap
            "+1-555-123-4567",   // dashes
            "+1 555 123 4567",   // spaces
            "+1234567890\n",     // newline injection — the load-bearing test
            "+1234567890\r\n",   // CRLF injection
            "+15551234567\"",    // quote injection
            "+0xff",             // hex
            "+abc",              // letters
            "+١٢٣٤",             // arabic-indic digits — ASCII only
        ] {
            assert!(
                !is_valid_e164(bad),
                "{bad:?} should be rejected (would be unsafe in JSON body)"
            );
        }
    }

    #[test]
    fn truncate_for_log_keeps_short_strings_intact() {
        assert_eq!(truncate_for_log("hello", 240), "hello");
        assert_eq!(truncate_for_log("", 240), "");
    }

    #[test]
    fn truncate_for_log_caps_long_strings_with_ellipsis() {
        let big = "x".repeat(500);
        let out = truncate_for_log(&big, 240);
        assert_eq!(out.chars().count(), 241); // 240 + the ellipsis
        assert!(out.ends_with('…'));
    }

    #[test]
    fn truncate_for_log_handles_multibyte_chars_correctly() {
        // Naïve byte-slicing would split a UTF-8 sequence and panic.
        let s = "🦀".repeat(100); // 4 bytes per crab × 100 = 400 bytes
        let out = truncate_for_log(&s, 50);
        assert_eq!(out.chars().count(), 51);
        // First 50 must all be crabs (no broken surrogates).
        for c in out.chars().take(50) {
            assert_eq!(c, '🦀');
        }
    }

    #[tokio::test]
    async fn send_truncates_giant_error_body() {
        // Mock returns a multi-KB body; the resulting error message
        // must be capped — chat shouldn't paste server stacktraces.
        let big_body = serde_json::json!({ "error": "x".repeat(5_000) });
        let mock = spawn_mock(500, big_body).await;
        let resolver = Arc::new(StaticEndpointResolver(format!("http://{}", mock.addr)));
        let t = transport_with_resolver(resolver, Some("+15551234567".into()), None);
        let err = t
            .send(SIGNAL_CHANNEL, "+15559998888", "hi")
            .await
            .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.len() < 600,
            "truncated error must be much smaller than the 5KB body, got {} chars",
            msg.len()
        );
        assert!(msg.contains('…'), "must mark truncation");
    }

    // ---- Phase 5 group ops -------------------------------------

    #[tokio::test]
    async fn list_groups_decodes_canonical_response() {
        let body = serde_json::json!([
            {
                "id": "group-base64-aaa",
                "name": "Movie Club",
                "members": ["+15551110000", "+15552220000", "+15553330000"]
            },
            {
                "id": "group-base64-bbb",
                "name": "",
                "members": ["+15554440000"]
            }
        ]);
        let mock = spawn_mock(200, body).await;
        let resolver = Arc::new(StaticEndpointResolver(format!("http://{}", mock.addr)));
        let t = transport_with_resolver(resolver, Some("+15551234567".into()), None);
        let groups = t.list_groups(SIGNAL_CHANNEL).await.unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].id, "group-base64-aaa");
        assert_eq!(groups[0].name.as_deref(), Some("Movie Club"));
        assert_eq!(groups[0].member_count, 3);
        // Empty name normalises to None.
        assert_eq!(groups[1].name, None);
        assert_eq!(groups[1].member_count, 1);
    }

    #[tokio::test]
    async fn list_groups_skips_entries_without_id() {
        // A single mis-shaped row must NOT fail the whole list —
        // operators rely on `signal.list_groups` to inventory their
        // groups and one bad row in five would otherwise lock them
        // out of the rest.
        let body = serde_json::json!([
            { "id": "good-1", "name": "OK", "members": [] },
            { "name": "no-id", "members": [] },
            { "id": "good-2", "name": "Also OK", "members": [] }
        ]);
        let mock = spawn_mock(200, body).await;
        let resolver = Arc::new(StaticEndpointResolver(format!("http://{}", mock.addr)));
        let t = transport_with_resolver(resolver, Some("+15551234567".into()), None);
        let groups = t.list_groups(SIGNAL_CHANNEL).await.unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].id, "good-1");
        assert_eq!(groups[1].id, "good-2");
    }

    #[tokio::test]
    async fn create_group_posts_canonical_body_and_inserts_binding() {
        let body = serde_json::json!({ "id": "new-group-id-base64" });
        let mock = spawn_mock(201, body).await;
        let resolver = Arc::new(StaticEndpointResolver(format!("http://{}", mock.addr)));
        let db = fresh_db();
        let t = SignalCliTransport::new(resolver, db.clone(), Some("+15551234567".into()), None);
        let id = t
            .create_group(
                SIGNAL_CHANNEL,
                Some("Movie Club"),
                &["+15559998888".into(), "+15553334444".into()],
            )
            .await
            .unwrap();
        assert_eq!(id, "new-group-id-base64");
        // Binding registered so subsequent send_message can dial
        // the new group_id without waiting for inbound traffic.
        let binding_pg = TransportBindingStore::new(&db)
            .lookup_principal_group(SIGNAL_CHANNEL, "new-group-id-base64")
            .unwrap();
        assert!(
            binding_pg.is_some(),
            "create_group must insert a TransportBinding for the new group"
        );
        // Body shape pinned: `name` + `members` array.
        let recorded = mock.recorded.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].path, "/v1/groups/+15551234567");
        assert_eq!(recorded[0].body["name"], "Movie Club");
        assert_eq!(
            recorded[0].body["members"],
            serde_json::json!(["+15559998888", "+15553334444"])
        );
    }

    #[tokio::test]
    async fn create_group_rejects_empty_members() {
        let resolver = Arc::new(StaticEndpointResolver("http://nowhere".into()));
        let db = fresh_db();
        let t = SignalCliTransport::new(resolver, db, Some("+15551234567".into()), None);
        let err = t
            .create_group(SIGNAL_CHANNEL, Some("Empty"), &[])
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::Validation(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn add_group_members_posts_to_correct_path() {
        let mock = spawn_mock(200, serde_json::json!({})).await;
        let resolver = Arc::new(StaticEndpointResolver(format!("http://{}", mock.addr)));
        let t = transport_with_resolver(resolver, Some("+15551234567".into()), None);
        t.add_group_members(SIGNAL_CHANNEL, "g-id-base64", &["+15553334444".into()])
            .await
            .unwrap();
        let recorded = mock.recorded.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(
            recorded[0].path,
            "/v1/groups/+15551234567/g-id-base64/members"
        );
        assert_eq!(
            recorded[0].body["members"],
            serde_json::json!(["+15553334444"])
        );
    }

    #[tokio::test]
    async fn leave_group_posts_to_quit_endpoint() {
        let mock = spawn_mock(200, serde_json::json!({})).await;
        let resolver = Arc::new(StaticEndpointResolver(format!("http://{}", mock.addr)));
        let t = transport_with_resolver(resolver, Some("+15551234567".into()), None);
        t.leave_group(SIGNAL_CHANNEL, "g-id-base64").await.unwrap();
        let recorded = mock.recorded.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].path, "/v1/groups/+15551234567/g-id-base64/quit");
    }

    #[tokio::test]
    async fn group_ops_reject_unknown_channels() {
        let resolver = Arc::new(StaticEndpointResolver("http://nowhere".into()));
        let db = fresh_db();
        let t = SignalCliTransport::new(resolver, db, Some("+15551234567".into()), None);
        for channel in ["whatsapp", "matrix", "email"] {
            assert!(matches!(
                t.list_groups(channel).await.unwrap_err(),
                ApiError::Validation(_)
            ));
            assert!(matches!(
                t.create_group(channel, None, &["+15559998888".into()])
                    .await
                    .unwrap_err(),
                ApiError::Validation(_)
            ));
            assert!(matches!(
                t.add_group_members(channel, "g", &["+1".into()])
                    .await
                    .unwrap_err(),
                ApiError::Validation(_)
            ));
            assert!(matches!(
                t.leave_group(channel, "g").await.unwrap_err(),
                ApiError::Validation(_)
            ));
        }
    }

    #[tokio::test]
    async fn add_group_members_and_leave_reject_empty_group_id() {
        let resolver = Arc::new(StaticEndpointResolver("http://nowhere".into()));
        let db = fresh_db();
        let t = SignalCliTransport::new(resolver, db, Some("+15551234567".into()), None);
        assert!(matches!(
            t.add_group_members(SIGNAL_CHANNEL, "", &["+1".into()])
                .await
                .unwrap_err(),
            ApiError::Validation(_)
        ));
        assert!(matches!(
            t.leave_group(SIGNAL_CHANNEL, "").await.unwrap_err(),
            ApiError::Validation(_)
        ));
    }

    // ---- Phase 5 audit fix: group_id URL-path injection defence ----

    #[test]
    fn is_valid_signal_group_id_accepts_real_base64_shapes() {
        for ok in [
            "abc123",
            "ABC123abc==",
            "url-safe-base64_-=",
            "AbCdEf012345-_",
            "x", // single char — minimum
        ] {
            assert!(is_valid_signal_group_id(ok), "{ok} should be accepted");
        }
    }

    #[test]
    fn is_valid_signal_group_id_rejects_standard_base64_chars() {
        // Standard base64's `+/` are deliberately rejected — `/`
        // is the path-traversal vector. Signal-cli emits URL-safe
        // ids on the endpoints we touch.
        assert!(!is_valid_signal_group_id("abc+def"));
        assert!(!is_valid_signal_group_id("abc/def"));
    }

    #[test]
    fn is_valid_signal_group_id_rejects_path_injection_payloads() {
        let too_long = "x".repeat(257);
        for bad in [
            "",                // empty
            "../etc/passwd",   // dot-dot escape
            "..",              // dot-dot
            "/etc/passwd",     // absolute path
            "abc/../def",      // slash + dot-dot
            "abc?query=1",     // query-string
            "abc#fragment",    // fragment
            "abc def",         // whitespace
            "abc\nbad",        // newline
            "abc\rbad",        // carriage return
            "abc%2Fbad",       // percent — not in our alphabet
            "abc:bad",         // colon
            "abc\\bad",        // backslash
            "abc<script>",     // angle brackets
            too_long.as_str(), // length cap
        ] {
            assert!(
                !is_valid_signal_group_id(bad),
                "{bad:?} must be rejected (URL-path injection risk)"
            );
        }
    }

    #[tokio::test]
    async fn add_group_members_rejects_path_traversal_in_group_id() {
        // The mock should NOT receive any request — validation must
        // fire BEFORE we touch the network.
        let mock = spawn_mock(200, serde_json::json!({})).await;
        let resolver = Arc::new(StaticEndpointResolver(format!("http://{}", mock.addr)));
        let t = transport_with_resolver(resolver, Some("+15551234567".into()), None);
        for bad in ["../etc/passwd", "abc?bad=1", "abc/../def", "abc\nbad"] {
            let err = t
                .add_group_members(SIGNAL_CHANNEL, bad, &["+15559998888".into()])
                .await
                .unwrap_err();
            assert!(
                matches!(err, ApiError::Validation(_)),
                "{bad:?} → {err:?}, expected Validation"
            );
        }
        assert_eq!(
            mock.recorded.lock().unwrap().len(),
            0,
            "validation must fail-fast before any HTTP call"
        );
    }

    #[tokio::test]
    async fn leave_group_rejects_path_traversal_in_group_id() {
        let mock = spawn_mock(200, serde_json::json!({})).await;
        let resolver = Arc::new(StaticEndpointResolver(format!("http://{}", mock.addr)));
        let t = transport_with_resolver(resolver, Some("+15551234567".into()), None);
        for bad in ["../../../etc/passwd", "abc?delete=true", "abc#frag"] {
            let err = t.leave_group(SIGNAL_CHANNEL, bad).await.unwrap_err();
            assert!(matches!(err, ApiError::Validation(_)), "{bad:?} → {err:?}");
        }
        assert_eq!(mock.recorded.lock().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn create_group_rejects_malformed_id_returned_by_sidecar() {
        // Defence in depth: even if signal-cli misbehaves and
        // returns `"id": "../etc/passwd"`, the transport must
        // refuse to register it as a binding (since the id flows
        // into URL paths on every subsequent group op).
        let body = serde_json::json!({ "id": "../etc/passwd" });
        let mock = spawn_mock(201, body).await;
        let resolver = Arc::new(StaticEndpointResolver(format!("http://{}", mock.addr)));
        let db = fresh_db();
        let t = SignalCliTransport::new(resolver, db.clone(), Some("+15551234567".into()), None);
        let err = t
            .create_group(SIGNAL_CHANNEL, Some("Trojan"), &["+15559998888".into()])
            .await
            .unwrap_err();
        match err {
            ApiError::Storage(msg) => {
                assert!(
                    msg.contains("non-base64 id"),
                    "error must call out the bad id, got {msg}"
                );
            }
            other => panic!("expected Storage, got {other:?}"),
        }
        // No binding inserted — the transport refused the bad id.
        assert!(
            TransportBindingStore::new(&db)
                .lookup_principal_group(SIGNAL_CHANNEL, "../etc/passwd")
                .unwrap()
                .is_none(),
            "must not insert a binding for a malformed group_id"
        );
    }

    // ---- Phase 6: fetch_attachment ---------------------------------

    #[tokio::test]
    async fn fetch_attachment_returns_bytes_and_strips_charset_from_mime() {
        // Mock returns a fixed body with a Content-Type carrying a
        // charset suffix; the transport must strip the suffix.
        use axum::Router;
        use axum::http::StatusCode;
        use axum::response::IntoResponse;
        use axum::routing::get;

        async fn serve_blob() -> impl IntoResponse {
            (
                StatusCode::OK,
                [(
                    axum::http::header::CONTENT_TYPE,
                    "image/jpeg; charset=binary",
                )],
                vec![0xff, 0xd8, 0xff, 0xe0],
            )
        }
        let app: Router = Router::new().route("/v1/attachments/{id}", get(serve_blob));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let resolver = Arc::new(StaticEndpointResolver(format!("http://{addr}")));
        let t = transport_with_resolver(resolver, Some("+15551234567".into()), None);
        let got = t
            .fetch_attachment(SIGNAL_CHANNEL, "att-base64-id")
            .await
            .unwrap();
        assert_eq!(got.bytes, vec![0xff, 0xd8, 0xff, 0xe0]);
        assert_eq!(got.mime_type, "image/jpeg");
        assert!(got.filename.is_none());
    }

    #[tokio::test]
    async fn fetch_attachment_rejects_path_traversal_id() {
        let resolver = Arc::new(StaticEndpointResolver("http://nowhere".into()));
        let t = transport_with_resolver(resolver, Some("+15551234567".into()), None);
        for bad in [
            "",
            "../etc/passwd",
            "abc..def",
            "abc/def",
            "abc?id=1",
            "abc\nbad",
            "abc:bad",
        ] {
            let err = t.fetch_attachment(SIGNAL_CHANNEL, bad).await.unwrap_err();
            assert!(matches!(err, ApiError::Validation(_)), "{bad:?} → {err:?}");
        }
    }

    #[tokio::test]
    async fn fetch_attachment_rejects_unknown_channel() {
        let resolver = Arc::new(StaticEndpointResolver("http://nowhere".into()));
        let t = transport_with_resolver(resolver, Some("+15551234567".into()), None);
        let err = t.fetch_attachment("whatsapp", "abc123").await.unwrap_err();
        assert!(matches!(err, ApiError::Validation(_)));
    }

    #[tokio::test]
    async fn fetch_attachment_falls_back_to_octet_stream_on_missing_content_type() {
        use axum::Router;
        use axum::http::StatusCode;
        use axum::response::IntoResponse;
        use axum::routing::get;

        async fn serve_blob() -> impl IntoResponse {
            // No Content-Type header — axum will still set one,
            // so we override to empty.
            let mut resp = (StatusCode::OK, vec![1u8, 2, 3]).into_response();
            resp.headers_mut().remove(axum::http::header::CONTENT_TYPE);
            resp
        }
        let app: Router = Router::new().route("/v1/attachments/{id}", get(serve_blob));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let resolver = Arc::new(StaticEndpointResolver(format!("http://{addr}")));
        let t = transport_with_resolver(resolver, Some("+15551234567".into()), None);
        let got = t.fetch_attachment(SIGNAL_CHANNEL, "abc123").await.unwrap();
        assert_eq!(got.bytes, vec![1, 2, 3]);
        assert_eq!(got.mime_type, "application/octet-stream");
    }

    // ---- Phase 7: send_with_attachments ----------------------------

    fn seed_attachment(
        db: &Database,
        cid: &str,
        bytes: &[u8],
        mime: &str,
    ) -> (execlaw_core::ids::AttachmentId, std::path::PathBuf) {
        use execlaw_core::attachments::{AttachmentRow, AttachmentStore};
        use execlaw_core::ids::{AttachmentId, ConversationId};
        let aid = AttachmentId::new();
        let dir = std::env::temp_dir().join("execlaw-test-att");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{}.bin", aid.as_str()));
        std::fs::write(&path, bytes).unwrap();
        AttachmentStore::new(db)
            .insert(&AttachmentRow {
                id: aid.clone(),
                conversation_id: ConversationId::from(cid),
                mime_type: mime.into(),
                path: path.to_string_lossy().into_owned(),
                sha256: "deadbeef".into(),
                received_at: 0,
            })
            .unwrap();
        (aid, path)
    }

    #[tokio::test]
    async fn send_with_attachments_posts_base64_data_url() {
        // Mock that returns the canonical send response.
        let body = serde_json::json!({ "timestamp": 1700000000_i64 });
        let mock = spawn_mock(201, body).await;
        let resolver = Arc::new(StaticEndpointResolver(format!("http://{}", mock.addr)));
        let db = fresh_db();
        let cid = "conv-att-out";
        let (aid, _path) = seed_attachment(&db, cid, &[0xff, 0xd8, 0xff], "image/jpeg");
        let t = SignalCliTransport::new(resolver, db, Some("+15551234567".into()), None)
            .with_caller_conversation_id(execlaw_core::ids::ConversationId::from(cid));
        let id = t
            .send_with_attachments(
                SIGNAL_CHANNEL,
                "+15559998888",
                "look at this",
                &[aid.as_str().to_owned()],
            )
            .await
            .unwrap();
        assert_eq!(id, "1700000000");
        let recorded = mock.recorded.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        let r = &recorded[0];
        assert_eq!(r.path, "/v2/send");
        assert_eq!(r.body["number"], "+15551234567");
        assert_eq!(r.body["recipients"], serde_json::json!(["+15559998888"]));
        assert_eq!(r.body["message"], "look at this");
        let atts = r.body["base64_attachments"]
            .as_array()
            .expect("base64_attachments must be an array");
        assert_eq!(atts.len(), 1);
        let url = atts[0].as_str().unwrap();
        assert!(url.starts_with("data:image/jpeg;base64,"), "got {url}");
        // Decode + verify roundtrip.
        let b64 = url.split(',').nth(1).unwrap();
        use base64::Engine;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        assert_eq!(decoded, vec![0xff, 0xd8, 0xff]);
    }

    #[tokio::test]
    async fn send_with_attachments_rejects_cross_conversation_id() {
        // Audit-critical: an attachment_id from conversation A
        // cannot be sent on a turn rooted in conversation B. The
        // refusal returns NotFound (not NotAuthorized) so the
        // agent can't probe the attachment-id space.
        let mock = spawn_mock(201, serde_json::json!({ "timestamp": 0 })).await;
        let resolver = Arc::new(StaticEndpointResolver(format!("http://{}", mock.addr)));
        let db = fresh_db();
        // Seed an attachment owned by conv-A.
        let (aid, _) = seed_attachment(&db, "conv-A", b"secret", "text/plain");
        // Build a transport pinned to conv-B.
        let t = SignalCliTransport::new(resolver, db, Some("+15551234567".into()), None)
            .with_caller_conversation_id(execlaw_core::ids::ConversationId::from("conv-B"));
        let err = t
            .send_with_attachments(
                SIGNAL_CHANNEL,
                "+15559998888",
                "exfil attempt",
                &[aid.as_str().to_owned()],
            )
            .await
            .unwrap_err();
        match err {
            ApiError::NotFound(msg) => {
                assert!(msg.contains(aid.as_str()), "got {msg}");
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
        // Mock must NOT have been hit — the validation runs before
        // any outbound HTTP fires.
        assert_eq!(mock.recorded.lock().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn send_with_attachments_requires_caller_conversation_id() {
        let resolver = Arc::new(StaticEndpointResolver("http://nowhere".into()));
        let db = fresh_db();
        // No caller_conversation_id wired.
        let t = SignalCliTransport::new(resolver, db, Some("+15551234567".into()), None);
        let err = t
            .send_with_attachments(SIGNAL_CHANNEL, "+15559998888", "hi", &["abc".into()])
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::Validation(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn send_with_attachments_falls_through_to_send_when_list_empty() {
        let body = serde_json::json!({ "timestamp": 42_i64 });
        let mock = spawn_mock(201, body).await;
        let resolver = Arc::new(StaticEndpointResolver(format!("http://{}", mock.addr)));
        let db = fresh_db();
        let t = SignalCliTransport::new(resolver, db, Some("+15551234567".into()), None)
            .with_caller_conversation_id(execlaw_core::ids::ConversationId::from("conv-x"));
        let id = t
            .send_with_attachments(SIGNAL_CHANNEL, "+15559998888", "hi", &[])
            .await
            .unwrap();
        assert_eq!(id, "42");
        // The body should NOT include base64_attachments since we
        // fell through to the plain-text send path.
        let recorded = mock.recorded.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert!(
            recorded[0].body.get("base64_attachments").is_none(),
            "fall-through path must not set base64_attachments",
        );
    }

    #[tokio::test]
    async fn send_with_attachments_rejects_too_many_attachments() {
        let resolver = Arc::new(StaticEndpointResolver("http://nowhere".into()));
        let db = fresh_db();
        let t = SignalCliTransport::new(resolver, db, Some("+15551234567".into()), None)
            .with_caller_conversation_id(execlaw_core::ids::ConversationId::from("conv-x"));
        let ids: Vec<String> = (0..9).map(|i| format!("att-{i}")).collect();
        let err = t
            .send_with_attachments(SIGNAL_CHANNEL, "+15559998888", "hi", &ids)
            .await
            .unwrap_err();
        match err {
            ApiError::Validation(msg) => {
                assert!(msg.contains("max 8"), "got {msg}");
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_with_attachments_accepts_empty_text_when_attachments_present() {
        // Audit fix #1: `send()` rejects empty text universally,
        // but `send_with_attachments` accepts it (signal-cli-rest-
        // api's `message` field is optional when
        // `base64_attachments` is populated). Pin the asymmetry so
        // a future "harmonise empty-text rejection" refactor
        // doesn't accidentally break attachment-only sends.
        let body = serde_json::json!({ "timestamp": 7_i64 });
        let mock = spawn_mock(201, body).await;
        let resolver = Arc::new(StaticEndpointResolver(format!("http://{}", mock.addr)));
        let db = fresh_db();
        let cid = "conv-empty-text";
        let (aid, _path) = seed_attachment(&db, cid, &[1, 2, 3], "image/png");
        let t = SignalCliTransport::new(resolver, db, Some("+15551234567".into()), None)
            .with_caller_conversation_id(execlaw_core::ids::ConversationId::from(cid));
        let id = t
            .send_with_attachments(
                SIGNAL_CHANNEL,
                "+15559998888",
                "", // <— empty text + attachments is OK
                &[aid.as_str().to_owned()],
            )
            .await
            .unwrap();
        assert_eq!(id, "7");
        let recorded = mock.recorded.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].body["message"], "");
        assert!(recorded[0].body["base64_attachments"].is_array());
    }

    #[tokio::test]
    async fn send_with_attachments_rejects_missing_attachment_id() {
        let mock = spawn_mock(201, serde_json::json!({ "timestamp": 0 })).await;
        let resolver = Arc::new(StaticEndpointResolver(format!("http://{}", mock.addr)));
        let db = fresh_db();
        let t = SignalCliTransport::new(resolver, db, Some("+15551234567".into()), None)
            .with_caller_conversation_id(execlaw_core::ids::ConversationId::from("conv-x"));
        let err = t
            .send_with_attachments(
                SIGNAL_CHANNEL,
                "+15559998888",
                "hi",
                &["nonexistent-id".into()],
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::NotFound(_)));
        // No HTTP call fired.
        assert_eq!(mock.recorded.lock().unwrap().len(), 0);
    }
}
