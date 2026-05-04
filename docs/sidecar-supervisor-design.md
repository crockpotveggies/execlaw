# Sidecar Supervisor — design memo

**Naming history:** originally drafted as the "Bridge supervisor" because every supervised companion container we had a use case for at the time bridged a transport (Signal-cli, WhatsApp, Matrix). Renamed 2026-05-04 — "sidecar" is the established container-pattern term and accommodates non-transport companions equally well: an OCR worker, an ffmpeg pool, a Whisper helper, anything a plugin author wants the control plane to keep running. The original "channel" field on `SidecarMeta` was bridge-era leakage; dropped same day. The supervisor's identity-key is now the parent service's `name`, which carries no transport-specific meaning.

## Why a separate supervisor

execlaw runs three long-lived supervisors:

- `backend_supervisor` — owns the inference-backend containers (vLLM, TTS, STT). Per-purpose ports, exponential-backoff restart, lifecycle stage tracked separately from container status.
- `runner_supervisor` — owns per-principal-group runner containers (the agent loop). Spawn-secret auth, idle reaping, WS attachment race recovery.
- `sidecar_supervisor` *(this)* — owns plugin-managed companion containers. The sidecars in tree today happen to all be transport bridges (signal-cli is the first), but the supervisor itself is generic — it just keeps containers alive and healthy.

Putting sidecars into either of the existing supervisors would muddle responsibilities:

- They aren't inference, so `backend_supervisor` is wrong.
- They serve all conversations, not one principal_group, so `runner_supervisor`'s per-group lifecycle is wrong.

## What the supervisor owns

1. **Sidecar container lifecycle** — spawn, healthcheck, restart, stop. Mirrors `backend_supervisor`'s tick + reconcile loop.
2. **Health / status surface** — `snapshot_status()` for the SPA's Sidecars page, `SidecarStatusChanged` events on the WS bus.
3. **Healthcheck → alert fan-out** — when a sidecar goes down, fingerprinted alert (`sidecar.<name>.unreachable`) using the existing alert subsystem. *(Phase 3 work.)*

For sidecars that happen to be transport bridges, additional consumer-side concerns live elsewhere:

- **Inbound message ingestion** — long-poll or websocket subscription against the sidecar's local RPC, decode each event, route to the right `principal_group_id` (or trigger first-contact intake). Lives in a per-bridge consumer module, not the supervisor.
- **Outbound dispatch RPC** — exposed as a `TransportApi` capability that plugin tools (`signal.send_message`, `signal.reply`) consume via `ToolCtx`. Implementation reads `state_transport_bindings` and dials the sidecar's RPC port that the supervisor publishes.

## What it does NOT own

- The signal-cli image build itself — that's a `[[services]]` declaration in the plugin manifest, deployed via the same `ServiceController` abstraction `backend_supervisor` uses.
- Conversation creation — when an inbound message arrives from an unknown contact, the bridge consumer calls into the existing first-contact / silent-hold flow; the supervisor doesn't even see it.
- Trust evaluation — that's `policy::trust`. The bridge consumer stamps the inbound `principal_id` + sender_trust on the conversation event and lets the existing pipeline take over.

## Manifest shape

```toml
[[services]]
name = "signal-cli"          # globally-unique sidecar identity
image = "asamuzak/signal-cli-rest-api:latest"
ports = ["8080:8080"]

[services.sidecar]            # presence ⇒ supervised
rpc_port = 8080               # container port the supervisor publishes
rpc_health_path = "/v1/about" # defaults to /healthz
```

Plain `[[services]]` entries (helper daemons that take no supervision) just omit the `[services.sidecar]` table.

`SidecarMeta` is intentionally tiny: `rpc_port` + `rpc_health_path`. **No `channel`, no `kind`, no transport-specific knobs.** The supervisor's job is generic container-lifecycle, so its declared shape is generic. Transport-specific concerns (which channel a bridge serves, which secrets to mount) belong to the bridge consumer / a future separate `[transport]` declaration, not to the sidecar meta.

## Capability shape (`TransportApi`)

(Used by transport-bridge plugins, not the supervisor itself.)

Added to `crates/core/src/tool.rs` alongside the other `Option<Arc<dyn FooApi>>` capabilities:

```rust
pub transport: Option<Arc<dyn TransportApi>>,
```

Notes vs the original sketch:

- `transport_id` arg renamed to `channel` everywhere (matches `state_principal_groups.channel`).
- `current_chat_jid` renamed to `current_chat_id` (JID is Signal-specific; the surface is generic) and made `async` so impls can swap to a DB-backed lookup without forcing `block_in_place`.
- All methods return `Result<_, ApiError>`; transport-specific failure modes collapse to existing variants.
- Capability is a single flat `Capability::Transport`; the per-call `channel` arg dispatches inside the impl.

## Storage

New table — see `crates/core/migrations/0032_transport_bindings.sql` for the live schema. Notes vs the original sketch:

- Column is `channel`, not `transport_id` — matches `state_principal_groups.channel`.
- PK is `(channel, foreign_id)` — inbound routing is the dominant hot path.
- No FK to `state_principal_groups` — the first-contact flow needs binding-first/group-second ordering.

