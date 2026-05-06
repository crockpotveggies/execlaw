// Persistent approval-event WebSocket subscription.
//
// Mirrors `AlertWatcher` (see that file for the architectural notes).
// Without this, the sidebar's pending-approvals badge only refreshes
// on Sidebar remount — a cold-contact landing while the operator
// sits on Settings / Research / Routines stays invisible until they
// navigate or hard-refresh.
//
// Listens for `approval_created` / `approval_resolved` and re-syncs
// the canonical list via `GET /api/admin/approvals`. We don't try to
// patch the store from the event payload alone: server-side filtering
// (a principal that's been reconciled away IS the resolution) means
// the count math has to come from the server.

import { useEffect, useRef } from "react";
import { useAuth } from "../auth/AuthContext";
import { WsClient, type WsEvent } from "../api/ws";
import { listPendingApprovals } from "../api/endpoints";
import { setPendingApprovals } from "../chat/store";

export function ApprovalWatcher() {
    const auth = useAuth();
    const getToken = auth.getAccessToken;
    const wsRef = useRef<WsClient | null>(null);

    useEffect(() => {
        if (auth.status !== "authenticated") {
            wsRef.current?.close();
            wsRef.current = null;
            return;
        }
        const refresh = () => {
            listPendingApprovals(getToken)
                .then((r) => setPendingApprovals(r.approvals))
                .catch(() => {
                    // Silent — Sidebar's mount-time fetch picks up
                    // any transient drop on the next navigation.
                });
        };
        refresh();

        const client = new WsClient({
            accessToken: getToken,
            onEvent: (ev: WsEvent) => {
                if (
                    ev.kind === "approval_created" ||
                    ev.kind === "approval_resolved"
                ) {
                    refresh();
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
