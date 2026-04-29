// Single source of truth for the current user + their tokens.
//
// Components consume this via `useAuth()`. It holds:
//
//   - tokens     — the live access/refresh pair (or null when logged out)
//   - user       — the /api/admin/me payload, fetched once after the
//                  token pair lands
//   - status     — "loading" while we boot / probe / refresh,
//                  "authenticated" / "unauthenticated" otherwise
//
// On boot the provider:
//   1. reads tokens from localStorage,
//   2. tries `/api/admin/me` with the access token,
//   3. on 401, attempts one refresh and retries; on second failure
//      it clears tokens and reports "unauthenticated".
//
// Setup + login screens call `signIn(pair)` which re-runs the /me
// probe so the rest of the SPA gets a populated user record.

import {
    createContext,
    useCallback,
    useContext,
    useEffect,
    useMemo,
    useRef,
    useState,
    type ReactNode,
} from "react";
import { ApiError, setRefreshHook } from "../api/client";
import {
    getMe,
    postLogout,
    postLogoutAll,
    postRefresh,
    type MeResponse,
} from "../api/endpoints";
import { clearTokens, loadTokens, saveTokens, type TokenPair } from "./tokens";

export type AuthStatus = "loading" | "authenticated" | "unauthenticated";

interface AuthState {
    status: AuthStatus;
    user: MeResponse | null;
    tokens: TokenPair | null;
}

interface AuthContextValue extends AuthState {
    signIn: (pair: TokenPair) => Promise<void>;
    signOut: () => Promise<void>;
    /** Phase 7 hardening: revoke every refresh token bound to the
     *  caller across every browser/device. The local session is
     *  cleared as part of the call so the SPA bounces to /login. */
    signOutEverywhere: () => Promise<{ revokedCount: number }>;
    /** Test seam — exposes the in-memory token getter without touching React state. */
    getAccessToken: () => string | null;
}

const initialState: AuthState = {
    status: "loading",
    user: null,
    tokens: null,
};

const AuthContext = createContext<AuthContextValue | null>(null);

/** Fallback access-token TTL used only when the JWT can't be parsed
 *  (malformed token, missing `exp` claim). The actual schedule is
 *  derived from the JWT's `exp` so a server-side TTL bump is picked
 *  up automatically. */
const FALLBACK_ACCESS_TOKEN_TTL_MS = 15 * 60 * 1000;
/** How early before `exp` we attempt a pre-emptive refresh. Picks
 *  up token rotation while still leaving headroom for a slow refresh
 *  round-trip and clock skew. */
const REFRESH_LEAD_MS = 60 * 1000;

/** Decode a JWT's `exp` claim into a unix-millis timestamp. Returns
 *  `null` for malformed tokens. We do NOT verify the signature here —
 *  the server is the only authority on validity; this is purely a
 *  client-side liveness hint so the SPA can decide whether to refresh
 *  or log out before paying for an API round-trip. */
function parseJwtExpMs(token: string | null | undefined): number | null {
    if (!token) return null;
    const parts = token.split(".");
    if (parts.length !== 3) return null;
    try {
        // base64url → base64 → JSON
        const b64 = parts[1].replace(/-/g, "+").replace(/_/g, "/");
        const padded = b64 + "=".repeat((4 - (b64.length % 4)) % 4);
        const json = atob(padded);
        const obj = JSON.parse(json) as { exp?: unknown };
        if (typeof obj.exp === "number") return obj.exp * 1000;
        return null;
    } catch {
        return null;
    }
}

/** Returns `true` when the access token's `exp` is in the past
 *  (with a small skew tolerance). Used by the boot path to short-
 *  circuit an obviously-stale token without an /api/admin/me round-
 *  trip, and by the expiry timer to decide between "refresh" and
 *  "force logout". */
function isExpired(token: string | null | undefined, nowMs: number): boolean {
    const exp = parseJwtExpMs(token);
    if (exp === null) return false; // can't tell — defer to server's 401
    return exp <= nowMs + 1_000; // 1s skew tolerance
}

