# Bridge Supervisor — design memo

Status: **planned, not yet implemented**. Phase 1 (manifest + trust gate) is in tree; this memo captures the design we'll implement next so the parallel branches don't drift.

## Why a separate supervisor

execlaw already runs two long-lived supervisors:

- `backend_supervisor` — owns the inference-backend containers (vLLM, TTS, STT). Per-purpose ports, exponential-backoff restart, lifecycle stage tracked separately from container status.
- `runner_supervisor` — owns per-principal-group runner containers (the agent loop). Spawn-secret auth, idle reaping, WS attachment race recovery.

A **bridge supervisor** is the third member of that family. It owns plugin-managed sidecar containers that mediate inbound/outbound traffic with a third-party messaging system (Signal first; WhatsApp, Matrix, Telegram are the natural follow-ons). It is *not* the runner — runners are agent execution; bridges are transport adapters.

Putting bridges into either of the existing supervisors would muddle responsibilities:

- They aren't inference, so `backend_supervisor` is wrong.
- They serve all conversations, not one principal_group, so `runner_supervisor`'s per-group lifecycle is wrong.

Hence a third supervisor parallel to those two.

## What the supervisor owns

1. **Bridge container lifecycle** — spawn, healthcheck, restart, stop. Mirrors `backend_supervisor`'s tick + reconcile loop.
2. **Inbound message ingestion** — long-poll or websocket subscription against the bridge's local RPC, decode each event, route to the right `principal_group_id` (or trigger first-contact intake).
3. **Outbound dispatch RPC** — exposed as a `TransportApi` capability that plugin tools (`signal.send_message`, `signal.reply`) consume via `ToolCtx`.
4. **Identity + JID storage** — one row per `(principal_group_id, transport_id)` mapping conversations to their foreign-id (Signal JID, WhatsApp WAID, etc.). Reused on every outbound dispatch and on inbound routing.
5. **Healthcheck → alert fan-out** — when the bridge goes down, fingerprinted alert (`bridge.signal.unreachable`) using the existing alert subsystem.

## What it does NOT own

- The signal-cli image build itself — that's a `[[services]]` declaration in the plugin manifest, deployed via the same `ServiceController` abstraction `backend_supervisor` uses.
- Conversation creation — when an inbound message arrives from an unknown contact, the supervisor calls into the existing first-contact / silent-hold flow; it does not invent its own intake.
- Trust evaluation — that's `policy::trust`. The supervisor just stamps the inbound `principal_id` + sender_trust on the conversation event and lets the existing pipeline take over.

## Capability shape (`TransportApi`)

Add to `crates/core/src/tool.rs` alongside the other `Option<Arc<dyn FooApi>>` capabilities:

```rust
pub transport: Option<Arc<dyn TransportApi>>,
```

```rust
#[async_trait]
pub trait TransportApi: Send + Sync {
    /// Resolve a free-form recipient string (name, phone, JID) to a
    /// canonical foreign-id. Backed by the Contacts plugin first,
    /// then by the bridge's own contact list.
    async fn resolve_recipient(
        &self,
        transport_id: &str,
        free_form: &str,
    ) -> Result<String, TransportError>;

    /// Send to a foreign-id, returning the bridge's message id for
    /// idempotency / read-receipt correlation.
    async fn send(
        &self,
        transport_id: &str,
        recipient: &str,
        text: &str,
    ) -> Result<String, TransportError>;

    /// The current turn's inbound JID, if the turn was triggered by
    /// an inbound message on this transport. Mirror of
    /// selfhosted-claw's `ctx.chatJid` — the `signal.reply` tool
    /// reads it without taking a recipient arg.
    fn current_chat_jid(&self, transport_id: &str) -> Option<String>;
}
```

Capability declared per-plugin via a new `capabilities = ["transport:signal"]` field on `ToolDecl` so the `Option<Arc<dyn TransportApi>>` is only populated for tools that asked for it.

## Storage

New table:

```sql
CREATE TABLE state_transport_bindings (
    principal_group_id TEXT NOT NULL,
    transport_id       TEXT NOT NULL,
    foreign_id         TEXT NOT NULL,
    is_group           INTEGER NOT NULL DEFAULT 0,
    created_at         INTEGER NOT NULL,
    last_seen_at       INTEGER,
    PRIMARY KEY (principal_group_id, transport_id)
);
CREATE INDEX idx_transport_bindings_foreign
    ON state_transport_bindings(transport_id, foreign_id);
```

Inbound routing: `SELECT principal_group_id FROM state_transport_bindings WHERE transport_id = ? AND foreign_id = ?`. Miss → first-contact intake.

Outbound: lookup by `principal_group_id` to find the foreign-id, dispatch via the supervisor's RPC handle.

## Bridge RPC

Each bridge container exposes a localhost-only HTTP+WS endpoint. Schema we settle on:

- `POST /v1/send` — outbound, returns `{ message_id }`
- `GET /v1/inbound/stream` — long-lived WS the supervisor consumes
- `GET /v1/contacts/resolve?q=...` — resolver fallback
- `GET /healthz` — for the supervisor's healthcheck loop

Plugin manifest `[transport]` table gains `rpc_port` + `rpc_host` so the supervisor knows where to dial.

## Phasing

| Phase | Scope | Status |
|---|---|---|
| 1 | trust_floor manifest knob + dispatcher enforcement; signal humaniser entries; signal plugin manifest stub | **shipped** |
| 2 | `TransportApi` capability trait + `state_transport_bindings` table + supervisor skeleton (no real RPC yet, just container lifecycle) | next |
| 3 | signal-cli sidecar container declaration + bridge RPC client + outbound dispatch wired to `signal.send_message` / `signal.reply` | |
| 4 | Inbound stream consumer + first-contact intake → conversation creation → existing trust pipeline | |
| 5 | Group ops (`create_group`, `add_group_members`, `leave_group`, `list_groups`) | |
| 6 | Attachments (images, voice notes) | |

## Open questions for the next planning pass

1. Does the bridge supervisor share `ServiceController` with `backend_supervisor`, or get its own? (Lean toward sharing — same image-pull + lifecycle plumbing.)
2. Where do per-bridge secrets (signal-cli's account-data tarball) live? Likely the existing encrypted vault under `secret://signal/account.tar.gz`, mounted into the container.
3. Multi-account: one bridge container per Signal number, or one container handling many? selfhosted-claw was single-account — we should plan for multi but not implement it in Phase 3.
4. Do inbound messages from unknown senders get an automatic conversation, or land in an "approval queue"? Consistent with the existing `silent-hold` policy for cold contacts.
