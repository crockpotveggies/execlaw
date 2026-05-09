# execlaw-server

Axum HTTP + WebSocket server. Ships:

- `/api/health` — liveness.
- `/api/setup`, `/api/login`, `/api/token/refresh`, `/api/logout` — JWT
  (Ed25519) + admin-password auth per §7.1, §8.3.
- `/api/openapi.json`, `/api/asyncapi.json`, `/api/docs` — Swagger UI +
  AsyncAPI viewer bundle per §8.4.

WS `/api/stream` — the live event stream — is scheduled for Phase 1; the
event vocabulary is already documented in `spec/asyncapi.yaml`.
