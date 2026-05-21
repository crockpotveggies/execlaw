# Automations — event-driven architecture (M6)

This document is the design baseline for the M6 milestone of the
Automations feature. It supersedes the original architecture sketch
in `docs/automations.md` §6 for everything event-routing and
reply-related; that document remains canonical for the M1-M5
substrate (event bus, automation store, run history, canvas
editor).

## TL;DR

Every "thing that happens" in the system — operator typing into
the web SPA, a WhatsApp message arriving, a calendar webhook
firing, a scheduled routine running — now flows through the same
event bus. Operator-authored Automations and plugin-shipped
default flows route the event to its agent, side effects, and
(when applicable) reply target. The web UI's chat surface is
*one consumer* of this architecture, not a separate path.

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

### `EventEnvelope` (core)

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

Persistence: `state_bus_events.envelope_json` (migration 0012). Old
NULL rows decode to `EventEnvelope::system_internal()` so legacy
events keep flowing through matchers without surprise.

### `ReplyPayload` (core)

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

Streaming variant (`ReplyParts::Streaming`) is wired into the
model but the actual per-token delta surface lands in M6 slice 6b
— for slice 6 only the lifecycle events fire on the per-run
channel.

### Registry (core)

`state_registered_event_kinds` + `state_registered_reply_handlers`.
Plugins declare in `plugin.toml`:

```toml
[[bus_events]]
kind = "whatsapp.message.received"
description = "Inbound WhatsApp message (DM or group)."
expects_reply = true
default_origin_kind = "plugin_channel"

[[reply_handlers]]
name = "whatsapp"
description = "..."
supports_attachments = true
supports_markdown = true
max_attachment_size_bytes = 16777216
allowed_mime_prefixes = ["image/", "video/", "audio/", "application/pdf"]
```

Imported into the runtime registry at plugin install + hydrate
(`crates/plugin-host/src/host.rs::import_m6_registry`). Plugin
uninstall removes contributions via `EventRegistry::remove_by_plugin`.
Core seeds its own built-ins (web.prompt.submitted, routine.fired,
scheduled.wakeup, webhook.received + four built-in reply handlers)
at boot via `register_core_event_kinds`.

## Flow chart

