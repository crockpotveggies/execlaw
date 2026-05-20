//! Per-turn prompt construction.
//!
//! Splits out the prose-building corner of the chats module:
//!
//!   * [`GroupTurnContext`] + [`resolve_group_turn_context`] — the
//!     "this turn ran in a group; here's the room" facts threaded
//!     into the system prompt.
//!   * [`build_turn_context_prose`] — the wall-clock / identity /
//!     channel / group block that prefaces every turn's system
//!     message.
//!   * [`assemble_system_prompt`] — composer that stitches the
//!     personality, static base, routing-prose, and turn-context
//!     into the final system message.
//!   * [`build_tool_routing_prose`] — dynamic "which tool family
//!     handles which task" block built from the live tool catalog.
//!   * [`humanise_tool_call`] + transport-tool helpers — the
//!     `(tool_name, args)` → operator-readable label the SPA
//!     renders in its activity pill.
//!
//! Everything here is a pure function; no DB writes, no side
//! effects beyond [`resolve_group_turn_context`]'s read-only
//! `principal_groups` + `conversations` lookups. Safe to call from
//! any turn-execution path.

use execlaw_core::conversation::ConversationStore;
use execlaw_core::ids::ConversationId;

use crate::state::AppState;

/// Per-turn group-conversation context. Threaded into
/// [`build_turn_context_prose`] so the agent's system prompt can
/// describe the room it's answering in: how many other humans are
/// present, what the group is called, and **why** the upstream
/// router decided this turn should run.
///
/// `None` for DM / web / single-actor conversations. The value of
/// telling a model "you're in a DM, behave normally" is low; the
/// value of telling it "you're in a group with 5 people, default
/// to staying quiet unless directly addressed" is high — so the
/// block only renders for actual groups.
#[derive(Debug, Clone)]
pub struct GroupTurnContext {
    /// User-facing group name when the transport supplies one
    /// (Signal does, WhatsApp doesn't yet, Slack channels surface
    /// as the channel id). Renders as a quoted phrase in the
    /// prompt; `None` falls back to "this group" wording.
    pub group_name: Option<String>,
    /// Total participants in the principal_group, including the
    /// Controller. Drives the "you are in a group with N other
    /// participants" line. We don't subtract the agent — the agent
    /// isn't a `Principal`, the group's members are the humans.
    pub member_count: usize,
    /// Why the router decided this turn should reach the agent.
    /// See [`crate::group_addressing::AddressedReason`] for the
    /// full taxonomy. The strength of the signal shapes how
    /// reserved the agent should be in its reply — a transport
    /// `<@mention>` is a clear invitation; a fall-open dispatch
    /// is a hint to be cautious.
    pub addressed_reason: crate::group_addressing::AddressedReason,
}

/// Resolve the group-conversation context from runtime state, when
/// applicable. Returns `None` for conversations the
/// [`build_turn_context_prose`] group block should NOT render for:
///
/// * No `principal_group` binding (web-only / unbridged conversation).
/// * Single-member group (just the Controller — degenerate "group").
/// * All-Controller group (every member is the operator's identity —
///   no other humans to barge in front of).
///
/// In every other case, builds a [`GroupTurnContext`] from the
/// `conversation_display_name` (group name) + member count + the
/// caller-supplied addressed reason. The reason MUST come from the
/// router's `should_dispatch_to_agent` decision so the agent's
/// prompt accurately reflects why the turn ran.
///
/// Cheap (one principal_group lookup, one members query, one
/// conversation row read). Safe to call on every turn — but the
/// production hot path threads the value through from
/// `route_inbound` to avoid a redundant DB round-trip.
pub fn resolve_group_turn_context(
    state: &AppState,
    cid: &ConversationId,
    addressed_reason: crate::group_addressing::AddressedReason,
) -> Option<GroupTurnContext> {
    use execlaw_core::principal::{PrincipalStore, TrustLevel};
    use execlaw_core::principal_groups::PrincipalGroupStore;

    let pg_store = PrincipalGroupStore::new(&state.db);
    let pg_id = pg_store
        .principal_group_id_for(cid.as_str())
        .ok()
        .flatten()?;
    let members = pg_store.members(&pg_id).ok()?;
    if members.len() < 2 {
        return None;
    }
    // Skip all-Controller groups for the same reason
    // `should_dispatch_to_agent` does — there's nobody to be careful
    // about. The `any` short-circuits the moment it sees a
    // non-Controller, so the lookup cost is bounded.
    let principals = PrincipalStore::new(&state.db);
    let has_non_controller = members.iter().any(|pid| {
        match principals.get(pid) {
            Ok(Some(p)) => !matches!(p.trust_level, TrustLevel::Controller),
            // Lookup failure → treat as non-controller. The block
            // is informational; over-rendering is harmless, under-
            // rendering misses the "behave like a group" nudge.
            _ => true,
        }
    });
    if !has_non_controller {
        return None;
    }

    // Read the conversation row's display_name as the group's
    // user-facing title — `apply_auto_display_name` seeds this from
    // the inbound `group_name` field for transports that supply one.
    let group_name = ConversationStore::new(&state.db)
        .get(cid)
        .ok()
        .flatten()
        .and_then(|row| row.display_name);

    Some(GroupTurnContext {
        group_name,
        member_count: members.len(),
        addressed_reason,
    })
}

