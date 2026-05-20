// python-sandbox plugin — Rhai stub.
//
// Every tool declared in plugin.toml is `host_implemented = true`, so
// dispatch never reaches this script. It exists because the manifest
// validator currently requires runtime.tier = "script" or "subprocess"
// and "script" requires a source file. v1.1 will populate this with
// admin-route handlers for plugin settings (per-conversation resource
// limits, optional network-egress allowlist).

fn on_enable() {
    log_info("python-sandbox enabled (host-implemented tools)");
}

fn on_disable() {
    log_info("python-sandbox disabled");
}

// Reachable only if a future non-host-implemented tool is added. Returns
// an explicit error rather than silently no-oping so a missed migration
// doesn't hide behind an empty response.
fn tool_call(name, args, oauth) {
    throw `python-sandbox tool '${name}' is not script-tier — every tool in this plugin is host_implemented = true. This dispatcher should never be reached; if you are seeing this throw, the host's tool registry lost the host_implemented marker for '${name}'.`;
}
