// Settings → Trust policy (Phase 9.2, MIGRATION_PLAN §2.6 + §850).
//
// Five operator-editable rules that govern the cold-contact trust
// ladder + mixed-trust group resolution:
//
//   * auto_trust_contacts          — bool toggle
//   * min_trust_hint_for_auto_trust — Contact | Colleague | Organization
//   * mixed_trust_policy           — min_wins (only choice today)
//   * identity_plugin_order        — newline-separated plugin ids
//   * delegated_trust_default_ttl  — duration string (e.g. "7d", "12h")
//
// Validation runs server-side; the SPA mirrors common cases for
// faster feedback (TTL format, comma-free plugin ids).

import { useCallback, useEffect, useState } from "react";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import {
    getTrustPolicy,
    putTrustPolicy,
    type AutoTrustClass,
    type MinTrustHint,
    type MixedTrustPolicy,
    type TrustPolicyView,
} from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";
import { ErrorBanner } from "../components/ErrorBanner";

interface FormState {
    auto_trust_contacts: boolean;
    min_trust_hint_for_auto_trust: MinTrustHint;
    auto_trust_class: AutoTrustClass;
    mixed_trust_policy: MixedTrustPolicy;
    /** Newline-separated for the textarea; serialised as array on save. */
    identity_plugin_order: string;
    delegated_trust_default_ttl: string;
}

function fromView(v: TrustPolicyView): FormState {
    return {
        auto_trust_contacts: v.auto_trust_contacts,
        min_trust_hint_for_auto_trust: v.min_trust_hint_for_auto_trust,
        auto_trust_class: v.auto_trust_class,
        mixed_trust_policy: v.mixed_trust_policy,
        identity_plugin_order: v.identity_plugin_order.join("\n"),
        delegated_trust_default_ttl: v.delegated_trust_default_ttl,
    };
}

function toView(f: FormState): TrustPolicyView {
    return {
        auto_trust_contacts: f.auto_trust_contacts,
        min_trust_hint_for_auto_trust: f.min_trust_hint_for_auto_trust,
        auto_trust_class: f.auto_trust_class,
        mixed_trust_policy: f.mixed_trust_policy,
        identity_plugin_order: f.identity_plugin_order
            .split(/\r?\n/)
            .map((s) => s.trim())
            .filter((s) => s.length > 0),
        delegated_trust_default_ttl: f.delegated_trust_default_ttl,
    };
}

const TTL_RE = /^\d+(s|m|h|d)$/;

function localValidationError(f: FormState): string | null {
    const ttl = f.delegated_trust_default_ttl.trim();
    if (!TTL_RE.test(ttl)) {
        return "Delegated TTL must look like '7d', '12h', '30m', '90s'.";
    }
    for (const id of f.identity_plugin_order.split(/\r?\n/)) {
        if (id.includes(",")) {
            return "Plugin ids can't contain commas.";
        }
    }
    return null;
}

