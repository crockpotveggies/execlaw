// Top-level Skills page (Phase B.3 + Phase D.1).
//
// Tab structure:
//   * Skills    — two-pane list + detail with inline body editor and
//                 version-history diff (D.1).
//   * Proposals — agent-generated drafts awaiting operator review,
//                 produced by the auto-capture worker in dry-run mode
//                 and the reuse-update worker (D.1, D.3).
//
// Backed by the new admin endpoints under `/api/admin/skills/*`.

import { useCallback, useEffect, useMemo, useState } from "react";
import {
    approveSkillProposal,
    archiveSkill,
    getSkill,
    getSkillsConfig,
    listSkillProposals,
    listSkillVersions,
    listSkills,
    promoteSkill,
    putSkillsConfig,
    rejectSkillProposal,
    updateSkillBody,
    type ProposalState,
    type ProposalStateFilter,
    type SkillDetail,
    type SkillListEntry,
    type SkillProposalView,
    type SkillState,
    type SkillVersionView,
    type SkillsConfigView,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";
import { ErrorBanner } from "../components/ErrorBanner";

const STATE_BADGE: Record<SkillState, string> = {
    trial: "bg-info",
    stable: "bg-success",
    archived: "bg-secondary",
};

const KIND_BADGE: Record<string, string> = {
    authored: "bg-primary",
    shipped: "bg-warning text-dark",
    registered: "bg-warning text-dark",
};

const PROPOSAL_STATE_BADGE: Record<ProposalState, string> = {
    pending: "bg-info",
    approved: "bg-success",
    rejected: "bg-secondary",
    superseded: "bg-secondary",
};

type Tab = "skills" | "proposals";

export function SkillsPage() {
    const auth = useAuth();
    const isController = auth.user?.role === "controller";
    const [tab, setTab] = useState<Tab>("skills");
    const [error, setError] = useState<string | null>(null);

    return (
        <div
            className="execlaw-page execlaw-skills"
            data-testid="skills-page"
        >
            <ErrorBanner
                message={error}
                onDismiss={() => setError(null)}
                className="m-3"
            />
            <ul className="nav nav-tabs px-3 pt-2" role="tablist">
                <li className="nav-item">
                    <button
                        type="button"
                        className={
                            "nav-link" + (tab === "skills" ? " active" : "")
                        }
                        onClick={() => setTab("skills")}
                        data-testid="skills-tab-skills"
                    >
                        Skills
                    </button>
                </li>
                <li className="nav-item">
                    <button
                        type="button"
                        className={
                            "nav-link" + (tab === "proposals" ? " active" : "")
                        }
                        onClick={() => setTab("proposals")}
                        data-testid="skills-tab-proposals"
                    >
                        Proposals
                    </button>
                </li>
            </ul>
            {tab === "skills" ? (
                <SkillsTab isController={isController} onError={setError} />
            ) : (
                <ProposalsTab isController={isController} onError={setError} />
            )}
        </div>
    );
}

// ----------------------------------------------------------------
// Skills tab — list + detail with editor + version diff
// ----------------------------------------------------------------

function SkillsTab({
    isController,
    onError,
}: {
    isController: boolean;
    onError: (msg: string | null) => void;
}) {
    const auth = useAuth();
    const getToken = auth.getAccessToken;

    const [skills, setSkills] = useState<SkillListEntry[] | null>(null);
    const [includeArchived, setIncludeArchived] = useState(false);
    const [selectedName, setSelectedName] = useState<string | null>(null);

    const refresh = useCallback(async () => {
        try {
            const r = await listSkills(getToken, { includeArchived });
            setSkills(r.skills);
        } catch (e) {
            onError(e instanceof Error ? e.message : String(e));
        }
    }, [getToken, includeArchived, onError]);

    useEffect(() => {
        if (auth.status !== "authenticated") return;
        void refresh();
    }, [auth.status, refresh]);

    useEffect(() => {
        if (selectedName === null && skills && skills.length > 0) {
            setSelectedName(skills[0].name);
        }
        if (
            selectedName &&
            skills &&
            !skills.some((s) => s.name === selectedName)
        ) {
            setSelectedName(skills[0]?.name ?? null);
        }
    }, [skills, selectedName]);

    return (
        <>
            <div className="d-flex align-items-center gap-3 px-3 py-2 border-bottom flex-wrap">
                <span className="execlaw-muted small">
                    {skills?.length ?? 0} skill
                    {skills?.length === 1 ? "" : "s"}
                </span>
                <AutoCaptureControl
                    isController={isController}
                    onError={(m) => onError(m)}
                />
                <label className="form-check form-switch ms-auto mb-0 small">
                    <input
                        type="checkbox"
                        className="form-check-input"
                        checked={includeArchived}
                        onChange={(e) => {
                            setIncludeArchived(e.target.checked);
                            setSelectedName(null);
                        }}
                        data-testid="skills-include-archived"
                    />
                    <span className="form-check-label">Show archived</span>
                </label>
            </div>
            {skills === null ? (
                <div className="m-3 execlaw-muted small">Loading…</div>
            ) : skills.length === 0 ? (
                <SkillsEmptyState includeArchived={includeArchived} />
            ) : (
                <div
                    className="execlaw-skills__split d-flex flex-grow-1"
                    style={{ minHeight: 0 }}
                >
                    <SkillsList
                        skills={skills}
                        selectedName={selectedName}
                        onSelect={setSelectedName}
                    />
                    <SkillsDetail
                        name={selectedName}
                        isController={isController}
                        onMutated={() => {
                            void refresh();
                        }}
                    />
                </div>
            )}
        </>
    );
}

function SkillsEmptyState({ includeArchived }: { includeArchived: boolean }) {
    return (
        <div
            className="m-3 execlaw-muted small"
            data-testid="skills-empty-state"
        >
            {includeArchived ? (
                <>
                    No skills exist yet — not even archived ones. Skills are
                    procedural-knowledge documents the agent reads to shape
                    its behavior.
                </>
            ) : (
                <>
                    No active skills. Toggle <strong>Show archived</strong>{" "}
                    to see retired skills, or ask the agent to{" "}
                    <code>skills.create</code> a new one.
                </>
            )}
        </div>
    );
}

function SkillsList({
    skills,
    selectedName,
    onSelect,
}: {
    skills: SkillListEntry[];
    selectedName: string | null;
    onSelect: (name: string) => void;
}) {
    return (
        <aside
            className="execlaw-skills__list d-flex flex-column overflow-auto border-end"
            style={{ width: 360, minWidth: 320 }}
            data-testid="skills-list"
        >
            {skills.map((s) => (
                <button
                    key={s.name}
                    type="button"
                    className={
                        "execlaw-skills__list-row text-start px-3 py-2 border-bottom bg-transparent border-0 border-bottom" +
                        (s.name === selectedName ? " bg-body-secondary" : "")
                    }
                    onClick={() => onSelect(s.name)}
                    data-testid="skills-list-row"
                    data-state={s.state}
                    data-kind={s.registration_kind}
                >
                    <div className="d-flex align-items-center gap-2">
                        <span
                            className={`badge ${STATE_BADGE[s.state]}`}
                            title={`State: ${s.state}`}
                        >
                            {s.state}
                        </span>
                        <span
                            className={`badge ${KIND_BADGE[s.registration_kind] ?? "bg-secondary"}`}
                            title={`Kind: ${s.registration_kind}`}
                        >
                            {s.registration_kind}
                        </span>
                        <span className="execlaw-muted small ms-auto">
                            v{s.version}
                        </span>
                    </div>
                    <div className="fw-semibold mt-1">{s.name}</div>
                    <div className="execlaw-muted small text-truncate">
                        {s.description}
                    </div>
                </button>
            ))}
        </aside>
    );
}

function SkillsDetail({
    name,
    isController,
    onMutated,
}: {
    name: string | null;
    isController: boolean;
    onMutated: () => void;
}) {
    const auth = useAuth();
    const getToken = auth.getAccessToken;
    const [detail, setDetail] = useState<SkillDetail | null>(null);
    const [loading, setLoading] = useState(false);
    const [actionError, setActionError] = useState<string | null>(null);
    const [busy, setBusy] = useState<"promote" | "archive" | "save" | null>(
        null,
    );
    const [editing, setEditing] = useState(false);
    const [editDescription, setEditDescription] = useState("");
    const [editBody, setEditBody] = useState("");

    useEffect(() => {
        if (name === null) {
            setDetail(null);
            setEditing(false);
            return;
        }
        setLoading(true);
        let cancelled = false;
        (async () => {
            try {
                const d = await getSkill(name, getToken);
                if (!cancelled) {
                    setDetail(d);
                    setEditing(false);
                }
            } catch (e) {
                if (!cancelled) {
                    setActionError(
                        e instanceof Error ? e.message : String(e),
                    );
                    setDetail(null);
                }
            } finally {
                if (!cancelled) setLoading(false);
            }
        })();
        return () => {
            cancelled = true;
        };
    }, [name, getToken]);

    const onPromote = useCallback(async () => {
        if (!detail) return;
        setBusy("promote");
        setActionError(null);
        try {
            const updated = await promoteSkill(detail.name, null, getToken);
            setDetail(updated);
            onMutated();
        } catch (e) {
            setActionError(e instanceof Error ? e.message : String(e));
        } finally {
            setBusy(null);
        }
    }, [detail, getToken, onMutated]);

    const onArchive = useCallback(async () => {
        if (!detail) return;
        if (!window.confirm(`Archive "${detail.name}"?`)) return;
        setBusy("archive");
        setActionError(null);
        try {
            const updated = await archiveSkill(detail.name, getToken);
            setDetail(updated);
            onMutated();
        } catch (e) {
            setActionError(e instanceof Error ? e.message : String(e));
        } finally {
            setBusy(null);
        }
    }, [detail, getToken, onMutated]);

    const onStartEdit = useCallback(() => {
        if (!detail) return;
        setEditDescription(detail.description);
        setEditBody(detail.body_md);
        setEditing(true);
        setActionError(null);
    }, [detail]);

    const onCancelEdit = useCallback(() => {
        setEditing(false);
        setActionError(null);
    }, []);

    const onSaveEdit = useCallback(async () => {
        if (!detail) return;
        setBusy("save");
        setActionError(null);
        try {
            const updated = await updateSkillBody(
                detail.name,
                {
                    description: editDescription,
                    body_md: editBody,
                    frontmatter_json: detail.frontmatter_json,
                },
                getToken,
            );
            setDetail(updated);
            setEditing(false);
            onMutated();
        } catch (e) {
            setActionError(e instanceof Error ? e.message : String(e));
        } finally {
            setBusy(null);
        }
    }, [detail, editDescription, editBody, getToken, onMutated]);

    if (name === null) {
        return (
            <section
                className="execlaw-skills__detail flex-grow-1 m-3 execlaw-muted small"
                data-testid="skills-detail"
            >
                Select a skill on the left.
            </section>
        );
    }
    if (loading && detail === null) {
        return (
            <section
                className="execlaw-skills__detail flex-grow-1 m-3 execlaw-muted small"
                data-testid="skills-detail"
            >
                Loading…
            </section>
        );
    }
    if (detail === null) {
        return (
            <section
                className="execlaw-skills__detail flex-grow-1 m-3"
                data-testid="skills-detail"
            >
                <ErrorBanner
                    message={actionError ?? "Failed to load skill."}
                    onDismiss={() => setActionError(null)}
                />
            </section>
        );
    }

    const canPromote = isController && detail.state === "trial" && !editing;
    const canArchive = isController && detail.state !== "archived" && !editing;
    const canEdit = isController && detail.state !== "archived";

    return (
        <section
            className="execlaw-skills__detail flex-grow-1 overflow-auto"
            data-testid="skills-detail"
            data-skill-name={detail.name}
        >
            <div className="px-3 py-3">
                <div className="d-flex align-items-center gap-2 flex-wrap">
                    <h3 className="h5 mb-0 me-2">{detail.name}</h3>
                    <span className={`badge ${STATE_BADGE[detail.state]}`}>
                        {detail.state}
                    </span>
                    <span
                        className={`badge ${KIND_BADGE[detail.registration_kind] ?? "bg-secondary"}`}
                    >
                        {detail.registration_kind}
                    </span>
                    <span className="execlaw-muted small ms-2">
                        v{detail.current_version}
                    </span>
                    <div className="ms-auto d-flex gap-2">
                        {canEdit && !editing && (
                            <button
                                type="button"
                                className="btn btn-sm btn-outline-primary"
                                onClick={onStartEdit}
                                data-testid="skills-edit-btn"
                            >
                                Edit body
                            </button>
                        )}
                        {canPromote && (
                            <button
                                type="button"
                                className="btn btn-sm btn-success"
                                onClick={onPromote}
                                disabled={busy !== null}
                                data-testid="skills-promote-btn"
                            >
                                {busy === "promote"
                                    ? "Promoting…"
                                    : "Promote to stable"}
                            </button>
                        )}
                        {canArchive && (
                            <button
                                type="button"
                                className="btn btn-sm btn-outline-danger"
                                onClick={onArchive}
                                disabled={busy !== null}
                                data-testid="skills-archive-btn"
                            >
                                {busy === "archive" ? "Archiving…" : "Archive"}
                            </button>
                        )}
                    </div>
                </div>
                {!editing && <p className="mt-2 mb-3">{detail.description}</p>}
                {actionError && (
                    <ErrorBanner
                        message={actionError}
                        onDismiss={() => setActionError(null)}
                        className="mb-3"
                    />
                )}
                <DetailMeta detail={detail} />
                <h4 className="h6 mt-4">Body</h4>
                {editing ? (
                    <div data-testid="skills-edit-form">
                        <label className="form-label small fw-semibold mt-2">
                            Description
                        </label>
                        <textarea
                            className="form-control"
                            rows={2}
                            value={editDescription}
                            onChange={(e) => setEditDescription(e.target.value)}
                            data-testid="skills-edit-description"
                        />
                        <label className="form-label small fw-semibold mt-3">
                            Body markdown
                        </label>
                        <textarea
                            className="form-control font-monospace"
                            rows={20}
                            value={editBody}
                            onChange={(e) => setEditBody(e.target.value)}
                            data-testid="skills-edit-body"
                        />
                        <div className="d-flex gap-2 mt-3">
                            <button
                                type="button"
                                className="btn btn-sm btn-primary"
                                onClick={onSaveEdit}
                                disabled={busy !== null}
                                data-testid="skills-edit-save"
                            >
                                {busy === "save"
                                    ? "Saving…"
                                    : "Save as new version"}
                            </button>
                            <button
                                type="button"
                                className="btn btn-sm btn-outline-secondary"
                                onClick={onCancelEdit}
                                disabled={busy !== null}
                                data-testid="skills-edit-cancel"
                            >
                                Cancel
                            </button>
                            <span className="execlaw-muted small ms-2 align-self-center">
                                Saving creates v
                                {detail.current_version + 1}; the previous
                                version is preserved in history.
                            </span>
                        </div>
                    </div>
                ) : (
                    <pre
                        className="p-3 rounded bg-body-tertiary"
                        style={{
                            whiteSpace: "pre-wrap",
                            wordBreak: "break-word",
                        }}
                        data-testid="skills-body-md"
                    >
                        {detail.body_md}
                    </pre>
                )}
                {detail.resource_paths.length > 0 && (
                    <>
                        <h4 className="h6 mt-4">Bundled resources</h4>
                        <ul className="execlaw-muted small">
                            {detail.resource_paths.map((p) => (
                                <li key={p}>
                                    <code>{p}</code>
                                </li>
                            ))}
                        </ul>
                    </>
                )}
                <h4 className="h6 mt-4">Frontmatter</h4>
                <pre
                    className="p-3 rounded bg-body-tertiary small"
                    style={{ whiteSpace: "pre-wrap" }}
                >
                    {prettyJson(detail.frontmatter_json)}
                </pre>
                <VersionHistory
                    name={detail.name}
                    currentVersion={detail.current_version}
                    refreshKey={detail.updated_at}
                />
            </div>
        </section>
    );
}

function DetailMeta({ detail }: { detail: SkillDetail }) {
    return (
        <dl className="row mb-0 small">
            <dt className="col-sm-3 execlaw-muted">Source</dt>
            <dd className="col-sm-9">
                <code>{detail.source}</code>
                {detail.owning_plugin_id && (
                    <>
                        {" "}
                        · plugin <code>{detail.owning_plugin_id}</code>
                    </>
                )}
            </dd>
            <dt className="col-sm-3 execlaw-muted">Authored by</dt>
            <dd className="col-sm-9">
                <code>{detail.authored_by}</code>
            </dd>
            <dt className="col-sm-3 execlaw-muted">Created</dt>
            <dd className="col-sm-9">{formatTime(detail.created_at)}</dd>
            <dt className="col-sm-3 execlaw-muted">Updated</dt>
            <dd className="col-sm-9">{formatTime(detail.updated_at)}</dd>
            {detail.archived_at !== null && (
                <>
                    <dt className="col-sm-3 execlaw-muted">Archived</dt>
                    <dd className="col-sm-9">
                        {formatTime(detail.archived_at)}
                    </dd>
                </>
            )}
        </dl>
    );
}

// ----------------------------------------------------------------
// Phase D.1 — Version history + side-by-side diff
// ----------------------------------------------------------------

function VersionHistory({
    name,
    currentVersion,
    refreshKey,
}: {
    name: string;
    currentVersion: number;
    refreshKey: number;
}) {
    const auth = useAuth();
    const getToken = auth.getAccessToken;
    const [versions, setVersions] = useState<SkillVersionView[] | null>(null);
    const [leftV, setLeftV] = useState<number | null>(null);
    const [rightV, setRightV] = useState<number | null>(null);

    useEffect(() => {
        let cancelled = false;
        (async () => {
            try {
                const r = await listSkillVersions(name, getToken);
                if (!cancelled) {
                    setVersions(r.versions);
                    if (r.versions.length >= 2) {
                        setRightV(currentVersion);
                        setLeftV(currentVersion - 1);
                    } else {
                        setRightV(null);
                        setLeftV(null);
                    }
                }
            } catch {
                if (!cancelled) setVersions([]);
            }
        })();
        return () => {
            cancelled = true;
        };
    }, [name, getToken, refreshKey, currentVersion]);

    if (versions === null) {
        return (
            <p className="execlaw-muted small mt-4">Loading version history…</p>
        );
    }
    if (versions.length <= 1) {
        return (
            <p className="execlaw-muted small mt-4" data-testid="skills-no-history">
                Version history will appear here after the next edit.
            </p>
        );
    }

    const left = versions.find((v) => v.version === leftV) ?? null;
    const right = versions.find((v) => v.version === rightV) ?? null;

    return (
        <div className="mt-4" data-testid="skills-version-history">
            <h4 className="h6">Version history ({versions.length})</h4>
            <div className="d-flex align-items-center gap-2 mb-2 flex-wrap">
                <label className="small mb-0">
                    Compare v
                    <select
                        className="form-select form-select-sm d-inline-block w-auto ms-1"
                        value={leftV ?? ""}
                        onChange={(e) =>
                            setLeftV(parseInt(e.target.value, 10) || null)
                        }
                        data-testid="skills-diff-left"
                    >
                        {versions.map((v) => (
                            <option key={v.version} value={v.version}>
                                {v.version}
                            </option>
                        ))}
                    </select>
                </label>
                <label className="small mb-0">
                    with v
                    <select
                        className="form-select form-select-sm d-inline-block w-auto ms-1"
                        value={rightV ?? ""}
                        onChange={(e) =>
                            setRightV(parseInt(e.target.value, 10) || null)
                        }
                        data-testid="skills-diff-right"
                    >
                        {versions.map((v) => (
                            <option key={v.version} value={v.version}>
                                {v.version}
                            </option>
                        ))}
                    </select>
                </label>
            </div>
            {left && right && (
                <div className="row g-2" data-testid="skills-diff-view">
                    <div className="col-md-6">
                        <div className="execlaw-muted small">
                            v{left.version} · {formatTime(left.authored_at)} ·{" "}
                            <code>{left.authored_by}</code>
                        </div>
                        <pre
                            className="p-2 rounded bg-body-tertiary small"
                            style={{
                                whiteSpace: "pre-wrap",
                                wordBreak: "break-word",
                                maxHeight: 400,
                                overflow: "auto",
                            }}
                        >
                            {left.body_md}
                        </pre>
                    </div>
                    <div className="col-md-6">
                        <div className="execlaw-muted small">
                            v{right.version} · {formatTime(right.authored_at)} ·{" "}
                            <code>{right.authored_by}</code>
                        </div>
                        <pre
                            className="p-2 rounded bg-body-tertiary small"
                            style={{
                                whiteSpace: "pre-wrap",
                                wordBreak: "break-word",
                                maxHeight: 400,
                                overflow: "auto",
                            }}
                        >
                            {right.body_md}
                        </pre>
                    </div>
                </div>
            )}
        </div>
    );
}

// ----------------------------------------------------------------
// Proposals tab
// ----------------------------------------------------------------

function ProposalsTab({
    isController,
    onError,
}: {
    isController: boolean;
    onError: (msg: string | null) => void;
}) {
    const auth = useAuth();
    const getToken = auth.getAccessToken;
    const [filter, setFilter] = useState<ProposalStateFilter>("pending");
    const [rows, setRows] = useState<SkillProposalView[] | null>(null);
    const [busyId, setBusyId] = useState<number | null>(null);
    const [reviewNotes, setReviewNotes] = useState<Record<number, string>>({});

    const refresh = useCallback(async () => {
        try {
            const r = await listSkillProposals(filter, getToken);
            setRows(r.proposals);
        } catch (e) {
            onError(e instanceof Error ? e.message : String(e));
        }
    }, [filter, getToken, onError]);

    useEffect(() => {
        if (auth.status !== "authenticated") return;
        void refresh();
    }, [auth.status, refresh]);

    const onApprove = useCallback(
        async (id: number) => {
            setBusyId(id);
            try {
                await approveSkillProposal(id, reviewNotes[id] ?? null, getToken);
                await refresh();
            } catch (e) {
                onError(e instanceof Error ? e.message : String(e));
            } finally {
                setBusyId(null);
            }
        },
        [getToken, refresh, reviewNotes, onError],
    );

    const onReject = useCallback(
        async (id: number) => {
            setBusyId(id);
            try {
                await rejectSkillProposal(id, reviewNotes[id] ?? null, getToken);
                await refresh();
            } catch (e) {
                onError(e instanceof Error ? e.message : String(e));
            } finally {
                setBusyId(null);
            }
        },
        [getToken, refresh, reviewNotes, onError],
    );

    return (
        <div
            className="d-flex flex-column flex-grow-1 overflow-auto"
            data-testid="skills-proposals-tab"
        >
            <div className="d-flex align-items-center gap-3 px-3 py-2 border-bottom flex-wrap">
                <label className="small mb-0">
                    State{" "}
                    <select
                        className="form-select form-select-sm d-inline-block w-auto"
                        value={filter}
                        onChange={(e) =>
                            setFilter(e.target.value as ProposalStateFilter)
                        }
                        data-testid="skills-proposals-filter"
                    >
                        <option value="pending">Pending</option>
                        <option value="approved">Approved</option>
                        <option value="rejected">Rejected</option>
                        <option value="superseded">Superseded</option>
                        <option value="all">All</option>
                    </select>
                </label>
                <span className="execlaw-muted small ms-auto">
                    {rows?.length ?? 0} proposal
                    {rows?.length === 1 ? "" : "s"}
                </span>
            </div>
            {rows === null ? (
                <div className="m-3 execlaw-muted small">Loading…</div>
            ) : rows.length === 0 ? (
                <div
                    className="m-3 execlaw-muted small"
                    data-testid="skills-proposals-empty"
                >
                    No {filter === "all" ? "" : filter + " "}proposals.
                    {filter === "pending" && (
                        <>
                            {" "}
                            The auto-capture worker (Phase C dry-run) and the
                            reuse-update worker (Phase D.3) write proposals
                            here for review.
                        </>
                    )}
                </div>
            ) : (
                <div className="px-3 py-3 d-flex flex-column gap-3">
                    {rows.map((p) => (
                        <ProposalCard
                            key={p.id}
                            proposal={p}
                            isController={isController}
                            busy={busyId === p.id}
                            notes={reviewNotes[p.id] ?? ""}
                            onChangeNotes={(s) =>
                                setReviewNotes((m) => ({ ...m, [p.id]: s }))
                            }
                            onApprove={() => onApprove(p.id)}
                            onReject={() => onReject(p.id)}
                        />
                    ))}
                </div>
            )}
        </div>
    );
}

function ProposalCard({
    proposal,
    isController,
    busy,
    notes,
    onChangeNotes,
    onApprove,
    onReject,
}: {
    proposal: SkillProposalView;
    isController: boolean;
    busy: boolean;
    notes: string;
    onChangeNotes: (s: string) => void;
    onApprove: () => void;
    onReject: () => void;
}) {
    const canReview = isController && proposal.state === "pending";
    return (
        <div
            className="card"
            data-testid="skills-proposal-card"
            data-proposal-id={proposal.id}
            data-proposal-state={proposal.state}
        >
            <div className="card-body">
                <div className="d-flex align-items-center gap-2 flex-wrap">
                    <span
                        className={`badge ${PROPOSAL_STATE_BADGE[proposal.state]}`}
                    >
                        {proposal.state}
                    </span>
                    <span className="badge bg-info">
                        {proposal.kind === "version_fork"
                            ? "fork"
                            : "new skill"}
                    </span>
                    <h5 className="h6 mb-0 me-2">{proposal.proposed_name}</h5>
                    <span className="execlaw-muted small">
                        {proposal.tool_calls_observed} tool calls observed
                    </span>
                    <span className="execlaw-muted small ms-auto">
                        {formatTime(proposal.created_at)}
                    </span>
                </div>
                <p className="mt-2 mb-2">{proposal.description}</p>
                {proposal.trajectory_summary && (
                    <p className="execlaw-muted small mb-2">
                        <em>{proposal.trajectory_summary}</em>
                    </p>
                )}
                <details className="mb-2">
                    <summary className="small text-decoration-underline">
                        View body
                    </summary>
                    <pre
                        className="p-2 rounded bg-body-tertiary small mt-2"
                        style={{
                            whiteSpace: "pre-wrap",
                            wordBreak: "break-word",
                            maxHeight: 300,
                            overflow: "auto",
                        }}
                        data-testid="skills-proposal-body"
                    >
                        {proposal.body_md}
                    </pre>
                </details>
                {proposal.reviewer && (
                    <div className="execlaw-muted small mb-2">
                        Reviewed by <code>{proposal.reviewer}</code> ·{" "}
                        {formatTime(proposal.reviewed_at ?? 0)}
                        {proposal.decision_notes && (
                            <>
                                {" "}
                                · <em>{proposal.decision_notes}</em>
                            </>
                        )}
                    </div>
                )}
                {canReview && (
                    <div className="d-flex gap-2 align-items-center mt-2 flex-wrap">
                        <input
                            type="text"
                            className="form-control form-control-sm"
                            placeholder="Decision notes (optional)"
                            value={notes}
                            onChange={(e) => onChangeNotes(e.target.value)}
                            disabled={busy}
                            style={{ maxWidth: 400 }}
                            data-testid="skills-proposal-notes"
                        />
                        <button
                            type="button"
                            className="btn btn-sm btn-success"
                            onClick={onApprove}
                            disabled={busy}
                            data-testid="skills-proposal-approve"
                        >
                            {busy ? "…" : "Approve"}
                        </button>
                        <button
                            type="button"
                            className="btn btn-sm btn-outline-danger"
                            onClick={onReject}
                            disabled={busy}
                            data-testid="skills-proposal-reject"
                        >
                            Reject
                        </button>
                    </div>
                )}
            </div>
        </div>
    );
}

// ----------------------------------------------------------------
// Auto-capture toggle (Phase C)
// ----------------------------------------------------------------

function AutoCaptureControl({
    isController,
    onError,
}: {
    isController: boolean;
    onError: (msg: string) => void;
}) {
    const auth = useAuth();
    const getToken = auth.getAccessToken;
    const [config, setConfig] = useState<SkillsConfigView | null>(null);
    const [busy, setBusy] = useState(false);

    useEffect(() => {
        if (auth.status !== "authenticated") return;
        let cancelled = false;
        (async () => {
            try {
                const c = await getSkillsConfig(getToken);
                if (!cancelled) setConfig(c);
            } catch (e) {
                if (!cancelled) {
                    onError(
                        `Failed to load skill capture config: ${e instanceof Error ? e.message : String(e)}`,
                    );
                }
            }
        })();
        return () => {
            cancelled = true;
        };
    }, [auth.status, getToken, onError]);

    const onToggle = useCallback(
        async (enabled: boolean) => {
            setBusy(true);
            try {
                const updated = await putSkillsConfig(
                    { auto_capture_enabled: enabled },
                    getToken,
                );
                setConfig(updated);
            } catch (e) {
                onError(
                    `Failed to update auto-capture: ${e instanceof Error ? e.message : String(e)}`,
                );
            } finally {
                setBusy(false);
            }
        },
        [getToken, onError],
    );

    const helpText = useMemo(() => {
        if (config === null) return "";
        return `Auto-capture proposes a draft skill after a successful turn with ≥${config.auto_capture_min_tool_calls} tool calls.`;
    }, [config]);

    if (config === null) {
        return (
            <span
                className="execlaw-muted small"
                data-testid="skills-auto-capture-loading"
            >
                auto-capture: …
            </span>
        );
    }

    return (
        <div
            className="d-flex align-items-center gap-2"
            data-testid="skills-auto-capture-control"
            data-enabled={config.auto_capture_enabled}
        >
            {isController ? (
                <label
                    className="form-check form-switch mb-0 small"
                    title={helpText}
                >
                    <input
                        type="checkbox"
                        className="form-check-input"
                        checked={config.auto_capture_enabled}
                        disabled={busy}
                        onChange={(e) => {
                            void onToggle(e.target.checked);
                        }}
                        data-testid="skills-auto-capture-toggle"
                    />
                    <span className="form-check-label">Auto-capture</span>
                </label>
            ) : (
                <span className="execlaw-muted small" title={helpText}>
                    Auto-capture:{" "}
                    <strong>
                        {config.auto_capture_enabled ? "on" : "off"}
                    </strong>
                </span>
            )}
            <span className="execlaw-muted small">
                ≥{config.auto_capture_min_tool_calls} tool calls
            </span>
            {config.auto_capture_dry_run && (
                <span
                    className="badge bg-warning text-dark"
                    title="Dry-run: pipeline runs but proposals land in the Proposals tab for review instead of becoming live skills."
                    data-testid="skills-dry-run-badge"
                >
                    dry-run
                </span>
            )}
            {config.reuse_update_enabled && (
                <span
                    className="badge bg-info"
                    title="Reuse-update worker is on: post-invocation, the agent may propose improvements as version forks for review."
                    data-testid="skills-reuse-update-badge"
                >
                    reuse-update
                </span>
            )}
        </div>
    );
}

function formatTime(ms: number): string {
    if (!ms) return "—";
    return new Date(ms).toLocaleString();
}

function prettyJson(s: string): string {
    try {
        return JSON.stringify(JSON.parse(s), null, 2);
    } catch {
        return s;
    }
}
