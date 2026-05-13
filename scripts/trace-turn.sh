#!/usr/bin/env bash
# trace-turn.sh — readable pretty-printer for the `agent::turn_timing`
# log surface. Tails the live execlaw JSON log, filters to just the
# turn-timing target, and prints one indented per-event line per turn.
#
# Usage (in a separate terminal from dev-server.sh):
#   bash scripts/trace-turn.sh
#
# Then send a "hi" in the SPA — the trace appears here in real time.
# Press Ctrl-C to stop.
#
# Prerequisite: dev-server.sh must be running with the right RUST_LOG.
# The script prints the exact env-var line you need at the top.

set -euo pipefail

LOG_DIR="${EXECLAW_LOG_DIR:-$HOME/.execlaw/logs}"
TODAY="$(date -u +%Y-%m-%d)"
LOG_FILE="$LOG_DIR/execlaw.jsonl.$TODAY"

cat <<EOF
# scripts/trace-turn.sh
#
# Before sending your "hi" — make sure dev-server was started with:
#
#   RUST_LOG="info,agent::turn_timing=debug" bash scripts/dev-server.sh
#
# (Or restart it with that env var now.)
#
# Tailing: $LOG_FILE
# Press Ctrl-C to stop.
#
EOF

if [[ ! -f "$LOG_FILE" ]]; then
    echo "log file not found yet: $LOG_FILE" >&2
    echo "start dev-server first; the file is created on the first log line." >&2
    exit 1
fi

# `tail -F` follows the file even if it gets rotated/recreated.
# `python -c '…'` parses each JSON line and pretty-prints just the
# turn-timing rows. Anything else is silently dropped so the view
# isn't swamped by sidecar-health pings and tower_http request logs.
tail -F "$LOG_FILE" 2>/dev/null | python -c "
import sys, json
last_conv = None
for raw in sys.stdin:
    raw = raw.strip()
    if not raw:
        continue
    try:
        d = json.loads(raw)
    except Exception:
        continue
    if d.get('target') != 'agent::turn_timing':
        continue
    fields = d.get('fields', {})
    msg = fields.get('message', '')
    ts = d.get('timestamp', '')[11:23]
    conv = fields.get('conversation_id', '?')
    if conv != last_conv:
        print()
        print(f'─── conversation {conv} ───')
        last_conv = conv
    # Build the key-value tail in a stable order so the trace lines
    # up visually across consecutive events.
    pieces = []
    for k, v in fields.items():
        if k in ('message', 'conversation_id'):
            continue
        # Numeric ms fields get a unit suffix for readability.
        if isinstance(v, (int, float)) and (k.endswith('_ms') or k == 'ms'):
            pieces.append(f'{k}={v}ms')
        else:
            pieces.append(f'{k}={v}')
    tail = '  ' + '  '.join(pieces) if pieces else ''
    # The message is the headline — indent the kv tail under it.
    print(f'  {ts}  {msg}{tail}')
    sys.stdout.flush()
"
