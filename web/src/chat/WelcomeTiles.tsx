// WelcomeTiles — single-active-tile picker that sits below the
// composer on the new-chat view.
//
// 2026-05-21 — initial iteration shipped a 2-column tile GRID
// showing all enabled tiles at once with a customise popover for
// visibility. Felt cluttered. Same day — restructured to a pill
// nav + ONE active tile at a time:
//
//   * Horizontal pill nav lets the operator pick between tiles.
//   * Only the selected tile renders below; the rest are hidden.
//   * Tile + container backgrounds are transparent — no double-
//     card stacking. Sub-elements (metric cells, thread rows,
//     prompt buttons) keep their subtle `$bg-elev` fill so they
//     still read as discrete clickable affordances.
//   * Active tile is persisted to localStorage so reloads stay on
//     the operator's pick. Defaults to "todays-brief" — the
//     LLM-tool-shaped daily summary — for first-run operators.
//
// Tiles in the picker (in order):
//   1. TODAYS_BRIEF      (default)
//   2. MISSION_CONTROL
//   3. QUICK_PROMPTS
//
// (RECENT_THREADS used to live here too; removed 2026-05-21 as
// redundant — recent threads are already in the sidebar nav.)

import {
    useCallback,
    useEffect,
    useMemo,
    useRef,
    useState,
} from "react";
import { useGSAP } from "@gsap/react";
import gsap from "gsap";
import { Link } from "react-router-dom";
import {
    getPythonSandbox,
    listAlerts,
    listPendingApprovals,
    listThreads,
} from "../api/endpoints";
import {
    getAutomationMetrics,
    type AutomationMetrics,
} from "../api/automations";
import type { InlineAttachment } from "../api/endpoints";

// ---- Shared types --------------------------------------------------

interface TileProps {
    onSend: (
        text: string,
        attachments: InlineAttachment[],
        skillNames: string[],
    ) => Promise<void> | void;
    getToken: () => string | null;
}

interface TileDef {
    id: string;
    /** Short label shown in the pill nav. */
    label: string;
    /** Bootstrap-icons name (sans `bi-` prefix) — surfaces in the
     *  pill nav next to the label. */
    icon: string;
    Component: React.ComponentType<TileProps>;
}

// ---- Active-tile preference (localStorage) -------------------------

const ACTIVE_TILE_KEY = "execlaw:welcome-tile-active";
const DEFAULT_TILE_ID = "todays-brief";

function readActiveTile(): string {
    try {
        return localStorage.getItem(ACTIVE_TILE_KEY) ?? DEFAULT_TILE_ID;
    } catch {
        return DEFAULT_TILE_ID;
    }
}

function writeActiveTile(id: string): void {
    try {
        localStorage.setItem(ACTIVE_TILE_KEY, id);
    } catch {
        /* ignore */
    }
}

// ---- Tile #1: Today's brief (LLM tool-shaped) ----------------------
//
// Visual mimics `.execlaw-card-task` — bordered card with an icon
// header, title, status pill, body text, and a footer action. The
// brief itself is composed from a parallel fetch of live data; the
// MVP synthesiser is deterministic, but the affordance for "ask
// the model to generate this" is wired through `onRegenerate` for
// the backend follow-up.

interface BriefData {
    threadCount: number;
    pendingApprovals: number;
    oldestApprovalAgeMin: number | null;
    runs24h: number;
    successRate24h: number | null;
    activeAutomations: number;
    firingAlerts: number;
}

function formatRelativeMinutes(min: number): string {
    if (min < 60) return `${min}m`;
    const h = Math.round(min / 60);
    if (h < 24) return `${h}h`;
    const d = Math.round(h / 24);
    return `${d}d`;
}

type BriefSeverity = "danger" | "warning" | "info" | "success" | "muted";

interface BriefItem {
    key: string;
    icon: string;
    severity: BriefSeverity;
    /** Bold, colour-emphasised — typically the count + noun. */
    headline: string;
    /** Conversational continuation of the headline. */
    detail: string;
}