/// Build the per-turn context block — runtime facts the model
/// needs to answer recency- and identity-sensitive questions
/// without round-tripping a tool. Selfhosted-claw baked these
/// into every turn; pre-fix execlaw shipped none of them.
///
/// Includes:
///   * current UTC time (RFC 3339) — answers "what time is it",
///     drives "today / this week" comparisons, lets the model
///     pick reasonable default windows for `calendar.list_events`
///     etc;
///   * conversation id — handy when the operator asks the agent
///     to "use this thread's id" in a tool call;
///   * caller's principal id — usually `controller`, sometimes a
///     plugin-resolved contact id;
///   * caller's trust class — drives the model's posture for
///     approval-gated tools and confidential output;
///   * **group context (when present)** — name, member count, and
///     the upstream router's "why this turn ran" verdict, plus an
///     explicit "default to silence in groups" posture nudge that
///     replaces the implicit "DM behavior" fallback.
///
/// Pure function — caller assembles the inputs. `pub` (not
/// `pub(crate)`) so the benchmark suite can measure it in isolation;
/// the function has no side effects so the wider visibility doesn't
/// invite misuse.
pub fn build_turn_context_prose(
    now_utc: chrono::DateTime<chrono::Utc>,
    conversation_id: &str,
    sender_principal_id: Option<&str>,
    sender_trust: &str,
    origin_channel: Option<&str>,
    caller_timezone: Option<&str>,
    group: Option<&GroupTurnContext>,
) -> String {
    let mut out = String::from("## Turn context\n\n");
    // Date framing matters more than it looks. Earlier prompts
    // emitted only the ISO timestamp ("2026-05-05T13:00:00Z");
    // models with a sub-2026 training cutoff recognized 2026 as
    // "future relative to me," second-guessed the system prompt,
    // and refused tasks ("I cannot do research about 2026 because
    // it's a future year"). Two-line fix:
    //   1. Render the date in human-prose form alongside the ISO
    //      timestamp — the model's tokenizer chunks "May 5, 2026"
    //      and "2026-05-05" differently, and the prose form
    //      reinforces "this is the actual current date" against a
    //      stale training prior.
    //   2. Add an explicit cutoff-confusion guard so the model
    //      doesn't refuse tasks that reference dates past its
    //      training data. Frames training data as a starting point,
    //      not a ceiling, and points at web_search / research as
    //      the right tools for "I don't know about events after my
    //      cutoff."
    //
    // Timezone framing: when the SPA supplies the operator's IANA
    // zone, render the local clock + offset alongside UTC and tell
    // the agent which one to anchor on. Without this, "create a
    // calendar event at 6pm" was being emitted as `2026-05-05T18:00:00Z`
    // — i.e. 11am Pacific, the regression that prompted this fix.
    let local_block = caller_timezone
        .and_then(|tz| tz.parse::<chrono_tz::Tz>().ok())
        .map(|tz| {
            let local = now_utc.with_timezone(&tz);
            format!(
                "* Current date: {} ({} {})\n  (UTC: {})\n",
                local.format("%A, %B %-d, %Y %-I:%M %p"),
                tz.name(),
                local.format("%Z"),
                now_utc.format("%Y-%m-%dT%H:%M:%SZ"),
            )
        });
    if let Some(b) = local_block {
        out.push_str(&b);
        out.push_str(
            "* Bare clock times in the operator's message (\"6pm\", \"tomorrow at 9\") \
             refer to the local zone above. When emitting RFC3339 timestamps to tools \
             (calendar, routines, etc.), use the local offset (e.g. \
             `2026-05-05T18:00:00-07:00`), NOT a `Z` suffix — a `Z` will land the event \
             in UTC and surface in Google Calendar shifted by the offset.\n",
        );
    } else {
        out.push_str(&format!(
            "* Current date: {} (UTC: {})\n",
            now_utc.format("%A, %B %-d, %Y"),
            now_utc.format("%Y-%m-%dT%H:%M:%SZ"),
        ));
        out.push_str(
            "* Operator timezone is unknown — if the user gives a bare clock time \
             (\"6pm\"), ask which zone they mean before emitting an RFC3339 timestamp.\n",
        );
    }
    out.push_str(
        "* Date above is real, not hypothetical — for post-cutoff facts use \
         `web_search` or `research_start`.\n",
    );
    out.push_str(&format!("* Conversation id: `{conversation_id}`\n"));
    if let Some(p) = sender_principal_id {
        out.push_str(&format!("* From principal: `{p}`\n"));
    }
    out.push_str(&format!("* Trust class: `{sender_trust}`\n"));
    if let Some(ch) = origin_channel {
        out.push_str(&format!("* Origin channel: `{ch}`\n"));
        if ch != "web" {
            // 2026-05-16 — trimmed from a 4-line "no card/chip/
            // download button" expansion to one short sentence.
            // The longer block correlated with the model emitting
            // less-rigorous tool_call.arguments on Signal (the
            // "JSON-in-JSON" / extra-`}` failure modes); shorter
            // context keeps the model closer to its web-tier
            // output distribution where tool calls stay valid.
            // The text-only nudge is preserved because operators
            // saw real "click the download button" leakage before.
            out.push_str(
                "* Text-only channel: reply naturally; do not describe web-UI surfaces \
                 (no \"card\", \"chip\", \"button\"). The host auto-delivers your text reply.\n",
            );
        }
    } else {
        out.push_str("* Origin channel: `web`\n");
    }
    // Group-conversation block. Only renders for actual groups
    // (mixed-membership with ≥1 non-Controller human) — the
    // resolver returns `None` for DMs / web / single-actor flows
    // so the prompt stays small there. The block has three jobs:
    //
    //   1. Tell the agent it's in a group (it had no way to know
    //      before this — the system prompt looked identical to a
    //      DM, which is why operators saw the agent barge into
    //      group conversations).
    //
    //   2. Give it the room's shape: name when known, member
    //      count so it can calibrate "many people are talking"
    //      vs "small group."
    //
    //   3. Surface the upstream router's verdict so the agent
    //      can match its posture to the strength of the signal
    //      that woke it up. An explicit `<@mention>` is a clear
    //      invitation; a fall-open dispatch is "you might NOT
    //      have been addressed, behave accordingly."
    if let Some(g) = group {
        // 2026-05-16 — trimmed. The hard-rules numbered list is
        // the load-bearing part (rule #1 fires for the
        // "addressing someone else" failure mode); the preamble
        // sentence and router-verdict paraphrase were ignored by
        // the model in practice and contributed to the
        // out-of-distribution drift that destabilised tool_call
        // emission on Signal chart turns. Group-name still
        // surfaces — the model uses it when it does choose to
        // reply. Member count dropped (rarely affected behaviour
        // and adds a number the model occasionally hallucinates
        // into its reply).
        let group_label = match &g.group_name {
            Some(n) => format!("\"{}\"", n.replace('"', "\\\"")),
            None => "an unnamed group".to_owned(),
        };
        out.push_str(&format!(
            "\n## Group conversation ({group_label})\n\n\
             Hard rules:\n\
             1. If this message addresses ANY person by name and that name is NOT yours, \
             reply with NOTHING. The named person will answer.\n\
             2. One-word messages, emoji, reactions, generic human chatter → reply with NOTHING.\n\
             3. Default is silence. Speak only when directly named, @-mentioned, or continuing \
             a thread you started.\n\
             4. When you do reply, one line. No \"happy to help,\" no unsolicited offers.\n",
        ));
        // Router verdict kept but as a single trailing line —
        // the agent benefits from knowing *why* it woke up
        // without the verbose "often wrong" hedge that the model
        // sometimes parroted back at users.
        out.push_str(&format!(
            "(Woke for: {desc}.)\n",
            desc = g.addressed_reason.description(),
        ));
    }
    out
}

