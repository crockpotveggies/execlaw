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

/** Background refresh fires at 80% of the access-token TTL. Keeps the
 *  user from ever seeing a 401 mid-action while still rotating the
 *  refresh token regularly. The /api/login response doesn't include
 *  the TTL, so we hard-code the same 15-min window the server uses
 *  in `ServerConfig::default()`. If the operator tunes the TTL, the
 *  worst case is one in-flight 401 followed by a silent apiFetch
 *  retry — still no UX surface. */
const ACCESS_TOKEN_TTL_MS = 15 * 60 * 1000;
const REFRESH_AT_MS = Math.floor(ACCESS_TOKEN_TTL_MS * 0.8);

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

    const setTokens = useCallback((pair: TokenPair | null) => {
        accessTokenRef.current = pair?.access_token ?? null;
        refreshTokenRef.current = pair?.refresh_token ?? null;
        if (pair) saveTokens(pair);
        else clearTokens();
    }, []);

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
     *  callers behind one in-flight promise. */
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
            } catch {
                // Refresh failed → drop local state so the SPA
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

    /** Background pre-emptive refresh. Schedules a refresh at 80% of
     *  the access-token TTL so the user never sees a 401-flash on
     *  their next call. Runs only while authenticated. */
    useEffect(() => {
        if (state.status !== "authenticated") return;
        if (refreshTimerRef.current) clearTimeout(refreshTimerRef.current);
        refreshTimerRef.current = setTimeout(() => {
            void performRefresh();
        }, REFRESH_AT_MS);
        return () => {
            if (refreshTimerRef.current) {
                clearTimeout(refreshTimerRef.current);
                refreshTimerRef.current = null;
            }
        };
    }, [state.status, state.tokens, performRefresh]);

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

            // Access expired — try one refresh.
            try {
                const fresh = await postRefresh(stored.refresh_token);
                accessTokenRef.current = fresh.access_token;
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
            getAccessToken: () => accessTokenRef.current,
        }),
        [state, signIn, signOut, signOutEverywhere],
    );

    return <AuthContext.Provider value={value}>{children}</AuthContext.Provider>;
}

export function useAuth(): AuthContextValue {
    const ctx = useContext(AuthContext);
    if (!ctx) throw new Error("useAuth() called outside <AuthProvider>");
    return ctx;
}