function synthesiseBriefItems(data: BriefData): BriefItem[] {
    const items: BriefItem[] = [];

    if (data.firingAlerts > 0) {
        items.push({
            key: "alerts",
            icon: "bi-exclamation-triangle-fill",
            severity: "danger",
            headline: `${data.firingAlerts} alert${data.firingAlerts === 1 ? "" : "s"} firing`,
            detail: "worth a look before you dig into anything else.",
        });
    }

    if (data.pendingApprovals > 0) {
        const age =
            data.oldestApprovalAgeMin !== null
                ? `oldest ${formatRelativeMinutes(data.oldestApprovalAgeMin)} ago.`
                : "queued up in your tray.";
        items.push({
            key: "approvals",
            icon: "bi-shield-exclamation",
            severity: "warning",
            headline: `${data.pendingApprovals} approval${data.pendingApprovals === 1 ? "" : "s"} pending`,
            detail: age,
        });
    }

    if (data.activeAutomations > 0 || data.runs24h > 0) {
        const successPct =
            data.successRate24h !== null
                ? Math.round(data.successRate24h * 100)
                : null;
        const headline =
            data.runs24h > 0
                ? `${data.runs24h} automation run${data.runs24h === 1 ? "" : "s"}`
                : `${data.activeAutomations} automation${data.activeAutomations === 1 ? "" : "s"} standing by`;
        const detail =
            data.runs24h > 0
                ? successPct !== null
                    ? `in the last 24h · ${successPct}% landing clean · ${data.activeAutomations} active.`
                    : `in the last 24h · ${data.activeAutomations} active.`
                : "no runs in the last 24h.";
        items.push({
            key: "automations",
            icon: "bi-lightning-charge-fill",
            severity: "info",
            headline,
            detail,
        });
    }

    if (items.length === 0 && data.threadCount > 0) {
        items.push({
            key: "threads",
            icon: "bi-chat-square-text",
            severity: "muted",
            headline: `${data.threadCount} thread${data.threadCount === 1 ? "" : "s"} in flight`,
            detail: "pick one up from the sidebar, or start something new below.",
        });
    }

    if (items.length === 0) {
        items.push({
            key: "quiet",
            icon: "bi-check2-circle",
            severity: "success",
            headline: "Quiet across the board",
            detail: "no alerts, no approvals, no surprises — you're clear.",
        });
    }

    return items;
}

function TodaysBriefTile({ getToken }: TileProps) {
    const [data, setData] = useState<BriefData | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [loading, setLoading] = useState(true);
    const briefRef = useRef<HTMLDivElement | null>(null);

    const fetchAll = useCallback(async () => {
        setLoading(true);
        setError(null);
        try {
            const now = Math.floor(Date.now() / 1000);
            const [threadsR, approvalsR, automationsR, alertsR] =
                await Promise.allSettled([
                    listThreads(getToken),
                    listPendingApprovals(getToken),
                    getAutomationMetrics(getToken),
                    listAlerts({ status: ["Firing"], limit: 50 }, getToken),
                ]);

            const threadCount =
                threadsR.status === "fulfilled"
                    ? threadsR.value.threads.length
                    : 0;

            const approvals =
                approvalsR.status === "fulfilled"
                    ? approvalsR.value.approvals
                    : [];

            // PendingApprovalSummary has no created_at field, so we
            // can't compute the oldest age — leave it null for now;
            // the synthesiser drops the age phrase when this is null.
            const oldestApprovalAgeMin: number | null = null;

            const metrics: AutomationMetrics | null =
                automationsR.status === "fulfilled" ? automationsR.value : null;

            const firingAlerts =
                alertsR.status === "fulfilled"
                    ? alertsR.value.firing_count
                    : 0;

            void now; // reserved for future age math when approval timestamps land
            setData({
                threadCount,
                pendingApprovals: approvals.length,
                oldestApprovalAgeMin,
                runs24h: metrics?.runs_24h ?? 0,
                successRate24h: metrics?.success_rate_24h ?? null,
                activeAutomations: metrics?.active_count ?? 0,
                firingAlerts,
            });
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
        } finally {
            setLoading(false);
        }
    }, [getToken]);

    useEffect(() => {
        void fetchAll();
    }, [fetchAll]);

    const items = data ? synthesiseBriefItems(data) : [];

    // Stagger fade-in on each brief item once data lands. Scoped
    // via `briefRef` so the selector doesn't escape the tile and
    // catch unrelated `.execlaw-welcome-tile__brief-item` nodes
    // elsewhere.
    useGSAP(
        () => {
            if (loading || error || items.length === 0) return;
            gsap.from(".execlaw-welcome-tile__brief-item", {
                opacity: 0,
                y: 10,
                duration: 0.45,
                stagger: 0.09,
                ease: "power2.out",
            });
        },
        {
            scope: briefRef,
            dependencies: [loading, error, items.length, items[0]?.key],
        },
    );

    return (
        <div
            ref={briefRef}
            className="execlaw-welcome-tile execlaw-welcome-tile--brief"
        >
            <button
                type="button"
                className="execlaw-welcome-tile__brief-refresh"
                onClick={() => void fetchAll()}
                disabled={loading}
                title="Refresh brief"
                aria-label="Refresh brief"
                data-testid="welcome-brief-refresh"
            >
                <i
                    className={
                        "bi bi-arrow-clockwise" +
                        (loading ? " execlaw-welcome-tile__brief-refresh-spin" : "")
                    }
                    aria-hidden
                />
            </button>
            <div className="execlaw-welcome-tile__body">
                {loading && (
                    <div className="execlaw-muted small">
                        Pulling live state…
                    </div>
                )}
                {!loading && error && (
                    <div className="execlaw-muted small">
                        Couldn't reach the control plane: {error}
                    </div>
                )}
                {!loading &&
                    !error &&
                    items.map((item) => (
                        <div
                            key={item.key}
                            className="execlaw-welcome-tile__brief-item"
                            data-severity={item.severity}
                        >
                            <span
                                className={
                                    "execlaw-welcome-tile__brief-badge" +
                                    ` severity-${item.severity}`
                                }
                                aria-hidden
                            >
                                <i className={`bi ${item.icon}`} />
                            </span>
                            <div className="execlaw-welcome-tile__brief-text">
                                <strong className="execlaw-welcome-tile__brief-headline">
                                    {item.headline}
                                </strong>{" "}
                                <span className="execlaw-welcome-tile__brief-detail">
                                    {item.detail}
                                </span>
                            </div>
                        </div>
                    ))}
            </div>
        </div>
    );
}

