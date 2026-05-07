// pushover — Pushover push-notification provider, fully script-tier.
//
// One agent-callable tool (`pushover.notify`) plus three admin
// routes (`GET /config`, `POST /config`, `POST /test`). Persists
// the operator-supplied user_key + app_token via the per-plugin
// vault — there's no manifest-side config, no OAuth flow, no
// sidecar.
//
// Wire format (https://pushover.net/api):
//
//   POST https://api.pushover.net/1/messages.json
//     Body: {token, user, message, title?, priority?, url?, url_title?}
//
//   Response on success: {"status": 1, "request": "..."}
//   Response on error:   {"status": 0, "errors": [...]}

// Rhai gotcha: module-level `const` isn't visible inside fn
// bodies. Inline the endpoint literal at every use site.
// PUSHOVER_ENDPOINT = "https://api.pushover.net/1/messages.json"

// =====================================================================
// AGENT TOOL DISPATCH
// =====================================================================

fn tool_call(name, args, oauth) {
    if name == "pushover.notify" { return notify_impl(args); }
    throw "unknown tool '" + name + "'";
}

fn notify_impl(args) {
    let message = args.message;
    if message == () || message == "" {
        throw "pushover.notify requires `message`";
    }
    let user_key = vault_get("user_key");
    let app_token = vault_get("app_token");
    if user_key == () || user_key == "" {
        throw "pushover not configured — set user_key + app_token in Settings → Plugins → Pushover";
    }
    if app_token == () || app_token == "" {
        throw "pushover not configured — set user_key + app_token in Settings → Plugins → Pushover";
    }

    let body = #{
        "token": app_token,
        "user": user_key,
        "message": message,
    };
    if args.title != () && args.title != "" {
        body.title = args.title;
    }
    if args.priority != () {
        body.priority = args.priority;
    }
    if args.url != () && args.url != "" {
        body.url = args.url;
    }
    if args.url_title != () && args.url_title != "" {
        body.url_title = args.url_title;
    }

    let resp = http_post("https://api.pushover.net/1/messages.json", body, "");
    // Pushover returns 200 with status=0 + errors[] on input
    // failures (bad token, message too long, etc.) — surface the
    // error array verbatim instead of a silent "ok".
    if resp == () { throw "pushover: no response from API"; }
    if resp.status != 1 {
        let errs = "unknown error";
        if resp.errors != () && type_of(resp.errors) == "array" && !resp.errors.is_empty() {
            errs = resp.errors[0];
        }
        throw "pushover rejected the notification: " + errs;
    }
    #{ "ok": true, "request_id": if resp.request != () { resp.request } else { "" } }
}

// =====================================================================
// ADMIN ROUTES (declared in plugin.toml's [[admin_routes]])
// =====================================================================

// GET /api/admin/plugins/pushover/config — return the configured
// keys, masking everything but the last 4 chars of each so the
// SPA can confirm "yes, a key is stored" without echoing the
// full secret back to the operator's browser DevTools.
fn admin_get_config(req) {
    let user_key = vault_get("user_key");
    let app_token = vault_get("app_token");
    #{
        "user_key_set": user_key != () && user_key != "",
        "user_key_masked": mask(user_key),
        "app_token_set": app_token != () && app_token != "",
        "app_token_masked": mask(app_token),
    }
}

// POST /api/admin/plugins/pushover/config — persist user_key +
// app_token from the operator-supplied JSON body. Empty strings
// delete the corresponding row so an operator can clear a stale
// key without uninstalling the plugin.
fn admin_set_config(req) {
    if req.body == () { throw "missing request body"; }
    let raw_user = if req.body.user_key != () { req.body.user_key } else { "" };
    let raw_token = if req.body.app_token != () { req.body.app_token } else { "" };
    let user_key = trim(raw_user);
    let app_token = trim(raw_token);

    if user_key == "" {
        vault_delete("user_key");
    } else {
        vault_put("user_key", user_key);
    }
    if app_token == "" {
        vault_delete("app_token");
    } else {
        vault_put("app_token", app_token);
    }
    log_info("pushover: config updated");
    #{ "ok": true }
}

// POST /api/admin/plugins/pushover/test — fire a one-shot
// notification so the operator can confirm end-to-end delivery
// from the SPA's "Test" button without going through the agent.
fn admin_test_notification(req) {
    let result = ();
    try {
        result = notify_impl(#{
            "title": "execlaw test",
            "message": "Pushover is wired up — you should see this on your device.",
            "priority": 0,
        });
    } catch (e) {
        return #{ "ok": false, "error": e.to_string() };
    }
    result
}

// ---- helpers --------------------------------------------------------

fn mask(s) {
    if s == () || s == "" { return ""; }
    let n = s.len();
    let dots = repeat_char("*", if n <= 4 { n } else { n - 4 });
    if n <= 4 { return dots; }
    dots + s.sub_string(n - 4)
}

fn repeat_char(ch, n) {
    let out = "";
    let i = 0;
    while i < n {
        out += ch;
        i += 1;
    }
    out
}
