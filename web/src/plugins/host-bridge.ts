/**
 * Host-bridge installation.
 *
 * Installs `globalThis.execlawHost` exactly once at SPA boot so any
 * plugin panel loaded dynamically via `DynamicPluginPanel` can reach
 * the host's React + helpers + shared components through a single
 * stable global. The plugin's own bundle externalises React,
 * ReactDOM, and `@execlaw/plugin-ui` — the esbuild wrapper used by
 * `scripts/build-plugin-ui.sh` rewrites those imports into reads
 * against this global, so plugins never ship their own React copy
 * and the "Invalid hook call" duplicate-React crash is structurally
 * impossible.
 *
 * Authentication: this module is host code (runs inside the SPA
 * bundle, not inside a plugin bundle). The bridge constructor
 * accepts a `getAccessToken` callback so we don't have to import
 * `useAuth` at module load (it requires a React context provider
 * higher in the tree).
 *
 * Idempotent: calling `installHostBridge` more than once is a no-op
 * — the first installation wins. The SPA's `main.tsx` calls it
 * once near the top of `<App>` rendering; HMR-driven re-renders
 * during development are harmless.
 *
 * @module host-bridge
 */

import * as React from "react";
import * as ReactDOM from "react-dom";
import { useCallback, useEffect, useRef, useState } from "react";

import type { BridgeApi, BridgeComponents } from "./types";
import { ErrorBanner } from "../components/ErrorBanner";
import { SidecarStatusBlock } from "../components/SidecarStatusBlock";
import Button from "react-bootstrap/Button";

/**
 * Accessor for the operator's current access token. Provided by the
 * host's `AuthContext` at install time. Synchronous (the auth
 * context keeps the latest token in a ref) — the bridge re-reads
 * it on every authenticated request so token-refresh by the
 * AuthContext background is transparent to plugins.
 */
export type GetAccessToken = () => string | null;

/**
 * Install `globalThis.execlawHost`. Safe to call multiple times.
 *
 * `getAccessToken` is the same closure exposed via `useAuth()` —
 * passing it in keeps this module free of React-context coupling
 * (it's loaded before the provider has had a chance to render).
 */
export function installHostBridge(getAccessToken: GetAccessToken): void {
    if (globalThis.execlawHost !== undefined) {
        // Idempotent: first install wins. A second call (HMR, double
        // mount, test re-render) would otherwise rebind the bridge
        // and tear down any in-flight plugin panel state.
        return;
    }
    globalThis.execlawHost = makeBridge(getAccessToken);
}

/**
 * Test seam — rebind the bridge unconditionally. Used by Vitest /
 * component tests to inject a mock `getAccessToken` between cases.
 * Production code MUST call `installHostBridge` instead.
 */
export function _replaceHostBridgeForTests(
    getAccessToken: GetAccessToken,
): void {
    globalThis.execlawHost = makeBridge(getAccessToken);
}

function makeBridge(getAccessToken: GetAccessToken): BridgeApi {
    return Object.freeze({
        React,
        ReactDOM,
        getAccessToken,
        fetchJson: makeFetchJson(getAccessToken),
        usePoll: makeUsePoll(),
        components: makeComponents(),
    });
}

function makeFetchJson(getAccessToken: GetAccessToken) {
    return async function fetchJson<T = unknown>(
        method: string,
        path: string,
        body?: unknown,
    ): Promise<T> {
        const token = getAccessToken();
        if (token === null) {
            // Operator signed out (or the AuthContext never fully
            // booted) — fail fast rather than send an unauthorised
            // request and surface a confusing 401.
            throw new Error(
                "access token unavailable — operator is signed out",
            );
        }
        const headers: Record<string, string> = {
            Authorization: `Bearer ${token}`,
        };
        let requestBody: BodyInit | null = null;
        if (body !== undefined && body !== null) {
            headers["Content-Type"] = "application/json";
            requestBody = JSON.stringify(body);
        }
        const resp = await fetch(path, {
            method: method.toUpperCase(),
            headers,
            body: requestBody,
        });
        const text = await resp.text();
        if (!resp.ok) {
            // Bubble the response body — admin routes return JSON
            // error objects (`{code, message}`) and surfacing the
            // raw text is what every existing call site does.
            throw new Error(
                `${method.toUpperCase()} ${path} → ${resp.status}: ${text}`,
            );
        }
        if (text.length === 0) {
            return undefined as unknown as T;
        }
        try {
            return JSON.parse(text) as T;
        } catch {
            // Some admin handlers return text/plain ("ok\n", etc).
            // Hand the raw text back as-is for the rare plugin that
            // explicitly asks for a non-JSON endpoint.
            return text as unknown as T;
        }
    };
}

function makeUsePoll() {
    return function usePoll<T>(
        fetcher: () => Promise<T>,
        intervalMs: number,
    ): { value: T | null; error: string | null } {
        const [value, setValue] = useState<T | null>(null);
        const [error, setError] = useState<string | null>(null);
        const fetcherRef = useRef(fetcher);
        // Keep the latest fetcher closure without retriggering the
        // interval; consumers typically inline an arrow function so
        // the reference changes every render.
        fetcherRef.current = fetcher;

        const tick = useCallback(async () => {
            try {
                const v = await fetcherRef.current();
                setValue(v);
                setError(null);
            } catch (e) {
                setError(e instanceof Error ? e.message : String(e));
            }
        }, []);

        useEffect(() => {
            void tick();
            if (intervalMs <= 0) return;
            const id = window.setInterval(() => {
                void tick();
            }, intervalMs);
            return () => window.clearInterval(id);
        }, [intervalMs, tick]);

        return { value, error };
    };
}

function makeComponents(): BridgeComponents {
    return Object.freeze({
        // The host's own components, type-erased into the bridge's
        // generic `PluginComponent<P>` contract. The bridge contract
        // is a structural-types subset of each host component's
        // actual prop set — passing them through `as unknown as ...`
        // is the standard pattern for crossing the
        // typed-host ↔ typed-plugin boundary without copying the
        // component code into the plugin bundle.
        ErrorBanner:
            ErrorBanner as unknown as BridgeComponents["ErrorBanner"],
        SidecarStatusBlock:
            SidecarStatusBlock as unknown as BridgeComponents["SidecarStatusBlock"],
        Button: Button as unknown as BridgeComponents["Button"],
    });
}