// ---- Tile #2: Mission control --------------------------------------

interface Metric {
    label: string;
    value: string | number;
    sub: string;
    icon: string;
    to: string;
}

function MissionControlTile({ getToken }: TileProps) {
    const [metrics, setMetrics] = useState<AutomationMetrics | null>(null);
    const [pending, setPending] = useState<number | null>(null);
    const [alerts, setAlerts] = useState<number | null>(null);

    useEffect(() => {
        let cancelled = false;
        void (async () => {
            const [m, p, a] = await Promise.allSettled([
                getAutomationMetrics(getToken),
                listPendingApprovals(getToken),
                listAlerts({ status: ["Firing"], limit: 1 }, getToken),
            ]);
            if (cancelled) return;
            if (m.status === "fulfilled") setMetrics(m.value);
            if (p.status === "fulfilled") setPending(p.value.approvals.length);
            if (a.status === "fulfilled") setAlerts(a.value.firing_count);
        })();
        return () => {
            cancelled = true;
        };
    }, [getToken]);

    const cells: Metric[] = [
        {
            label: "Automations",
            value: metrics?.active_count ?? "—",
            sub: metrics
                ? `${metrics.runs_24h} runs · 24h`
                : "active",
            icon: "bi-lightning-charge-fill",
            to: "/automations",
        },
        {
            label: "Approvals",
            value: pending ?? "—",
            sub: pending && pending > 0 ? "review now" : "all clear",
            icon: "bi-shield-check",
            to: "/approvals",
        },
        {
            label: "Untriaged",
            value: metrics?.untriaged_kinds_24h ?? "—",
            sub: "event kinds · 24h",
            icon: "bi-funnel",
            to: "/automations",
        },
        {
            label: "Alerts",
            value: alerts ?? "—",
            sub: alerts && alerts > 0 ? "firing" : "quiet",
            icon: "bi-bell",
            to: "/settings/alerts",
        },
    ];

    return (
        <div className="execlaw-welcome-tile execlaw-welcome-tile--metrics">
            <div className="execlaw-welcome-tile__metrics">
                {cells.map((c) => (
                    <Link
                        key={c.label}
                        to={c.to}
                        className="execlaw-welcome-tile__metric"
                        data-testid={`welcome-metric-${c.label.toLowerCase()}`}
                    >
                        <div className="execlaw-welcome-tile__metric-head">
                            <i className={`bi ${c.icon}`} aria-hidden />
                            <span className="execlaw-welcome-tile__metric-label">
                                {c.label}
                            </span>
                        </div>
                        <div className="execlaw-welcome-tile__metric-value">
                            {c.value}
                        </div>
                        <div className="execlaw-welcome-tile__metric-sub">
                            {c.sub}
                        </div>
                    </Link>
                ))}
            </div>
        </div>
    );
}

