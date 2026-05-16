# execlaw — Operator setup walkthroughs

Per-transport / per-integration pairing flows. The
[`docs/plugins.md`](plugins.md) doc is plugin-author-facing; this is
operator-facing — what a person actually clicks through after
`execlaw install` to get inbound messages flowing.

Common assumptions for every section below:

- The control plane is running (`execlaw service status` →
  `running`) and reachable at `http://127.0.0.1:3031`.
- You're signed in as the controller in the SPA.
- The plugin you're configuring is **installed and enabled**
  (Settings → Plugins → upload ZIP → enable). All routing,
  registry registration, and sidecar supervision happens on
  enable; the per-plugin admin UI assumes that's already done.

---

## Signal (signal-cli sidecar)

Signal's transport plugin runs the bbernhard signal-cli REST API as a
supervised sidecar. Pairing is QR-based — execlaw shows a QR image,
you scan it from the official Signal mobile app under "Linked
devices."

### Steps

1. **Install + enable** `plugins/signal/`. The sidecar
   (`bbernhard/signal-cli-rest-api:latest`) starts in JSON-RPC mode
   on host port `:8501` and persists state under
   `~/.execlaw/sidecars/signal/signal-cli/data/` so the linked-device
   pairing survives restarts.

2. Open **Settings → Plugins → Signal**. The page polls
   `/api/admin/plugins/signal/status` every few seconds.

3. Wait for the status block to show `Sidecar healthy` (green).
   Initial sidecar spawn includes a Docker image pull — first run
   takes 10–30 s.

4. Click **Pair a device**. A QR code data-URL is fetched from
   `/api/admin/plugins/signal/qrcodelink` and rendered.

5. On your phone: Signal app → **Settings → Linked devices →
   Add new device** (the `+` icon), then scan the QR.

6. The page refreshes when the pairing completes. Status flips to
   `Paired as <your phone number>` and the agent will start receiving
   inbound messages.

### Common pitfalls

- **QR doesn't appear / shows "no session"** — the sidecar's
  `/session/connect` endpoint hasn't fired yet. Hit
  **Refresh** on the admin page once or twice; the plugin's
  `admin_qrcodelink` handler retries the connect call before
  returning the QR.
- **QR scanned but no inbound** — check that Signal-cli's
  read-receipts feature flipped on. The plugin ships an
  `enable-read-receipts.sh` script as a stage-mounted file
  inside the sidecar; the supervisor runs it on first health.
  If you see `Sidecar healthy` but no read receipts after sending
  yourself a test message, redeploy the plugin
  (uninstall → re-upload → enable) so the script re-runs.
- **Unpair + re-pair** — Settings → Plugins → Signal → Danger
  zone → **Unregister account**. Drops the sidecar's stored
  identity. Re-running the QR flow links a fresh device slot.

---

## WhatsApp (wuzapi sidecar)

WhatsApp Multi-Device transport via the wuzapi Go wrapper around
whatsmeow. Same QR pattern as Signal but with a webhook-based inbound
event delivery (wuzapi has no WebSocket event surface).

### Steps

1. **Install + enable** `plugins/whatsapp/`. The wuzapi sidecar
   (`asternic/wuzapi:latest`) starts on host port `:8502`. SQLite
   state for paired sessions persists under
   `~/.execlaw/sidecars/whatsapp/wuzapi/data/`.

2. Open **Settings → Plugins → WhatsApp**. The page polls the same
   `/api/admin/plugins/whatsapp/status` admin route.

3. The plugin auto-provisions a wuzapi user (`execlaw`) and registers
   the webhook callback URL on first enable. Wait for status to show
   `Sidecar healthy` and `Webhook registered`.

4. Click **Pair**. The QR data URL is rendered, fetched from
   `/api/admin/plugins/whatsapp/qrcodelink`.

5. On your phone: WhatsApp → **Settings → Linked devices → Link a
   device** → scan.

6. Status flips to `Logged in as <your number>` and the JID
   (`<number>:<device>@s.whatsapp.net`) appears on the page.

7. Send yourself a test message. You should see typing indicator,
   read receipt (single → double-blue tick), and the agent's reply.

### Common pitfalls

- **`already connected` 500 on connect** — non-issue; the plugin
  swallows it as the desired state. If you see it surface in the SPA
  it's a UI bug worth filing.
- **Inbound messages not reaching the agent** — wuzapi's webhook
  retries failed deliveries (default 5 × 30 s; bumped to 10 × 30 s
  via `WEBHOOK_RETRY_COUNT` in plugin.toml). If execlaw was down
  longer than the retry envelope, the messages WhatsApp's servers
  buffered will replay through wuzapi on next reconnect — give it
  a minute. If still nothing, check
  `docker logs execlaw-sidecar-whatsapp-wuzapi | grep "Calling user webhook"`.
