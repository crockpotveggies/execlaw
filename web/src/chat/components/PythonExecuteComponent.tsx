// Inline renderer for python.execute tool_results.
//
// The host's PythonExecuteTool returns a JSON payload carrying the
// fields below alongside `chat_component_kind: "python_execute"`,
// which chatComponentRegistry dispatches to us:
//
//   {
//     "chat_component_kind": "python_execute",
//     "outputs": ExecuteOutput[],       // see crates/server/src/python_sandbox/mime.rs
//     "execution_count": number,
//     "duration_ms": number,
//     "status": "ok" | "error" | "timeout" | "kernel_died" | "output_too_large",
//     "created_files"?: CreatedFileRef[]
//   }
//
// ExecuteOutput is a tagged union (`kind` discriminator):
//   { kind: "stream", name: "stdout"|"stderr", text: string }
//   { kind: "execute_result", execution_count: number, bundle: MimeBundle[] }
//   { kind: "display_data", bundle: MimeBundle[] }
//   { kind: "error", ename: string, evalue: string, traceback: string[] }
//
// MimeBundle.data is keyed on MimeBundle.mime_type. The renderer
// picks the richest representation it knows: text/html (sandboxed
// iframe) > image/png (inline base64) > text/plain (<pre>).
//
// No agent-supplied HTML or script reaches the chat DOM unsandboxed.
// HTML mime bundles render inside a sandbox iframe with no allow-*
// flags so even a maliciously-formed DataFrame _repr_html_ can't
// execute JS or read cookies.

import { registerChatComponent, type ChatComponentProps } from "../chatComponentRegistry";

interface MimeBundleEntry {
    mime_type: string;
    data: unknown;
}

type ExecuteOutput =
    | { kind: "stream"; name: "stdout" | "stderr"; text: string }
    | { kind: "execute_result"; execution_count: number; bundle: MimeBundleEntry[] }
    | { kind: "display_data"; bundle: MimeBundleEntry[] }
    | { kind: "error"; ename: string; evalue: string; traceback: string[] };

type ExecuteStatus =
    | "ok"
    | "error"
    | "timeout"
    | "kernel_died"
    | "output_too_large";

interface CreatedFileRef {
    name: string;
    size: number;
    mime: string;
    attachment_id: string;
}

interface PythonExecutePayload {
    outputs?: ExecuteOutput[];
    execution_count?: number;
    duration_ms?: number;
    status?: ExecuteStatus;
    created_files?: CreatedFileRef[];
}