// ---- Tile #3: Quick prompts (tool-shaped pills) --------------------
//
// Compact pills that each fire a concrete, tool-shaped prompt —
// the agent's tools / sandbox path does the rest. Renders
// horizontally to match the row-of-affordances pattern (visually
// echoes Claude's "Write / Learn / Code" pills below the composer;
// the labels are ours).
//
// Pills:
//   * Analyze CSV    — Palmer Penguins dataset → group by species
//                       → bar chart via the Python sandbox. Only
//                       rendered when the operator has the sandbox
//                       enabled (sandbox can't run otherwise).
//   * Deep Research  — picks a random topic from a curated list
//                       and kicks off a deep research job via
//                       `research_start`. Exercises the planner +
//                       gather-worker pipeline end-to-end.
//   * Latest news    — fetches the BBC News top-stories RSS feed
//                       and asks the agent to summarise. Pure
//                       web-fetch + XML/text synthesis — works
//                       with any text-only model. BBC's RSS has
//                       been openly available, no key/auth, for
//                       20+ years and is a stable demo target.
//   * Current Weather — picks a random city at click time and
//                        asks the agent to fetch live conditions
//                        via the open-meteo API.
//
// 2026-05-21 — replaced the "Describe image" pill (required a
// multimodal backend, broke silently when the loaded model was
// text-only) with the Research + news pills, both of which
// exercise the universal web-fetch tool path.

const PENGUINS_CSV_URL =
    "https://raw.githubusercontent.com/mwaskom/seaborn-data/master/penguins.csv";

// Eclectic research topics — picked so the planner has genuine
// breadth to work with and back-to-back clicks land somewhere
// different.
const RESEARCH_TOPICS = [
    "the history of the Voyager spacecraft missions",
    "how mechanical watch escapements actually work",
    "the geopolitics of rare earth metal mining",
    "the science behind sourdough fermentation",
    "the evolution of typography in software UIs",
    "how nuclear waste storage works in practice today",
    "underwater volcanoes and the ecosystems around them",
    "the architecture of the Roman Pantheon",
    "the chemistry of pigment in oil paintings",
    "why airliners cruise at 35,000 feet",
    "the rise and fall of FORTRAN",
    "how a particle accelerator finds new physics",
    "the state of fusion energy in 2026",
    "how DNS got centralised over time",
];

// City sample for the weather pill — deliberately eclectic so
// back-to-back clicks land in genuinely different climates /
// hemispheres / time zones.
const WEATHER_CITIES = [
    "Tokyo, Japan",
    "Lisbon, Portugal",
    "Reykjavik, Iceland",
    "Buenos Aires, Argentina",
    "Auckland, New Zealand",
    "Marrakech, Morocco",
    "Vancouver, Canada",
    "Seoul, South Korea",
    "Quito, Ecuador",
    "Stockholm, Sweden",
    "Cape Town, South Africa",
    "Mumbai, India",
    "Helsinki, Finland",
    "Wellington, New Zealand",
];

interface QuickPill {
    id: string;
    icon: string;
    label: string;
    /** Build the prompt at click time — allows for runtime
     *  randomisation (e.g. weather city) without re-rendering. */
    buildPrompt: () => string;
    /** Optional gate — when present, the pill only renders if the
     *  predicate returns true for the current environment. */
    isAvailable?: (env: QuickPillEnv) => boolean;
}

interface QuickPillEnv {
    pythonSandboxEnabled: boolean;
}

