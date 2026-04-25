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
import { ApiError } from "../api/client";
import {
    getMe,
    postLogout,
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
    /** Test seam — exposes the in-memory token getter without touching React state. */
    getAccessToken: () => string | null;
}

const initialState: AuthState = {
    status: "loading",
    user: null,
    tokens: null,
};

const AuthContext = createContext<AuthContextValue | null>(null);

export function AuthProvider({ children }: { children: ReactNode }) {
    const [state, setState] = useState<AuthState>(initialState);

    // Mirror the live access token in a ref so the apiFetch token
    // accessor can read it synchronously during a request without
    // tripping a re-render.
    const accessTokenRef = useRef<string | null>(null);

    const setTokens = useCallback((pair: TokenPair | null) => {
        accessTokenRef.current = pair?.access_token ?? null;
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
            getAccessToken: () => accessTokenRef.current,
        }),
        [state, signIn, signOut],
    );

    return <AuthContext.Provider value={value}>{children}</AuthContext.Provider>;
}

export function useAuth(): AuthContextValue {
    const ctx = useContext(AuthContext);
    if (!ctx) throw new Error("useAuth() called outside <AuthProvider>");
    return ctx;
}
