// Thin WebSocket subscriber for `/api/stream`.
//
// The Rust server broadcasts UI events (chat tokens, thread state
// changes, alerts, …) over a single WS endpoint. This module:
//
//   - opens a connection (auth token in query string — same origin, OK
//     for Phase-6 single-controller; Phase-7 hardening moves to an
//     Authorization-style upgrade header once we have refresh-token
//     binding),
//   - reconnects with capped exponential backoff on close,
//   - dispatches each parsed event to a single subscriber callback.
//
// We deliberately do NOT pull a lib (e.g., reconnecting-websocket)
// for this — the surface is small and the dependency footprint stays
// honest.

import { reportWsState } from "./connection";

export interface WsEvent {
    /** Event kind tag (matches server-side `UiEvent` enum). */
    kind: string;
    /** Optional conversation id the event belongs to. */
    conversation_id?: string;
    /** Free-form payload — shape depends on `kind`. */
    [field: string]: unknown;
}

export type WsListener = (event: WsEvent) => void;

interface ConnectionOpts {
    /// Live accessor that returns the current access token. Called
    /// on every reconnect so a token-rotation by AuthContext (silent
    /// retry / background refresh / manual signIn) propagates to
    /// the next WS handshake. Pre-Phase-8.7 the constructor took a
    /// static snapshot, which left stale tokens stuck against a
    /// restarted backend — every reconnect failed with 401 forever.
    accessToken: () => string | null;
    onEvent: WsListener;
    /** Override the WS URL (mostly for tests). */
    urlOverride?: string;
}

const MIN_BACKOFF_MS = 500;
const MAX_BACKOFF_MS = 30_000;

export class WsClient {
    private socket: WebSocket | null = null;
    private listener: WsListener;
    private tokenAccessor: () => string | null;
    private urlOverride: string | undefined;
    private backoffMs = MIN_BACKOFF_MS;
    private closed = false;
    private reconnectTimer: ReturnType<typeof setTimeout> | null = null;

    constructor(opts: ConnectionOpts) {
        this.tokenAccessor = opts.accessToken;
        this.listener = opts.onEvent;
        this.urlOverride = opts.urlOverride;
    }

    /** Open the connection. Idempotent — calling twice is a no-op. */
    open(): void {
        if (this.socket || this.closed) return;
        const url = this.buildUrl();
        try {
            this.socket = new WebSocket(url);
        } catch {
            reportWsState("reconnecting");
            this.scheduleReconnect();
            return;
        }
        this.socket.onopen = () => {
            // Reset backoff on a successful handshake.
            this.backoffMs = MIN_BACKOFF_MS;
            reportWsState("open");
        };
        this.socket.onmessage = (ev) => {
            this.handleRawMessage(ev.data);
        };
        this.socket.onclose = () => {
            this.socket = null;
            if (this.closed) {
                // Deliberate close — caller is shutting the SPA down
                // or navigating off the chat shell. Drop back to
                // `idle` so the banner doesn't surface.
                reportWsState("idle");
                return;
            }
            // Auto-reconnect path: surface as `reconnecting` until
            // the next `open` lands or the operator gives up.
            reportWsState("reconnecting");
            this.scheduleReconnect();
        };
        this.socket.onerror = () => {
            // No-op — `onclose` always fires after this.
        };
    }

    /**
     * Send a binary audio frame upstream — Phase 13.A voice mode.
     *
     * Returns `true` when the socket was open and the bytes were
     * queued, `false` otherwise. Callers (the voice composer)
     * should drop frames silently when this returns false; the
     * agent's voice pipeline tolerates short audio gaps via VAD.
     *
     * Reconnect behaviour: this method does NOT auto-buffer for a
     * pending reconnect. Audio that fires before the socket
     * stabilises is dropped on the floor — buffering would be a
     * latency footgun that disagrees with the VAD's
     * end-of-utterance heuristic.
     */
    sendBinary(bytes: ArrayBuffer | Uint8Array): boolean {
        if (!this.socket || this.socket.readyState !== WebSocket.OPEN) {
            return false;
        }
        try {
            this.socket.send(bytes);
            return true;
        } catch {
            return false;
        }
    }

    /**
     * Send a text control message upstream — Phase 13.C voice mode.
     *
     * Used for `voice_stop` (mic toggled off, finalize transcript)
     * and `voice_interrupt` (operator barged in mid-reply). Server
     * side is `crate::events::handle_voice_control`. JSON is
     * stringified once here so callers can pass the structured
     * shape.
     *
     * Returns `false` when the socket isn't open. Voice control
     * is fire-and-forget; the SPA shouldn't retry.
     */
    sendText(payload: object): boolean {
        if (!this.socket || this.socket.readyState !== WebSocket.OPEN) {
            return false;
        }
        try {
            this.socket.send(JSON.stringify(payload));
            return true;
        } catch {
            return false;
        }
    }

    /** Close the connection permanently. Disables auto-reconnect. */
    close(): void {
        this.closed = true;
        reportWsState("idle");
        if (this.reconnectTimer !== null) {
            clearTimeout(this.reconnectTimer);
            this.reconnectTimer = null;
        }
        if (!this.socket) return;
        const sock = this.socket;
        this.socket = null;
        // 2026-04-28 — StrictMode-friendly close: never yank a
        // socket out of CONNECTING. React 18 dev mounts every
        // effect twice (mount → cleanup → mount), and the first
        // cleanup fires within ~1ms — well before the WS handshake
        // can complete. Calling `close()` on a CONNECTING socket
        // makes Firefox surface a console error
        // ("interrupted while the page was loading" /
        //  "can't establish a connection"). The errors are cosmetic
        // — the second mount opens its own socket that lives — but
        // they're noisy. Defer the close until the handshake lands;
        // the supervisor's `handle_socket` sees a clean Close frame
        // instead of an abort and a one-line warn-level log
        // disappears server-side too.
        if (sock.readyState === WebSocket.CONNECTING) {
            sock.onopen = () => {
                try {
                    sock.close();
                } catch {
                    /* ignore */
                }
            };
            sock.onmessage = null;
            sock.onerror = null;
            sock.onclose = null;
            return;
        }
        sock.onopen = null;
        sock.onmessage = null;
        sock.onclose = null;
        sock.onerror = null;
        try {
            sock.close();
        } catch {
            /* ignore */
        }
    }

    /** Test seam — exposed only to make unit tests deterministic. */
    handleRawMessage(raw: unknown): void {
        if (typeof raw !== "string") return;
        let parsed: unknown;
        try {
            parsed = JSON.parse(raw);
        } catch {
            return;
        }
        if (
            typeof parsed === "object" &&
            parsed !== null &&
            "kind" in parsed &&
            typeof (parsed as { kind: unknown }).kind === "string"
        ) {
            this.listener(parsed as WsEvent);
        }
    }

    private buildUrl(): string {
        if (this.urlOverride) return this.urlOverride;
        const proto = window.location.protocol === "https:" ? "wss:" : "ws:";
        const base = `${proto}//${window.location.host}/api/stream`;
        const token = this.tokenAccessor();
        if (token) {
            const qs = new URLSearchParams();
            qs.set("token", token);
            return `${base}?${qs.toString()}`;
        }
        return base;
    }

    private scheduleReconnect(): void {
        if (this.closed) return;
        if (this.reconnectTimer !== null) return;
        const delay = this.backoffMs;
        this.reconnectTimer = setTimeout(() => {
            this.reconnectTimer = null;
            this.open();
        }, delay);
        this.backoffMs = Math.min(this.backoffMs * 2, MAX_BACKOFF_MS);
    }
}
