# Automations — event-driven architecture (M6)

Branch: `crockpot/event-driven-arch`.

## TL;DR

Every "thing that happens" in the system — operator typing into the
web SPA, a WhatsApp message arriving, a calendar webhook firing, a
scheduled routine running — flows through the same event bus.
Operator-authored Automations and plugin-shipped default flows route
the event to its agent, side effects, and (when applicable) reply
target. Replies fan out per-transport through the `ReplyRouter`'s
capability-driven degradation matrix and a four-tier fallback ladder
that lands the work product in the operator Inbox on transport
failure.

Four moving pieces:

1. **`EventEnvelope`** — the metadata travelling with every bus
   event. Carries the reply target (`OriginRef`), sender identity,
   correlation id.
2. **`RegisteredEventKind` / `RegisteredReplyHandler`** — runtime
   registry populated from plugin manifests. Validator + UI consult
   it; reply handlers consult their capability matrix.
3. **`ReplyRouter`** — given an envelope + a `ReplyPayload`, pack
   parts per the destination transport's capabilities, walk the
   degradation tiers, fall back to the operator Inbox on failure.
4. **`FlowChannelHub`** — per-run broadcast hub. Executor publishes
   lifecycle events; SPA subscribes for live trace.

## Data model

### `EventEnvelope`

```rust
pub struct EventEnvelope {
    pub origin: OriginRef,
    pub identity: SenderIdentity,
    pub correlation_id: String,
    pub parent_event_id: Option<String>,
}

pub enum OriginRef {
    WebSocketSession { session_id: String },
    PluginChannel {
        plugin_id: String,
        channel_ref: serde_json::Value, // plugin-opaque
        expires_at: Option<i64>,
    },
    ChatAppend { conversation_id: String },
    Alert,
    None,
}

pub enum SenderIdentity {
    Principal { id: PrincipalId, trust: TrustClass },
    External { plugin_id: String, handle: String, trust: TrustClass },
    System,
}
```

Persistence: `state_bus_events.envelope_json` (migration 0012). Pre-
migration rows decode to `EventEnvelope::system_internal()`.

### `ReplyPayload`

```rust
pub struct ReplyPayload {
    pub text: String,           // always present, fallback body
    pub parts: Vec<ReplyPart>,  // structured rich content
    pub hints: ReplyHints,      // per-reply degradation overrides
}

pub enum ReplyPart {
    Attachment { attachment_id, caption },
    Artifact { artifact_id, caption },
    Chart { spec, theme, caption },        // Vega-Lite spec
    Table { columns, rows, caption },
    Card { title, fields },
    ExternalFile { url, filename, mime_type, size_bytes },
}
```

### Registry

`state_registered_event_kinds` + `state_registered_reply_handlers`.
Plugins declare in `plugin.toml`:

```toml
[[bus_events]]
kind = "whatsapp.message.received"
description = "..."
expects_reply = true
default_origin_kind = "plugin_channel"

[[reply_handlers]]
name = "whatsapp"
supports_attachments = true
supports_markdown = true
max_attachment_size_bytes = 16777216
allowed_mime_prefixes = ["image/", "video/", "audio/", "application/pdf"]

[[default_automations]]
name = "WhatsApp inbound default"
flow_path = "flows/default.json"
enabled = true
```

Imported into the runtime registry at plugin install + hydrate
(`crates/plugin-host/src/host.rs::import_m6_registry`). Plugin
uninstall removes contributions via `EventRegistry::remove_by_plugin`.

Core seeds its own built-ins (`web.prompt.submitted`, `routine.fired`,
`scheduled.wakeup`, `webhook.received` + four built-in reply handlers
`web_socket_session` / `chat_append` / `alert` / `drop`) at boot via
`register_core_event_kinds`.

## Flow chart

