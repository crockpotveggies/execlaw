# Automations (Flows)

> **Status (2026-05-22):** the M6 event-driven backend was torn out
> in favor of a pre-turn middleware design that hasn't been built
> yet. This doc describes **what remains today** — the SPA canvas
> + the storage schema + the dry-run path — and points at the
> deletion commits for context.

## What's live

* **`/automations` SPA page** — landing page with metrics, suggestions,
  list of flows. Source: `web/src/settings/AutomationsPage.tsx`.
* **Flow editor canvas** — drag-drop graph editor over the
  `AutomationDef` JSON shape. Sources:
  * `web/src/settings/AutomationCanvas.tsx`
  * `web/src/settings/AutomationNodePanel.tsx`
  * `web/src/settings/automation-nodes.tsx`
* **Admin API**: CRUD over `state_automations`, suggestions
  endpoints, registered-events introspection, test-run.
* **Executor (test-run only)** — `Filter / Transform / Branch /
  Terminal / Notify / CallPlugin` nodes execute against a
  synthesized `FlowEventInput`. `AskAgent` / `SendReply` remain in
  the schema for deserialization but the executor returns a clean
  "not supported in this build" error.

## What got ripped out (2026-05-22)

The M6 design was an event-driven pipeline: webhooks/routines/plugins
publish to a durable bus → matching flows fire → AskAgent calls the
LLM → SendReply routes through a 4-tier ReplyRouter.

In practice the SPA chat composer never went through this path
(`postMessage` still hits the legacy chat handler), the M6 AskAgent
abstraction didn't fit chat's multi-turn-with-tools shape, and the
infrastructure (~3,000 lines of bus + dispatcher + retention + reply
router + agent invoker + flow channel) wasn't load-bearing.

Removed:

| Module / file | Why |
|---|---|
| `crates/server/src/reply_router/` | Chat commits its own ModelTurn. |
| `crates/server/src/flow_channel.rs` | Chat streams via `UiEvent::ChatTokenDelta`. |
| `crates/server/src/web_prompt.rs` | `/api/web/prompt` was never called from the SPA. |
| `crates/server/src/automation_agent.rs` | AskAgent invoker — wrong abstraction for chat. |
| `crates/server/src/automation_bus.rs` | Durable bus dispatcher — overbuilt for the use case. |
| `crates/core/src/automation_bus.rs` | Bus storage + types. |
| `crates/core/src/bus_event_retention.rs` | Retention sweeper for the dead table. |
| `crates/server/src/automation_suggestions_sweeper.rs` | Bus-event sweeper. |
| Plugin `[[default_automations]]` importer + JSON files | Defaults are operator territory; ship empty. |

Kept untouched:

* `AutomationDef` wire format + validator (`crates/core/src/automations.rs`).
* `state_automations` table.
* `state_automation_suggestions` + `state_automation_muted_patterns`
  tables (the SuggestionStore CRUD survives as a passive store).
* Executor for the surviving node kinds.
* Entire SPA Flow editor.

## The next iteration: pre-turn middleware

The redesign treats Flows as **pre-turn middleware** instead of a
parallel turn driver:

1. Operator's prompt hits the existing chat handler.
2. Before the legacy turn runs, the middleware:
   * Loads matching flows (`trigger.kind = "chat.prompt"` or a
     more specific kind like `chat.prompt.whatsapp`).
   * Evaluates each flow's graph, accumulating mutations into a
     `TurnMutation` struct (add skills, narrow tools, inject
     memory, add attachments, set trust, rewrite prompt).
3. Legacy turn runs **as today**, with the mutated context.

New node kinds the redesign introduces:

| Node | Effect on the upcoming turn |
|---|---|
| `SetSkills` | Add skill names |
| `SetTools` | Narrow/expand `caller_caps` |
| `AddMemory` | Inject specific memory entries |
| `AddAttachment` | Append `attachment_ids` |
| `SetTrust` | Override `caller_trust` |
| `RewritePrompt` | Mutate user text via Rhai |

Side-effect nodes (`Notify`, `CallPlugin`) stay as they are; their
outputs land in the existing turn's `tool_use`/`tool_result` audit
trail.

The `AskAgent` and `SendReply` schema variants are reserved for now;
they may return repurposed or may be retired in favor of the
middleware shape.

## Reference commits

* Triage A/B/C (auto-heal default flow / spawn timeout / partial-turn persist).
* M6 rip-out 1 — `reply_router` + `flow_channel` + `web_prompt`.
* M6 rip-out 2 — `automation_agent` (AskAgent invoker).
* M6 rip-out 3 — `automation_bus` + retention + suggestions sweeper + plugin defaults.