/// Phase 11.B — assemble the turn's system prompt. Four halves:
///
///   1. **Operator-editable personality** (§5.5). Pulled from
///      `config_personality` via `compose_system_prompt`. Includes
///      the conversation-scope override merged on top of the global
///      default. Best-effort — a missing/corrupt personality table
///      collapses to an empty chunk so the static base alone still
///      flies.
///   2. **Static system base** (§2.8). The trust-class rules,
///      refusal behaviour, etc. that operators don't tweak. Comes
///      from `state.config.system_prompt`. Sits *after* the
///      personality so it has the final word on conflict.
///   3. **Tool routing prose** (delta #2 from the agent-prompting
///      audit, 2026-05). Built dynamically from the live tool
///      catalogue so the model gets a "Quick reference: which tool
///      family handles which kind of task" map BEFORE it scans the
///      individual descriptions. Mirrors what selfhosted-claw's
///      `buildSystemPrompt` baked in statically; here the prefixes
///      we recognise drive emission so newly-installed plugins
///      light up automatically without a code change.
///
/// Operators override "agent voice"; the static base owns
/// non-negotiable safety rules; the routing block teaches the
/// model when to reach for which family.
/// Turn `(tool_name, args)` into a one-liner the operator can
/// read while the agent works. Mirrors the "Searching for X…" UX
/// selfhosted-claw used to surface in its activity pill, but
/// generated server-side so the SPA stays a dumb subscriber.
///
/// The mapping is a hand-tuned table for the families execlaw
/// ships today; tools without a match get a generic fallback so a
/// freshly-installed plugin's tool still surfaces something
/// readable instead of a raw symbol name.
///
/// Args are inspected with `serde_json::Value::get` — every lookup
/// returns `Option`, so a missing or wrongly-shaped arg never
/// panics; we just fall back to the no-arg form of the verb.
pub(crate) fn humanise_tool_call(tool_name: &str, args: &serde_json::Value) -> String {
    let s = |key: &str| args.get(key).and_then(|v| v.as_str()).map(str::to_owned);
    let truncate = |opt: Option<String>, max: usize| -> Option<String> {
        opt.map(|v| {
            if v.chars().count() > max {
                let mut t = v.chars().take(max).collect::<String>();
                t.push('…');
                t
            } else {
                v
            }
        })
    };
    match tool_name {
        "web_search" => match truncate(s("query"), 60) {
            Some(q) => format!("Searching the web for “{q}”"),
            None => "Searching the web".into(),
        },
        "web_fetch" => match truncate(s("url"), 80) {
            Some(u) => format!("Reading {u}"),
            None => "Fetching a page".into(),
        },
        "read_memory" => match s("key") {
            Some(k) => format!("Looking up note ‘{k}’"),
            None => "Looking through saved notes".into(),
        },
        "list_memory" => "Listing saved notes".into(),
        "write_memory" => match s("key") {
            Some(k) => format!("Saving note ‘{k}’"),
            None => "Saving a note".into(),
        },
        "read_chat_history" => "Reviewing the conversation".into(),
        "list_chats" => "Listing your chats".into(),
        "get_thread" => "Inspecting a chat thread".into(),
        "set_thread_name" => "Renaming this thread".into(),
        "notify_controller" => "Pinging you on your priority channel".into(),
        "delegate_task" => "Spinning up a sub-agent".into(),
        "research_start" => match truncate(s("query"), 60) {
            Some(q) => format!("Kicking off research on “{q}”"),
            None => "Starting a research job".into(),
        },
        "research_status" => "Checking research status".into(),
        "research_list" => "Listing research jobs".into(),
        "research_get_report" => "Reading a research report".into(),
        "routine_create" => match s("name") {
            Some(n) => format!("Creating routine ‘{n}’"),
            None => "Creating a routine".into(),
        },
        "routine_list" => "Listing routines".into(),
        "routine_get" | "routine_pause" | "routine_resume" | "routine_update"
        | "routine_delete" => {
            // routine_<verb> — fold them into one phrasing.
            let verb = tool_name.trim_start_matches("routine_");
            let pretty = match verb {
                "get" => "checking",
                "pause" => "pausing",
                "resume" => "resuming",
                "update" => "updating",
                "delete" => "deleting",
                _ => verb,
            };
            format!(
                "{} a routine",
                pretty
                    .chars()
                    .next()
                    .map(|c| c.to_uppercase().collect::<String>() + &pretty[c.len_utf8()..])
                    .unwrap_or_else(|| pretty.into())
            )
        }
        // === Transport-plugin tools (any channel) ===
        //
        // The convention is `{channel}.{verb}` — `signal.send_message`,
        // `sms.send_message`, `whatsapp.create_group`, etc. We
        // recognise the verb and surface a transport-aware label,
        // formatting the channel name human-readable. Previously
        // these arms were hardcoded per-Signal; that broke the
        // moment plugins for sms / whatsapp / slack landed because
        // their tools fell through to the generic dotted-namespace
        // fallback below ("send message via whatsapp"), which is
        // awkward and doesn't surface the recipient.
        n if n.contains('.') && is_transport_tool_verb(n) => {
            let (channel, verb) = n.split_once('.').unwrap();
            humanise_transport_tool(channel, verb, &s)
        }
        // Plugin-namespaced tools (`google.calendar.list_events`
        // etc) get a "<verb> via <namespace>" rendering. Operators
        // have plugin descriptions in the catalogue; the loader
        // just needs to read like English.
        n if n.contains('.') => {
            let parts: Vec<&str> = n.splitn(2, '.').collect();
            let ns = parts[0];
            let verb = parts.get(1).copied().unwrap_or("call").replace('_', " ");
            format!("{verb} via {ns}")
        }
        // Last-resort fallback. `read_chat_history` style
        // snake_case becomes "Read chat history".
        _ => {
            let pretty = tool_name.replace('_', " ");
            let mut chars = pretty.chars();
            match chars.next() {
                Some(c) => c.to_uppercase().chain(chars).collect(),
                None => "Working".into(),
            }
        }
    }
}

