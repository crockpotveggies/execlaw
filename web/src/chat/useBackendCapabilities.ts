// 2026-05-15 — runtime probe of the Standard inference backend's
// vision-capability flag. The chat shell uses this to decide whether
// the Composer surfaces the image-attach affordance.
//
// Shape mirrors the server response — the SPA never assumes a
// specific model id; whether the loaded model is multimodal is the
// server's call (it probes `/v1/models` and applies a curated known-
// vision-model matcher).
//
// Refresh cadence: fetch on mount, then on token rotation. The
// operator hot-swapping a multimodal backend mid-session is rare
// enough that a manual page reload is acceptable until we add a WS
// "backend changed" event in a follow-up. The fetch is cheap (single
// HTTP RTT to the backend's /v1/models) so the on-mount hit is fine
// even on slow networks.

import { useEffect, useState } from "react";
import { ApiError } from "../api/client";
import {
    getBackendCapabilities,
    type BackendCapabilitiesResponse,
} from "../api/endpoints";

export interface BackendCapabilities {
    /** True only when the server confirmed the loaded model is a known
     *  multimodal family. Defaults to false (no probe yet / probe
     *  failed / model is text-only). */
    multimodal: boolean;
    /** Was the most recent probe able to reach the backend? */
    reachable: boolean;
    /** Model id reported by the backend's `/v1/models`. */
    modelId: string | null;
    /** Target long-edge dimension the SPA should downscale to before
     *  base64. 0 when the backend isn't multimodal. */
    recommendedImageEdge: number;
}

const DEFAULT_CAPS: BackendCapabilities = {
    multimodal: false,
    reachable: false,
    modelId: null,
    recommendedImageEdge: 0,
};

export function useBackendCapabilities(
    getToken: () => string | null,
    enabled: boolean = true,
): BackendCapabilities {
    const [caps, setCaps] = useState<BackendCapabilities>(DEFAULT_CAPS);

    useEffect(() => {
        if (!enabled) {
            setCaps(DEFAULT_CAPS);
            return;
        }
        let cancelled = false;
        (async () => {
            try {
                const resp: BackendCapabilitiesResponse =
                    await getBackendCapabilities("Standard", getToken);
                if (cancelled) return;
                setCaps({
                    multimodal: resp.multimodal,
                    reachable: resp.reachable,
                    modelId: resp.model_id,
                    recommendedImageEdge: resp.recommended_image_edge,
                });
            } catch (e) {
                // Auth failures bubble up to the chat shell's auth flow;
                // any other error → caller already shows a generic error
                // banner, no need to noisily log this probe.
                if (!cancelled && !(e instanceof ApiError)) {
                    setCaps(DEFAULT_CAPS);
                }
            }
        })();
        return () => {
            cancelled = true;
        };
    }, [enabled, getToken]);

    return caps;
}
