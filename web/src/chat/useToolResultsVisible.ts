// 2026-05-16 — operator preference for whether `tool_use` / `tool_result`
// messages render inline in the chat stream. The agent occasionally
// produces long monospace tool-output blocks (search results, raw API
// JSON) that crowd the conversation when the operator just wants to
// read the agent's prose. The toggle lives in the active-thread
// header as a popup menu and applies to every thread (persisted in
// localStorage). Default OFF — the operator-facing complaint that
// prompted this toggle was "tool results crowd the transcript by
// default"; surface them only on explicit opt-in.

import { useCallback, useEffect, useState } from "react";

const STORAGE_KEY = "execlaw.chat.tool_results_visible";

function readInitial(): boolean {
    if (typeof window === "undefined") return false;
    try {
        const raw = window.localStorage.getItem(STORAGE_KEY);
        // Missing key → default false. A previously-set value (from
        // the operator flipping the toggle) is honoured verbatim so
        // an explicit `on` survives reloads / new threads.
        if (raw === null) return false;
        return raw === "1";
    } catch {
        // Quota errors / disabled storage → fall through to default.
        return false;
    }
}

export function useToolResultsVisible(): [boolean, (next: boolean) => void] {
    const [visible, setVisibleState] = useState<boolean>(readInitial);

    const setVisible = useCallback((next: boolean) => {
        setVisibleState(next);
    }, []);

    useEffect(() => {
        try {
            window.localStorage.setItem(STORAGE_KEY, visible ? "1" : "0");
        } catch {
            // Best-effort; the in-memory state still tracks the
            // operator's choice for this session.
        }
    }, [visible]);

    return [visible, setVisible];
}