```
            ┌────────────────────────────────────────────────────┐
            │  Sources of events (all use the same envelope)     │
            │                                                    │
            │   POST /api/web/prompt    plugin webhooks    routine
            │   (flow-driven prompts)   (channel plugins)  schedules
            └────────────────────┬────────────────────────────────┘
                                 │
                                 ▼
            ┌────────────────────────────────────────┐
            │  AutomationBus (state_bus_events)      │
            │  fingerprint dedup, envelope persist   │
            └────────────────────┬───────────────────┘
                                 │
                                 ▼
                ┌─────────────────────────────────┐
                │  Matcher                        │
                │  1. operator-authored flow      │─── yes ──┐
                │     whose trigger matches?      │          │
                │  2. else: default flow (plugin- │          │
                │     or core-shipped)            │          │
                └─────────────────┬───────────────┘          │
                                  │ no                       │
                                  ▼                          │
                ┌────────────────────────────────────┐       │
                │  Plugin/core default flow          │       │
                │  (source = 'plugin:<id>' or 'core',│       │
                │   imported from manifest at        │       │
                │   install / hydrate)               │       │
                └─────────────────┬──────────────────┘       │
                                  │                          │
                                  ├──────────────────────────┘
                                  ▼
                ┌─────────────────────────────────────┐
                │  ExecutorContext { db, pool,        │
                │   plugin_host, flow_channel,        │
                │   events, event_log_hmac_key }      │
                │                                     │
                │   walks the graph, publishes        │
                │   FlowChannelEvent per node start/  │
                │   finish onto the bus keyed by      │
                │   run_id                            │
                └─────────────────┬───────────────────┘
                                  │
       ┌──────────────┬───────────┼──────────────┬───────────────┐
       ▼              ▼           ▼              ▼               ▼
   Filter/        AskAgent     Branch         Notify /        SendReply
   Transform     (block_on    (no-op,        CallPlugin       (resolves
   (Rhai sandbox  invoker)     routes via                    envelope.origin)
                              edge whens)                         │
                                                                   ▼
                                                  ┌────────────────────────────┐
                                                  │  ReplyRouter               │
                                                  │  - resolve OriginRef       │
                                                  │  - degrade per capability  │
                                                  │  - walk tier ladder        │
                                                  │  - on exhaustion: Inbox    │
                                                  │    + Warning alert         │
                                                  └────────────────────────────┘
```

## ReplyRouter

### Degradation matrix

| Part            | web (ws) | WhatsApp | Signal | SMS | Email | Alert | Voice | ChatAppend |
|---|---|---|---|---|---|---|---|---|
| `Attachment`    | inline   | attach   | attach | URL+text | attach | URL in detail | "I sent a file" | inline |
| `Artifact`      | inline   | attach   | attach | URL+text | attach | URL in detail | "I sent a chart" | inline |
| `Chart` (spec)  | inline   | text caption | text caption | URL to PNG | text caption | URL | spoken caption | inline |
| `Table`         | card     | text rows (≤10) | text rows (≤10) | "table: N rows" | HTML table | flattened | summarized | card |
| `Card`          | card     | flattened text | flattened text | flattened | HTML | k:v in detail | "title: …" | card |
| `ExternalFile`  | download chip | fetch→attach if ≤max | fetch→attach | URL+text | attach | URL in detail | "file: <name>" | download chip |

### Fallback tiers

1. **Tier 1 — Full** — every part packed per the degrade matrix.
2. **Tier 2 — Attachments-only** — drop tables/cards/charts, keep
   files.
3. **Tier 3 — Text + URLs** — drop attachments → signed URLs in
   text body.
4. **Tier 4 — Text-only** — bare text, no attachments.
5. **`FailureFallback` hint** — default `ChatAppendHome`: auto-mint
   operator Inbox (`ensure_operator_home`), append the full
   payload there with a banner explaining the original target
   failed, fire a Warning alert so the operator notices.

### Built-in reply handlers

- **`web_socket_session`** — placeholder; chat surface uses
  `ChatAppend` instead. Reserved for future per-session non-chat
  surfaces.
- **`chat_append`** — appends a `ModelTurn` event into the
  conversation's event log via `EventLog::commit_turn` (with HMAC
  chaining if a key is wired) AND broadcasts
  `UiEvent::ChatMessageOutbound` on the live event bus so SPA
  subscribers see the reply.
- **`alert`** — inserts a `Firing` row into `state_alerts` via
  `AlertStore::insert_firing` (Info severity by default, title
  truncated from `prepared.text`).
- **`drop`** — silent no-op.
- **plugin-channel handlers** — resolved by name in the
  registry; the router calls `plugin_host.call_tool("<name>.send_reply",
  { text, parts, origin })`.

## Streaming

The `FlowChannelHub` is the per-run broadcast bus. Executor
publishes `FlowChannelEvent` frames keyed by `run_id`:

```rust
pub enum FlowChannelEvent {
    NodeStarted { run_id, node_id, node_kind },
    NodeFinished { run_id, node_id, output, ms, error },
    AgentTurnStarted { run_id, node_id },
    AgentTextDelta { run_id, node_id, index, text },
    AgentToolCallDelta { run_id, node_id, ... },
    AgentTurnFinished { run_id, node_id, exit_tool, args },
    ReplyRouted { run_id, node_id, outcome },
    RunFinished { run_id, outcome },
}
```

SPA subscribes via `GET /api/automations/flow-runs/{run_id}/events`
(SSE). The endpoint converts each `FlowChannelEvent` to a typed
`event:` frame.

Per-token streaming integration with the inference layer is a
follow-up — the lifecycle events fire today; the per-token deltas
need `runner-local::TurnExecutor` to expose a callback path that
feeds the sink.

## What ships in this branch

- ✅ Event envelope + reply payload data model
- ✅ Manifest schema extensions (`[[bus_events]]`,
  `[[reply_handlers]]`, `[[default_automations]]`) + registry
  storage + install/hydrate/uninstall import
