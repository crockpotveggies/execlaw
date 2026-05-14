# execlaw — Plugin System

Reference for the plugin framework. This is what a contributor needs in order to extend execlaw without touching host crates.

Relationship to other docs:

- [`architecture.md`](architecture.md) — full system topology + the design principles (esp. #6 _"Plugins, not hardcoded built-ins"_) that this doc operationalises.
- [`sidecar-supervisor-design.md`](sidecar-supervisor-design.md) — deep dive on the supervised-container layer plugins compose against via `[[services]]`.
- [`MIGRATION_PLAN.md`](../MIGRATION_PLAN.md) — design rationale and trade-off discussion.

---

## 1. What a plugin is

> _"Every extension is a plugin."_ — Architecture principle #6.

A plugin is a ZIP bundle the operator uploads to a running execlaw control plane. It declares (via TOML manifest) a set of capabilities the host should mount: agent-callable tools, sidecar containers, admin/webhook HTTP routes, identity providers, OAuth client metadata, skills, alert sources, transport bindings, UI panels. The host registers everything the manifest declares atomically at install time and unwires it atomically at uninstall. **No host code change is required to add, upgrade, or remove a plugin** — that's the architectural contract.

A plugin's runtime behaviour is one of two tiers (`crates/plugin-sdk/src/manifest.rs:535-591`):

- **Script** — a Rhai source file (`main.rhai`) that runs in a per-plugin embedded interpreter inside the control-plane process. Most in-tree plugins (Signal, WhatsApp, Slack, Google Calendar, Google Places, …) use this tier.
- **Subprocess** — a native binary the host spawns and talks to over JSON-RPC stdio. Used when a plugin needs a language runtime the script tier can't provide (audio decoding, ONNX inference, native crypto). The reference example is `plugins/hello/`.

Both tiers expose the same capabilities to the agent. Tier choice is an implementation detail.

---

## 2. Plugins vs. MCP servers

execlaw supports two ways to add tools the agent can call: **plugins** (this doc) and **MCP servers** (`crates/mcp-client/src/lib.rs`, `config_mcp_servers` table). They look identical to the model — both surface as `{name, description, json_schema}` entries in the per-turn tool list — but the runtime contract is very different.

Pick **plugin** when any of these is true:

- The integration needs **OAuth** (plugins get tokens auto-injected as `params._oauth.<account_name>`; MCP has no OAuth machinery).
- The integration needs **trust-class gating** (plugins enforce `trust_floor` at the host before dispatch; MCP can't see the caller's trust class).
- The integration **must be supervised**: a long-running sidecar, a health-checked daemon, an inbound webhook listener. MCP servers are short-lived child processes spawned per call.
- The integration needs to **publish events** (inbound messages, identity resolutions, alerts). MCP only does request/response tool calls.
- The integration is **part of the product surface** (Signal/WhatsApp/Slack transports, the Google bridge, etc.). Plugins are the right home for first-party functionality because they ship as auditable bundles with manifest-declared capabilities and are uninstallable by the operator.
- You need a **UI panel** mounted in the SPA (`[[ui_panels]]`). MCP has none.

Pick **MCP server** when:

- You're consuming a **third-party MCP server** that already exists (e.g. a community Notion MCP, Linear MCP, GitHub MCP).
- You want **operator-controlled, ad-hoc tool surface expansion** without authoring a Rust/Rhai bundle. The operator drops a `command + args + env` row into `config_mcp_servers` and the tools light up.
- The integration is **stateless and one-shot** — call → response — with no auth, no sidecar, no inbound events.

The two are not mutually exclusive. Plugins can shell out to MCP servers internally (via `http_post` to a long-running MCP HTTP endpoint), and MCP servers can dispatch into plugin-installed tools through the host's call surface. In practice: **first-party = plugin, third-party = MCP**.

---

## 3. Manifest reference

Every plugin ships with `plugin.toml`. The full schema lives in `crates/plugin-sdk/src/manifest.rs`. Below is the operationally complete subset.

### `[plugin]` (required)

| field          | type   | required | semantic                                                                  |
| -------------- | ------ | -------- | ------------------------------------------------------------------------- |
| `id`           | string | yes      | Lowercase `[a-z0-9-_]+`. Globally unique. Becomes the URL slug.           |
| `name`         | string | yes      | Display name in the SPA.                                                  |
| `version`      | string | yes      | SemVer. Used by `if_existing=upgrade` install path.                       |
| `description`  | string | no       | One-paragraph blurb, shown in the plugin list.                            |
| `author`       | string | no       | Display only.                                                             |
| `homepage`     | url    | no       | Link in the plugin list.                                                  |
| `license`      | string | no       | SPDX identifier.                                                          |
| `core_version` | string | no       | Minimum execlaw core version this plugin requires.                        |

### `[runtime]` (required if the plugin runs code)

```toml
[runtime]
tier   = "script"        # or "subprocess"
source = "main.rhai"     # script tier only — relative to plugin root
# executable = "./bin"   # subprocess tier only
# args = []
# env = { TOKEN = "secret://my_secret" }
```

Subprocess tier resolves `secret://name` env values from the per-plugin vault at spawn time.

### `[[tools]]` (zero or more — the agent-callable surface)

```toml
[[tools]]
name        = "myplugin.do_thing"
description = "..."
schema      = "schemas/myplugin.do_thing.json"   # optional but strongly encouraged
latency     = "low"                               # "low" | "medium" | "high"
trust_floor = "Controller"                        # see §6
required_capabilities = ["tools.safe"]            # capability gate
host_internal = false                             # if true, registered but hidden from agent catalog
```

- `latency = "low"` is the only tier the **voice runner** exposes (sub-second-budget turns). `medium`/`high` are still callable from the chat runner.
- `trust_floor` rejects the call before dispatch when the principal's trust class ranks below the floor.
- `host_internal = true` registers the tool for host-side dispatch (e.g. the auto-bridge calling `whatsapp.send_message`, or `signal.set_typing` driven by the typing-indicator guard) without offering it to the model.
- `schema` is a path to a JSON Schema file inside the bundle. The host validates `args` against it _before_ invoking the plugin. Highly recommended — it doubles as model-facing documentation.

### `[transport]` (at most one — declares this plugin as a transport)

```toml
[transport]
transport_id        = "whatsapp"
supports_attachments = true
supports_groups      = true
icon                 = "whatsapp"   # Bootstrap-icons name
```

Marks the plugin as ownership-claimant for inbound messages on `transport_id`. The host's auto-bridge picks transports based on these declarations.

### `[identity_provider]` (at most one)

```toml
[identity_provider]
resolves              = ["email", "phone"]
trust_hint_default    = "Contact"             # Contact | Colleague | Family | Organization | Unknown
confidence_ceiling    = 0.95
```

Plugin must implement `fn identity_resolve(transport, handle, oauth)` — host calls this when an inbound from an unrecognised handle arrives.

### `[[services]]` and `[services.sidecar]` (zero or more)

See §4.

### `[[admin_routes]]` (authenticated, mounted at `/api/admin/plugins/{plugin_id}{path}`)

```toml
[[admin_routes]]
method      = "GET"
path        = "/status"
handler     = "admin_status"           # Rhai fn name
description = "Pairing + sidecar status."
```

Handler signature: `fn admin_status(args)` where `args = #{ method, path, query, body, headers }`. Return value is JSON-encoded to the client.

### `[[webhook_routes]]` (UNAUTHENTICATED, mounted at `/api/webhooks/{plugin_id}{path}`)

Same shape as admin routes. The HTTP layer skips auth — the handler **must** verify the caller, typically by matching a `?token=` query param against a vault-stored shared secret with constant-time comparison. See `crates/server/src/plugin_webhook_routes.rs` and the WhatsApp plugin for the pattern.

### `[[oauth_accounts]]`, `[[ui_panels]]`, `[[event_subscriptions]]`, `[[alert_sources]]`, `[[health_checks]]`, `[[skills]]`, `[[chat_components]]`

All optional; see `crates/plugin-sdk/src/manifest.rs` for full per-section schemas. The most commonly-used:

- `[[oauth_accounts]]` declares an OAuth client the operator pairs from the SPA. The access token is auto-injected into every tool call as `params._oauth.<account_name>` (refresh tokens and client secrets are never exposed to plugin code).
- `[[ui_panels]]` registers an operator-facing settings page. `mount = "admin/plugins/myplugin"` maps the panel to `/settings/plugins/myplugin` in the SPA; `entry = "ui/panel.js"` names the file inside the ZIP that the host serves at `GET /api/admin/plugins/{plugin_id}/ui/panel.js`. The SPA's `DynamicPluginPanel` fetches that JS, blob-URLs it, and `import()`s the module at runtime. **See section 11 for the full authoring walkthrough.** Install-time validation refuses any plugin whose `entry` file is missing from the staged ZIP.
- `[[skills]]` registers operator-authored skill markdown files into the host's `SkillStore` namespaced as `<plugin_id>/<skill_name>`.

---

## 4. Sidecar model

Plugins that need a long-running helper (a Go HTTP wrapper, a Java daemon, a database) declare `[[services]]` entries. Each entry with a `[services.sidecar]` block becomes a **supervised container** managed by `crates/server/src/sidecar_supervisor.rs`.

```toml
[[services]]
name  = "wuzapi"
image = "asternic/wuzapi:latest"

[services.env]
WUZAPI_ADMIN_TOKEN = "execlaw-wuzapi-admin"

[[services.mounts]]
source    = "state://data"        # persistent volume managed by the host
target    = "/app/dbdata"
read_only = false

[services.sidecar]
rpc_port        = 8080
rpc_health_path = "/"
```

**Volume sources:**

- `state://name` — persistent volume at `<execlaw_data>/sidecars/<plugin_id>/<sidecar_name>/<name>/`. Survives sidecar restart, plugin re-enable, host reboot. The supervisor creates the directory.
- `stage://relative/path` — read-only mount from inside the plugin's extracted ZIP. Use for shipping helper scripts or static configs (e.g. Signal's `enable-read-receipts.sh`).
- `/absolute/path` — passes straight through to Docker. Operator-managed.