export function AuthProvider({ children }: { children: ReactNode }) {
    const [state, setState] = useState<AuthState>(initialState);

    // Mirror the live access token in a ref so the apiFetch token
    // accessor can read it synchronously during a request without
    // tripping a re-render.
    const accessTokenRef = useRef<string | null>(null);
    const refreshTokenRef = useRef<string | null>(null);
    /** Coalesces parallel /refresh calls when several requests 401 at
     *  once. The first caller starts the rotation; everyone else
     *  awaits the same promise. */
    const inflightRefreshRef = useRef<Promise<string | null> | null>(null);
    const refreshTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
    /// Hard-deadline timer set to the access JWT's `exp`. If it fires
    /// while still authenticated, the SPA force-logs-out — we're past
    /// the point where any API call can succeed. The pre-emptive
    /// refresh timer is supposed to rotate well before this, so this
    /// only fires when refresh failed (network down, server down,
    /// laptop slept across both windows, refresh token revoked).
    const expiryTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

    const setTokens = useCallback((pair: TokenPair | null) => {
        accessTokenRef.current = pair?.access_token ?? null;
        refreshTokenRef.current = pair?.refresh_token ?? null;
        if (pair) saveTokens(pair);
        else clearTokens();
    }, []);

    /// Stable accessor — returns the current access token via a ref
    /// read so its identity is reference-equal across every render.
    /// Consumers can pass `auth.getAccessToken` straight through to
    /// hooks / `apiFetch` without wrapping in a `useCallback`. The
    /// wrapped form (`() => auth.getAccessToken()`) was the source
    /// of the 2026-04-28 render-loop bug because it changed identity
    /// on every render that touched `auth`.
    const getAccessToken = useCallback(() => accessTokenRef.current, []);

    const fetchMe = useCallback(async (): Promise<MeResponse> => {
        return getMe(() => accessTokenRef.current);
    }, []);

    const signIn = useCallback(
        async (pair: TokenPair) => {
            setTokens(pair);
            setState({ status: "loading", user: null, tokens: pair });
            try {
                const user = await fetchMe();
                setState({ status: "authenticated", user, tokens: pair });
            } catch (e) {
                // The just-issued token didn't work — surface as
                // unauthenticated rather than wedging in "loading".
                setTokens(null);
                setState({
                    status: "unauthenticated",
                    user: null,
                    tokens: null,
                });
                throw e;
            }
        },
        [fetchMe, setTokens],
    );

    const signOut = useCallback(async () => {
        const refresh = state.tokens?.refresh_token ?? null;
        // Best-effort — a 4xx here is fine, the SPA discards local
        // state regardless.
        try {
            if (refresh) await postLogout(refresh);
        } catch {
            /* swallow */
        }
        setTokens(null);
        setState({ status: "unauthenticated", user: null, tokens: null });
    }, [setTokens, state.tokens]);

    /** Phase 7 hardening: revoke every refresh token for the caller
     *  on the server, then drop the local session. Useful when the
     *  user suspects a stolen device — every other browser tab
     *  silently bounces to /login on its next API call. */
    const signOutEverywhere = useCallback(async () => {
        let revokedCount = 0;
        try {
            const r = await postLogoutAll(() => accessTokenRef.current);
            revokedCount = r.revoked_session_count;
        } catch {
            /* swallow — local clear still happens */
        }
        setTokens(null);
        setState({ status: "unauthenticated", user: null, tokens: null });
        return { revokedCount };
    }, [setTokens]);

    /** The single refresh path used by both apiFetch's silent retry
     *  hook AND the background pre-emptive timer. Coalesces parallel
     *  callers behind one in-flight promise.
     *
     *  2026-04-28 — distinguish network failure from auth-expired:
     *    * `code === "network"` → keep tokens, return null. The
     *      ConnectionStatus banner will show "Reconnecting…"; the
     *      next user action retries naturally.
     *    * any other failure (401 unauthorized, refresh token revoked,
     *      malformed response) → clear tokens. The SPA bounces to
     *      /login, which is the correct "auth expired" UX. Previously
     *      a transient backend hiccup kicked the operator to /login
     *      and made dev mode hostile every time the rust server
     *      restarted. */
    const performRefresh = useCallback(async (): Promise<string | null> => {
        if (inflightRefreshRef.current) return inflightRefreshRef.current;
        const tok = refreshTokenRef.current;
        if (!tok) return null;
        const p = (async (): Promise<string | null> => {
            try {
                const fresh = await postRefresh(tok);
                accessTokenRef.current = fresh.access_token;
                refreshTokenRef.current = fresh.refresh_token;
                saveTokens(fresh);
                // Don't snap React state — the access_token sits
                // in the ref so any in-flight retry already sees
                // the new value. We DO need to update the visible
                // tokens slot so signOut etc. find the latest
                // refresh_token.
                setState((prev) =>
                    prev.status === "authenticated"
                        ? { ...prev, tokens: fresh }
                        : prev,
                );
                return fresh.access_token;
            } catch (e) {
                if (e instanceof ApiError && e.code === "network") {
                    // Server unreachable — leave the tokens alone,
                    // surface as "still authenticated, just offline"
                    // so the connection banner is the operator's
                    // signal, not a forced /login redirect.
                    return null;
                }
                // Real auth failure → drop local state so the SPA
                // bounces to /login.
                clearTokens();
                accessTokenRef.current = null;
                refreshTokenRef.current = null;
                setState({
                    status: "unauthenticated",
                    user: null,
                    tokens: null,
                });
                return null;
            } finally {
                inflightRefreshRef.current = null;
            }
        })();
        inflightRefreshRef.current = p;
        return p;
    }, []);

    /** Install + tear down the silent-retry hook. Lives for the
     *  AuthProvider's lifetime; nested mounts (StrictMode in dev)
     *  re-install harmlessly. */
    useEffect(() => {
        setRefreshHook(performRefresh);
        return () => {
            setRefreshHook(null);
        };
    }, [performRefresh]);

    /** Force-logout path triggered by the JWT-expiry hard deadline
     *  AND by the focus-resume sweep below. Tries one last refresh
     *  in case the user just tabbed back from a long sleep and the
     *  refresh token is still good — if that succeeds, we stay
     *  signed in. Otherwise we drop tokens and the route guard
     *  bounces to /login. We deliberately do NOT call /api/logout
     *  here: the access JWT is already past `exp` so the server
     *  would 401 the call anyway. */
    const enforceExpiry = useCallback(async (): Promise<void> => {
        if (!isExpired(accessTokenRef.current, Date.now())) {
            // Token rotated under us between the timer firing and
            // this callback running — nothing to do.
            return;
        }
        const fresh = await performRefresh();
        if (fresh) return;
        // Refresh didn't recover — drop local state. performRefresh
        // already cleared tokens on auth-fail; on network-fail we
        // kept them, but the JWT is server-expired so we still log
        // out.
        clearTokens();
        accessTokenRef.current = null;
        refreshTokenRef.current = null;
        setState({
            status: "unauthenticated",
            user: null,
            tokens: null,
        });
    }, [performRefresh]);

    /** Tab-resume guard: when the page becomes visible (laptop wake,
     *  switched tabs back) re-check the access JWT's `exp`. setTimeout
     *  doesn't fire reliably across OS sleep, so this catches the
     *  case where the hard-deadline timer was supposed to fire during
     *  sleep but didn't. */
    useEffect(() => {
        if (state.status !== "authenticated") return;
        const onResume = () => {
            if (document.visibilityState === "visible") {
                if (isExpired(accessTokenRef.current, Date.now())) {
                    void enforceExpiry();
                }
            }
        };
        document.addEventListener("visibilitychange", onResume);
        window.addEventListener("focus", onResume);
        return () => {
            document.removeEventListener("visibilitychange", onResume);
            window.removeEventListener("focus", onResume);
        };
    }, [state.status, enforceExpiry]);

    /** JWT-aware refresh schedule.
     *
     *  Two timers, both rescheduled every time `state.tokens` flips:
     *
     *    1. **Pre-emptive refresh** at `exp - REFRESH_LEAD_MS`. Picks
     *       up rotation before any user-action API call sees a 401.
     *
     *    2. **Hard expiry deadline** at `exp` itself. If reached
     *       without a successful refresh — laptop slept past the
     *       refresh window, machine offline, refresh server-side
     *       failure, anything — force a logout. After the deadline
     *       the access JWT is server-rejected anyway; staying
     *       "authenticated" in the SPA just lets the user click into
     *       broken pages and watch every fetch 401.
     *
     *  Falls back to the legacy 80% window when the access token
     *  can't be parsed (malformed, missing `exp`). */
    useEffect(() => {
        if (state.status !== "authenticated") return;
        if (refreshTimerRef.current) clearTimeout(refreshTimerRef.current);
        if (expiryTimerRef.current) clearTimeout(expiryTimerRef.current);

        const access = state.tokens?.access_token ?? null;
        const expMs = parseJwtExpMs(access);
        const now = Date.now();

        let preemptDelay: number;
        let expiryDelay: number;
        if (expMs === null) {
            // No usable exp — assume the documented 15-min TTL and
            // fire pre-emptively at 80%. The hard deadline timer
            // doesn't fire (we'd have nothing reliable to fire on).
            preemptDelay = Math.floor(FALLBACK_ACCESS_TOKEN_TTL_MS * 0.8);
            expiryDelay = -1;
        } else {
            preemptDelay = Math.max(0, expMs - now - REFRESH_LEAD_MS);
            expiryDelay = Math.max(0, expMs - now);
        }

        refreshTimerRef.current = setTimeout(() => {
            void performRefresh();
        }, preemptDelay);

        if (expiryDelay >= 0) {
            expiryTimerRef.current = setTimeout(() => {
                void enforceExpiry();
            }, expiryDelay);
        }

        return () => {
            if (refreshTimerRef.current) {
                clearTimeout(refreshTimerRef.current);
                refreshTimerRef.current = null;
            }
            if (expiryTimerRef.current) {
                clearTimeout(expiryTimerRef.current);
                expiryTimerRef.current = null;
            }
        };
    }, [state.status, state.tokens, performRefresh, enforceExpiry]);

    // Boot: try existing tokens once.
    useEffect(() => {
        let cancelled = false;
        (async () => {
            const stored = loadTokens();
            if (!stored) {
                if (!cancelled)
                    setState({
                        status: "unauthenticated",
                        user: null,
                        tokens: null,
                    });
                return;
            }
            accessTokenRef.current = stored.access_token;
            refreshTokenRef.current = stored.refresh_token;

            // 2026-04-28 — proactively check the stored access JWT's
            // `exp` before the /api/admin/me probe. If it's already
            // expired (laptop slept past TTL, page reopened from a
            // very old tab, etc.) skip the predictable 401 and go
            // straight to the refresh path. If the refresh token's
            // also expired, this flips to /login one round-trip
            // sooner.
            if (isExpired(stored.access_token, Date.now())) {
                try {
                    const fresh = await postRefresh(stored.refresh_token);
                    accessTokenRef.current = fresh.access_token;
                    refreshTokenRef.current = fresh.refresh_token;
                    saveTokens(fresh);
                    const user = await fetchMe();
                    if (!cancelled)
                        setState({
                            status: "authenticated",
                            user,
                            tokens: fresh,
                        });
                } catch (e) {
                    if (e instanceof ApiError && e.code === "network") {
                        // Server unreachable — keep tokens, surface
                        // unauthenticated for routing; the connection
                        // banner explains the offline state and the
                        // visibility-resume guard retries on focus.
                        if (!cancelled)
                            setState({
                                status: "unauthenticated",
                                user: null,
                                tokens: stored,
                            });
                        return;
                    }
                    clearTokens();
                    accessTokenRef.current = null;
                    refreshTokenRef.current = null;
                    if (!cancelled)
                        setState({
                            status: "unauthenticated",
                            user: null,
                            tokens: null,
                        });
                }
                return;
            }

            try {
                const user = await fetchMe();
                if (!cancelled)
                    setState({
                        status: "authenticated",
                        user,
                        tokens: stored,
                    });
                return;
            } catch (e) {
                if (!(e instanceof ApiError) || e.code !== "unauthorized") {
                    // Network / server error — surface as unauthenticated
                    // and let the caller retry. We don't drop the tokens
                    // here because they might still be valid once the
                    // server is reachable again.
                    if (!cancelled)
                        setState({
                            status: "unauthenticated",
                            user: null,
                            tokens: stored,
                        });
                    return;
                }
            }

            // Server-side rejected the access token (e.g. signing key
            // rotated server-side) — try one refresh.
            try {
                const fresh = await postRefresh(stored.refresh_token);
                accessTokenRef.current = fresh.access_token;
                refreshTokenRef.current = fresh.refresh_token;
                const user = await fetchMe();
                if (!cancelled) {
                    saveTokens(fresh);
                    setState({
                        status: "authenticated",
                        user,
                        tokens: fresh,
                    });
                }
            } catch {
                clearTokens();
                accessTokenRef.current = null;
                refreshTokenRef.current = null;
                if (!cancelled)
                    setState({
                        status: "unauthenticated",
                        user: null,
                        tokens: null,
                    });
            }
        })();
        return () => {
            cancelled = true;
        };
    }, [fetchMe]);

    const value = useMemo<AuthContextValue>(
        () => ({
            ...state,
            signIn,
            signOut,
            signOutEverywhere,
            getAccessToken,
        }),
        [state, signIn, signOut, signOutEverywhere, getAccessToken],
    );

    return <AuthContext.Provider value={value}>{children}</AuthContext.Provider>;
}

export function useAuth(): AuthContextValue {
    const ctx = useContext(AuthContext);
    if (!ctx) throw new Error("useAuth() called outside <AuthProvider>");
    return ctx;
}