- ✅ Operator Inbox thread (`ConversationKind::OperatorHome`)
- ✅ ReplyRouter with capability-driven degradation + 4-tier
  fallback ladder + ChatAppendHome auto-mint
  - ✅ `chat_append` handler appends a ModelTurn into the event log
    + broadcasts `UiEvent::ChatMessageOutbound`
  - ✅ `alert` handler inserts via `AlertStore`
  - ✅ `drop` no-op
  - ✅ Plugin-channel dispatch via `plugin_host.call_tool`
- ✅ `SendReply` node + validator gates + envelope threading
  through `execute_graph`
- ✅ `FlowChannelHub` broadcast bus + lifecycle events + SSE
  subscription endpoint
- ✅ `POST /api/web/prompt` entrypoint + default web flow seeded
  enabled
- ✅ Plugin manifests for whatsapp, signal, google-apps declare
  bus_events, reply_handlers, AND default_automations (with JSON
  flow files shipped at `plugins/<id>/flows/`)
- ✅ Plugin install imports `[[default_automations]]` flow JSONs
  into `state_automations` (operator edits preserved on upgrade —
  if a row with the same name already exists we skip the upsert)
- ✅ Registry inspection API + SPA TypeScript client

## What's NOT in this branch (deliberate follow-ups)

These have explicit notes in their commits and don't block the
architecture from being usable:

1. **Per-token AskAgent streaming** — `AgentTextDelta` events fire
   at turn boundaries today; per-token deltas require touching
   `crates/runner-local::TurnExecutor`'s callback path. Mechanical
   when we touch the invoker.
2. **Full chat-surface migration** — the SPA composer still hits
   `POST /api/chats/:cid/messages` because the existing chat path
   has features (skill_names, incognito, prior_messages,
   timezone, group context, cold contact handling, voice
   integration) that haven't been reimplemented in the M6 flow
   path. The two paths *coexist* — the legacy one for the chat
   surface, the M6 one for automations and operator-authored
   flows. As each chat feature gets a flow-path equivalent, that
   feature migrates.
3. **Plugin Rhai bindings for `host_publish_event`** — channel
   plugins (whatsapp, signal) inbound paths still call
   `dispatch_external_turn` directly. The M6 path is ready for
   them; adding a `host_publish_event(kind, payload, envelope)`
   Rhai binding + each plugin's `main.rhai` migration is the
   plugin-side cutover sprint.
4. **`web_socket_session` reply handler** — placeholder no-op.
   Chat replies use `ChatAppend`. Reserved for future per-session
   non-chat surfaces (e.g., a flow-trace renderer that wants to
   push events to ONE SPA session).
5. **Chart server-side rasterization** — Vega specs render inline
   on the web transport. Adding `vl-convert-rs` (~40 MB Deno
   bundle) brings server-side raster for poor-transport channels;
   today they get a caption line + a URL pointing at the SPA's
   inline renderer.

## Where to find the code

| Concern | Path |
|---|---|
| Event envelope types | `crates/core/src/event_envelope.rs` |
| Reply payload types | `crates/core/src/reply.rs` |
| Event registry storage | `crates/core/src/event_registry.rs` |
| Operator Inbox helper | `crates/core/src/operator_home.rs` |
| Migration | `crates/core/migrations/0012_event_envelope_and_registry.sql` |
| Manifest sections | `crates/plugin-sdk/src/manifest.rs` |
| Manifest registry import | `crates/plugin-host/src/host.rs::import_m6_registry` |
| Reply router | `crates/server/src/reply_router/` |
| Flow channel hub | `crates/server/src/flow_channel.rs` |
| SendReply node executor | `crates/server/src/automation_runtime.rs::execute_send_reply` |
| Web prompt entrypoint | `crates/server/src/web_prompt.rs` |
| Chart theme presets | `crates/core/src/chart_themes/` |
| Registry inspection HTTP | `crates/server/src/automations_admin.rs` |
| Plugin default flows | `plugins/whatsapp/flows/default.json`, `plugins/signal/flows/default.json`, `plugins/google-apps/flows/calendar_briefing.json` |

## How to verify

```sh
cargo test --workspace --lib
cd web && npx vitest run

cargo run --bin execlaw -- serve

# Registry seeded?
curl http://localhost:3031/api/admin/automations/registered-events
curl http://localhost:3031/api/admin/automations/registered-reply-handlers
curl http://localhost:3031/api/admin/automations/default-flows

# Submit a web-prompt-driven flow run (replace with a real conv id)
curl -X POST http://localhost:3031/api/web/prompt \
  -H 'content-type: application/json' \
  -d '{"text": "hello", "conversation_id": "<your-conv-id>"}'

# Subscribe to the run's events
curl -N http://localhost:3031/api/automations/flow-runs/<run_id>/events
```