const PILLS: ReadonlyArray<QuickPill> = [
    {
        id: "analyze-csv",
        icon: "bi-bar-chart-line",
        label: "Analyze CSV",
        isAvailable: (env) => env.pythonSandboxEnabled,
        buildPrompt: () =>
            `Pull the Palmer Penguins dataset from ${PENGUINS_CSV_URL}, ` +
            `group the penguins by their unique species, and generate ` +
            `a bar chart of the per-species counts. Run the analysis ` +
            `in the Python sandbox and return the chart inline.`,
    },
    {
        id: "deep-research",
        icon: "bi-binoculars",
        label: "Deep Research",
        buildPrompt: () => {
            const topic =
                RESEARCH_TOPICS[
                    Math.floor(Math.random() * RESEARCH_TOPICS.length)
                ];
            return (
                `Kick off deep research on ${topic}. Have the planner ` +
                `outline a few sub-questions, fan out gather workers, ` +
                `and bring back a thorough report.`
            );
        },
    },
    {
        id: "latest-news",
        icon: "bi-newspaper",
        label: "Latest News",
        buildPrompt: () =>
            `Fetch the BBC News top-stories RSS feed at ` +
            `https://feeds.bbci.co.uk/news/rss.xml. Parse the XML, ` +
            `pull the top 5 items, summarise each in one line, and ` +
            `tell me which one looks most worth my time.`,
    },
    {
        id: "weather",
        icon: "bi-cloud-sun",
        label: "Current Weather",
        buildPrompt: () => {
            const city =
                WEATHER_CITIES[
                    Math.floor(Math.random() * WEATHER_CITIES.length)
                ];
            return (
                `Pull the current weather conditions for ${city} using ` +
                `the open-meteo API (https://open-meteo.com). Include ` +
                `temperature, wind, precipitation, and an overall ` +
                `one-line summary of conditions there right now.`
            );
        },
    },
];

function QuickPromptsTile({ onSend, getToken }: TileProps) {
    const [pythonSandboxEnabled, setPythonSandboxEnabled] = useState(false);

    // Python sandbox state — used to gate the Analyze CSV pill so
    // we don't surface a prompt the runtime can't actually execute.
    // Failure is silently swallowed (treated as "not enabled");
    // operator can still send the prompt via the composer if they
    // want to.
    useEffect(() => {
        let cancelled = false;
        void (async () => {
            try {
                const r = await getPythonSandbox(getToken);
                if (!cancelled) {
                    setPythonSandboxEnabled(
                        Boolean(r.config?.enabled) &&
                            Boolean(r.docker_available),
                    );
                }
            } catch {
                if (!cancelled) setPythonSandboxEnabled(false);
            }
        })();
        return () => {
            cancelled = true;
        };
    }, [getToken]);

    const env = useMemo<QuickPillEnv>(
        () => ({ pythonSandboxEnabled }),
        [pythonSandboxEnabled],
    );
    const available = useMemo(
        () =>
            PILLS.filter((p) => !p.isAvailable || p.isAvailable(env)),
        [env],
    );

    return (
        <div className="execlaw-welcome-tile execlaw-welcome-tile--prompts">
            <div className="execlaw-welcome-pills">
                {available.map((p) => (
                    <button
                        key={p.id}
                        type="button"
                        className="execlaw-welcome-pill"
                        onClick={() => void onSend(p.buildPrompt(), [], [])}
                        data-testid="welcome-suggestion"
                        data-pill-id={p.id}
                    >
                        <i className={`bi ${p.icon}`} aria-hidden />
                        <span>{p.label}</span>
                    </button>
                ))}
            </div>
        </div>
    );
}

// ---- Tile registry -------------------------------------------------

const TILES: ReadonlyArray<TileDef> = [
    {
        id: "todays-brief",
        label: "Today's brief",
        icon: "bi-stars",
        Component: TodaysBriefTile,
    },
    {
        id: "mission-control",
        label: "Mission control",
        icon: "bi-speedometer2",
        Component: MissionControlTile,
    },
    {
        id: "quick-prompts",
        label: "Quick prompts",
        icon: "bi-lightning-charge",
        Component: QuickPromptsTile,
    },
];

// ---- Customise popover ---------------------------------------------
//
// Replaces the always-visible pill nav with a "Customize" button
// that opens a radio-style picker. One tile shows on the welcome
// view at a time; the popover is the affordance for swapping
// which one.

