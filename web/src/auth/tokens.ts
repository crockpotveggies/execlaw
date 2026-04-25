// Persisted token store.
//
// localStorage is fine for the Phase-6 single-controller deployment:
// the app is same-origin (Vite proxy in dev, rust-embed in prod), the
// SPA bundle is fully trusted, and the platform is local-first by
// design. Phase 7 hardening evaluates moving to httpOnly cookies as
// part of the multi-user invite flow.

const ACCESS_KEY = "execlaw.access_token";
const REFRESH_KEY = "execlaw.refresh_token";

export interface TokenPair {
    access_token: string;
    refresh_token: string;
}

export function loadTokens(): TokenPair | null {
    try {
        const access = localStorage.getItem(ACCESS_KEY);
        const refresh = localStorage.getItem(REFRESH_KEY);
        if (!access || !refresh) return null;
        return { access_token: access, refresh_token: refresh };
    } catch {
        // localStorage can throw in private-browsing modes / certain
        // embedded webviews. Treat as "no tokens stored."
        return null;
    }
}

export function saveTokens(pair: TokenPair): void {
    try {
        localStorage.setItem(ACCESS_KEY, pair.access_token);
        localStorage.setItem(REFRESH_KEY, pair.refresh_token);
    } catch {
        /* swallow — storage failures shouldn't crash the SPA */
    }
}

export function clearTokens(): void {
    try {
        localStorage.removeItem(ACCESS_KEY);
        localStorage.removeItem(REFRESH_KEY);
    } catch {
        /* ditto */
    }
}
