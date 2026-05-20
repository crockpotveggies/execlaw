// Client for the M5 /admin/inference observability surface.

import { apiFetch } from "./client";

export type InferenceConsumer =
    | "chat"
    | "routines"
    | "research"
    | "automations"
    | "other";

export interface ConsumerSnapshot {
    consumer: InferenceConsumer;
    in_flight: number;
    total_calls: number;
    total_failures: number;
    sample_count: number;
    p50_ms: number | null;
    p95_ms: number | null;
}

export interface MetricsSnapshot {
    consumers: ConsumerSnapshot[];
}

export async function getInferenceMetrics(
    tokenAccessor: () => string | null,
): Promise<MetricsSnapshot> {
    return apiFetch<MetricsSnapshot>(
        "/api/admin/inference/metrics",
        {},
        tokenAccessor,
    );
}

export function consumerLabel(c: InferenceConsumer): string {
    switch (c) {
        case "chat":
            return "Chat";
        case "routines":
            return "Routines";
        case "research":
            return "Research";
        case "automations":
            return "Automations";
        case "other":
            return "Other";
    }
}