- **Duplicate replies** — check the plugin version is `0.1.7+` (the
  pre-fix version blocks the webhook on the agent's whole turn,
  causing wuzapi to time out and retry). Reinstall from the latest
  `dist/whatsapp-*.zip` if needed.
- **`webhook=""` empty in `/admin/users`** — manifest uses old
  field names. Plugin must be `0.1.3+` (uses `webhookurl` not
  `webhook`, `events` not `subscribe`).

---

## Slack (Socket Mode + multi-workspace)

Slack transport uses Socket Mode — no public URL required, no
incoming webhooks, no sidecar. Each workspace pairs independently
via OAuth.

### One-time: create a Slack app

1. Go to <https://api.slack.com/apps> → **Create New App** → **From
   scratch**. Name it (e.g. `execlaw`), pick a workspace.

2. Under **OAuth & Permissions**:
   - Add scopes: `channels:read`, `groups:read`, `chat:write`,
     `files:read`, `im:history`, `mpim:history`, `groups:history`,
     `channels:history`, `users:read`.
   - Click **Install to Workspace** (or "Reinstall" if scopes
     change later).
   - Copy the **Bot User OAuth Token** (`xoxb-…`).

3. Under **Basic Information** → **App-Level Tokens** →
   **Generate Token and Scopes**. Add the `connections:write` scope
   (Socket Mode requirement). Copy the **App-Level Token**
   (`xapp-…`).

4. Under **Socket Mode** — toggle **Enable Socket Mode** on.

5. Under **Event Subscriptions** — toggle **Enable Events** on. Add
   bot events: `message.channels`, `message.groups`, `message.im`,
   `message.mpim`. Save.

### Pair the workspace in execlaw

1. Open **Settings → Plugins → Slack**.

2. Click **Add workspace** → paste the Bot User OAuth Token
   (`xoxb-…`) and the App-Level Token (`xapp-…`). Save.

3. The plugin opens a Socket Mode connection on the operator's
   behalf and registers the workspace under its team ID. Subsequent
   inbound messages on bot-invited channels arrive as agent
   inbound.

4. To invite the bot to a channel: in Slack, type
   `/invite @<bot-name>`. The agent will see DMs without an invite.

### Common pitfalls

- **Token rejected** — scopes don't match. Re-install the app to the
  workspace after editing scopes; tokens minted before a scope
  change don't gain the new scope.
- **No inbound** — the bot wasn't invited to the channel. DM works
  out of the box; channel/group messages need an invite.
- **"app not enabled for Socket Mode"** — toggle in step 4 above.
  Without it the App-Level Token can't open the WebSocket.
- **Multi-workspace** — the plugin supports N workspaces in
  parallel; each uses a separate token pair stored under its team
  ID in the vault.

---

## SMS (Android-side WebSocket gateway)

