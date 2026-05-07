// Settings → Search. Operator manages the search-provider registry
// here: which providers are configured + enabled, which is the
// rotation seed (default), and per-provider config (SearxNG base
// URL, Brave/Exa/Tavily API keys).
//
// 2026-05-04: built to give the operator a way out of DDG's bot-
// detection bouncing.
// 2026-05-06: rotation refactor — the resolver now wraps every
// enabled provider in a round-robin pool with per-provider 60s
// cooldown on 429s. The "default" still drives the rotation seed
// position; the per-row toggle decides whether a provider
// participates at all.

import { useCallback, useEffect, useMemo, useState } from "react";
import { Alert, Badge, Button, Card, Form, Spinner } from "react-bootstrap";
import { useAuth } from "../auth/AuthContext";
import {
    deleteSearchProvider,
    listSearchProviders,
    setDefaultSearchProvider,
    testSearchProvider,
    upsertSearchProvider,
    type SearchProviderView,
    type SearchTestResponse,
} from "../api/endpoints";

/// Closed list of provider kinds the host knows about. Mirrors
/// `SearchProviderKind` in `core::search_providers`. Keeping it
/// here (instead of fetching from the server) is fine because
/// adding a new provider requires both code AND UI changes — the
/// list literally can't drift.
///
/// Descriptions are kept to a single short line — the card
/// truncates with ellipsis on overflow, so any prose past ~70
/// chars gets clipped on narrow viewports. The full README-style
/// blurb lives on the provider's own docs site.
const KNOWN_KINDS: ReadonlyArray<{
    kind: string;
    display: string;
    description: string;
    /// Per-kind config field shape. The page renders these as the
    /// inputs in the edit form.
    fields: ReadonlyArray<{
        key: string;
        label: string;
        type: "text" | "url" | "password";
        placeholder: string;
        helpText: string;
    }>;
}> = [
    {
        kind: "duckduckgo",
        display: "DuckDuckGo",
        description: "Free HTML scrape. No key. Bot-detection on busy days.",
        fields: [],
    },
    {
        kind: "searxng",
        display: "SearxNG (self-hosted)",
        description: "Self-hosted meta-search aggregator. No key, no quota.",
        fields: [
            {
                key: "base_url",
                label: "Base URL",
                type: "url",
                placeholder: "https://searx.example.com",
                helpText:
                    "Root URL of your SearxNG instance — the adapter appends /search itself. Make sure JSON output is enabled (search.formats: [html, json] in settings.yml).",
            },
        ],
    },
    {
        kind: "brave",
        display: "Brave Search API",
        description: "Paid AI-tuned search. ~$5/1k; 2k/month free tier.",
        fields: [
            {
                key: "api_key",
                label: "API key",
                type: "password",
                placeholder: "sk-...",
                helpText:
                    "Your Brave Search Subscription Token. Stored server-side; the SPA receives it back when reading the provider config.",
            },
        ],
    },
    {
        kind: "exa",
        display: "Exa (neural search)",
        description: "Neural/semantic search for AI agents. Paid.",
        fields: [
            {
                key: "api_key",
                label: "API key",
                type: "password",
                placeholder: "exa-...",
                helpText:
                    "Your Exa API key, sent as the x-api-key header. Stored server-side.",
            },
        ],
    },
    {
        kind: "tavily",
        display: "Tavily (RAG-optimised)",
        description: "RAG-tuned search. 1k free credits/month.",
        fields: [
            {
                key: "api_key",
                label: "API key",
                type: "password",
                placeholder: "tvly-...",
                helpText:
                    "Your Tavily API key (typically starts with 'tvly-'), sent as a Bearer token. Stored server-side.",
            },
        ],
    },
];

interface EditingState {
    kind: string;
    enabled: boolean;
    /// String form of every config value, keyed by field name.
    /// We don't keep typed forms here — converting at submit time
    /// is one place to enforce shape.
    config: Record<string, string>;
}

/// One-line truncation style: the description lives in a `<div>`
/// that occupies whatever width the title bar gives it. Bootstrap
/// has `.text-truncate` utility but it requires a width-bounded
/// parent; we set min-width:0 on the flex item so the truncate
/// works inside `d-flex`.
const TRUNCATE_STYLE: React.CSSProperties = {
    overflow: "hidden",
    textOverflow: "ellipsis",
    whiteSpace: "nowrap",
};

