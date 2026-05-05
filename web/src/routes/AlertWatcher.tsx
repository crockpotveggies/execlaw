// Persistent alert-event WebSocket subscription.
//
// Why a dedicated top-level component:
//   * The Sidebar renders the alert badge on every authenticated
//     route (Chat, Settings, Research, Routines, Skills, Approvals).
//   * Pre-fix the only WsClient in the SPA lived inside Chat.tsx —
//     so navigating off /chat tore the WS down, leaving the badge
//     reliant on the 60s poll. New alerts that fired while the
//     operator was on Settings only surfaced after a manual refresh
//     (the symptom the operator hit on the OAuth-scope alert).
//   * Each per-route Sidebar is remounted on navigation; putting
//     the WS inside the Sidebar would flap the connection on every
//     click.
//
// This component is headless (returns null), mounted ONCE at the
// App level inside AuthProvider, and:
//   1. Opens a WsClient when auth is authenticated.
//   2. Listens for `alert_fired` and `alert_resolved` frames.
//   3. Re-queries the canonical firing count via getAlertCount()
//      and writes it to the chat store. (The local count math is
//      avoided on purpose — server-side dedup means a "fired"
//      event might be a no-op for an existing fingerprint.)
//   4. Closes the WS when auth flips back to unauthenticated.
//
// Chat.tsx still owns its own WsClient for chat-specific events
// (chat_message_*, conversation_phase_changed, etc.) — overlap is
// acceptable; the server's broadcast channel handles many
// subscribers cheaply.

import { useEffect, useRef } from "react";
import { useAuth } from "../auth/AuthContext";
import { WsClient, type WsEvent } from "../api/ws";
import { getAlertCount } from "../api/endpoints";
import { setAlertFiringCount } from "../chat/store";

export function AlertWatcher() {
    const auth = useAuth();
    const getToken = auth.getAccessToken;
    const wsRef = useRef<WsClient | null>(null);

    useEffect(() => {
        if (auth.status !== "authenticated") {
            // Tear down any open connection on sign-out so a fresh
            // sign-in opens a new one with the new token.
            wsRef.current?.close();
            wsRef.current = null;
            return;
        }
        const refreshCount = () => {
            getAlertCount(getToken)
                .then((r) => setAlertFiringCount(r.firing_count))
                .catch(() => {
                    // Silent — transient failures shouldn't pollute
                    // the UI; the 60s Sidebar poll catches up.
                });
        };
        // Initial sync so a freshly-loaded SPA has the count even
        // before the first WS event arrives.
        refreshCount();

        const client = new WsClient({
            accessToken: getToken,
            onEvent: (ev: WsEvent) => {
                if (ev.kind === "alert_fired" || ev.kind === "alert_resolved") {
                    refreshCount();
                }
            },
        });
        client.open();
        wsRef.current = client;
        return () => {
            client.close();
            wsRef.current = null;
        };
    }, [auth.status, getToken]);

    return null;
}
