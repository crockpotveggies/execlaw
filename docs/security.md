# execlaw — Security

This document describes:

1. The disclosure path for security issues.
2. The threat model — what we defend against, what we don't.
3. The cryptography in use.
4. Trust assumptions about plugins, operators, and contacts.
5. Known limitations and unresolved attack vectors.

It complements [`docs/architecture.md`](architecture.md), which covers
the structural design choices that flow from these security postures.

---

## 1. Reporting a vulnerability

> **Do not open a public GitHub issue for a security report.**

Email the maintainer at the address in the repository's `package`
metadata or top-level `Cargo.toml` `authors` field, with the subject
line prefixed `[execlaw security]`. If you don't get an
acknowledgement within 72 hours, escalate via a GitHub Security
Advisory — go to the repo's Security tab → Advisories → Report a
vulnerability.

What to include:

- Affected version (commit SHA from `git rev-parse HEAD` or the
  release tag).
- Reproduction steps with the smallest example that exhibits the
  issue.
- Your assessment of impact (information disclosure, RCE, privilege
  escalation, denial of service, …).
- Whether you're willing to be credited in the fix announcement, and
  if so, how to credit you.

What to expect:

- Initial acknowledgement within 72 hours.
- A maintainer-side severity assessment within one week.
- For confirmed Critical / High issues: a coordinated disclosure
  window of up to 90 days from acknowledgement, during which we'll
  patch on `foundation`, prepare an advisory, and coordinate
  release. For Medium and Low: we'll patch on `foundation` and ship
  in the next regular release.
- Public advisory + CVE assignment when the patch ships.

Please do not exploit the vulnerability against any execlaw install
you don't own, do not exfiltrate data, and do not pivot from the
vulnerability to other targets. Good-faith research within those
limits is welcome.

There is no bug bounty.

---

## 2. Threat model

execlaw is a **single-operator self-hosted agent platform**. The
threat model is shaped by that fact: there is one administrator, and
the host machine is assumed to be physically and digitally controlled
by that administrator. Multi-tenant abuse, server-side privilege
escalation between tenants, and "evil platform operator" attacks are
out of scope because there is no platform operator distinct from the
user.

### What we defend against

- **Inbound prompt injection** from contacts on bridged transports
  (Signal / WhatsApp / SMS / Slack / email). Untrusted text never
  reaches any tool-using model role unmodified — see
  [`docs/agent-model.md` §8](agent-model.md) (planner/executor split)
  and the spotlighting layer in `crates/policy/src/spotlighting.rs`.
- **First-contact attack vector** — a stranger messaging the agent
  for the first time cannot drive any model inference. The cold-
  contact escalation flow parks the conversation in
  `AwaitingTrustDecision` until the controller decides — see
  [`docs/architecture.md` §9.3](architecture.md).
- **Compromised conversation runner** — the per-conversation runner
  container holds an Ed25519-signed JWT scoped to exactly one
  `(conversation_id, turn_seq)`. A poisoned context cannot reach
  cross-conversation memory, cross-conversation state, or any tool
  outside the capability set the policy engine granted for that
  turn.
- **Tampered event log** — every `state_events` row carries an
  HMAC-SHA256 tag in a chain (`tag_n = HMAC(key, prev_tag ||
  payload)`). Replay verifies the chain; tampering surfaces as
  `DbError::TamperDetected`. The HMAC key lives in the SQLCipher-
  encrypted vault.
- **Encrypted state at rest** — production builds enable the
  `sqlcipher` Cargo feature; the SQLite database is encrypted with
  a key derived from a master key held in the OS keyring (Keychain
  on macOS, Credential Manager on Windows, Secret Service on Linux).
- **Forged approval responses** — every cold-contact / sensitive-
  tool approval emits a JWT-signed `approval_token`. The respond
  endpoint verifies the token's `jti` matches the approval id;
  guessing an approval id alone is not enough to forge a verdict.
- **Replay of outbound messages** — outbox rows carry a framework-
  minted idempotency key derived from `(conversation_id, turn_seq,
  tool_call_ordinal)`. The LLM cannot influence the key; transport
  plugins use it to dedup at delivery.
- **Cross-trust memory leakage** — `memory_entries` is keyed on
  `(scope, trust_class, key)`. A `KnownTrusted` caller cannot read
  `Controller`-scoped memory rows; the read-down cascade is
  enforced at the storage shim (`crates/core/src/tool_apis.rs`),
  not the tool layer.
