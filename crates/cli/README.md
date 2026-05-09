# execlaw (CLI)

Operator-facing binary. Production deployment is **always bare-metal** —
the control plane is a native binary registered as a host service via
the `service-manager` crate (systemd / launchd / Windows SCM). There is
no `docker compose` deployment for the control plane.

## Subcommands

- `execlaw install` — first-run install: migrate DB → register service → start it.
- `execlaw service install` / `start` / `restart` / `stop` / `uninstall` / `status` — service lifecycle.
- `execlaw doctor` — preflight checks (DB, vault, OS keyring, optional Docker for sidecars/runner).
- `execlaw db migrate` — apply pending migrations directly (run by `install` automatically).
- `execlaw db status` — print applied migration count.
- `execlaw hw rescan` — sysfs / WMI / IOKit hardware scan (JSON).
- `execlaw serve` — run the Axum server in the foreground (dev / debug; the host-service path uses this internally).
- `execlaw replay <conversation_id> --at <seq>` — reconstruct the exact prompt history, capability set, policy decision, and committed events for one turn.
- `execlaw eval flag` / `eval list` — tag regression-target event ranges for the LLM-judge harness.
- `execlaw backup` / `restore` — snapshot the SQLCipher DB and atomically swap a snapshot back in.
- `execlaw backfill-events` / `resign-events` — Phase-7 hardening: HMAC-tag historical event rows under the current key.

`cargo bootstrap`, `cargo start`, `cargo stop`, `cargo restart`,
`cargo svc-status`, and `cargo doctor` are convenience aliases that
forward to the equivalent `execlaw …` invocations (see
`.cargo/config.toml`).

The default bind address is `127.0.0.1:3031`. Override per-run with
`--bind` or persistently via Settings → General in the SPA (which writes
to `config_general.bind_address`).
