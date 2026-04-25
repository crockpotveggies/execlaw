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
            this.scheduleReconnect();
            return;
        }
        this.socket.onopen = () => {
            // Reset backoff on a successful handshake.
            this.backoffMs = MIN_BACKOFF_MS;
        };
        this.socket.onmessage = (ev) => {
            this.handleRawMessage(ev.data);
        };
        this.socket.onclose = () => {
            this.socket = null;
            if (!this.closed) this.scheduleReconnect();
        };
        this.socket.onerror = () => {
            // No-op — `onclose` always fires after this.
        };
    }

    /** Close the connection permanently. Disables auto-reconnect. */
    close(): void {
        this.closed = true;
        if (this.reconnectTimer !== null) {
            clearTimeout(this.reconnectTimer);
            this.reconnectTimer = null;
        }
        if (this.socket) {
            this.socket.onopen = null;
            this.socket.onmessage = null;
            this.socket.onclose = null;
            this.socket.onerror = null;
            try {
                this.socket.close();
            } catch {
                /* ignore */
            }
            this.socket = null;
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
