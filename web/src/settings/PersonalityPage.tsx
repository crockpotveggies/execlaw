// Settings → Personality (Phase 9, MIGRATION_PLAN §5.5).
//
// The operator-editable half of the agent's system prompt: name,
// tone, custom instructions, persona, voice id. Edits the `default`
// scope here; the conversation-override surface is a list-with-delete
// (creating overrides inline from a settings page is awkward — that
// flow lands on a future "Customize this thread" affordance in the
// chat shell). The underlying API already supports per-conversation
// overrides; the SPA surfaces them so the operator can audit and
// remove drift even without inline creation.

import { useCallback, useEffect, useMemo, useState } from "react";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import {
    deletePersonalityConversation,
    listPersonality,
    previewPersonality,
    upsertPersonalityDefault,
    type PersonalityListResponse,
    type PersonalityView,
    type UpsertPersonalityBody,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";
import { ErrorBanner } from "../components/ErrorBanner";

interface FormState {
    display_name: string;
    role: string;
    tone: string;
    communication_style: string;
    initiative: string;
    about_agent: string;
    about_controller: string;
    custom_instructions: string;
    voice_id: string;
}

function fromView(v: PersonalityView): FormState {
    return {
        display_name: v.display_name,
        role: v.role,
        tone: v.tone,
        communication_style: v.communication_style,
        initiative: v.initiative,
        about_agent: v.about_agent,
        about_controller: v.about_controller,
        custom_instructions: v.custom_instructions,
        voice_id: v.voice_id ?? "",
    };
}

function toUpsert(f: FormState): UpsertPersonalityBody {
    return {
        display_name: f.display_name,
        role: f.role,
        tone: f.tone,
        communication_style: f.communication_style,
        initiative: f.initiative,
        about_agent: f.about_agent,
        about_controller: f.about_controller,
        custom_instructions: f.custom_instructions,
        voice_id: f.voice_id.trim() === "" ? null : f.voice_id,
    };
}

export function PersonalityPage() {
    const auth = useAuth();
    const getToken = auth.getAccessToken;

    const [data, setData] = useState<PersonalityListResponse | null>(null);
    const [form, setForm] = useState<FormState | null>(null);
    const [preview, setPreview] = useState<string | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [savedAt, setSavedAt] = useState<number | null>(null);
    const [busy, setBusy] = useState(false);

    const refresh = useCallback(async () => {
        try {
            const r = await listPersonality(getToken);
            setData(r);
            setForm(fromView(r.default));
            setError(null);
            try {
                const p = await previewPersonality(null, getToken);
                setPreview(p.system_prompt);
            } catch {
                // Preview is best-effort — never block the editor on it.
                setPreview(null);
            }
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
        }
    }, [getToken]);

    useEffect(() => {
        void refresh();
    }, [refresh]);

    const onChange = useCallback(
        <K extends keyof FormState>(field: K, value: FormState[K]) => {
            setForm((prev) => (prev ? { ...prev, [field]: value } : prev));
        },
        [],
    );

    const onSave = useCallback(async () => {
        if (!form) return;
        setBusy(true);
        try {
            await upsertPersonalityDefault(toUpsert(form), getToken);
            setSavedAt(Date.now());
            setError(null);
            await refresh();
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
        } finally {
            setBusy(false);
        }
    }, [form, getToken, refresh]);

    const onDeleteOverride = useCallback(
        async (conversationId: string) => {
            if (
                !confirm(
                    `Drop personality override for conversation ${conversationId}? The conversation will fall back to default on its next turn.`,
                )
            )
                return;
            try {
                await deletePersonalityConversation(conversationId, getToken);
                await refresh();
            } catch (e) {
                setError(e instanceof Error ? e.message : String(e));
            }
        },
        [getToken, refresh],
    );

    const versionHint = useMemo(() => {
        if (!data) return null;
        return `v${data.default.version} · last saved ${new Date(
            data.default.updated_at * 1000,
        ).toLocaleString()}`;
    }, [data]);

    if (!form || !data) {
        return (
            <div data-testid="settings-personality">
                {error ? (
                    <ErrorBanner
                        message={error}
                        onDismiss={() => setError(null)}
                        className="mb-3"
                    />
                ) : (
                    <div className="execlaw-muted small">Loading personality…</div>
                )}
            </div>
        );
    }

    return (
        <div data-testid="settings-personality">
            <h3 className="h6 mb-1">Personality</h3>
            <p className="execlaw-muted small mb-3">
                Operator-editable system-prompt fields — the agent's voice,
                tone, persona, and any standing instructions you want it to
                follow. Changes apply to every conversation that doesn't have
                a per-conversation override (see below).
            </p>

            <ErrorBanner message={error} onDismiss={() => setError(null)} className="mb-3" />

            <div className="execlaw-card mb-3" data-testid="personality-default-form">
                <div className="d-flex align-items-baseline gap-2 mb-3">
                    <span className="execlaw-card__title flex-grow-1">
                        Default scope
                    </span>
                    <span className="execlaw-muted small">{versionHint}</span>
                </div>

                <Form.Group className="mb-3" controlId="personality-display-name">
                    <Form.Label className="small mb-1">Display name</Form.Label>
                    <Form.Control
                        type="text"
                        value={form.display_name}
                        onChange={(e) => onChange("display_name", e.target.value)}
                        data-testid="personality-display-name"
                        placeholder="execlaw"
                    />
                    <Form.Text className="execlaw-muted">
                        What the agent calls itself in introductions.
                    </Form.Text>
                </Form.Group>

                <Form.Group className="mb-3" controlId="personality-role">
                    <Form.Label className="small mb-1">Role</Form.Label>
                    <Form.Control
                        type="text"
                        value={form.role}
                        onChange={(e) => onChange("role", e.target.value)}
                        data-testid="personality-role"
                        placeholder="Personal assistant"
                    />
                </Form.Group>

                <Form.Group className="mb-3" controlId="personality-tone">
                    <Form.Label className="small mb-1">Tone</Form.Label>
                    <Form.Control
                        as="textarea"
                        rows={2}
                        value={form.tone}
                        onChange={(e) => onChange("tone", e.target.value)}
                        data-testid="personality-tone"
                        placeholder="Concise, practical, no filler."
                    />
                </Form.Group>

                <Form.Group className="mb-3" controlId="personality-style">
                    <Form.Label className="small mb-1">Communication style</Form.Label>
                    <Form.Control
                        as="textarea"
                        rows={2}
                        value={form.communication_style}
                        onChange={(e) =>
                            onChange("communication_style", e.target.value)
                        }
                        data-testid="personality-style"
                        placeholder="Single-sentence replies. No markdown unless asked."
                    />
                </Form.Group>

                <Form.Group className="mb-3" controlId="personality-initiative">
                    <Form.Label className="small mb-1">Initiative</Form.Label>
                    <Form.Control
                        as="textarea"
                        rows={2}
                        value={form.initiative}
                        onChange={(e) => onChange("initiative", e.target.value)}
                        data-testid="personality-initiative"
                        placeholder="Ask before scheduling. Don't volunteer summaries unless prompted."
                    />
                </Form.Group>

                <Form.Group className="mb-3" controlId="personality-about-agent">
                    <Form.Label className="small mb-1">
                        About me (the agent)
                    </Form.Label>
                    <Form.Control
                        as="textarea"
                        rows={3}
                        value={form.about_agent}
                        onChange={(e) => onChange("about_agent", e.target.value)}
                        data-testid="personality-about-agent"
                        placeholder="Persona / backstory the agent should embody when it speaks."
                    />
                </Form.Group>

                <Form.Group className="mb-3" controlId="personality-about-controller">
                    <Form.Label className="small mb-1">
                        About you (the controller)
                    </Form.Label>
                    <Form.Control
                        as="textarea"
                        rows={3}
                        value={form.about_controller}
                        onChange={(e) =>
                            onChange("about_controller", e.target.value)
                        }
                        data-testid="personality-about-controller"
                        placeholder="Facts the agent should know about you. The agent's learned-facts memory layer is appended after this — your hand-edited note wins on conflict."
                    />
                </Form.Group>

                <Form.Group className="mb-3" controlId="personality-custom">
                    <Form.Label className="small mb-1">Additional instructions</Form.Label>
                    <Form.Control
                        as="textarea"
                        rows={6}
                        value={form.custom_instructions}
                        onChange={(e) =>
                            onChange("custom_instructions", e.target.value)
                        }
                        data-testid="personality-custom"
                        placeholder="Free-form directives. Long edits live here."
                    />
                </Form.Group>

                <Form.Group className="mb-3" controlId="personality-voice-id">
                    <Form.Label className="small mb-1">Voice ID</Form.Label>
                    <Form.Control
                        type="text"
                        value={form.voice_id}
                        onChange={(e) => onChange("voice_id", e.target.value)}
                        data-testid="personality-voice-id"
                        placeholder="bf_emma"
                    />
                    <Form.Text className="execlaw-muted">
                        Pins the TTS voice for the voice pipeline. Default
                        <code className="ms-1">bf_emma</code>; alternatives include
                        <code className="ms-1">am_michael</code>.
                    </Form.Text>
                </Form.Group>

                <div className="d-flex align-items-center gap-2">
                    <Button
                        variant="primary"
                        size="sm"
                        onClick={() => void onSave()}
                        disabled={busy}
                        data-testid="personality-save"
                    >
                        {busy ? "Saving…" : "Save"}
                    </Button>
                    {savedAt && !busy && (
                        <span
                            className="execlaw-muted small"
                            data-testid="personality-saved-at"
                        >
                            Saved.
                        </span>
                    )}
                </div>
            </div>

            {preview !== null && (
                <div
                    className="execlaw-card mb-3"
                    data-testid="personality-preview"
                >
                    <div className="execlaw-card__title mb-2">
                        <i className="bi bi-eye me-2" aria-hidden />
                        Composed system prompt
                    </div>
                    <p className="execlaw-muted small mb-2">
                        What the agent sees on a fresh conversation, with the
                        operator-editable half above the built-in trust-class
                        rules.
                    </p>
                    <pre
                        className="execlaw-muted small mb-0"
                        style={{
                            whiteSpace: "pre-wrap",
                            background: "transparent",
                            padding: 0,
                        }}
                    >
                        {preview}
                    </pre>
                </div>
            )}

            <div
                className="execlaw-card"
                data-testid="personality-overrides-card"
            >
                <div className="execlaw-card__title mb-2">
                    Per-conversation overrides
                </div>
                {data.overrides.length === 0 ? (
                    <p className="execlaw-muted small mb-0">
                        No overrides on file. The runner uses default-scope
                        values for every conversation.
                    </p>
                ) : (
                    <ul className="list-unstyled mb-0">
                        {data.overrides.map((o) => (
                            <li
                                key={o.scope_ref}
                                className="d-flex align-items-baseline gap-2 mb-2"
                                data-testid="personality-override-row"
                            >
                                <code className="small">{o.scope_ref}</code>
                                <span className="execlaw-muted small flex-grow-1">
                                    overrides{" "}
                                    {o.override_fields
                                        .filter((f) => f !== "voice_id")
                                        .join(", ") || "voice only"}
                                </span>
                                <Button
                                    size="sm"
                                    variant="outline-danger"
                                    onClick={() =>
                                        void onDeleteOverride(o.scope_ref)
                                    }
                                    data-testid="personality-override-delete"
                                >
                                    Drop
                                </Button>
                            </li>
                        ))}
                    </ul>
                )}
            </div>
        </div>
    );
}