/// True iff the part after the first `.` in a dotted tool name is
/// one of the known transport-plugin verbs. Keeps `humanise_tool_call`'s
/// transport-aware branch from claiming non-transport namespaced
/// tools (e.g. `google_calendar.create_event` would otherwise look
/// like a "create_event" verb on a `google_calendar` channel).
fn is_transport_tool_verb(tool_name: &str) -> bool {
    match tool_name.split_once('.') {
        Some((_, verb)) => matches!(
            verb,
            "send_message"
                | "reply"
                | "create_group"
                | "add_group_members"
                | "remove_group_members"
                | "list_groups"
                | "leave_group"
        ),
        None => false,
    }
}

/// Format a transport-plugin tool call into human prose, given the
/// channel and the verb. Recipient / title args come through `s`
/// (a getter built from the JSON args earlier in `humanise_tool_call`).
fn humanise_transport_tool(
    channel: &str,
    verb: &str,
    s: &impl Fn(&str) -> Option<String>,
) -> String {
    let label = display_label_for_channel(channel);
    match verb {
        "send_message" => match s("to") {
            Some(to) => format!("Sending {label} message to {to}"),
            None => format!("Sending a {label} message"),
        },
        "reply" => format!("Replying on {label}"),
        "create_group" => match s("title").or_else(|| s("name")) {
            Some(t) => format!("Creating {label} group “{t}”"),
            None => format!("Creating a {label} group"),
        },
        "add_group_members" => match s("groupName").or_else(|| s("group_name")) {
            Some(g) => format!("Adding members to “{g}”"),
            None => format!("Adding members to a {label} group"),
        },
        "remove_group_members" => match s("groupName").or_else(|| s("group_name")) {
            Some(g) => format!("Removing members from “{g}”"),
            None => format!("Removing members from a {label} group"),
        },
        "list_groups" => format!("Listing {label} groups"),
        "leave_group" => match s("groupName").or_else(|| s("group_name")) {
            Some(g) => format!("Leaving {label} group “{g}”"),
            None => format!("Leaving a {label} group"),
        },
        _ => format!("{verb} via {channel}").replace('_', " "),
    }
}

