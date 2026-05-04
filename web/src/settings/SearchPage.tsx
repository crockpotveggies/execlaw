// Settings → Search. Operator manages the search-provider registry
// here: which providers are configured, which is the default for
// the deep-research gather phase + the agent's web_search tool, and
// per-provider config (SearxNG base URL, Brave API key).
//
// Built 2026-05-04 to give the operator a way out of DDG's bot-
// detection bouncing — the previous design had a free-text
// "default_search_provider" field on the Research page that pointed
// at a "Settings → Search" page that didn't yet exist.

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
        description:
            "HTML scrape of html.duckduckgo.com. No config. Free, no API key, but rate-limits aggressively on busy sessions and serves a bot-detection page when flagged. Default on first boot.",
        fields: [],
    },
    {
        kind: "searxng",
        display: "SearxNG (self-hosted)",
        description:
            "Self-hosted meta-search. Run a SearxNG container alongside execlaw, point this at it. Aggregates results from Google, Bing, DDG, Wikipedia. No shared rate-limit ceiling, no API key. Aligns with execlaw's no-mandatory-external-services rule.",
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
        description:
            "Paid (~$5/1k queries; free tier of 2k/month). Purpose-built for AI/agent use. Fast, no scraping, no rate-limit roulette. Get a key at search.brave.com.",
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
        description:
            "Neural / semantic web search built for AI agents. Returns higher-signal results than keyword engines for natural-language queries. Includes extracted page text + highlights so the gather worker doesn't have to re-fetch every URL. Paid by query + content fee. Get a key at exa.ai.",
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
        description:
            "Search API purpose-built for LLM/RAG pipelines. Free tier (1k credits/month) is enough for personal research workloads. Returns clean per-result snippets; we skip the optional answer summary since gather has its own synthesizer. Get a key at tavily.com.",
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

export function SearchPage() {
    const { getAccessToken } = useAuth();
    const [providers, setProviders] = useState<SearchProviderView[] | null>(null);
    const [loading, setLoading] = useState(true);
    const [error, setError] = useState<string | null>(null);
    const [busy, setBusy] = useState(false);
    const [editing, setEditing] = useState<EditingState | null>(null);
    const [testFor, setTestFor] = useState<string | null>(null);
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
                    Pick the search provider the deep-research gather phase
                    and the agent's <code>web_search</code> tool use. Switch
                    away from DuckDuckGo if you're hitting bot-detection
                    bounces.
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
                        return (
                            <Card
                                key={meta.kind}
                                data-testid={`provider-card-${meta.kind}`}
                            >
                                <Card.Body>
                                    <div className="d-flex justify-content-between align-items-start mb-2">
                                        <div>
                                            <h5 className="h6 mb-1">
                                                {meta.display}
                                                {isDefault && (
                                                    <Badge
                                                        bg="success"
                                                        className="ms-2"
                                                        data-testid={`provider-default-${meta.kind}`}
                                                    >
                                                        Active
                                                    </Badge>
                                                )}
                                                {configured && !isDefault && (
                                                    <Badge
                                                        bg="secondary"
                                                        className="ms-2"
                                                    >
                                                        Configured
                                                    </Badge>
                                                )}
                                            </h5>
                                            <div className="execlaw-muted small">
                                                {meta.description}
                                            </div>
                                        </div>
                                        <div className="d-flex gap-2">
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
                                                    {!isDefault && (
                                                        <Button
                                                            size="sm"
                                                            variant="success"
                                                            onClick={() =>
                                                                promote(meta.kind)
                                                            }
                                                            disabled={busy}
                                                            data-testid={`provider-promote-${meta.kind}`}
                                                        >
                                                            Set as default
                                                        </Button>
                                                    )}
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

                                    {configured && (
                                        <div
                                            className="border-top pt-3 mt-2"
                                            data-testid={`provider-test-${meta.kind}`}
                                        >
                                            <div className="d-flex gap-2 align-items-end mb-2">
                                                <Form.Group className="flex-grow-1">
                                                    <Form.Label className="execlaw-muted small mb-1">
                                                        Test query
                                                    </Form.Label>
                                                    <Form.Control
                                                        type="text"
                                                        placeholder="rust async patterns"
                                                        value={
                                                            testFor === meta.kind
                                                                ? testQuery
                                                                : ""
                                                        }
                                                        onChange={(e) => {
                                                            setTestFor(meta.kind);
                                                            setTestQuery(
                                                                e.target.value,
                                                            );
                                                        }}
                                                        onFocus={() => {
                                                            if (
                                                                testFor !==
                                                                meta.kind
                                                            ) {
                                                                setTestFor(
                                                                    meta.kind,
                                                                );
                                                                setTestResult(
                                                                    null,
                                                                );
                                                                setTestError(
                                                                    null,
                                                                );
                                                            }
                                                        }}
                                                    />
                                                </Form.Group>
                                                <Button
                                                    size="sm"
                                                    variant="outline-primary"
                                                    onClick={() =>
                                                        runTest(meta.kind)
                                                    }
                                                    disabled={busy}
                                                    data-testid={`provider-run-test-${meta.kind}`}
                                                >
                                                    Run test
                                                </Button>
                                            </div>
                                            {testFor === meta.kind &&
                                                testError && (
                                                    <Alert
                                                        variant="danger"
                                                        data-testid={`provider-test-error-${meta.kind}`}
                                                    >
                                                        {testError}
                                                    </Alert>
                                                )}
                                            {testFor === meta.kind &&
                                                testResult && (
                                                    <div
                                                        data-testid={`provider-test-results-${meta.kind}`}
                                                    >
                                                        <div className="execlaw-muted small mb-2">
                                                            {
                                                                testResult.results
                                                                    .length
                                                            }{" "}
                                                            results in{" "}
                                                            {
                                                                testResult.elapsed_ms
                                                            }
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
                                                                            href={
                                                                                h.url
                                                                            }
                                                                            target="_blank"
                                                                            rel="noopener noreferrer"
                                                                        >
                                                                            {
                                                                                h.title
                                                                            }
                                                                        </a>
                                                                        {h.snippet && (
                                                                            <div className="execlaw-muted">
                                                                                {
                                                                                    h.snippet
                                                                                }
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