export function SearchPage() {
    const { getAccessToken } = useAuth();
    const [providers, setProviders] = useState<SearchProviderView[] | null>(null);
    const [loading, setLoading] = useState(true);
    const [error, setError] = useState<string | null>(null);
    const [busy, setBusy] = useState(false);
    /// `kind` whose enabled-toggle is mid-flight. Stops a double-
    /// click from firing two upserts; keeps the switch from looking
    /// frozen while the request lands.
    const [togglingKind, setTogglingKind] = useState<string | null>(null);
    const [editing, setEditing] = useState<EditingState | null>(null);
    /// `kind` whose Test panel is currently expanded. Click "Test"
    /// to open / collapse. Only one panel open at a time so the
    /// page stays compact.
    const [testOpenFor, setTestOpenFor] = useState<string | null>(null);
    const [testQuery, setTestQuery] = useState("");
    const [testResult, setTestResult] = useState<SearchTestResponse | null>(null);
    const [testError, setTestError] = useState<string | null>(null);

    const reload = useCallback(async () => {
        setLoading(true);
        setError(null);
        try {
            const r = await listSearchProviders(getAccessToken);
            setProviders(r.providers);
        } catch (e) {
            setError(e instanceof Error ? e.message : "couldn't load providers");
        } finally {
            setLoading(false);
        }
    }, [getAccessToken]);

    useEffect(() => {
        void reload();
    }, [reload]);

    const knownByKind = useMemo(() => {
        const m = new Map<string, (typeof KNOWN_KINDS)[number]>();
        for (const k of KNOWN_KINDS) m.set(k.kind, k);
        return m;
    }, []);

    const startEdit = useCallback(
        (kind: string) => {
            const meta = knownByKind.get(kind);
            if (!meta) return;
            const existing = providers?.find((p) => p.kind === kind);
            const config: Record<string, string> = {};
            for (const f of meta.fields) {
                const v = existing?.config?.[f.key];
                config[f.key] = typeof v === "string" ? v : "";
            }
            setEditing({
                kind,
                enabled: existing?.enabled ?? true,
                config,
            });
        },
        [providers, knownByKind],
    );

    const cancelEdit = useCallback(() => {
        setEditing(null);
    }, []);

    const saveEdit = useCallback(async () => {
        if (!editing) return;
        setBusy(true);
        setError(null);
        try {
            const cfg: Record<string, unknown> = {};
            for (const [k, v] of Object.entries(editing.config)) {
                cfg[k] = v;
            }
            const existing = providers?.find((p) => p.kind === editing.kind);
            await upsertSearchProvider(
                {
                    kind: editing.kind,
                    enabled: editing.enabled,
                    is_default: existing?.is_default ?? false,
                    config: cfg,
                },
                getAccessToken,
            );
            await reload();
            setEditing(null);
        } catch (e) {
            setError(e instanceof Error ? e.message : "save failed");
        } finally {
            setBusy(false);
        }
    }, [editing, getAccessToken, providers, reload]);

    /// Quick on/off toggle from the card header. Preserves the
    /// existing config + is_default — only flips `enabled`.
    const toggleEnabled = useCallback(
        async (kind: string, next: boolean) => {
            const existing = providers?.find((p) => p.kind === kind);
            if (!existing) return;
            setTogglingKind(kind);
            setError(null);
            try {
                await upsertSearchProvider(
                    {
                        kind,
                        enabled: next,
                        is_default: existing.is_default,
                        config: existing.config ?? {},
                    },
                    getAccessToken,
                );
                await reload();
            } catch (e) {
                setError(e instanceof Error ? e.message : "toggle failed");
            } finally {
                setTogglingKind(null);
            }
        },
        [getAccessToken, providers, reload],
    );

    const promote = useCallback(
        async (kind: string) => {
            setBusy(true);
            setError(null);
            try {
                await setDefaultSearchProvider(kind, getAccessToken);
                await reload();
            } catch (e) {
                setError(e instanceof Error ? e.message : "promote failed");
            } finally {
                setBusy(false);
            }
        },
        [getAccessToken, reload],
    );

    const remove = useCallback(
        async (kind: string) => {
            // Defensive: never let the operator delete the active
            // default — they'd be left with no working provider.
            const row = providers?.find((p) => p.kind === kind);
            if (row?.is_default) {
                setError(
                    "Can't delete the active default. Promote a different provider first.",
                );
                return;
            }
            // eslint-disable-next-line no-alert -- intentional confirm dialog
            if (!window.confirm(`Delete ${row?.display_name ?? kind}?`)) return;
            setBusy(true);
            setError(null);
            try {
                await deleteSearchProvider(kind, getAccessToken);
                await reload();
            } catch (e) {
                setError(e instanceof Error ? e.message : "delete failed");
            } finally {
                setBusy(false);
            }
        },
        [getAccessToken, providers, reload],
    );

    /// Toggle the Test panel for `kind`. Closing clears any prior
    /// result/error so reopening starts clean.
    const toggleTestPanel = useCallback((kind: string) => {
        setTestOpenFor((prev) => {
            if (prev === kind) {
                setTestQuery("");
                setTestResult(null);
                setTestError(null);
                return null;
            }
            // Switching to a different provider — reset state.
            setTestQuery("");
            setTestResult(null);
            setTestError(null);
            return kind;
        });
    }, []);

    const runTest = useCallback(
        async (kind: string) => {
            if (!testQuery.trim()) {
                setTestError("enter a query first");
                return;
            }
            setBusy(true);
            setTestError(null);
            setTestResult(null);
            try {
                const r = await testSearchProvider(
                    kind,
                    testQuery.trim(),
                    getAccessToken,
                );
                setTestResult(r);
            } catch (e) {
                setTestError(e instanceof Error ? e.message : "test failed");
            } finally {
                setBusy(false);
            }
        },
        [getAccessToken, testQuery],
    );

    const configuredKinds = useMemo(
        () => new Set((providers ?? []).map((p) => p.kind)),
        [providers],
    );

    /// Order the cards: active default first (most-relevant for the
    /// operator who's looking at this page because something with
    /// the active provider isn't working), then any other configured
    /// providers, then unconfigured ones in the canonical
    /// KNOWN_KINDS order. Stable across renders so cards don't
    /// shuffle while the user is reading.
    const orderedKinds = useMemo(() => {
        const defaultKind = (providers ?? []).find((p) => p.is_default)?.kind;
        const score = (kind: string): number => {
            if (kind === defaultKind) return 0;
            if (configuredKinds.has(kind)) return 1;
            return 2;
        };
        return [...KNOWN_KINDS].sort((a, b) => {
            const sa = score(a.kind);
            const sb = score(b.kind);
            if (sa !== sb) return sa - sb;
            // Same priority bucket → preserve canonical order.
            return KNOWN_KINDS.indexOf(a) - KNOWN_KINDS.indexOf(b);
        });
    }, [providers, configuredKinds]);

    return (
        <div className="execlaw-settings__page" data-testid="settings-search">
            <header className="mb-3">
                <h3 className="h5 mb-1">
                    <i className="bi bi-search me-2" aria-hidden />
                    Search providers
                </h3>
                <div className="execlaw-muted small">
                    Enable any combination of providers — the agent rotates
                    across them, parking any that hit a 429 / quota limit for
                    60s. The default sets the rotation's seed position.
                </div>
            </header>

            {loading && (
                <div className="d-flex align-items-center execlaw-muted">
                    <Spinner animation="border" size="sm" className="me-2" />
                    Loading…
                </div>
            )}
            {error && (
                <Alert variant="danger" data-testid="search-error">
                    {error}
                </Alert>
            )}

            {!loading && (
                <div className="d-flex flex-column gap-3">
                    {orderedKinds.map((meta) => {
                        const row = providers?.find((p) => p.kind === meta.kind);
                        const configured = configuredKinds.has(meta.kind);
                        const isDefault = row?.is_default ?? false;
                        const enabled = row?.enabled ?? false;
                        const isToggling = togglingKind === meta.kind;
                        const isTestOpen = testOpenFor === meta.kind;
                        return (
                            <Card
                                key={meta.kind}
                                data-testid={`provider-card-${meta.kind}`}
                            >
                                <Card.Body>
                                    <div className="d-flex justify-content-between align-items-center mb-2 gap-3">
                                        <div className="flex-grow-1" style={{ minWidth: 0 }}>
                                            <h5 className="h6 mb-1 d-flex align-items-center gap-2">
                                                <span>{meta.display}</span>
                                                {isDefault && (
                                                    <Badge
                                                        bg="success"
                                                        data-testid={`provider-default-${meta.kind}`}
                                                    >
                                                        Default
                                                    </Badge>
                                                )}
                                                {configured && !isDefault && enabled && (
                                                    <Badge bg="secondary">
                                                        Enabled
                                                    </Badge>
                                                )}
                                                {configured && !enabled && (
                                                    <Badge bg="light" text="dark">
                                                        Disabled
                                                    </Badge>
                                                )}
                                            </h5>
                                            <div
                                                className="execlaw-muted small"
                                                style={TRUNCATE_STYLE}
                                                title={meta.description}
                                            >
                                                {meta.description}
                                            </div>
                                        </div>
                                        <div className="d-flex gap-2 align-items-center flex-shrink-0">
                                            {!configured && (
                                                <Button
                                                    size="sm"
                                                    variant="primary"
                                                    onClick={() =>
                                                        startEdit(meta.kind)
                                                    }
                                                    data-testid={`provider-add-${meta.kind}`}
                                                >
                                                    Configure
                                                </Button>
                                            )}
                                            {configured && (
                                                <>
                                                    {!isDefault && (
                                                        <Button
                                                            size="sm"
                                                            variant="outline-secondary"
                                                            onClick={() =>
                                                                promote(meta.kind)
                                                            }
                                                            disabled={busy}
                                                            data-testid={`provider-promote-${meta.kind}`}
                                                        >
                                                            Make default
                                                        </Button>
                                                    )}
                                                    <Button
                                                        size="sm"
                                                        variant="outline-secondary"
                                                        onClick={() =>
                                                            startEdit(meta.kind)
                                                        }
                                                        disabled={busy}
                                                        data-testid={`provider-edit-${meta.kind}`}
                                                    >
                                                        Edit
                                                    </Button>
                                                    <Button
                                                        size="sm"
                                                        variant={
                                                            isTestOpen
                                                                ? "primary"
                                                                : "outline-secondary"
                                                        }
                                                        onClick={() =>
                                                            toggleTestPanel(
                                                                meta.kind,
                                                            )
                                                        }
                                                        disabled={busy}
                                                        aria-expanded={isTestOpen}
                                                        data-testid={`provider-test-toggle-${meta.kind}`}
                                                    >
                                                        Test
                                                    </Button>
                                                    {!isDefault && (
                                                        <Button
                                                            size="sm"
                                                            variant="outline-danger"
                                                            onClick={() =>
                                                                remove(meta.kind)
                                                            }
                                                            disabled={busy}
                                                            data-testid={`provider-delete-${meta.kind}`}
                                                        >
                                                            Delete
                                                        </Button>
                                                    )}
                                                    <Form.Check
                                                        type="switch"
                                                        id={`provider-switch-${meta.kind}`}
                                                        checked={enabled}
                                                        disabled={isToggling || busy}
                                                        onChange={(e) =>
                                                            void toggleEnabled(
                                                                meta.kind,
                                                                e.target.checked,
                                                            )
                                                        }
                                                        aria-label={`${enabled ? "Disable" : "Enable"} ${meta.display}`}
                                                        data-testid={`provider-toggle-${meta.kind}`}
                                                        className="ms-1"
                                                    />
                                                </>
                                            )}
                                        </div>
                                    </div>

                                    {editing?.kind === meta.kind && (
                                        <div
                                            className="border-top pt-3 mt-2"
                                            data-testid={`provider-editor-${meta.kind}`}
                                        >
                                            {meta.fields.length === 0 && (
                                                <div className="execlaw-muted small mb-2">
                                                    This provider has no
                                                    configurable fields.
                                                </div>
                                            )}
                                            {meta.fields.map((f) => (
                                                <Form.Group
                                                    key={f.key}
                                                    className="mb-2"
                                                >
                                                    <Form.Label className="execlaw-muted small mb-1">
                                                        {f.label}
                                                    </Form.Label>
                                                    <Form.Control
                                                        type={f.type}
                                                        placeholder={
                                                            f.placeholder
                                                        }
                                                        value={
                                                            editing.config[
                                                                f.key
                                                            ] ?? ""
                                                        }
                                                        onChange={(e) =>
                                                            setEditing((prev) =>
                                                                prev
                                                                    ? {
                                                                          ...prev,
                                                                          config: {
                                                                              ...prev.config,
                                                                              [f.key]:
                                                                                  e
                                                                                      .target
                                                                                      .value,
                                                                          },
                                                                      }
                                                                    : prev,
                                                            )
                                                        }
                                                        data-testid={`provider-field-${meta.kind}-${f.key}`}
                                                    />
                                                    <Form.Text className="execlaw-muted">
                                                        {f.helpText}
                                                    </Form.Text>
                                                </Form.Group>
                                            ))}
                                            <Form.Check
                                                type="checkbox"
                                                id={`provider-enabled-${meta.kind}`}
                                                label="Enabled"
                                                checked={editing.enabled}
                                                onChange={(e) =>
                                                    setEditing((prev) =>
                                                        prev
                                                            ? {
                                                                  ...prev,
                                                                  enabled:
                                                                      e.target
                                                                          .checked,
                                                              }
                                                            : prev,
                                                    )
                                                }
                                                className="mb-2"
                                            />
                                            <div className="d-flex gap-2">
                                                <Button
                                                    size="sm"
                                                    variant="primary"
                                                    onClick={saveEdit}
                                                    disabled={busy}
                                                    data-testid={`provider-save-${meta.kind}`}
                                                >
                                                    Save
                                                </Button>
                                                <Button
                                                    size="sm"
                                                    variant="outline-secondary"
                                                    onClick={cancelEdit}
                                                    disabled={busy}
                                                >
                                                    Cancel
                                                </Button>
                                            </div>
                                        </div>
                                    )}

                                    {isTestOpen && (
                                        <div
                                            className="border-top pt-3 mt-2"
                                            data-testid={`provider-test-panel-${meta.kind}`}
                                        >
                                            <div className="d-flex gap-2 align-items-end mb-2">
                                                <Form.Group className="flex-grow-1">
                                                    <Form.Label className="execlaw-muted small mb-1">
                                                        Test query
                                                    </Form.Label>
                                                    <Form.Control
                                                        type="text"
                                                        placeholder="rust async patterns"
                                                        value={testQuery}
                                                        onChange={(e) =>
                                                            setTestQuery(
                                                                e.target.value,
                                                            )
                                                        }
                                                        onKeyDown={(e) => {
                                                            if (
                                                                e.key === "Enter"
                                                            ) {
                                                                e.preventDefault();
                                                                void runTest(
                                                                    meta.kind,
                                                                );
                                                            }
                                                        }}
                                                        autoFocus
                                                        data-testid={`provider-test-input-${meta.kind}`}
                                                    />
                                                </Form.Group>
                                                <Button
                                                    size="sm"
                                                    variant="primary"
                                                    onClick={() =>
                                                        runTest(meta.kind)
                                                    }
                                                    disabled={busy}
                                                    data-testid={`provider-run-test-${meta.kind}`}
                                                >
                                                    Run
                                                </Button>
                                            </div>
                                            {testError && (
                                                <Alert
                                                    variant="danger"
                                                    data-testid={`provider-test-error-${meta.kind}`}
                                                >
                                                    {testError}
                                                </Alert>
                                            )}
                                            {testResult && (
                                                <div
                                                    data-testid={`provider-test-results-${meta.kind}`}
                                                >
                                                    <div className="execlaw-muted small mb-2">
                                                        {testResult.results.length}{" "}
                                                        results in{" "}
                                                        {testResult.elapsed_ms}
                                                        ms
                                                    </div>
                                                    <ul className="list-unstyled small mb-0">
                                                        {testResult.results
                                                            .slice(0, 5)
                                                            .map((h, i) => (
                                                                <li
                                                                    key={`${h.url}-${i}`}
                                                                    className="mb-2"
                                                                >
                                                                    <a
                                                                        href={h.url}
                                                                        target="_blank"
                                                                        rel="noopener noreferrer"
                                                                    >
                                                                        {h.title}
                                                                    </a>
                                                                    {h.snippet && (
                                                                        <div className="execlaw-muted">
                                                                            {h.snippet}
                                                                        </div>
                                                                    )}
                                                                </li>
                                                            ))}
                                                    </ul>
                                                </div>
                                            )}
                                        </div>
                                    )}
                                </Card.Body>
                            </Card>
                        );
                    })}
                </div>
            )}
        </div>
    );
}
