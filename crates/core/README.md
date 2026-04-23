# execlaw-core

The durability heart of execlaw.

Owns:

- `state_events` append-only event log (§2.3 of MIGRATION_PLAN.md)
- Conversation FSM (`state_conversations`, phase transitions)
- Turn-as-transaction commit (§2.4) — enforces the `tool_use`/`tool_result` pairing
  invariant (§2.2 axiom #3)
- Work-queue leases
- SQLite connection pool, WAL mode, SQLCipher pragmas, migration runner
- Per-connection `foreign_keys = ON` enforcement

Intentionally has **zero** knowledge of:

- any transport (signal, email, voice) — those are plugins, see `transport-api`
- any inference backend — see `inference-api` + `runner-local`
- any cloud vendor SDK — never, per §0 axiom #1.
