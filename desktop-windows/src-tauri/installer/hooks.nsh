; NSIS installer hooks for execlaw.exe.
;
; Tauri 2's NSIS bundler `!include`s this file from the generated
; installer template. The four macros below are the documented
; extension points; we only override two of them — the others stay
; empty no-ops so the bundler's defaults run unchanged.
;
;   * NSIS_HOOK_POSTINSTALL  — fires AFTER file extraction. We use
;                              it to register + start the SCM
;                              service.
;   * NSIS_HOOK_PREUNINSTALL — fires BEFORE file removal. We stop +
;                              deregister the service so the
;                              uninstaller doesn't leave an orphan
;                              SCM entry pointing at a deleted .exe.
;
; This is the Windows analogue of `desktop-macos/src-tauri/macos/
; LaunchAgents/com.execlaw.agent.plist` + the
; `SMAppService.register()` call the macOS tray makes on first
; launch. The macOS bundle relies on the OS auto-cleaning the agent
; when the .app is dragged to Trash; Windows has no equivalent, so
; we explicitly tear down in PREUNINSTALL.
;
; A few NSIS quirks we deliberately handle:
;
; - `$INSTDIR` is the install destination — defaults to
;   `$PROGRAMFILES64\execlaw` for our perMachine install. We must
;   double-quote it on every `ExecWait` because it can contain
;   spaces (the default contains "Program Files").
; - `$PROFILE` resolves to the install-time admin's profile dir
;   (e.g. `C:\Users\Admin`). We pass it as the `--db` parent so the
;   SCM service (LocalSystem) reads/writes the SAME path the operator
;   sees when they click "Open data folder" in the tray. SYSTEM has
;   full ACL access to any user profile by default, so this works.
; - `ExecWait` returns exit code in $0 — we don't bail on nonzero
;   because `service install` on top of an existing install is
;   harmless (idempotent) and `service uninstall` on a missing
;   service is also harmless. We log via DetailPrint either way; the
;   operator can read the install log if anything goes wrong.

!include "WordFunc.nsh"

; -----------------------------------------------------------------
; Post-install: register the SCM service + start it.
; -----------------------------------------------------------------
!macro NSIS_HOOK_POSTINSTALL
    DetailPrint "Registering execlaw with the Windows Service Control Manager…"

    ; Make sure the per-user data directory exists for LocalSystem
    ; to find on first run. `service install` creates the path lazily,
    ; but pre-creating it makes the install log cleaner and surfaces
    ; ACL surprises here rather than at first server start.
    CreateDirectory "$PROFILE\.execlaw"

    ; `service install` reads the optional `--db` arg verbatim. We
    ; pin it to the install-time admin's `%USERPROFILE%\.execlaw\`
    ; so the SCM-installed service (running as LocalSystem) writes
    ; to the same path the operator sees in their own profile when
    ; they click the tray's "Open data folder" item.
    nsExec::ExecToLog '"$INSTDIR\execlaw.exe" service install --system --db "$PROFILE\.execlaw\execlaw.db"'
    Pop $0
    ${If} $0 != 0
        DetailPrint "execlaw service install returned non-zero exit ($0) — continuing."
    ${EndIf}

    DetailPrint "Starting execlaw service…"
    nsExec::ExecToLog '"$INSTDIR\execlaw.exe" service start --system'
    Pop $0
    ${If} $0 != 0
        DetailPrint "execlaw service start returned non-zero exit ($0) — the tray will show the current SCM state."
    ${EndIf}

    DetailPrint "execlaw service registered. The tray app will reflect the live SCM state."
!macroend

; -----------------------------------------------------------------
; Pre-uninstall: stop the service and deregister it before we
; remove the binaries from $INSTDIR. If we removed the binaries
; first, sc.exe would still hold an SCM entry pointing at a missing
; image path — Windows would refuse to start the service on next
; boot, and the entry would linger until manual `sc.exe delete`.
; -----------------------------------------------------------------
!macro NSIS_HOOK_PREUNINSTALL
    DetailPrint "Stopping execlaw service…"
    nsExec::ExecToLog '"$INSTDIR\execlaw.exe" service stop --system'
    Pop $0
    ; `service stop` returns nonzero if the service was already
    ; stopped or never installed; both are fine for an uninstall.
    ${If} $0 != 0
        DetailPrint "execlaw service stop returned non-zero ($0) — likely already stopped."
    ${EndIf}

    DetailPrint "Deregistering execlaw service from the SCM…"
    nsExec::ExecToLog '"$INSTDIR\execlaw.exe" service uninstall --system'
    Pop $0
    ${If} $0 != 0
        DetailPrint "execlaw service uninstall returned non-zero ($0) — likely already absent."
    ${EndIf}
!macroend

; -----------------------------------------------------------------
; Empty no-op hooks for completeness — Tauri's template `!include`s
; this file unconditionally and references all four macros. Leaving
; the unused ones undefined would fail the NSIS compile.
; -----------------------------------------------------------------
!macro NSIS_HOOK_PREINSTALL
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
!macroend