function TileCustomizer({
    activeId,
    onSelect,
    onClose,
}: {
    activeId: string;
    onSelect: (id: string) => void;
    onClose: () => void;
}) {
    const ref = useRef<HTMLDivElement | null>(null);

    // Click-outside to dismiss. Mousedown on the trigger button is
    // intercepted by the toggle handler in the parent, but if the
    // operator clicks anywhere ELSE we close.
    useEffect(() => {
        const onDocClick = (e: MouseEvent) => {
            if (
                ref.current &&
                e.target instanceof Node &&
                !ref.current.contains(e.target)
            ) {
                onClose();
            }
        };
        // Defer so the same click that OPENS the popover doesn't
        // immediately close it via the bubble.
        const id = setTimeout(() => {
            document.addEventListener("mousedown", onDocClick);
        }, 0);
        return () => {
            clearTimeout(id);
            document.removeEventListener("mousedown", onDocClick);
        };
    }, [onClose]);

    return (
        <div
            ref={ref}
            className="execlaw-welcome-tiles__customizer"
            role="dialog"
            aria-label="Pick a welcome tile"
            data-testid="welcome-tiles-customizer"
        >
            <div className="execlaw-welcome-tiles__customizer-head">
                Show on welcome
            </div>
            {TILES.map((t) => {
                const isActive = t.id === activeId;
                return (
                    <button
                        key={t.id}
                        type="button"
                        className={
                            "execlaw-welcome-tiles__customizer-row" +
                            (isActive ? " is-active" : "")
                        }
                        onClick={() => {
                            onSelect(t.id);
                            onClose();
                        }}
                        data-testid={`welcome-tiles-pick-${t.id}`}
                        aria-pressed={isActive}
                    >
                        <i
                            className={`bi ${t.icon} execlaw-welcome-tiles__customizer-icon`}
                            aria-hidden
                        />
                        <span className="execlaw-welcome-tiles__customizer-label">
                            {t.label}
                        </span>
                        {isActive && (
                            <i
                                className="bi bi-check2 execlaw-welcome-tiles__customizer-check"
                                aria-hidden
                            />
                        )}
                    </button>
                );
            })}
        </div>
    );
}

// ---- WelcomeTiles wrapper ------------------------------------------

interface WelcomeTilesProps {
    onSend: (
        text: string,
        attachments: InlineAttachment[],
        skillNames: string[],
    ) => Promise<void> | void;
    getToken: () => string | null;
}

export function WelcomeTiles({ onSend, getToken }: WelcomeTilesProps) {
    // Persisted active-tile selection. Defaults to `todays-brief`
    // for first-run operators; falls back to the same default if
    // localStorage holds an unknown id (e.g. a tile we've since
    // removed).
    const [activeId, setActiveId] = useState<string>(() => {
        const stored = readActiveTile();
        return TILES.some((t) => t.id === stored)
            ? stored
            : DEFAULT_TILE_ID;
    });
    const [customizerOpen, setCustomizerOpen] = useState(false);

    const select = useCallback((id: string) => {
        setActiveId(id);
        writeActiveTile(id);
    }, []);

    const active = useMemo(
        () => TILES.find((t) => t.id === activeId) ?? TILES[0],
        [activeId],
    );

    return (
        <div
            className="execlaw-welcome-tiles"
            data-testid="welcome-tiles"
        >
            <div
                className="execlaw-welcome-tiles__active"
                data-tile-id={active.id}
            >
                <active.Component onSend={onSend} getToken={getToken} />
            </div>
            <div className="execlaw-welcome-tiles__foot">
                <button
                    type="button"
                    className={
                        "execlaw-welcome-tiles__customize" +
                        (customizerOpen ? " is-open" : "")
                    }
                    onClick={() => setCustomizerOpen((v) => !v)}
                    aria-expanded={customizerOpen}
                    aria-haspopup="dialog"
                    data-testid="welcome-tiles-customize"
                    title="Pick which tile shows on the welcome view"
                >
                    <i className="bi bi-sliders" aria-hidden />
                    <span>Customize</span>
                </button>
                {customizerOpen && (
                    <TileCustomizer
                        activeId={activeId}
                        onSelect={select}
                        onClose={() => setCustomizerOpen(false)}
                    />
                )}
            </div>
        </div>
    );
}