/// Strip ANSI escape sequences from a string. Tracebacks arrive
/// pre-colored by IPython; we render them as plain text and let
/// the CSS handle styling.
function stripAnsi(s: string): string {
    // Matches CSI sequences: ESC [ ... letter
    // eslint-disable-next-line no-control-regex
    return s.replace(/\[[0-9;]*[A-Za-z]/g, "");
}

/// Pick the richest representation the renderer knows. Priority:
/// 1. text/html (sandbox iframe — DataFrames, Plotly HTML)
/// 2. image/png (base64 inline)
/// 3. image/jpeg / webp
/// 4. text/plain (fallback)
function pickBest(bundle: MimeBundleEntry[]): MimeBundleEntry | null {
    const priorities = [
        "text/html",
        "image/png",
        "image/jpeg",
        "image/webp",
        "application/json",
        "text/markdown",
        "text/plain",
    ];
    for (const mime of priorities) {
        const hit = bundle.find((b) => b.mime_type === mime);
        if (hit) return hit;
    }
    return bundle[0] ?? null;
}

function renderBundle(bundle: MimeBundleEntry[], keyPrefix: string) {
    const pick = pickBest(bundle);
    if (!pick) return null;
    const data = pick.data;
    if (pick.mime_type === "text/html" && typeof data === "string") {
        return (
            <iframe
                key={keyPrefix}
                sandbox=""
                srcDoc={data}
                style={{
                    width: "100%",
                    minHeight: 60,
                    maxHeight: 480,
                    border: "1px solid var(--bs-border-color)",
                    borderRadius: 4,
                    background: "white",
                }}
                title="python.execute html output"
            />
        );
    }
    if (
        (pick.mime_type === "image/png" ||
            pick.mime_type === "image/jpeg" ||
            pick.mime_type === "image/webp") &&
        typeof data === "string"
    ) {
        return (
            <img
                key={keyPrefix}
                src={`data:${pick.mime_type};base64,${data}`}
                alt="python.execute image output"
                style={{ maxWidth: "100%", maxHeight: 480, display: "block" }}
            />
        );
    }
    // application/json, text/markdown, text/plain → preformatted
    const text =
        typeof data === "string" ? data : JSON.stringify(data, null, 2);
    return (
        <pre
            key={keyPrefix}
            className="mb-0"
            style={{
                fontFamily: "var(--bs-font-monospace)",
                fontSize: "0.85em",
                whiteSpace: "pre-wrap",
                margin: 0,
                padding: "0.4em 0.6em",
                background: "var(--bs-tertiary-bg)",
                borderRadius: 4,
            }}
        >
            {text}
        </pre>
    );
}

function PythonExecute({ data }: ChatComponentProps) {
    const payload = data as PythonExecutePayload;
    const outputs = payload.outputs ?? [];
    const status = payload.status ?? "ok";
    const durationMs = payload.duration_ms;
    const createdFiles = payload.created_files ?? [];

    const statusBadgeClass: Record<ExecuteStatus, string> = {
        ok: "text-bg-success",
        error: "text-bg-danger",
        timeout: "text-bg-warning",
        kernel_died: "text-bg-danger",
        output_too_large: "text-bg-warning",
    };

    return (
        <div
            className="python-execute-result border rounded"
            style={{ padding: "0.5em", background: "var(--bs-body-bg)" }}
        >
            <div
                className="d-flex align-items-center gap-2 mb-2"
                style={{ fontSize: "0.85em" }}
            >
                <span
                    className={`badge ${statusBadgeClass[status] ?? "text-bg-secondary"}`}
                >
                    python · {status}
                </span>
                {durationMs !== undefined && (
                    <span className="text-body-secondary">
                        {durationMs} ms
                    </span>
                )}
                {payload.execution_count !== undefined && (
                    <span className="text-body-secondary">
                        cell #{payload.execution_count}
                    </span>
                )}
            </div>

            {outputs.length === 0 && (
                <div className="text-body-secondary" style={{ fontSize: "0.85em" }}>
                    (no output)
                </div>
            )}

            {outputs.map((out, i) => {
                const key = `out-${i}`;
                if (out.kind === "stream") {
                    return (
                        <pre
                            key={key}
                            className="mb-1"
                            style={{
                                fontFamily: "var(--bs-font-monospace)",
                                fontSize: "0.85em",
                                whiteSpace: "pre-wrap",
                                margin: 0,
                                padding: "0.3em 0.6em",
                                color:
                                    out.name === "stderr"
                                        ? "var(--bs-danger)"
                                        : undefined,
                                background: "var(--bs-tertiary-bg)",
                                borderRadius: 4,
                            }}
                        >
                            {stripAnsi(out.text)}
                        </pre>
                    );
                }
                if (out.kind === "error") {
                    return (
                        <details
                            key={key}
                            className="mb-1"
                            style={{
                                border: "1px solid var(--bs-danger)",
                                borderRadius: 4,
                                padding: "0.3em 0.6em",
                            }}
                        >
                            <summary
                                style={{
                                    color: "var(--bs-danger)",
                                    fontFamily: "var(--bs-font-monospace)",
                                    fontSize: "0.85em",
                                    cursor: "pointer",
                                }}
                            >
                                {out.ename}: {out.evalue}
                            </summary>
                            <pre
                                style={{
                                    fontFamily: "var(--bs-font-monospace)",
                                    fontSize: "0.8em",
                                    whiteSpace: "pre-wrap",
                                    margin: "0.4em 0 0 0",
                                }}
                            >
                                {out.traceback.map(stripAnsi).join("\n")}
                            </pre>
                        </details>
                    );
                }
                // execute_result | display_data
                return <div key={key} className="mb-1">{renderBundle(out.bundle, key)}</div>;
            })}

            {createdFiles.length > 0 && (
                <div className="mt-2" style={{ fontSize: "0.85em" }}>
                    <div className="text-body-secondary mb-1">
                        Files created:
                    </div>
                    {createdFiles.map((f) => (
                        <a
                            key={f.attachment_id}
                            href={`/api/attachments/${f.attachment_id}`}
                            className="d-block text-decoration-none"
                            download={f.name}
                            style={{ fontFamily: "var(--bs-font-monospace)" }}
                        >
                            📎 {f.name} ({f.size} bytes)
                        </a>
                    ))}
                </div>
            )}
        </div>
    );
}

registerChatComponent("python_execute", PythonExecute);