/// Convert a channel id (`signal`, `sms`, `whatsapp`, `slack`) into
/// the prose form operators expect to read in the loader pill. We
/// special-case channels with non-Title-case canonical spellings
/// (`SMS`, `MMS`-future) and Title-case the rest.
///
/// For unknown channels installed at runtime we fall back to
/// Title-Case-ing the id; future improvement: read the channel's
/// human name from the plugin manifest's `[transport].label` field.
fn display_label_for_channel(channel: &str) -> String {
    match channel {
        "sms" | "mms" => channel.to_uppercase(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(c) => c.to_uppercase().chain(chars).collect(),
                None => other.to_string(),
            }
        }
    }
}

pub(crate) fn assemble_system_prompt(
    db: &execlaw_core::Database,
    conversation_id: Option<&str>,
    static_base: &str,
    routing_prose: &str,
    turn_context: &str,
) -> String {
    let store = execlaw_core::personality::PersonalityStore::new(db);
    let personality_chunk =
        execlaw_core::personality::compose_system_prompt(&store, conversation_id)
            .unwrap_or_default();
    let p = personality_chunk.trim();
    let b = static_base.trim();
    let r = routing_prose.trim();
    let c = turn_context.trim();
    let mut out = String::new();
    let sep = |s: &mut String| {
        if !s.is_empty() {
            s.push_str("\n\n---\n\n");
        }
    };
    if !p.is_empty() {
        out.push_str(p);
    }
    if !b.is_empty() {
        sep(&mut out);
        out.push_str(b);
    }
    if !r.is_empty() {
        sep(&mut out);
        out.push_str(r);
    }
    // Turn context is LAST so the most-recent runtime facts are
    // closest to the user message in the request order — recency
    // bias generally helps the model pick them up.
    if !c.is_empty() {
        sep(&mut out);
        out.push_str(c);
    }
    out
}