**Supervisor lifecycle:**

- 5 s reconcile tick: compares desired state (registry's `all_sidecars()`) to running containers via `bollard` (Docker socket).
- Health probe: `GET http://127.0.0.1:<host_port><rpc_health_path>` every 5 s. Default `/healthz`.
- Crash-loop guard: 5 consecutive restart-without-Healthy events parks the sidecar in `CrashLooping`. An alert fires; operator must `kick` it via the SPA. Counter resets on a successful `Healthy` transition.
- Host port allocation: stable per `(plugin_id, sidecar_name)` — once assigned (e.g. 8501 for signal-cli, 8502 for wuzapi) the URL `sidecar_url("name")` returns is durable across restarts.

**How plugin code talks to a sidecar:**

```rhai
// In on_enable:
let base = sidecar_url_blocking("wuzapi", 60000);   // poll up to 60s for first spawn
if base == () { /* sidecar didn't come up */ return; }

// In tool calls / handlers:
let base = sidecar_url("wuzapi");                   // non-blocking; () if not Healthy
sidecar_http_post(base + "/chat/send/text", body, user_headers());
```

`sidecar_http_*` bindings are SSRF-aware: they reject loopback URLs unless the URL resolves to a registered supervised sidecar's host:port. Regular `http_post` rejects loopback in production.

---

## 5. Rhai primitives (script tier)

Public surface is registered in `crates/script/src/primitives.rs`. Grouped by purpose:

**HTTP** (general internet egress; loopback rejected in prod)
- `http_get(url, query_map, bearer)` / 4-arg variant `http_get(url, query_map, bearer, headers_map)`
- `http_post(url, body, bearer)` / `http_post(url, body, bearer, headers_map)`
- `http_patch(url, body, bearer)`
- `http_delete(url, query_map, bearer)`
- `http_get_cached(url, query_map, bearer, ttl_secs)` — per-plugin LRU keyed on sha256(url+query+bearer)

**Sidecar** (loopback to supervised containers only)
- `sidecar_url(name)` / `sidecar_url_blocking(name, timeout_ms)`
- `sidecar_http_get` / `sidecar_http_post` / `sidecar_http_delete`

**Vault** (per-plugin encrypted secret store)
- `vault_get(name) -> string | ()` / `vault_put(name, value)` / `vault_delete(name) -> bool`

**WebSocket**
- `ws_subscribe(url, on_frame, on_close)` / `ws_subscribe_bidi(url, on_frame, on_close)`
- `ws_send_to_active(msg)` — write to the bidi handle bound to this plugin

**Routing**
- `host_route_inbound(inbound_map)` — synchronous: blocks until host runs the agent + dispatch; returns outcome string. Use only from background tasks (WS handlers).
- `host_route_inbound_spawn(inbound_map)` — fire-and-forget. Use from webhook handlers where upstream HTTP timeouts matter (wuzapi's 30 s, etc.).

**JSON / data**
- `parse_json(s)` / `to_json_string(value)` / `json_path(value, path)` / `base64_encode` / `base64_decode`

**Time / parsing**
- `now() -> i64` (sec) / `now_ms() -> i64`
- `parse_rfc3339_ms(s) -> i64 | ()`
- `host_parse_int(s) -> i64 | ()` / `host_parse_float(s) -> f64 | ()`

**String helpers**
- `digits_only(s)` / `lower(s)` / `trim(s)` / `hash(s)` (sha256 → 16 hex chars)

**Logging**
- `log_info(msg)` / `log_warn(msg)` — emit to host logs with `plugin_id` tag

**Attachments + charts**
- `host_get_attachment_bytes(attachment_id) -> { data_url, mime_type, size_bytes }` — read inbound attachments AND plugin-rendered artifacts (one read path; both stores share the UUID namespace).
- `host_create_attachment(data_url_or_b64, mime_type, filename, ttl_seconds) -> { attachment_id, sha256, size_bytes }` — persist plugin bytes as a downloadable attachment. The returned id flows verbatim into `{transport}.send_with_attachments` and into the SPA's `/api/attachments/<id>` route. 10 MiB cap. `ttl_seconds = 0` means no TTL; positive values trigger the ephemeral sweeper. Accepts either a raw base64 payload or a `data:<mime>;base64,...` URL.
- `host_render_chart(spec_json, width, height, filename, ttl_seconds) -> { attachment_id, sha256, size_bytes, svg, png_data_url, width, height }` — render a declarative `ChartSpec` (defined in `crates/charting`) to both SVG (returned inline for SPA rendering) and PNG (stored as an artifact for transports that accept file uploads). `width = 0` or `height = 0` requests the renderer's defaults (720×400); other values are clamped to 240..2400.

The `ChartSpec` shape (`crates/charting/src/lib.rs`):

```jsonc
{
  "title": "Week-ahead temperature",
  "kind": "line",         // "line" | "bar" | "area" | "scatter"
  "x_label": "Hour",
  "y_label": "°C",
  "time_axis": false,     // when true, x-values are Unix-milliseconds
  "series": [
    {
      "name": "Temp",
      "points": [{"x": 0.0, "y": 12.0}, {"x": 1.0, "y": 14.0}],
      "color": [0, 114, 178]    // optional RGB triple; palette default otherwise
    }
  ],
  "band": {                 // optional — ensemble fan band
    "low":  [{"x": 0, "y": 10}],
    "high": [{"x": 0, "y": 18}]
  },
  "y_unit": "°C"           // optional suffix on y-axis tick labels
}
```

A typical plugin tool returns a result like:

```rhai
let chart = host_render_chart(to_json_string(spec), 720, 400, "forecast.png", 7 * 86400);
#{
    "chat_component_kind": "chart",
    "title": spec.title,
    "svg": chart.svg,
    "attachment_id": chart.attachment_id,
}
```

The SPA's chat-component registry (`web/src/chat/chatComponentRegistry.ts`) detects the `chat_component_kind` field and dispatches the payload to the matching renderer. Built-in kinds: `chart`, `weather_current`, `weather_daily`. Plugins ship additional renderers by adding files to `web/src/chat/components/` and registering them with `registerChatComponent(kind, component)` at module load.

For transport delivery (Discord/Signal/etc.), the agent forwards `chart.attachment_id` to `{transport}.send_with_attachments(channel, content, [attachment_id])`. The transport plugin's existing `host_get_attachment_bytes` path picks up the artifact transparently.

---

## 6. Tool dispatch + trust gating

Flow inside `PluginHost::call_tool` (`crates/plugin-host/src/host.rs`):

1. `HookRegistry::tool(name)` → `RegisteredTool` (plugin_id, schema, trust_floor, …).
2. **Capability gate** — caller must hold every entry in `required_capabilities`, unless they hold the wildcard `"*"` (controller).
3. **Trust-floor gate** — caller's `trust_level.class_tag()` ranked against `trust_floor`. Ranks (`crates/core/src/principal.rs`):
   - `Controller = 5`, `Delegated = 4`, `KnownTrusted = 3`, `KnownLimited = 2`, `UnknownPending = 1`, `Blocked = 0`.
   - Reject when caller_rank < floor_rank.
4. **JSON-Schema validation** — args validated against the tool's `schema` file (if declared) before dispatch.
5. **OAuth injection** — for each `[[oauth_accounts]]` declared by the owning plugin, lookup token in `state_oauth_tokens` and merge into `args._oauth.<account_name>`.
6. **Dispatch** — script tier calls the plugin's `tool_call(name, args, oauth)`; subprocess tier sends a JSON-RPC `tool_call` request over stdio.

The agent never sees `_oauth`, refresh tokens, or client secrets — that data is mounted into args inside the host, after gating.

---

## 7. Install / enable lifecycle

`crates/server/src/plugins.rs` + `crates/plugin-host/src/host.rs`.

```
ZIP upload                           if_existing=reject  → 409 already_installed
  │                                  if_existing=upgrade → uninstall old then install
  ▼
stage_zip()  →  <stage_root>/<plugin_id>-<version>/      manifest + source files
  │
  ▼
PluginHost::install / upgrade
  │
  ├─ parse + validate manifest
  ├─ (script) compile main.rhai to AST
  ├─ (subprocess) spawn child + open JSON-RPC channel
  ├─ HookRegistry::enable_with_stage()  ← atomic registration of EVERY hook
  ├─ persist to state_plugins (manifest_toml + stage_path + enabled=true)
  ├─ register sidecars with supervisor
  ├─ register skills with SkillStore
  └─ fire on_enable() (script tier)
```

`state_plugins` row layout:

| column          | type    | purpose                                                              |
| --------------- | ------- | -------------------------------------------------------------------- |
| `plugin_id`     | TEXT PK | manifest's `id`                                                      |
| `version`       | TEXT    | manifest's `version`                                                 |
| `manifest_toml` | TEXT    | full TOML, re-parsed on every server boot for deterministic re-hydrate |
| `stage_path`    | TEXT    | absolute path to extracted ZIP                                       |
| `enabled`       | INTEGER | soft on/off; flipped by `/enable` and `/disable` admin endpoints      |
| `installed_at`  | INTEGER | epoch seconds                                                        |
| `updated_at`    | INTEGER | epoch seconds                                                        |

**Enable**: load script / spawn subprocess, register hooks, register sidecars, fire `on_enable`.
**Disable**: unregister hooks, kill subprocess, close WS subs, deregister sidecars. Vault entries and OAuth tokens persist (so re-enable doesn't lose pairings).
**Uninstall** (`DELETE`): disable + drop the `state_plugins` row + remove the stage directory. Vault entries are also dropped.

The host re-hydrates every enabled plugin on boot by replaying `state_plugins` rows. Cargo-watch / OS reboots / deploys are transparent to plugin state.

---

## 8. Writing a plugin: script tier walkthrough

This is what most plugin authors will be doing. We'll build a hypothetical `weather` plugin that exposes a single `weather.lookup(city)` tool against a public HTTP API.

### 8.1 Skeleton

```
plugins/weather/
├── plugin.toml
├── main.rhai
└── schemas/
    └── weather.lookup.json
```

### 8.2 Manifest

```toml
# plugins/weather/plugin.toml
[plugin]
id          = "weather"
name        = "Weather"
version     = "0.1.0"
description = "Look up current conditions for a city via Open-Meteo."
author      = "your-name"
license     = "Apache-2.0"

[[tools]]
name        = "weather.lookup"
description = "Get current temperature + conditions for a city. Args: { city: string }."
schema      = "schemas/weather.lookup.json"
latency     = "medium"

[[admin_routes]]
method      = "GET"
path        = "/status"
handler     = "admin_status"
description = "Plugin health snapshot."

[[ui_panels]]
mount = "admin/plugins/weather"
entry = "ui/panel.js"

[runtime]
tier   = "script"
source = "main.rhai"
```

### 8.3 Tool dispatch script

```rhai
// plugins/weather/main.rhai

fn on_enable() {
    log_info("weather: enabled");
    ()
}

fn tool_call(name, args, oauth) {
    if name == "weather.lookup" { return weather_lookup(args); }
    throw "unknown tool '" + name + "'";
}

fn weather_lookup(args) {
    let city = args.city;
    if city == () || city == "" { throw "missing city"; }

    // 1. Geocode
    let geo = http_get(
        "https://geocoding-api.open-meteo.com/v1/search",
        #{ "name": city, "count": "1" },
        ()    // no bearer
    );
    if geo.results == () || geo.results.len() == 0 {
        return #{ "ok": false, "error": "no city match" };
    }
    let lat = geo.results[0].latitude;
    let lng = geo.results[0].longitude;

    // 2. Forecast
    let wx = http_get(
        "https://api.open-meteo.com/v1/forecast",
        #{ "latitude": lat.to_string(),
           "longitude": lng.to_string(),
           "current": "temperature_2m,weather_code" },
        ()
    );
    #{ "ok": true,
       "city": geo.results[0].name,
       "temperature_c": wx.current.temperature_2m,
       "weather_code": wx.current.weather_code }
}

fn admin_status(req) {
    #{ "ok": true, "service": "open-meteo", "configured": true }
}
```

### 8.4 JSON Schema (recommended)

```json
{
  "type": "object",
  "required": ["city"],
  "properties": {
    "city": { "type": "string", "minLength": 1 }
  },
  "additionalProperties": false
}
```

### 8.5 Build + install

```bash
# From plugins/weather/:
powershell -Command "Compress-Archive -Path main.rhai,plugin.toml,schemas \
  -DestinationPath ../../dist/weather-0.1.0.zip -Force"

# With a controller JWT in $JWT:
curl -X POST "http://127.0.0.1:3031/api/admin/plugins/install" \
  -H "Authorization: Bearer $JWT" \
  -F "file=@dist/weather-0.1.0.zip"

curl -X POST "http://127.0.0.1:3031/api/admin/plugins/weather/enable" \
  -H "Authorization: Bearer $JWT"
```

The tool will appear in the agent's catalog within one turn.

---

## 9. Writing a plugin with a sidecar

The shape, end to end, using a hypothetical Postgres-backed plugin:

```toml
# plugin.toml additions to the §8 skeleton
[[services]]
name  = "pg"
image = "postgres:16-alpine"

[services.env]
POSTGRES_PASSWORD = "secret://pg_password"   # vault-resolved at spawn
POSTGRES_DB       = "myplugin"

[[services.mounts]]
source    = "state://pgdata"
target    = "/var/lib/postgresql/data"
read_only = false

[services.sidecar]
rpc_port        = 5432
rpc_health_path = "/"   # postgres has no HTTP; supply a wrapper or use TcpProbe via [[health_checks]]
```

In `main.rhai`:

```rhai
fn on_enable() {
    let base = sidecar_url_blocking("pg", 60000);
    if base == () || base == "" {
        log_warn("pg sidecar didn't come up — Settings → Plugins to retry");
        return;
    }
    // Bootstrap schema, vault-store creds, etc.
    ()
}
```

For non-HTTP sidecars (databases, message queues), drop `[services.sidecar]` and add an explicit `[[health_checks]]` block with a TCP probe. The supervisor still owns lifecycle; only the auto-discovery via `sidecar_url` requires HTTP.

---

## 10. Writing a subprocess-tier plugin

Use this when the script tier can't do what you need (native libraries, ML inference, tight CPU loops). The reference example is `plugins/hello/`.

### 10.1 Manifest

```toml
[plugin]
id          = "plugin-hello"
name        = "Hello (reference plugin)"
version     = "0.1.0"
license     = "Apache-2.0"

[[tools]]
name        = "hello.echo"
description = "Echo the provided `message` field."
schema      = "schemas/hello.echo.json"
latency     = "low"
required_capabilities = ["tools.safe"]

[runtime]
tier       = "subprocess"
executable = "./hello-plugin"
args       = []
```

### 10.2 RPC contract

The host speaks JSON-RPC 2.0 over the child's stdin/stdout. The plugin must implement at minimum:

- `tool_call({tool_name, arguments, oauth})` → result value or RPC error.

OAuth tokens (when applicable) arrive under `oauth.<account_name>` exactly like the script tier. Ship the binary inside the ZIP; the host runs it from the stage path.

The script tier covers ~95 % of plugins in-tree. Subprocess is a real escape hatch — reach for it last.

---

## 11. Writing a UI panel

Every plugin that declares `[[ui_panels]]` ships a self-contained
React panel inside its ZIP. The host SPA loads it dynamically — the
plugin's TypeScript/JSX never bleeds into the host bundle. This is the
load-bearing containment invariant: **plugins are self-contained
end-to-end, backend AND frontend**.

### 11.1 What the host gives you

When the operator navigates to `/settings/plugins/<your-id>` the host:

1. Renders its own page chrome (back button, plugin id, version
   badge, **Danger Zone with Uninstall button**). Plugins **cannot
   override** the chrome — every plugin gets the same lifecycle UI
   so an operator can always reach Uninstall, even if your panel
   crashes.
2. Authenticated-fetches `GET /api/admin/plugins/<id>/ui/panel.js`
   from your staged ZIP and turns it into a Blob URL.
3. Dynamic `import()`s the Blob URL.
4. Calls your default export with one prop:

   ```ts
   interface PluginPanelProps {
       readonly identity: PluginIdentity;   // { id, displayName, version }
       readonly bridge: BridgeApi;          // see § 11.4
   }
   ```

5. Renders the returned React element inside a `PluginErrorBoundary`
   so a render-time throw from your panel can't take down the host's
   Danger Zone.

### 11.2 Skeleton

```tsx
// plugins/my-plugin/ui/panel.tsx
import type { PluginPanelComponent, PluginPanelProps } from "@execlaw/plugin-ui";

// React comes from the host bridge — DO NOT `import React from "react"`.
// You'd ship a duplicate React copy and trigger the "Invalid hook
// call" crash. The build's classic JSX transform compiles `<div>` to
// `React.createElement('div', ...)` against this module-scope const.
const React = globalThis.execlawHost!.React;
const { useCallback, useEffect, useState } = React;

const Panel: PluginPanelComponent = (props: PluginPanelProps) => {
    const { identity, bridge } = props;
    const { ErrorBanner, Button } = bridge.components;

    const [status, setStatus] = useState<string>("loading…");
    const [error, setError] = useState<string | null>(null);

    const refresh = useCallback(async () => {
        try {
            const r = await bridge.fetchJson<{ status: string }>(
                "GET",
                `/api/admin/plugins/${identity.id}/status`,
            );
            setStatus(r.status);
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
        }
    }, [bridge, identity.id]);

    useEffect(() => { void refresh(); }, [refresh]);

    return (
        <div data-testid={`${identity.id}-config`}>
            <ErrorBanner message={error} onDismiss={() => setError(null)} />
            <p>Status: <code>{status}</code></p>
            <Button onClick={refresh}>Refresh</Button>
        </div>
    );
};
export default Panel;
```

### 11.3 Building

The repo ships a shared build script that uses esbuild to compile
your TSX to a self-contained ES module:

```bash
# One-shot build:
node scripts/build-plugin-ui.mjs my-plugin

# Watch mode for hot-dev:
node scripts/build-plugin-ui.mjs my-plugin --watch

# Build every plugin in the tree:
node scripts/build-plugin-ui.mjs --all
```

Or via the npm scripts in `web/package.json`:

```bash
cd web
npm run build-plugin -- my-plugin
npm run build-all-plugins
```

The output lands at `plugins/my-plugin/ui/panel.js`. Externals are
configured so the build refuses to bundle `react`, `react-dom`, or
`@execlaw/plugin-ui` — if you see esbuild complain about an
unresolved external in your output, your panel source is importing
something it shouldn't. Audit and remove.

The shared TypeScript config at `plugins/_shared/tsconfig.plugin.json`
gives you strict-mode type checking + IDE autocomplete against the
bridge types. To wire it into your plugin, drop a tiny
`plugins/my-plugin/tsconfig.json` next to your panel:

```json
{
    "extends": "../_shared/tsconfig.plugin.json",
    "include": ["ui/**/*"]
}
```

### 11.4 The bridge API

Everything your panel can reach from the host is on `props.bridge`
(also available as `globalThis.execlawHost`, but use the prop —
testable, makes the dependency explicit). Full TypeScript contract
lives in `web/src/plugins/types.ts`:

| Field                            | What it is                                                                                                                                            |
| -------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------- |
| `bridge.React`                   | The host's React instance. Use this for hooks (`useState`, `useEffect`, `useCallback`, `useRef`, etc.).                                              |
| `bridge.ReactDOM`                | The host's ReactDOM (portals, `flushSync`).                                                                                                          |
| `bridge.getAccessToken()`        | Sync: returns the operator's JWT or `null` if signed out. Already threaded into `fetchJson`; only call directly if you need to construct an `<img src>` with the token in a query string. |
| `bridge.fetchJson(method, path, body?)` | Authenticated JSON helper. Throws on non-2xx with the response body in `.message`. The endpoint URL is whatever your plugin's `[[admin_routes]]` declared. |
| `bridge.usePoll(fetcher, intervalMs)` | Convenience hook: poll a fetcher on an interval while the panel is mounted. Returns `{ value, error }`.                                  |
| `bridge.components.ErrorBanner`  | Dismissable red banner. Props: `{ message, onDismiss, className? }`.                                                                                 |
| `bridge.components.SidecarStatusBlock` | Health chip for sidecar-backed transport plugins. See Signal/WhatsApp panels for use.                                                          |
| `bridge.components.Button`       | Bootstrap-styled button. Variants: `primary`, `secondary`, `danger`, `outline-primary`, `outline-secondary`, `outline-danger`. Sizes: `sm`, `lg`.       |

If you need a UI component the bridge doesn't expose
(`<Form>`, `<Spinner>`, `<Modal>`, etc.), **inline a plain HTML +
Bootstrap-classes equivalent in your panel**. Don't `import` from
`react-bootstrap` directly — esbuild's external rule will leave the
import in the output and the dynamic loader will fail with a clear
"cannot resolve module" error. Examples of inlined equivalents:

```tsx
{/* Spinner — replaces react-bootstrap's Spinner */}
<div className="spinner-border spinner-border-sm" role="status" />

{/* Form.Check switch — replaces react-bootstrap's <Form.Check type="switch">  */}
<div className="form-check form-switch">
    <input
        type="checkbox"
        role="switch"
        className="form-check-input"
        checked={value}
        onChange={(e) => setValue(e.target.checked)}
        id="my-toggle"
    />
    <label className="form-check-label" htmlFor="my-toggle">Enable X</label>
</div>
```

### 11.5 Hot-reload during dev

The dev loop for a plugin author:

1. `node scripts/build-plugin-ui.mjs my-plugin --watch` (terminal A —
   stays open, rebuilds on every save).
2. Navigate to `/settings/plugins/my-plugin` in the SPA, edit your
   `panel.tsx`, save.
3. Reinstall the plugin (drag-drop the rebuilt ZIP into the SPA's
   Install Plugin page, or repackage + use the Upgrade affordance)
   to refresh the staged copy under `~/.execlaw/plugins/`.
4. Navigate away + back to the settings page. The SPA's
   `DynamicPluginPanel` re-fetches `ui/panel.js` and imports the
   fresh module instance.

No execlaw restart. No SPA rebuild. Backend `main.rhai` changes
work the same way — repackage + reinstall picks them up live.

### 11.6 What the SPA host owns vs. what your panel owns

| Owned by host scaffold                                   | Owned by your panel.tsx                            |
| --------------------------------------------------------- | -------------------------------------------------- |
| URL routing (`/settings/plugins/<id>`)                    | Everything inside the panel body                  |
| Back button + plugin id/version/enabled badge in header   | Plugin-specific status displays                   |
| **Danger Zone with Uninstall button** (un-overridable)    | Plugin-specific actions (pair, disconnect, etc.)  |
| Error boundary catching render-time crashes               | Your own error-banner for API-call failures       |
| Loading state while `panel.js` is fetched                 | Loading state for your own API calls              |

If a behaviour you need isn't in this list, propose it on the
`BridgeApi` interface — adding helpers to the bridge is preferable
to side-stepping the contract.

### 11.7 Testing

Plugin panels are testable in isolation by providing a mock
`bridge` object that conforms to `BridgeApi`. There's no shared
Vitest config under `plugins/` yet — drop a `plugins/<id>/ui/__tests__/`
directory with your own `vitest.config.ts` if/when you want to
ship tests inside your plugin ZIP.

### 11.8 Common pitfalls

- **Forgetting the `const React = globalThis.execlawHost!.React`** at
  module top. JSX expands to `React.createElement(...)` which expects
  a `React` identifier in scope; without it, you'll see a runtime
  `ReferenceError`.
- **Importing React** to use `useState` etc. You'd ship duplicate
  React + cause "Invalid hook call" crashes. Always destructure
  from the bridge: `const { useState } = bridge.React;` or
  `const { useState } = React;` if you already have the module-scope const.
- **Importing from `web/src/...` or `../../web`**. Plugin code must be
  fully self-contained. Inline any helper you need or move it into
  `plugins/<id>/ui/` as a sibling file (esbuild will bundle siblings).
- **Forgetting to bump `plugin.toml` version after a panel change.**
  The host's plugin upgrade flow keys on the version string —
  same-version reinstalls take a different code path.

---

## 12. Reference plugins in tree

Browse these for working examples. Each lives under `plugins/<id>/`.

| plugin                         | tier       | sidecar       | OAuth        | webhook | identity provider | notes                                                                                       |
| ------------------------------ | ---------- | ------------- | ------------ | ------- | ----------------- | ------------------------------------------------------------------------------------------- |
| `signal`                       | script     | signal-cli    | —            | —       | —                 | Transport. WS-driven inbound. Pairing via QR. 9 tools.                                      |
| `whatsapp`                     | script     | wuzapi        | —            | yes     | —                 | Transport. Webhook-driven inbound. QR pairing.                                              |
| `slack`                        | script     | —             | yes          | yes     | —                 | Transport. OAuth + Slack Web API. Multi-workspace.                                          |
| `sms-socket`                   | script     | —             | —            | —       | —                 | Transport. Talks to an Android-side gateway over WebSocket.                                 |
| `google-calendar`              | script     | —             | yes          | —       | —                 | HTTP-only. 7 tools against Google Calendar v3.                                              |
| `google-contacts`              | script     | —             | yes          | —       | yes               | Identity provider that resolves email/phone via Google People API.                          |
| `google-places`                | script     | —             | API-key only | —       | —                 | Public Places (New) API. No OAuth — uses 4-arg `http_get`/`http_post` with custom headers.  |
| `pushover`                     | script     | —             | —            | —       | —                 | One-way outbound notification.                                                              |
| `identity-local-address-book` | subprocess | —             | —            | —       | yes               | Resolves identifiers from an operator-curated address book.                                 |
| `hello`                        | subprocess | —             | —            | —       | —                 | Reference subprocess plugin. Used by integration tests.                                     |

When you start a new plugin, the closest cognate is your fastest path to a working bundle:

- New transport with a Go/Java sidecar → copy `whatsapp/` (webhook flavour) or `signal/` (WS flavour).
- New HTTP-only OAuth integration → copy `google-calendar/`.
- New API-key HTTP integration → copy `google-places/`.
- New identity provider → copy `google-contacts/`.
- New subprocess plugin → copy `hello/`.

---

## 13. Common pitfalls

- **Module-level `const` is invisible inside `fn` bodies.** Rhai scopes constants to the file's top-level evaluation, not into function scopes. Inline literals at the call site or pass through args.
- **Webhook handlers must validate caller identity.** `[[webhook_routes]]` are unauthenticated. Always compare a `?token=…` URL param against a vault-stored secret with constant-time comparison. See WhatsApp's `on_webhook_event` for the canonical pattern.
- **Webhook handlers must return fast.** Third-party services (wuzapi @ 30 s, Slack Events @ 3 s) treat slow acks as failures and retry. If your handler routes to the agent, use `host_route_inbound_spawn` not `host_route_inbound`. Add idempotency on the upstream message ID — even one retry causes user-visible duplicates.
- **`sidecar_url(name)` returns `()` until the sidecar is `Healthy`.** Always handle Unit. In `on_enable`, prefer `sidecar_url_blocking(name, 60000)` so the plugin waits for first spawn.
- **OAuth refresh tokens are never visible to plugin code.** You get the access token in `args._oauth.<account_name>`. The host owns refresh.
- **Trust floors apply to host-internal tools too.** `host_internal = true` hides a tool from the agent catalog but does **not** bypass trust gating. Pin `trust_floor = "Controller"` on internal tools that should never be invoked by sub-controller principals.
- **Sidecar host ports are stable per `(plugin_id, sidecar_name)` but assigned on first spawn.** Don't hardcode port numbers in code or docs — go through `sidecar_url`.
- **Plugins must not write to host crates.** If you find yourself adding `if plugin_id == "myplugin"` in `crates/server/src/...`, stop. The host should only know plugins through manifest-declared surfaces. The `plugins.md` audit rule: zero hardcoded plugin IDs in host code.
- **Hot-reload changes the in-memory engine, not on-disk staged files (and vice versa).** During development, edit the source-controlled `plugins/<id>/main.rhai`, rebuild the ZIP, and reinstall via `/api/admin/plugins/install` — never edit the staged copy in place unless you also restart the server, because the script is loaded into the engine at enable time.

---

## 14. Where to dig deeper

- Manifest schema source of truth: [`crates/plugin-sdk/src/manifest.rs`](../crates/plugin-sdk/src/manifest.rs)
- Hook registry + lifecycle: [`crates/plugin-host/src/host.rs`](../crates/plugin-host/src/host.rs), [`crates/plugin-host/src/hook_registry.rs`](../crates/plugin-host/src/hook_registry.rs)
- Rhai primitive registration: [`crates/script/src/primitives.rs`](../crates/script/src/primitives.rs)
- Sidecar supervisor: [`crates/server/src/sidecar_supervisor.rs`](../crates/server/src/sidecar_supervisor.rs) + [`docs/sidecar-supervisor-design.md`](sidecar-supervisor-design.md)
- Admin/webhook router: [`crates/server/src/plugin_admin_routes.rs`](../crates/server/src/plugin_admin_routes.rs), [`crates/server/src/plugin_webhook_routes.rs`](../crates/server/src/plugin_webhook_routes.rs)
- Install + persistence: [`crates/server/src/plugins.rs`](../crates/server/src/plugins.rs)
- MCP integration (the alternative path): [`crates/mcp-client/src/lib.rs`](../crates/mcp-client/src/lib.rs)
- Trust class + capability gating: [`crates/core/src/principal.rs`](../crates/core/src/principal.rs)
