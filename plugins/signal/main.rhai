// signal — Signal Messenger transport.
//
// PHASE-5 DEFENSIVE STUB.
//
// Every declared tool is `host_implemented = true` in the manifest:
//
//   * signal.send_message       (Phase 3)
//   * signal.reply              (Phase 3)
//   * signal.list_groups        (Phase 5)
//   * signal.create_group       (Phase 5)
//   * signal.add_group_members  (Phase 5)
//   * signal.leave_group        (Phase 5)
//
// Each lands as an `Arc<dyn ToolImpl>` builtin in the host crate so it
// can reach the host-side `TransportApi` capability. The plugin host's
// dispatcher does NOT forward any of these here — the manifest entries
// exist purely for catalog/attribution.
//
// This file remains as a defensive guard: if a future refactor ever
// regresses the `host_implemented = true` skip in
// `enable_with_stage` and a tool dispatch leaks down to the rhai
// tier, the body below fails loudly so the bug is obvious instead of
// silently masking the real builtin's output.

fn tool_call(name, args, oauth) {
    #{
        "error": "host-implemented tool '" + name + "' was dispatched to the rhai layer — this is a bug; " +
                 "every Signal tool is registered as a builtin in crates/server/src/signal_tools.rs",
        "tool": name,
    }
}
