# execlaw (CLI)

Operator-facing CLI. Phase 0 subcommands:

- `execlaw up` — thin wrapper over `docker compose up -d`.
- `execlaw down` — `docker compose down`.
- `execlaw doctor` — preflight checks (docker, data dir, SQLCipher, keyring).
- `execlaw db migrate` — apply pending migrations.
- `execlaw db status` — print applied migration count.
- `execlaw hw rescan` — Tier 1 sysfs hardware scan (JSON).
- `execlaw serve` — run the Axum server directly (dev).

Production deployment is **always the container** — `execlaw up` is how
operators start it. `execlaw serve` exists for local testing.