/// Build the per-turn tool-routing block from the live tool
/// catalogue. The prose is grouped by tool-name prefix:
///
///   * known prefixes from the built-in catalogue get a curated
///     one-liner that tells the model when to reach for that
///     family;
///   * plugin namespaces (anything containing a `.`) get a generic
///     "tools prefixed `X.` come from the X plugin — read each
///     tool's description for usage" line so newly-installed
///     plugins are surfaced automatically;
///   * the block is empty when no tools are registered (defensive
///     — a turn with zero tools shouldn't read like the model is
///     forgetting capabilities).
///
/// This runs once per turn; it allocates a few small strings and
/// walks the catalogue once. Cheap relative to the LLM call.
pub(crate) fn build_tool_routing_prose(
    builtin_names: &[String],
    plugin_names: &[String],
) -> String {
    use std::collections::BTreeSet;

    // Sentences keyed by tool-family prefix. Hand-tuned to mirror
    // the routing prose selfhosted-claw used to bake into its
    // system prompt — the model needs WHEN, not just WHAT.
    let routing_lines: &[(&str, &str)] = &[
        (
            "memory",
            "* `read_memory` / `list_memory` / `write_memory` — durable per-controller notes. \
             Read BEFORE answering questions about prior conversations or operator preferences. \
             Write only when the operator explicitly says \"remember\" or shares a stable fact \
             (preferences, recurring contacts, ongoing projects). Do NOT write summaries of \
             every chat.",
        ),
        (
            "web",
            "* `web_search` + `web_fetch` — use as a PAIR for facts you don't know or that may \
             have changed since training. Search to find URLs, then fetch the most-promising 1-3 \
             to read. Don't fetch arbitrary URLs the operator didn't ask about.",
        ),
        (
            "research",
            "* `research_*` — multi-step deep-research jobs that run in the background. Use ONLY \
             when the operator asks for a written report or comparative analysis; for a quick \
             question, prefer `web_search` + `web_fetch` directly.",
        ),
        (
            "routine",
            "* `routine_*` — schedule recurring agent work (cron-shaped). Use when the operator \
             says \"every Monday\", \"each morning\", or describes anything that should fire \
             repeatedly without re-prompting. Do NOT use for one-shot reminders.",
        ),
        (
            "chat",
            "* `read_chat_history` / `list_chats` / `get_thread` / `set_thread_name` — inspect \
             other conversations the operator is having. Use when the user references \"that \
             thread\", \"the conversation about X\", or to find a thread to rename.",
        ),
        (
            "controller",
            "* `notify_controller` — sends a private message to the operator on their highest-\
             priority channel. Use ONLY when (a) you need approval before acting, (b) you hit a \
             blocker that needs a human decision, or (c) the operator told you to follow up out-\
             of-band. Do NOT use for normal answers in this thread.",
        ),
        (
            "delegate",
            "* `delegate_task` — spin up a sub-agent for a self-contained task you can finish \
             in the background. Use sparingly: it costs a fresh inference round and an isolated \
             context.",
        ),
        (
            "mcp",
            "* `mcp_list_servers` / `mcp_add_server` / `mcp_remove_server` — install MCP \
             (Model Context Protocol) servers so their tools flow into your catalog. Use when \
             the operator says \"integrate / install / wire up the X MCP server\" (e.g. \
             Atlassian, Linear, GitHub). Workflow: (1) `mcp_list_servers` to check what's \
             already wired so you don't double-add; (2) `web_search` + `web_fetch` if you \
             don't know the server's URL or auth shape; (3) ASK the user for any required API \
             token / bearer in plain text — do NOT make one up; (4) call `mcp_add_server` with \
             `transport: \"streamable_http\"`, the URL, and `auth_token` if needed. After the \
             call returns, the server's tools auto-flow into your catalog as `mcp:<id>:<name>` \
             — you can call them on the next turn. For Atlassian Rovo specifically: URL is \
             `https://mcp.atlassian.com/v1/mcp/authv2`, auth via API token from \
             id.atlassian.com.",
        ),
        (
            // 2026-05-15 — added because the routing prose previously
            // had no chart entry, and `chart.render` was bucketed as
            // a "plugin-prefixed" tool with the generic
            // "read its description" prose. Result: model didn't
            // recognise chart-rendering as a workflow with a fetch
            // step, defaulted to the "no tool helps, answer from
            // own knowledge" escape hatch, and hallucinated stock
            // prices. Top-level entry tells the model the chain
            // BEFORE it scans individual descriptions.
            "chart",
            "* `chart.render` (built-in) — visualisation. ALWAYS fetch real data first \
             (stocks → `yahoo_finance.historical_candles`; weather → `open_meteo.*`; \
             else `web_fetch`), then pipe via `points: {\"$data_ref\": \"<id>\"}` when \
             the data source returned a `data_refs` map. Never invent points; never \
             retype data into points. Reply with one short line after rendering.",
        ),
        (
            // 2026-05-18 — added when the python-sandbox plugin was
            // wired (Phase 8). Without this the model treated
            // `python.execute` as a "plugin-prefixed" tool with the
            // generic "read its description" prose, and operators
            // had to babysit every "summarize this csv" turn through
            // multiple back-and-forth nudges. Surfaces the workflow
            // BEFORE the model scans individual descriptions:
            // attachments are real files at /work/uploads/, the
            // kernel persists across turns in a conversation, and
            // chart-rendering pairs naturally with pandas.
            "python",
            "* `python.execute` (+ `python.list_files` / `python.reset` / `python.interrupt`) — \
             run Python inside a sandboxed Jupyter kernel scoped to this conversation. \
             Files the operator attached appear at `/work/uploads/<filename>` (see the \
             'Attached files' block above when present). Files YOUR code writes to \
             `/work/outputs/` get auto-surfaced to the operator as download chips. The \
             kernel keeps variables / imports across calls within the same conversation. \
             Use for: parsing CSV/JSON/PDF, pandas analysis, plotting (matplotlib → \
             auto-rendered inline), any \"compute / transform / chart this data\" task. \
             Prefer this over guessing values or asking the operator to summarize a file.",
        ),
    ];

    // 2026-05-15 — built-in tool namespaces that look plugin-like
    // (have a `.` in the name) but aren't actually plugin-supplied.
    // Catch these BEFORE the plugin-namespace bucketing so they don't
    // get the misleading "comes from the X plugin" prose. The
    // matching `routing_lines` entry above carries the right guidance.
    //
    // 2026-05-18 — `python` joined the list. The python-sandbox
    // plugin's tools are technically supplied by a plugin manifest,
    // BUT they register via `wire_python_sandbox`'s `register_builtin`
    // calls (against a live kernel-gateway sidecar) so DB rows show
    // `source = builtin`. From a routing-prose standpoint they
    // deserve the curated entry above, not the generic "comes from
    // the python plugin" line.
    const BUILTIN_NAMESPACES: &[&str] = &["chart", "python"];

    // Bucket every tool by its family prefix.
    let mut present: BTreeSet<&str> = BTreeSet::new();
    let mut plugin_namespaces: BTreeSet<String> = BTreeSet::new();
    for name in builtin_names.iter().chain(plugin_names.iter()) {
        if let Some(dot) = name.find('.') {
            let ns = &name[..dot];
            // Built-in dotted names route through the explicit
            // `routing_lines` entry (e.g. "chart"). Look up the
            // 'static str so we can insert into `present` (which
            // holds &'static str matching the routing_lines keys),
            // and skip the plugin-namespace bucket below so we
            // don't double-emit prose.
            if let Some(builtin_ns) = BUILTIN_NAMESPACES.iter().find(|b| **b == ns) {
                present.insert(*builtin_ns);
                continue;
            }
            // `calendar.list_events` → namespace `calendar`.
            plugin_namespaces.insert(ns.to_owned());
            continue;
        }
        let prefix = name.split('_').next().unwrap_or(name);
        match prefix {
            "read" | "write" | "list" => {
                if name.contains("memory") {
                    present.insert("memory");
                } else if name.contains("chat") || name == "list_chats" {
                    present.insert("chat");
                }
            }
            "web" => {
                present.insert("web");
            }
            "research" => {
                present.insert("research");
            }
            "routine" => {
                present.insert("routine");
            }
            "set" if name.contains("thread") => {
                present.insert("chat");
            }
            "get" if name.contains("thread") => {
                present.insert("chat");
            }
            "notify" => {
                present.insert("controller");
            }
            "delegate" => {
                present.insert("delegate");
            }
            "mcp" => {
                present.insert("mcp");
            }
            _ => {}
        }
    }

    let mut lines: Vec<String> = Vec::new();
    for (key, prose) in routing_lines {
        if present.contains(*key) {
            lines.push((*prose).to_owned());
        }
    }
    for ns in &plugin_namespaces {
        lines.push(format!(
            "* Tools prefixed `{ns}.` come from the `{ns}` plugin — read each tool's \
             description for usage hints. The plugin's OAuth account (if any) is already \
             connected when these tools appear in your catalogue.",
        ));
    }

    if lines.is_empty() {
        return String::new();
    }

    let mut out = String::from(
        "## Tool routing — quick reference\n\n\
         Match the operator's request to the right tool family BEFORE scanning individual \
         descriptions:\n\n",
    );
    for line in lines {
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str(
        "\nWhen multiple families could apply, prefer the most specific one. If no tool helps \
         AND the question is general knowledge (definitions, history, how-things-work, your own \
         opinion), answer from your own knowledge. If no tool helps AND the question needs LIVE \
         or DATED data (current prices, today's weather, breaking news, the operator's calendar, \
         anything that may have changed since training), say you cannot fetch it — never \
         invent values.",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-05-16 — prompt-budget regression tests.
    //
    // Background: the Signal-channel chart-rendering 400s
    // ("Expecting ',' delimiter at column N") that prompted the
    // round-N+1 canonicalization fix correlated directly with the
    // *length* of the per-turn system prompt. The same prompt
    // rendered fine on the web channel because web's turn_context
    // is shorter; Signal-tier additions (origin-channel nudge,
    // group block, trust class line, sender principal id) tipped
    // a quantized 27B model just enough off-distribution to start
    // emitting malformed JSON in tool_call.arguments.
    //
    // These tests pin a character-count ceiling per channel so a
    // future edit to `build_turn_context_prose` can't quietly
    // re-introduce the drift. The ceilings are set with ~20-25%
    // headroom over the measured 2026-05-16 sizes — enough room
    // for one normal edit, tight enough to alarm before a "let me
    // just add one more clarifying paragraph" doubles the budget.
    //
    // When this test fails, the right responses (in order):
    //   1. Trim the new prose — most additions can be tightened.
    //   2. Move the new prose into the static `system_prompt`
    //      base (in `config_system_prompt`) so it doesn't grow
    //      the *per-turn* surface specifically.
    //   3. Only if neither works: raise the ceiling intentionally,
    //      with a comment explaining why, and verify the Signal
    //      chart-render flow still passes end-to-end.
    //
    // Reference values (2026-05-16 measurement):
    //   * web:           667 chars
    //   * signal DM:     596 chars
    //   * signal group:  1180 chars
    //
    // The web vs signal-DM comparison is misleading — web is
    // longer because it includes timezone framing (which signal
    // doesn't get). Signal-DM + signal-group is the curve that
    // matters; group adds the hard-rules block.

    /// Hard ceiling on the web channel's `turn_context` prose.
    /// Pinned 2026-05-16 — see module-level rationale.
    pub(super) const WEB_TURN_CONTEXT_CHARS_MAX: usize = 800;

    /// Hard ceiling on a Signal DM's `turn_context` prose.
    pub(super) const SIGNAL_DM_TURN_CONTEXT_CHARS_MAX: usize = 750;

    /// Hard ceiling on a Signal group conversation's `turn_context`
    /// prose. This is the channel/shape that exhibited the
    /// JSON-in-JSON failure on chart turns; tightest budget.
    pub(super) const SIGNAL_GROUP_TURN_CONTEXT_CHARS_MAX: usize = 1400;

    fn fixed_now() -> chrono::DateTime<chrono::Utc> {
        use chrono::TimeZone;
        // Pinned date so the prose's date-framing is stable.
        // Length varies across days/months otherwise (e.g.
        // "Wednesday, September 30, 2026" vs "Friday, May 1, 2026").
        chrono::Utc
            .with_ymd_and_hms(2026, 5, 16, 18, 30, 0)
            .unwrap()
    }

    #[test]
    fn web_turn_context_stays_under_ceiling() {
        let prose = build_turn_context_prose(
            fixed_now(),
            "conv-x",
            Some("controller"),
            "Controller",
            None, // origin_channel: web (None)
            Some("America/Los_Angeles"),
            None, // no group
        );
        let chars = prose.chars().count();
        assert!(
            chars <= WEB_TURN_CONTEXT_CHARS_MAX,
            "web turn_context grew to {chars} chars; ceiling is {WEB_TURN_CONTEXT_CHARS_MAX}. \
             See module-level comment for the rationale — prompt drift on the chats path \
             correlates with tool-call JSON malformations on quantized 27B inference. \
             Trim the prose, move it into the static system prompt, or raise the ceiling \
             intentionally (with a rationale).\n\nPROSE:\n{prose}",
        );
    }

    #[test]
    fn signal_dm_turn_context_stays_under_ceiling() {
        let prose = build_turn_context_prose(
            fixed_now(),
            "conv-x",
            Some("pri_signal_+15555550100"),
            "KnownTrusted",
            Some("signal"),
            None, // no timezone — external transport
            None, // DM, no group
        );
        let chars = prose.chars().count();
        assert!(
            chars <= SIGNAL_DM_TURN_CONTEXT_CHARS_MAX,
            "signal-DM turn_context grew to {chars} chars; ceiling is \
             {SIGNAL_DM_TURN_CONTEXT_CHARS_MAX}. Signal-channel context length correlates \
             with tool-call JSON validity on quantized 27B inference. Trim, move to static \
             base, or raise the ceiling intentionally.\n\nPROSE:\n{prose}",
        );
    }

    #[test]
    fn signal_group_turn_context_stays_under_ceiling() {
        let g = GroupTurnContext {
            group_name: Some("Project Loon".into()),
            member_count: 5,
            addressed_reason: crate::group_addressing::AddressedReason::TransportMention,
        };
        let prose = build_turn_context_prose(
            fixed_now(),
            "conv-x",
            Some("pri_signal_+15555550100"),
            "KnownTrusted",
            Some("signal"),
            None,
            Some(&g),
        );
        let chars = prose.chars().count();
        assert!(
            chars <= SIGNAL_GROUP_TURN_CONTEXT_CHARS_MAX,
            "signal-group turn_context grew to {chars} chars; ceiling is \
             {SIGNAL_GROUP_TURN_CONTEXT_CHARS_MAX}. This is the tightest budget — the group \
             block already includes hard rules + router verdict, and chart-render failures \
             on Signal correlated with growth in this exact prose. If you need to add \
             material here, trim something else first.\n\nPROSE:\n{prose}",
        );
    }
}
