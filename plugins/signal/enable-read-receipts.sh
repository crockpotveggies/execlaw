#!/bin/sh
# Wrapper entrypoint that patches signal-cli's supervisor config to add
# --send-read-receipts after jsonrpc2-helper generates it, then
# reconciles the RUNNING process once supervisord is stable.
#
# Ported from selfhosted-claw's enable-read-receipts.sh. Without
# this, signal-cli silently drops read receipts even though
# bbernhard's /v1/configuration endpoint claims they're on — the
# CLI flag is only read at daemon-start time and there's no
# in-process reload knob.
#
# History of things that didn't work and why:
#   1. supervisorctl restart — doesn't re-read config; running
#      process keeps the old cmdline even after the file is patched.
#   2. supervisorctl reread+update+restart called during startup —
#      races bbernhard's entrypoint `supervisorctl start all`, two
#      clients fight supervisord mid-boot and `update` hangs.
#   3. Watching the config file — the file looks correct but the
#      running process still has no flag, so file-only checks give
#      false positives.
#
# The working pattern: patch the file (cheap), then in a background
# loop wait for the program to reach RUNNING state (boot dance
# finished), then check the *live cmdline* and, only if wrong,
# reread+update to force a restart with the patched cmdline. Retry
# with backoff until verified.

exec /entrypoint.sh &
PID=$!

CONF=/etc/supervisor/conf.d/signal-cli-json-rpc-1.conf
FLAG="send-read-receipts"
PROG=signal-cli-json-rpc-1

patch_config_file() {
  [ -f "$CONF" ] || return 1
  grep -q -- "--${FLAG}" "$CONF" 2>/dev/null && return 1
  grep -q "daemon" "$CONF" 2>/dev/null || return 1
  sed -i "s|daemon |daemon --${FLAG} |" "$CONF"
  echo "[read-receipts] Config patched with --${FLAG}"
  return 0
}

running_process_has_flag() {
  # Ground-truth: read the live java cmdline. Returns 0=has flag,
  # 1=running but missing flag, 2=no daemon process found yet.
  for d in /proc/[0-9]*; do
    [ -r "$d/cmdline" ] || continue
    line=$(tr '\0' ' ' < "$d/cmdline" 2>/dev/null)
    case "$line" in
      *signal-cli*daemon*)
        case "$line" in
          *--${FLAG}*) return 0 ;;
          *) return 1 ;;
        esac
      ;;
    esac
  done
  return 2
}

program_is_running() {
  state=$(supervisorctl status "$PROG" 2>/dev/null | awk '{print $2}')
  [ "$state" = "RUNNING" ]
}

force_reload_and_restart() {
  # Only call this AFTER the program has reached RUNNING — otherwise
  # we race bbernhard's startup `supervisorctl start all` and
  # `update` hangs.
  supervisorctl reread >/dev/null 2>&1 || true
  supervisorctl update >/dev/null 2>&1 || true
  sleep 1
  supervisorctl restart "$PROG" >/dev/null 2>&1 || true
}

# ── Initial patch ────────────────────────────────────────────────
# Wait for jsonrpc2-helper to write the daemon subcommand, then
# patch the file. No supervisorctl calls here — defer all
# reconciliation to the background loop once supervisord is stable.
for i in $(seq 1 90); do
  if grep -q "daemon" "$CONF" 2>/dev/null; then
    patch_config_file || true
    break
  fi
  sleep 1
done

# ── Background reconciler ────────────────────────────────────────
(
  # Step 1: wait for the program to reach RUNNING. This signals that
  # bbernhard's entrypoint has finished its startup dance and
  # supervisorctl commands will not deadlock.
  for i in $(seq 1 120); do
    program_is_running && break
    sleep 1
  done

  # Step 2: reconcile the live cmdline up to 10 times with backoff.
  # Each iteration re-patches the file if it drifted, then forces
  # supervisord to re-read and restart the program. Verifies ground
  # truth afterwards.
  attempt=0
  while [ $attempt -lt 10 ]; do
    attempt=$((attempt + 1))
    running_process_has_flag
    rc=$?
    if [ $rc -eq 0 ]; then
      echo "[read-receipts] Verified --${FLAG} is live on signal-cli process"
      break
    fi
    if [ $rc -eq 2 ]; then
      sleep 3
      continue
    fi
    # rc=1 — process is running without the flag
    echo "[read-receipts] Live process missing --${FLAG} (attempt ${attempt}); reconciling"
    patch_config_file || true
    force_reload_and_restart
    sleep 5
  done

  # Step 3: long-running watcher. Re-reconcile if the config is
  # regenerated without the flag, or if the running process drifts.
  while kill -0 $PID 2>/dev/null; do
    sleep 60
    if [ -f "$CONF" ] && ! grep -q -- "--${FLAG}" "$CONF" 2>/dev/null; then
      echo "[read-receipts] Config regenerated without --${FLAG}, re-patching"
      patch_config_file || true
      force_reload_and_restart
      continue
    fi
    running_process_has_flag
    if [ $? -eq 1 ]; then
      echo "[read-receipts] Live process drifted from --${FLAG}; forcing reload"
      force_reload_and_restart
    fi
  done
) &

wait $PID