Inbound routing: `lookup_principal_group(channel, foreign_id)`. Miss → first-contact intake. Outbound: `bindings_for_group(group_id, channel)`.

## Sidecar RPC convention

For sidecars that are also transport bridges, we settle on:

- `POST /v1/send` — outbound, returns `{ message_id }`
- `GET /v1/inbound/stream` — long-lived WS the bridge consumer subscribes to
- `GET /v1/contacts/resolve?q=...` — resolver fallback
- `GET /healthz` (default, overridable) — supervisor's healthcheck loop

For sidecars that aren't transports (an OCR worker, ffmpeg pool), the supervisor only cares about `<rpc_health_path>` — the rest of the RPC surface is whatever the consumer plugin and the sidecar agree on.

## Phase 2b shipped knobs

Constants the supervisor invented that the original memo didn't pin — back-ported here so future work has a single source of truth:

- `MAX_RESTART_ATTEMPTS = 5` — mirrors `backend_supervisor`
- `DEFAULT_TICK_INTERVAL = 5s` — same
- `SIDECAR_PORT_POOL_START / END = 8501..=8600` — 100-port window above `backend_supervisor`'s 8101+ range, bounded so we can't drift into the ephemeral range; exhaustion parks `CrashLooping` (operator-visible) rather than silently colliding
- container name format: `execlaw-sidecar-<plugin_id>-<service_name>` — mirrors `execlaw-backend-<purpose>`

## Phase 2b lessons-learned (applied audit fixes)

- **Port reuse on respawn.** First draft minted a fresh port on every respawn (RPC-fail restart, drift respawn, post-crash loop). Each leak was small but the supervisor's "URLs stay stable" promise was a lie. Fixed by storing `host_port: Option<u16>` on `SidecarSlot` and only minting on the very first spawn.
- **Idle-tick `restart_attempts` double-count.** First draft did `restart_count.max(slot.restart_attempts + 1)` on every tick observing `CrashLooping` — five idle ticks would burn the cap. Fix: adopt the controller's count verbatim; the controller is the source of truth for crash-loop counting.
- **Status-change event spam.** First draft published `SidecarStatusChanged` on every reconcile pass even when status was unchanged. Centralised via `transition_status(name, slot, new_status)` which dedups + publishes only on transitions.
- **Port allocator overflow.** `saturating_add(1)` would silently map every excess sidecar onto port 65535. Now `allocate_port` returns `Option<u16>` and the supervisor parks `CrashLooping` on exhaustion.
- **Dropped `SidecarMeta.channel`.** First draft required every supervised sidecar to carry a `channel` string. That was bridge-era leakage that didn't generalize: an ffmpeg pool isn't a "channel." The supervisor's identity-key is now the parent service's `name`, which carries no transport-specific meaning.

## Phase 2b known limitations (next-phase work)

- **Lock contention.** `slots: Mutex<HashMap>` is held across every `spawn`/`stop`/`inspect`/`health_check` await inside `reconcile_once`. A slow Docker call blocks `snapshot_status` and `reset_attempts`. Same pattern as `backend_supervisor` — both will be refactored together when Phase 3 adds the RPC client. Fix shape: snapshot desired+slot data under lock, drop, do controller calls, re-acquire to commit.
- **`Healthy → vanished` flap escape hatch.** When `inspect` returns `NotFound`/`Stopped` the supervisor drops the handle but does NOT bump `restart_attempts`. A sidecar crashed-and-removed by the operator should respawn freely; a sidecar that genuinely flaps every 30 seconds deserves an alert. Phase 3 alert routing surfaces the flap without depending on the cap.
- **No `LifecycleStage` analogue yet.** `backend_supervisor` has `Stage::DownloadingModel` etc. so a 5-minute "Starting" doesn't read as "stuck." Sidecars will hit the same UX problem when signal-cli takes minutes to register; lift the pattern in Phase 3.

## Phasing

| Phase | Scope | Status |
|---|---|---|
| 1 | trust_floor manifest knob + dispatcher enforcement; signal humaniser entries; signal plugin manifest stub | **shipped** |
| 2a | `TransportApi` capability trait + `state_transport_bindings` table + `TransportBindingStore` + criterion bench | **shipped** |
| 2b | Sidecar supervisor skeleton (container lifecycle reusing `ServiceController`, healthcheck/restart loop, no RPC yet) + Settings → Sidecars admin page | **shipped** |
| 3 | signal-cli sidecar container declaration + sidecar RPC client + outbound dispatch wired to `signal.send_message` / `signal.reply` | next |
| 4 | Inbound stream consumer + first-contact intake → conversation creation → existing trust pipeline | |
| 5 | Group ops (`create_group`, `add_group_members`, `leave_group`, `list_groups`) | |
| 6 | Attachments (images, voice notes) | |

## Open questions for the next planning pass

1. Does the sidecar supervisor share `ServiceController` with `backend_supervisor`, or get its own? (Lean toward sharing.)
2. Where do per-sidecar secrets (signal-cli's account-data tarball) live? Likely the existing encrypted vault under `secret://signal/account.tar.gz`, mounted into the container.
3. Multi-account: one sidecar container per Signal number, or one container handling many?
4. Do inbound messages from unknown senders get an automatic conversation, or land in an "approval queue"? Consistent with the existing `silent-hold` policy for cold contacts.