SMS transport uses a companion Android app
([`sms-socket-app`](https://github.com/justinelgenlong/sms-socket-app))
running on a phone with a SIM. The phone exposes an HTTP+WebSocket
gateway on the local network; execlaw connects out to it.

### Steps

1. Install the `sms-socket-app` APK on an Android phone with a SIM.
   Grant SMS, dialer, and notification permissions when prompted.

2. In the app, set an API key and start the gateway. The app shows
   the gateway's local URL (e.g. `ws://192.168.1.42:8787/`) and the
   API key.

3. Open **Settings → Plugins → SMS**. Paste the gateway URL and the
   API key. Save.

4. The plugin's `on_enable` connects, runs a `getGatewayState`
   handshake, and issues a `rehydrate` request to pull any messages
   the gateway buffered since the last cursor. Status shows the
   phone's connection state, last-seen timestamp, and inbound count.

### Common pitfalls

- **Phone goes to sleep** — Android battery optimization will kill
  the gateway. Disable battery optimization for the
  `sms-socket-app` in Android Settings → Battery → Battery
  optimization → All apps → execlaw SMS → Don't optimize.
- **WebSocket keeps disconnecting** — usually a network issue (Wi-Fi
  drops, phone changes networks). The plugin auto-reconnects and
  re-anchors the rehydrate cursor; the host log shows
  `sms-socket: anchoring rehydrate cursor at <ms>`.
- **Duplicate inbound on reconnect** — the plugin's rehydrate cursor
  prevents this from `0.1.0+`; older builds replayed history on
  every reconnect. Update if you see it.

---

## Google Apps / Google Places

The Google integrations consolidate into two plugins:

- `google-apps` — OAuth, single grant covering Gmail, Calendar,
  Contacts, Tasks, and Drive. **Also an identity provider** (resolves
  inbound email/phone to a known contact via the People API). Replaces
  the legacy `google-calendar` + `google-contacts` plugins (removed
  2026-05-14).
- `google-places` — **API key only**, no OAuth. Search nearby
  businesses + place details.

### One-time: create a Google Cloud project

1. <https://console.cloud.google.com/> → **Create project** (or use
   an existing one).

2. **APIs & Services → Library** → enable each of (for `google-apps`):
   - **Gmail API**
   - **Google Calendar API**
   - **People API**
   - **Tasks API**
   - **Google Drive API**

   And for `google-places`:
   - **Places API (New)**

3. **APIs & Services → OAuth consent screen** (for `google-apps`):
   - User type: **External** (single-operator deployment is fine
     under "Testing" status indefinitely).
   - Scopes: add the scopes `google-apps` requests (gmail, calendar,
     contacts, tasks, drive).
   - Test users: add the operator's Google account email.

4. **APIs & Services → Credentials → Create credentials**:
   - For OAuth: **OAuth client ID** → **Desktop app** type. Copy
     the client ID + client secret.
   - For Places (API-key plugin): **API key**. Restrict it to
     **Places API (New)** under API restrictions.

### Pair google-apps in execlaw

1. **Settings → Plugins → Google Apps**.

2. Paste the OAuth client ID + client secret. Redirect URI defaults
   to `http://localhost:3031/api/oauth/google/callback`. Save.

3. Click **Authorize**. A new browser tab opens to Google's consent
   screen. Sign in as the operator account, grant the requested
   scopes.

4. Google redirects back to `http://localhost:3031/api/oauth/google/callback`.
   The host's OAuth handler exchanges the code for a refresh +
   access token, stores them in the vault keyed on
   `(plugin_id="google-apps", account_name="controller")`.

5. Per-module toggles surface in the same panel (Calendar /
   Contacts / Gmail / Tasks / Drive can be enabled or disabled
   without re-running the consent flow).

6. The plugin's tools become available to the agent on the next turn.

### Configure google-places

1. **Settings → Plugins → Google Places**.

2. Paste the API key. The plugin issues a 1-result `coffee` text
   search to validate the key + scope before saving.

3. Save. The agent can immediately call `google_places.search`,
   `.nearby`, and `.details` tools.

### Common pitfalls

- **"redirect_uri_mismatch"** — Google's OAuth client config rejects
  any redirect URI not in its allow-list. The dev / production
  default is `http://localhost:3031/api/oauth/google/callback`.
  If you bound the control plane to a different port via Settings
  → General, update the redirect URI in Google Cloud Console to
  match.
- **Refresh token never arrives** — Google only returns a refresh
  token on the *first* OAuth grant. If you've previously authorized
  the same client + same Google account, revoke the prior grant at
  <https://myaccount.google.com/permissions> and re-authorize.
- **API key 403** — for Places, the key wasn't restricted to
  Places API or the API isn't enabled in the project. Both are
  fixed in Cloud Console.
- **Identity provider not auto-resolving** — `google-apps` includes
  an identity provider, but the host only consults it when the
  inbound transport handle is an email or phone (per the
  `[identity_provider].resolves` manifest field). Inbound from
  WhatsApp/Signal/Slack uses transport-specific resolution paths
  first; google-apps is a fallback.
- **Migrating from the old `google-calendar` / `google-contacts`
  plugins** — those were removed in 2026-05-14. Uninstall both,
  install `google-apps`, re-run the OAuth pairing once. Tool names
  changed: `calendar.list_calendars`, `contacts.list`, etc. now
  live under the `google-apps` plugin id, but the agent-facing
  tool names themselves are unchanged.

---

## After-pairing sanity checks

For any of the above:

```bash
# Confirm the plugin is enabled + sidecar (if any) is healthy:
curl -s http://127.0.0.1:3031/api/admin/plugins | jq '.plugins[] | select(.enabled)'

# Watch the live log for inbound:
tail -f ~/.execlaw/logs/execlaw.jsonl.<today>

# Send yourself a test message and look for:
#   "<plugin>: webhook hit ENTERED" / "<plugin>: ws-frame received"
#   "host_route_inbound outcome=Dispatched"
#   "auto-bridged agent text reply via originating transport"
```

If any of those three log lines is missing for a transport you've
just paired, file a [bug
report](https://github.com/crockpotveggies/execlaw/issues/new?template=bug.yml)
with the relevant log excerpt.