```
            ┌────────────────────────────────────────────────────┐
            │  Sources of events (all use the same envelope)     │
            │                                                    │
            │   POST /api/web/prompt    plugin webhooks    routine
            │   (web SPA chat input)    (channel plugins)  schedules
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
                │  Channel default flow              │       │
                │  (source = 'plugin:<id>' or 'core',│       │
                │   editable = forks to operator)    │       │
                └─────────────────┬──────────────────┘       │
                                  │                          │
                                  ├──────────────────────────┘
                                  ▼
                ┌─────────────────────────────────────┐
                │  ExecutorContext { db, pool,        │
                │   plugin_host, flow_channel }       │
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

## ReplyRouter degradation matrix

Per-cell mapping for the most-common parts × transports. The router
implements one helper per cell in `degrade.rs`; the tier ladder in
`tiers.rs` composes them.

| Part            | web (ws) | WhatsApp | Signal | SMS | Email | Alert | Voice | ChatAppend |
|---|---|---|---|---|---|---|---|---|
| `Attachment`    | inline   | attach   | attach | URL+text | attach | URL in detail | "I sent a file" | inline |
| `Artifact`      | inline   | attach   | attach | URL+text | attach | URL in detail | "I sent a chart" | inline |
| `Chart` (spec)  | inline   | text caption (TODO raster) | text caption | URL to PNG | text caption | URL | spoken caption | inline |
| `Table`         | card     | text rows (≤10) | text rows (≤10) | "table: N rows" | HTML table | flattened | summarized | card |
| `Card`          | card     | flattened text | flattened text | flattened | HTML | k:v in detail | "title: …" | card |
| `ExternalFile`  | download chip | fetch→attach if ≤max | fetch→attach | URL+text | attach | URL in detail | "file: <name>" | download chip |

## Fallback tiers

When the resolved handler refuses delivery, the router descends:

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

Each tier failure carries the underlying error forward in
`RouteResult` so the flow trace records WHY a tier failed.

## Streaming

The `FlowChannelHub` is the per-run broadcast bus. Executor
publishes `FlowChannelEvent` frames keyed by `run_id`:

```rust
pub enum FlowChannelEvent {
    NodeStarted { run_id, node_id, node_kind },
    NodeFinished { run_id, node_id, output, ms, error },
    AgentTurnStarted { run_id, node_id },
    AgentTextDelta { run_id, node_id, index, text },     // slice 6b
    AgentToolCallDelta { run_id, node_id, ... },         // slice 6b
    AgentTurnFinished { run_id, node_id, exit_tool, args },
    ReplyRouted { run_id, node_id, outcome },
    RunFinished { run_id, outcome },
}
```

SPA subscribes via `GET /api/automations/flow-runs/{run_id}/events`
(SSE). The endpoint converts each `FlowChannelEvent` to a typed
`event:` frame with the variant tag as the event name + the JSON
payload as the data.

Per-token streaming integration with the inference layer is the
slice 6b follow-up. The Anthropic SDK already emits
`text_delta` frames; piping them into the sink + emitting
`AgentTextDelta` events is mechanical once we touch the invoker.

## What's done in slices 1-10

- ✅ Event envelope + reply payload data model (slice 1)
- ✅ Manifest schema extensions + registry storage (slice 2)
- ✅ Operator Inbox thread (`ConversationKind::OperatorHome`) (slice 3)
- ✅ ReplyRouter with capability-driven degradation + 4-tier
  fallback ladder + ChatAppendHome auto-mint (slice 4)
- ✅ `SendReply` node + validator gates + envelope threading
  through `execute_graph` (slice 5)
- ✅ `FlowChannelHub` broadcast bus + lifecycle events + SSE
  subscription endpoint (slice 6)
- ✅ `POST /api/web/prompt` entrypoint + default web flow seeded
  in shadow mode (slice 7)
- ✅ Plugin manifests declare `bus_events` + `reply_handlers` for
  whatsapp, signal, google-apps (gmail + calendar) (slice 8)
- ✅ Registry inspection API + SPA client (slice 9)

## What's deferred (with explicit follow-up notes in commits)

1. **Slice 6b — per-token AskAgent streaming.** The
   `AgentTextDelta` event variant is in the enum and the SSE
   endpoint will fan it out, but the inference invoker still
   returns a buffered final exit-tool call. Wiring requires
   touching `crates/runner-local::TurnExecutor`'s callback path.
2. **Slice 7b — actual chat-surface swap.** `POST /api/web/prompt`
   exists and publishes; the default flow runs in shadow mode
   (disabled). The existing `send_message` / `dispatch_external_turn`
   path is untouched. Operator-by-operator opt-in + dual-run
   comparison harness lands when we trust the shadow runs.
3. **Slice 7c — `web_socket_session` reply handler wiring.** The
   handler stub returns `Ok` so the tier ladder doesn't hang.
   Actual `UiEvent::*` push happens when slice 6b lands.
4. **Slice 7d — `chat_append` reply handler wiring.** Same shape
   as 7c — stub returns Ok, real append needs a new `EventKind`
   variant (`SystemMsg` or `AutomationMsg`).
5. **Slice 8b — plugin default flows ship as JSON.** Manifests
   declare `[[bus_events]]` + `[[reply_handlers]]` today; the
   `[[default_automations]]` section is parsed but not imported
   into `state_automations` yet. Lands alongside Phase 6 of the
   channel-migration plan.
6. **Slice 9b — SPA UI for registry inspector / default flows
   tab / live trace renderer.** API endpoints + TypeScript client
   exist; rendering them in `AutomationsPage.tsx` is the
   follow-up.
7. **Chart server-side rasterization.** Vega-Lite specs render
   inline on the web transport; raster fallback to PNG for
   poor-transport channels currently emits a text caption ("📊
   <caption> (rendered chart available in the web UI)"). Adding
   `vl-convert-rs` to the control plane container brings full
   server-side raster (~40 MB dep weight) — flagged as a separate
   architectural choice.
8. **Phase 5/6/7 of the migration plan** — the irreversible chat
   path replacement + per-channel migration + delete of
   `send_message` — remains a deliberate next-session task. M6's
   branch is fully reviewable + shippable as-is in shadow mode.

## Where to find the code

| Concern | Path |
|---|---|
| Event envelope types | `crates/core/src/event_envelope.rs` |
| Reply payload types | `crates/core/src/reply.rs` |
| Event registry storage | `crates/core/src/event_registry.rs` |
| Operator Inbox helper | `crates/core/src/operator_home.rs` |
| Migration | `crates/core/migrations/0012_event_envelope_and_registry.sql` |
| Manifest sections | `crates/plugin-sdk/src/manifest.rs` (BusEventDecl, ReplyHandlerDecl, DefaultAutomationDecl) |
| Manifest registry import | `crates/plugin-host/src/host.rs::import_m6_registry` |
| Reply router | `crates/server/src/reply_router/` |
| Flow channel hub | `crates/server/src/flow_channel.rs` |
| SendReply node executor | `crates/server/src/automation_runtime.rs::execute_send_reply` |
| Web prompt entrypoint | `crates/server/src/web_prompt.rs` |
| Chart theme presets | `crates/core/src/chart_themes/{execlaw_dark,execlaw_light}.json` |
| Registry inspection HTTP | `crates/server/src/automations_admin.rs` (list_registered_*) |

## How to verify in the morning

```sh
# Should show 1058+ server lib tests, 636+ core lib tests
cargo test --workspace --lib

# SPA: 479 tests
cd web && npx vitest run

# Boot the server
cargo run --bin execlaw -- serve

# Verify the registry seeded
curl http://localhost:3031/api/admin/automations/registered-events
curl http://localhost:3031/api/admin/automations/registered-reply-handlers
curl http://localhost:3031/api/admin/automations/default-flows

# Submit a web prompt (replace SESSION with anything for now)
curl -X POST http://localhost:3031/api/web/prompt \
  -H 'content-type: application/json' \
  -d '{"text": "hello", "session_id": "test-1"}'
```

The default web flow is disabled in shadow mode by default — see
the disabled toggle at `/automations` and flip it manually to
opt-in once you're ready to A/B against the existing chat path.
