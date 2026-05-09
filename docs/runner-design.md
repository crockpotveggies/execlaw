# Runner system design notes

What execlaw inherits from selfhosted-claw's `HotRunnerPool`, what it
fixes, and the optimisations the original author baked in that we
must not lose in the Rust port.

The selfhosted-claw runner code is the **most performance-tuned**
subsystem in the predecessor project. Every entry below was
hard-won, often through a production incident — preserve the
intent, even when the Rust idiom for implementing it differs.

## 1. Per-conversation isolation, automatic lifecycle

Each conversation (group) gets **its own runner**. Selfhosted-claw
proved this is the right unit of isolation — data from group A
cannot leak into group B's runner via shared memory, file
descriptors, or errant tool-call routing.

| Group | Lifecycle |
|---|---|
| **Controller direct** | Runner stays hot **indefinitely**. No idle reap. The operator's primary thread is the most latency-sensitive surface. |
| **All other groups** (cold contacts, KnownTrusted DMs, group chats) | Hot for **~10 min idle**, then reaped. Configurable via `HOT_RUNNER_POOL_IDLE_TTL_MS`. |
| **Voice sessions** | Stay hot for the duration of the call. Voice has different latency budgets — see §10. |

The operator **does not create runners**. The control plane spawns
on demand and reaps on idle. The Settings → Runners page is
view-only with a restart button (force-reap when one is stuck).

## 2. Hot pool with prewarmed minIdle

`HotRunnerPool` (selfhosted-claw `src/runner/common/hot-runner-pool.ts`):

- **`minIdle`** (default 1, env `HOT_RUNNER_POOL_MIN_IDLE`) — at least
  one runner is always warm, ready to accept the next conversation.
- **`maxSize`** (default 2) — cap on concurrent runners. Beyond this,
  requesters wait on a promise queue with a 2-minute watchdog.
- **Synchronous waiter wake-up** — when a runner returns to idle, the
  pool resolves the oldest waiter's promise immediately rather than
  letting the event loop poll. Comment: "avoids up-to-25ms polling
  jitter when the pool is saturated."
- **TTL eviction** — sessions idle for > `idleTtlMs` (default 5 min)
  are closed, except the floor `minIdle` always survives.

**Rust port intent:** mirror the same semantics in `container-manager`
crate. `RunnerPool` keeps a `min_idle` of warm containers; `max_size`
caps concurrency; `idle_ttl` evicts; the controller's runner is
exempt from the TTL.

## 3. Pre-warming — checksum-based recompile avoidance

Selfhosted-claw's runner Dockerfile + `entrypoint.sh` implement a
clever fast path for "code didn't change since the image was built":

```
# Dockerfile build step
RUN find src -type f \( -name "*.ts" -o -name "*.json" \) \
    -printf '%s %T@ %p\n' | sort | md5sum > /app/.src-checksum

# entrypoint.sh on container start
NEW_CHECKSUM=$(find /app/src -type f ... | sort | md5sum)
if [ "$NEW_CHECKSUM" = "$(cat /app/.src-checksum)" ] && [ -f /app/dist/index.js ]; then
    DIST_DIR=/app/dist          # use prebuilt
else
    npx tsc -p /app -d /tmp/dist  # recompile to /tmp
    DIST_DIR=/tmp/dist
fi
```

Comment in the entrypoint: `"~10–30x faster than md5sum'ing every
TS file"` — stat-based fingerprint (size + mtime + path) instead of
hashing file contents.

**Saves 1–3s of TypeScript compilation per cold start.** For
prewarmed containers it's the difference between sub-50ms and
~2-second handoff.

**Rust port intent:** less critical because Rust runners are AOT
compiled. The analogous optimisation is the **plugin install path**
where ZIP staging may need similar checksum-based skip-if-unchanged
logic for `.customized` plugin variants.

## 4. Selective bind-mounts (no recursive copies)

Two earlier attempts FAILED:

1. `bind-mount project root :ro + shadow .env via /dev/null`. **runc
   refuses to create a mount point inside an RO parent** — kernel
   constraint, not configurable.
2. `fs.cpSync(projectRoot, mountTarget)` on every spawn. **Blocks
   the Node event loop for hundreds of ms on a real project**;
   control plane stalls under load.

