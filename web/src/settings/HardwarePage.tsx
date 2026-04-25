// Hardware page — fetch /api/admin/hardware once and render the
// detected GPU + CPU + memory profile. Read-only; deployments edit
// lands later (Phase 6c-extension).

import { useCallback, useEffect, useState } from "react";
import { getHardware, type HardwareProfile } from "../api/endpoints";
import { useAuth } from "../auth/AuthContext";

export function HardwarePage() {
    const auth = useAuth();
    const getToken = useCallback(() => auth.getAccessToken(), [auth]);
    const [profile, setProfile] = useState<HardwareProfile | null>(null);
    const [error, setError] = useState<string | null>(null);

    useEffect(() => {
        let cancelled = false;
        (async () => {
            try {
                const r = await getHardware(getToken);
                if (!cancelled) setProfile(r);
            } catch (e) {
                if (!cancelled)
                    setError(e instanceof Error ? e.message : String(e));
            }
        })();
        return () => {
            cancelled = true;
        };
    }, [getToken]);

    return (
        <div data-testid="settings-hardware">
            <h3 className="h6 mb-3">Hardware</h3>

            {error && (
                <div className="execlaw-error-banner mb-3" role="alert">
                    {error}
                </div>
            )}

            {profile === null ? (
                <div className="execlaw-muted small">Probing hardware…</div>
            ) : (
                <>
                    <GpuList gpus={Array.isArray(profile.gpus) ? profile.gpus : []} />
                    <details className="execlaw-card">
                        <summary className="execlaw-muted small">
                            Raw profile JSON
                        </summary>
                        <pre className="mt-2 mb-0 small">
                            {JSON.stringify(profile, null, 2)}
                        </pre>
                    </details>
                </>
            )}
        </div>
    );
}

function GpuList({ gpus }: { gpus: HardwareProfile["gpus"] & object[] }) {
    if (!gpus || gpus.length === 0) {
        return (
            <div className="execlaw-card">
                <div className="execlaw-card__title">GPUs</div>
                <div className="execlaw-muted small">
                    No GPUs detected. The control plane is running CPU-only —
                    inference plugins that need a GPU will be unavailable.
                </div>
            </div>
        );
    }
    return (
        <div className="execlaw-card">
            <div className="execlaw-card__title">GPUs ({gpus.length})</div>
            {gpus.map((g, i) => (
                <div key={i} className="execlaw-card__row">
                    <div>
                        <div>
                            <strong>
                                {(g.vendor as string) ?? "GPU"}{" "}
                                {(g.model as string) ?? ""}
                            </strong>
                        </div>
                        <div className="execlaw-muted small">
                            {(g.pci_vendor_id as string | undefined) ?? "?"}:
                            {(g.pci_device_id as string | undefined) ?? "?"}
                        </div>
                    </div>
                </div>
            ))}
        </div>
    );
}