- **Webhook spoofing** — `[[webhook_routes]]` are unauthenticated
  by design (third-party services don't carry execlaw JWTs), but
  every plugin handler validates a per-install secret via
  constant-time comparison against a vault-stored value. The
  WhatsApp plugin's `on_webhook_event` is the canonical pattern.
- **Cloud LLM exfiltration** — there is no cloud LLM code path.
  Inference is local-only against an OpenAI-compatible endpoint
  (vLLM / OpenArc / Whisper / Kokoro). Removing the rule is not a
  configuration option; it requires editing source.

### What we explicitly do NOT defend against

- **Compromised host machine.** If an attacker has root / Admin on
  the machine running execlaw, they can read the SQLCipher key from
  process memory, dump the OS keyring, or replace the binary. Host
  compromise is total compromise. We rely on the operator's host
  hygiene.
- **Malicious plugins.** Plugins are *trusted code* (see §4 below).
  An installed plugin can read the vault, mint outbound messages,
  scrape memory, and inject events. The control surface for malice
  is "don't install plugins from sources you don't trust" — there
  is no in-process sandbox for the script tier (Rhai runs in the
  same process as the host) and no privilege boundary for the
  subprocess tier (it runs as the same OS user as the control
  plane).
- **Cryptographic-quality protection of LLM outputs.** The model
  may emit anything in any conversation, including content that
  *appears* to be commands or assertions. Our defense is
  architectural (the LLM's output is data, not control flow), not
  cryptographic — we do not "verify" model output beyond
  schema-validating tool-call args.
- **Side-channel attacks against the LLM.** Timing, token-count,
  cache-residency side channels are not defended against. The
  threat model assumes the LLM is a black-box oracle that the
  attacker can query freely.
- **Universal prompt-injection prevention.** Per
  [`docs/agent-model.md` §8](agent-model.md), CaMeL-style
  containment closes the highest-impact vector cheaply but is not
  a complete defense. See §5 below.
- **DoS at the network edge.** A determined attacker can flood
  `/api/webhooks/...` with traffic; the host doesn't ship rate
  limiting at the HTTP layer. Operators who expose execlaw to the
  public internet should put a reverse proxy in front.

---

## 3. Cryptography in use

| Purpose | Algorithm | Where |
|---|---|---|
| At-rest DB encryption | SQLCipher (AES-256-CBC + PBKDF2-HMAC-SHA512 KDF) | `crates/core` with `sqlcipher` feature |
| Vault master-key storage | OS keyring + file fallback at `~/.execlaw/master.key` | `crates/vault/src/keyring_key.rs` |
| Admin password | Argon2id (default params) | `crates/server/src/routes.rs` (`verify_password`) |
| Event-log tamper-evidence | HMAC-SHA256 chain | `crates/core/src/event_hmac.rs` |
| Capability tokens (runner) | Ed25519 (EdDSA), short-lived JWTs | `crates/server/src/auth.rs` |
| Session tokens (SPA) | Ed25519 access JWT (15 min) + refresh JWT (7 d) | `crates/server/src/auth.rs` |
| Approval tokens (cold contact) | Ed25519 JWT with `jti = approval_id` | `crates/server/src/approvals.rs` |
| WebAuthn (second-factor) | Whatever the registered authenticator supports | `crates/server/src/webauthn.rs` |
| TLS to local inference | rustls (no system CA dep) | `crates/inference-api` |
| TLS to plugin endpoints | rustls | per-plugin via `reqwest` |

Keys are generated per-install. Rotation playbooks for the HMAC key
(`execlaw resign-events`) and the JWT signing key (replace the
keyring entry + restart) are described in `crates/cli/src/main.rs --help`
output; a polished operator doc lands with the 1.0 release.

There is no cloud HSM, no remote KMS, no key escrow. Keys are local;
the vault export bundle (`execlaw backup`) is encrypted with a
passphrase the operator chooses at backup time.

---

## 4. Trust assumptions

### The operator (Controller principal)

**Fully trusted.** All capabilities, all tool access, all memory
scopes. The Controller's identity is bound cryptographically via a
WebAuthn credential and / or an Argon2id-hashed password.

### Other principals (contacts on bridged transports)

Trust is assigned per the trust ladder — see
[`docs/architecture.md` §5.4](architecture.md):

```
Controller > Delegated > KnownTrusted > KnownLimited > UnknownPending > Blocked
```

The `Blocked` state is universal — it applies to strangers AND to
previously-trusted principals the controller has revoked.
`UnknownPending` is the cold-contact entry state; the agent does not
run inference until the controller decides.

### Plugins

**Trusted code** — see the explicit non-defense in §2 above. Three
implications:

1. **Don't install plugins from sources you don't audit.** The script
   tier (Rhai) runs in the host process; the subprocess tier runs as
   the same OS user. There is no sandbox.
2. **Plugin manifest validation is structural, not behavioural.**
   The host parses the TOML, registers declared hooks, validates the
   JSON Schema for tool args. It does not analyze plugin code for
   intent.
3. **Plugin updates are operator-approved**. The install API
   refuses to overwrite an installed plugin without
   `if_existing=upgrade`, and the operator must explicitly enable
   the new version.

### MCP servers

**Untrusted, but firewalled.** MCP tools dispatch through a separate
client (`crates/mcp-client/`) that does not pass the caller's trust
class or capability set to the MCP server. MCP servers can return
arbitrary content; that content flows through the same spotlighting +
planner/executor containment as any other untrusted input. MCP
servers cannot mint outbox rows directly — they can only return
tool-call results that the policy engine still gates.

---

## 5. Known limitations

These are real and documented, not hypothetical.

### Prompt injection at the model level

Per Meta's "Agents Rule of Two" (and DeepMind's CaMeL paper, and the
2025 "The Attacker Moves Second" red-team study), there is no model-
level defense against prompt injection that survives motivated
adversaries. execlaw's posture is **architectural containment**:

- The Rule of Two policy gate stops any single turn from combining
  more than two of {ingests-untrusted, accesses-sensitive,
  produces-external-effect}.
- Untrusted-content turns route through a planner/executor split —
  the role with tools never sees the injection; the role that sees
  the injection has no tools.
- Spotlighting wraps untrusted text in randomized delimiters.
- Sideband HITL ensures the controller is notified via a *different*
  transport than the one carrying the untrusted content.

These narrow the blast radius. They do not eliminate the vector. If
your threat model includes "skilled adversary deliberately targeting
the operator," assume they will succeed at reaching the model and
plan accordingly (e.g. don't grant `Controller` trust to a contact
whose phone is shared with someone hostile).

### Windows OS keyring drift

Windows Credential Manager has documented issues with credential
loss across user-profile touch events and session-token rotations.
`crates/vault/src/keyring_key.rs` implements a defensive fallback:
the keyring is treated as a cache, the on-disk
`~/.execlaw/master.key` file is the durable sink. The fallback has
not been validated in CI on Windows yet (no Windows CI runs at the
time of writing — pending merge of `.github/workflows/ci.yml`).

### Plugin sandboxing

There is none. See §4. The roadmap discusses a possible WASM-tier
plugin runtime that would offer real isolation; until then, the
trust model is "operator-curated set of audited plugin sources."

### CI absence (until first push of `.github/workflows/ci.yml`)

The four supported targets (Linux x86_64, macOS x86_64 + arm64,
Windows MSVC) are claimed in the README but until the GitHub Actions
matrix lands, none are continuously validated. Regressions on the
less-tested targets (notably Windows) are detectable only by
operator reports.

### `execlaw backup` is encrypted but not authenticated against the
operator's identity

The backup bundle uses a passphrase the operator picks at backup
time. There is no cryptographic binding between "the operator who
made this backup" and "the install that restores it." A leaked
backup file plus its passphrase fully discloses the install's state.
Treat backup files as you would treat the SQLite database itself.

### Webhook routes are public

`[[webhook_routes]]` mounts at `/api/webhooks/{plugin_id}{path}`
without HTTP-layer auth. The plugin handler must validate caller
identity, typically with `?token=<secret>` against a vault-stored
secret. **This is a per-plugin contract**; a plugin that doesn't
validate is a security bug in *that plugin*. The host cannot force
correct validation. Audit any plugin's webhook handler before
relying on it.

### Logs may contain sensitive data

`tracing` events are mirrored into `~/.execlaw/logs/*.jsonl` and the
`log_entries` SQLite table. Plugins occasionally include user-
content excerpts in log lines (the WhatsApp plugin's diagnostic logs
in the v0.1.x series did this during debugging). Plugin authors are
expected to redact, but enforcement is by convention. Treat the log
files as confidential.

---

## 6. Hardening checklist (operator)

If you're deploying execlaw on a machine that's network-reachable:

1. Bind the control plane to loopback only (`127.0.0.1:3031` is the
   default). Don't change to `0.0.0.0:...` without a reverse proxy
   in front — the Settings → General "bind every interface" path is
   for operators who already have TLS termination.
2. Put a reverse proxy (nginx / Caddy / Traefik) in front for TLS
   when exposing the SPA externally. WebAuthn requires HTTPS in any
   non-`localhost` setting.
3. Set up the OS service registration via `execlaw install` rather
   than running `execlaw serve` from a terminal — the service path
   restarts on crash, runs as the right user, and integrates with
   the OS log surface.
4. Lock down the `~/.execlaw/` directory to the service user only
   (`chmod 700 ~/.execlaw` on POSIX). It contains the SQLCipher
   database, the file-fallback master key, log files, and per-
   plugin sidecar volumes.
5. Audit installed plugins. Each one is trusted code in your
   process / your user account. The signal-cli, wuzapi, and similar
   sidecars are similarly trusted — pin to known-good image
   digests, not `:latest`, in production.
6. Rotate the HMAC key on a schedule; `execlaw resign-events`
   re-signs the historical event log under the new key.
7. Back up `~/.execlaw/execlaw.db` (and the file-fallback master
   key, if you're using it) on the same cadence as any other
   operator-critical state. `execlaw backup` produces an encrypted
   bundle suitable for offsite storage.
