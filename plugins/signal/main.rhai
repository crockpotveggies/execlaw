// signal — Signal Messenger transport.
//
// PHASE-1 STUB. The manifest declares the full tool surface (six
// entry points, mirrored from selfhosted-claw `src/integrations/
// signal.ts`) so the agent's tool catalogue, the operator's loader
// pill, the per-tool trust-floor enforcement, and the install-time
// validator all light up. The actual signal-cli RPC plumbing waits on
// the bridge supervisor (see `docs/bridge-supervisor-design.md`).
//
// Until the bridge ships, every tool returns a structured error of
// the form `{ "error": "signal bridge not yet running" }`. The agent's
// existing tool-error UX surfaces this gracefully — operator sees
// "Sending Signal message to Alice — failed: signal bridge not yet
// running" rather than a 500.
//
// When the supervisor lands, this file gets its real bodies — each
// tool will:
//   * resolve the principal_group's transport binding to a Signal JID
//   * call into ctx.transport.signal.{send,reply,...}
//   * for mutations, attach `ack_text` + `agent_instruction` exactly
//     as selfhosted-claw did, so the agent doesn't double up the
//     user-visible reply with a paraphrase.

fn tool_call(name, args, oauth) {
    // The shape `{ "error": ... }` matches the tool-error contract
    // the dispatcher knows how to surface in the chat shell. Plain
    // strings would be rendered as a successful tool result.
    if name == "signal.send_message"
        || name == "signal.reply"
        || name == "signal.list_groups"
        || name == "signal.create_group"
        || name == "signal.add_group_members"
        || name == "signal.leave_group"
    {
        #{
            "error": "signal bridge not yet running — Phase-1 stub. " +
                     "Install the bridge supervisor + signal-cli sidecar " +
                     "to enable real dispatch.",
            "tool": name,
        }
    } else {
        #{
            "error": "unknown tool: " + name,
            "tool": name,
        }
    }
}