The shipped strategy mounts only the subpaths the runner actually
needs:
- `/workspace/group` (writable, per-group folder)
- `/workspace/state` (writable, per-group runtime state)
- `/workspace/ipc` (writable, per-group IPC dir)
- `/workspace/global` (RO, optional shared notes)
- `/workspace/controller-notes` (RO, only for non-controller groups, points at controller's folder)
- `/app/dist` from the image; only **opt-in** customisations re-mount
  via a `.customized` marker file (avoids legacy snapshot recompile
  for groups that haven't actually changed anything).

**Rust port intent:** carry the principle. The container-manager
must NOT recursively copy the workspace per spawn. Bind-mount only
what's needed; everything else stays in the image.

## 5. State externalization — runners are stateless against the log

This is the biggest *correction* over selfhosted-claw. Their runners
were **stateful mid-run**: a crash mid-turn dropped work and left
cursors out of sync.

execlaw fixes that:

- **Event log is the source of truth.** The runner reconstructs
  context from `state_events` on spawn (~10–50ms hydration).
- **No persistent state inside the container.** Memory, scratchpad,
  and turn outputs are committed via `commit_turn` to the event log
  before the runner declares the turn done.
- **Crash → respawn is safe.** A crashed runner gets its open
  `tool_use` paired with a synthesized cancellation `tool_result`;
  the next runner picks up exactly where the log left off, no double
  execution.

What still goes to disk (carried over from selfhosted-claw):

- **Per-group IPC directory** (`/data/ipc/{group_folder}/{input,messages,responses,tasks}`)
  — used for streaming output and inter-container signalling. **Each
  group's IPC is isolated** from every other group's; one group's
  compromise can't reach another's IPC.

**Rust port intent:** runner-local crate already gets this right;
TurnExecutor + commit_turn already form the transactional boundary.

## 6. Per-group queue serialization with global concurrency cap

`GroupQueue` (selfhosted-claw `src/group-queue.ts`) enforces:

- **Strict per-group order** — two messages for the same group never
  race through different runners. While a group is active, new
  messages set `pendingMessages = true`; on completion, `drainGroup`
  re-runs if more arrived.
- **Global concurrency cap** (`MAX_CONCURRENT_CONTAINERS`, default 5)
  — overflow groups join `waitingGroups[]`. On the next slot
  release, `drainWaiting()` schedules the next.
- **Task de-duplication by id** — preventing the scheduler from
  spawning two containers for the same task.
- **Idle-waiting state** — a running container can signal it's
  waiting for further IPC. Tasks that arrive during idle close
  stdin (write `_close` sentinel) to wake the loop without
  respawning.

- **Crash recovery: exponential backoff + cooldown** — 5s, 10s, 20s,
  40s, 80s, then 10-min cooldown. After cooldown, retry counter
  resets. Prevents a permanently-broken group from saturating Docker
  logs.

**Rust port intent:** the `WakeupScheduler` in `crates/outbox` plus
the conversation-resolver pinning logic already enforce per-group
ordering. The global concurrency cap + cooldown semantics need to
go into the runner-spawn path explicitly.

## 7. Streaming output via sentinel markers

Container output is wrapped in `---NANOCLAW_OUTPUT_START---` /
`---NANOCLAW_OUTPUT_END---`. Control plane parses incrementally as
chunks arrive, so:

- Results are delivered the moment they finish, not on container exit.
- Activity-based timeout reset: every parsed output marker resets
  the hard timeout. A long-running tool call that produces partial
  outputs is recognised as alive.
- **Stderr does NOT reset the timeout.** Comment: "SDK writes debug
  logs continuously. Don't let chatty debug noise keep a hung
  container alive forever."
- **"Crashed but already produced output" is not a failure.** A
  container that emits results and then dies is treated as "idle
  cleanup," not error. Avoids spurious error logs.

**Rust port intent:** today's runner is in-process and uses tokio
channels for streaming, which is simpler. When the container-manager
work lands and runners are real subprocesses, this sentinel-and-
activity-based timeout pattern is the model.

## 8. Resource hygiene

- `--rm` on every container spawn — Docker cleans up exited
  containers automatically.
- Background reaper (`reapStaleContainers`, every 5 min) — kills any
  container older than 45 min that the control plane lost track of
  (daemon crash, etc.).
- Output size capped at `CONTAINER_MAX_OUTPUT_SIZE` (10 MB). Stops
  accumulating after the cap; doesn't crash.
- Async log writes — never block the control plane on disk I/O.

**Rust port intent:** the container-manager crate should mirror this.
A periodic sweeper alongside `LogRetentionSweeper` /
`RefreshTokenSweeper` / `EphemeralSweeper` that reaps orphaned
containers.

## 9. Activity-based timeouts (NOT wall-clock)

Two timeouts in selfhosted-claw:

- `IDLE_TIMEOUT` — reset on every output marker. Generous default
  (30s) because tool calls can be slow.
- `HARD_TIMEOUT` — `max(configTimeout, IDLE_TIMEOUT + 30s)`. Even if
  the container is producing output, force-kill at this point. Catch
  for runaway tool loops.

The 30-second cushion before hard kill **lets graceful shutdown
complete** so any final output is captured before SIGKILL.

**Rust port intent:** `tokio::time::timeout` paired with a sentinel-
reset channel. Same two-tier semantic.

## 10. Voice runner — different from text

Voice has hard real-time constraints. The voice runner's tuning:

- **Session lifetime: minutes, not seconds.** Voice calls stay open;
  the runner accumulates transcript + tool-history in memory across
  many RPC turns within one call.
- **Streaming-first.** Text chunks and audio samples ship as soon as
  the LLM emits them. **Filler audio** (`filler_delay_ms = 150ms`)
  fills dead air while the model thinks. Without this, the caller
  hears uncomfortable silence.
- **Token budget per turn** (`MAX_TOKENS_INITIAL = 1200`). Comment
  in `live-runner.ts:265-281`: "production incidents where Qwen3's
  uncontrolled `<think>` blocks consumed the entire budget,
  producing silent turns. The ceiling is defense in depth."
- **History windowing.** Last 6 turns sent verbatim; up to 60 kept
  for background digest. Older context gets summarised so the
  prompt stays bounded.
- **Digest timeout** (`DIGEST_TIMEOUT_MS = 10s`). Background summary
  task is aborted if it can't finish in 10s, so a slow summariser
  never blocks the next turn.

**Rust port intent:** the `voice-pipeline` crate already has the
two-lane graph + endpointer + barge-in primitives. The remaining
work is wiring the runner-side analogues of:

- Filler audio while the model thinks (Phase 9 with real `service-kokoro`).
- Per-turn token ceiling in the runner's `TurnConfig`.
- History windowing in `runner-local::memory_tool` (already trust-class scoped).
- Digest task with hard timeout.

## 11. Other hard-won lessons (from comments)

- **Per-group customisation must be opt-in.** A `.customized` marker
  file in the group's runner-source dir tells the entrypoint to
  recompile. Without the marker, every group would force a recompile
  on first start because most groups have legacy snapshot copies of
  the default runner that aren't actually customised.

- **Bind-mount the controller's notes RO into non-controller groups,
  not the other way.** Non-controller groups can read controller
  preferences (contacts, names, etc.) but can never write to the
  controller's folder.

- **The same runner image serves both text and voice.** Voice is
  not a different binary; it's the same runner reading from the
  voice-pipeline graph instead of the chat WS. Image proliferation
  was rejected.

- **Per-group cooldown after retry exhaustion** — once a group has
  failed 5 times in a row, it's locked out for 10 minutes
  regardless of new messages. Comment: "prevents one broken group
  from saturating logs and starving healthy groups."

## 12. Summary — what to carry to the Rust port

1. **Per-conversation runner with auto lifecycle.** Operator never
   creates one.
2. **Controller's runner is always hot.** Other groups: 10-min idle
   reap.
3. **Hot pool with prewarmed `min_idle`** — at least one warm
   runner ready, capped at `max_size`, immediate waiter wake.
4. **Stateless against the event log.** Hydration on spawn, no
   persistent in-runner state.
5. **Selective bind-mounts** — never recursive copies, never the
   whole project root.
6. **Per-group IPC isolation** — each group's IPC dir is its own.
7. **Per-group serialization + global concurrency cap + cooldown**
   on persistent failure.
8. **Streaming output with sentinel markers + activity-based
   timeout reset.** Stderr does not count as activity.
9. **Background reaper** alongside the existing sweepers, killing
   orphaned containers older than ~45 min.
10. **Voice-specific tuning**: session-long lifetime, fillers, token
    ceiling per turn, history windowing with bounded digest.

These items belong to the `container-manager` and `runner-local`
crates. The Settings → Runners page is the operator surface for
**observing** them; it must not have create/delete affordances.