export function TrustPolicyPage() {
    const auth = useAuth();
    const getToken = auth.getAccessToken;

    const [form, setForm] = useState<FormState | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [savedAt, setSavedAt] = useState<number | null>(null);
    const [busy, setBusy] = useState(false);

    const refresh = useCallback(async () => {
        try {
            const v = await getTrustPolicy(getToken);
            setForm(fromView(v));
            setError(null);
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
        const localErr = localValidationError(form);
        if (localErr) {
            setError(localErr);
            return;
        }
        setBusy(true);
        try {
            const r = await putTrustPolicy(toView(form), getToken);
            setForm(fromView(r));
            setSavedAt(Date.now());
            setError(null);
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
        } finally {
            setBusy(false);
        }
    }, [form, getToken]);

    if (!form) {
        return (
            <div data-testid="settings-trust-policy">
                {error ? (
                    <ErrorBanner
                        message={error}
                        onDismiss={() => setError(null)}
                        className="mb-3"
                    />
                ) : (
                    <div className="execlaw-muted small">
                        Loading trust policy…
                    </div>
                )}
            </div>
        );
    }

    return (
        <div data-testid="settings-trust-policy">
            <p className="execlaw-muted small mb-3">
                Rules the cold-contact trust ladder and mixed-trust group
                resolution follow. Defaults match the documented behaviour;
                changes apply on save and audit.
            </p>

            <ErrorBanner message={error} onDismiss={() => setError(null)} className="mb-3" />

            <div className="execlaw-card">
                <Form.Group className="mb-3" controlId="trust-auto">
                    <Form.Check
                        type="switch"
                        label="Auto-trust plugin-matched contacts"
                        checked={form.auto_trust_contacts}
                        onChange={(e) =>
                            onChange(
                                "auto_trust_contacts",
                                e.target.checked,
                            )
                        }
                        data-testid="trust-auto-toggle"
                    />
                    <Form.Text className="execlaw-muted">
                        When on, contacts whose trust hint clears the
                        threshold below are admitted at the class set in
                        &quot;Auto-trust class&quot; without prompting the
                        controller.
                    </Form.Text>
                </Form.Group>

                <Form.Group className="mb-3" controlId="trust-min-hint">
                    <Form.Label className="small mb-1">
                        Minimum trust hint for auto-trust
                    </Form.Label>
                    <Form.Select
                        value={form.min_trust_hint_for_auto_trust}
                        onChange={(e) =>
                            onChange(
                                "min_trust_hint_for_auto_trust",
                                e.target.value as MinTrustHint,
                            )
                        }
                        data-testid="trust-min-hint"
                    >
                        <option value="Contact">Contact</option>
                        <option value="Colleague">Colleague</option>
                        <option value="Organization">Organization</option>
                    </Form.Select>
                    <Form.Text className="execlaw-muted">
                        Plugin-supplied identity strength must reach at least
                        this level for auto-trust to fire.
                    </Form.Text>
                </Form.Group>

                <Form.Group className="mb-3" controlId="trust-auto-class">
                    <Form.Label className="small mb-1">
                        Auto-trust class
                    </Form.Label>
                    <Form.Select
                        value={form.auto_trust_class}
                        onChange={(e) =>
                            onChange(
                                "auto_trust_class",
                                e.target.value as AutoTrustClass,
                            )
                        }
                        data-testid="trust-auto-class"
                    >
                        <option value="KnownLimited">
                            KnownLimited · reply on the originating transport only
                        </option>
                        <option value="KnownTrusted">
                            KnownTrusted · reply + memory + safe tools
                        </option>
                    </Form.Select>
                    <Form.Text className="execlaw-muted">
                        Trust class auto-admitted senders enter at. The
                        conservative default is KnownLimited so a saved
                        contact who messages the agent for the first time
                        can&apos;t e.g. read memory without explicit operator
                        opt-in.
                    </Form.Text>
                </Form.Group>

                <Form.Group className="mb-3" controlId="trust-mixed">
                    <Form.Label className="small mb-1">
                        Mixed-trust group policy
                    </Form.Label>
                    <Form.Select
                        value={form.mixed_trust_policy}
                        onChange={(e) =>
                            onChange(
                                "mixed_trust_policy",
                                e.target.value as MixedTrustPolicy,
                            )
                        }
                        data-testid="trust-mixed"
                    >
                        <option value="min_wins">
                            min_wins · effective trust = lowest participant
                        </option>
                    </Form.Select>
                </Form.Group>

                <Form.Group className="mb-3" controlId="trust-ttl">
                    <Form.Label className="small mb-1">
                        Default Delegated grant TTL
                    </Form.Label>
                    <Form.Control
                        type="text"
                        value={form.delegated_trust_default_ttl}
                        onChange={(e) =>
                            onChange(
                                "delegated_trust_default_ttl",
                                e.target.value,
                            )
                        }
                        data-testid="trust-ttl"
                        placeholder="7d"
                    />
                    <Form.Text className="execlaw-muted">
                        Duration string: digits + s | m | h | d. Examples:
                        <code className="ms-1">90s</code>,
                        <code className="ms-1">12h</code>,
                        <code className="ms-1">7d</code>.
                    </Form.Text>
                </Form.Group>

                <Form.Group className="mb-3" controlId="trust-plugin-order">
                    <Form.Label className="small mb-1">
                        Identity-plugin priority (one plugin id per line)
                    </Form.Label>
                    <Form.Control
                        as="textarea"
                        rows={4}
                        value={form.identity_plugin_order}
                        onChange={(e) =>
                            onChange(
                                "identity_plugin_order",
                                e.target.value,
                            )
                        }
                        data-testid="trust-plugin-order"
                        placeholder="signal-contacts&#10;google-contacts"
                    />
                    <Form.Text className="execlaw-muted">
                        First-match wins when multiple identity plugins claim
                        the same handle. Empty list means install order.
                    </Form.Text>
                </Form.Group>

                <div className="d-flex align-items-center gap-2">
                    <Button
                        variant="primary"
                        size="sm"
                        onClick={() => void onSave()}
                        disabled={busy}
                        data-testid="trust-save"
                    >
                        {busy ? "Saving…" : "Save"}
                    </Button>
                    {savedAt && !busy && (
                        <span
                            className="execlaw-muted small"
                            data-testid="trust-saved-at"
                        >
                            Saved.
                        </span>
                    )}
                </div>
            </div>
        </div>
    );
}
