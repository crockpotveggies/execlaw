// Composer — text input + send button rendered as a single Bootstrap
// input-group so they're visually flush. Auto-grows the textarea up
// to a max height; sends on Enter, Shift+Enter inserts a newline.
//
// Phase 13.A — also hosts the voice mic button to the left of the
// send button. Voice capture is gated on the chat shell providing a
// `sendVoiceFrame` accessor; when absent (e.g. settings shell) the
// button is hidden.

import { useEffect, useRef, useState, type DragEvent, type FormEvent, type KeyboardEvent } from "react";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import Spinner from "react-bootstrap/Spinner";
import type { InlineAttachment, SkillListEntry } from "../api/endpoints";
import type { VoiceReadiness } from "./useVoiceReadiness";
import { VoiceCaptureButton } from "./VoiceCaptureButton";

/// One attachment staged in the composer before send. The
/// component holds the data URL inline; on submit it's handed to
/// `onSend` as an `InlineAttachment` and the server persists. A
/// local-only `localId` lets the preview chip render before the
/// file is even finished reading; `dataUrl` lands the moment
/// `FileReader` resolves.
///
/// `kind` discriminates rendering: `"image"` shows a thumbnail (the
/// vision-content path), `"file"` shows a file icon + filename
/// chip (the python-sandbox hydration path, added 2026-05-18 to
/// let the operator hand a CSV / PDF / JSON / etc. to the agent
/// so it can operate on it via `python.execute`).
interface PendingAttachment {
    localId: string;
    name: string;
    mime: string;
    dataUrl: string;
    sizeBytes: number;
    kind: "image" | "file";
}

/// Accept-list for the "Attach file" picker (the non-image path).
/// Mirrors `ALLOWED_ATTACHMENT_MIMES` on the server (minus the
/// image group, which has its own picker). Operator picks one of
/// these from disk, the SPA base64-encodes inline, the server
/// validates against the same list before persisting.
const FILE_PICKER_ACCEPT = [
    ".csv",
    ".tsv",
    ".json",
    ".txt",
    ".md",
    ".pdf",
    ".xlsx",
    ".xls",
].join(",");

/// Image MIMEs accepted by the server (`ALLOWED_ATTACHMENT_MIMES`
/// image subset). Used by the drag-drop classifier to route a
/// dropped file to the image branch — `mime.startsWith("image/")`
/// would be too permissive (image/svg+xml would slip through here
/// but get rejected at the server, leaving an orphan chip).
const IMAGE_MIME_ALLOWLIST: ReadonlySet<string> = new Set([
    "image/png",
    "image/jpeg",
    "image/webp",
    "image/gif",
]);

/// Data MIMEs accepted by the server. Same source of truth as the
/// backend's `ALLOWED_ATTACHMENT_MIMES`. The drag-drop classifier
/// uses this for the file branch; anything outside both
/// IMAGE_MIME_ALLOWLIST and this set is rejected client-side with
/// an inline ErrorBanner so the operator gets immediate feedback
/// instead of a silent server 400.
const DATA_MIME_ALLOWLIST: ReadonlySet<string> = new Set([
    "text/csv",
    "text/tab-separated-values",
    "application/json",
    "text/plain",
    "text/markdown",
    "application/pdf",
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    "application/vnd.ms-excel",
]);

/// Drag-drop classification result. The drop handler walks every
/// dropped file, classifies, then routes the image group through
/// the image prep path, the file group through the data path, and
/// surfaces the rejected names to the operator via ErrorBanner.
type DropClassification = "image" | "file" | "reject";

/// Decide which path a dropped file goes through. The browser
/// reports `file.type` from the OS — usually accurate, but
/// occasionally empty for less-common MIMEs (`.csv` on some
/// Windows installs, `.md` everywhere). Falls back to extension
/// via `inferFileMime` for those cases. Returns "reject" for
/// anything outside the two allowlists — the operator sees an
/// inline error instead of a silent server rejection later.
function classifyDroppedFile(file: File): DropClassification {
    // Prefer the browser-reported MIME; only fall through to
    // extension-derived for the empty case.
    const reported = file.type;
    if (reported && IMAGE_MIME_ALLOWLIST.has(reported)) return "image";
    if (reported && DATA_MIME_ALLOWLIST.has(reported)) return "file";
    // Empty `file.type` is common for .csv / .md on some browsers;
    // ask `inferFileMime` to look at the extension.
    const inferred = inferFileMime(file);
    if (IMAGE_MIME_ALLOWLIST.has(inferred)) return "image";
    if (DATA_MIME_ALLOWLIST.has(inferred)) return "file";
    return "reject";
}

/// Best-effort MIME for a picked file. Browsers populate `file.type`
/// from the OS for most common extensions, but a handful of
/// extensions (`.csv` on some Windows installs, `.md` everywhere)
/// come through as the empty string. Fall back to the extension.
function inferFileMime(file: File): string {
    if (file.type && file.type.length > 0) return file.type;
    const name = file.name.toLowerCase();
    if (name.endsWith(".csv")) return "text/csv";
    if (name.endsWith(".tsv")) return "text/tab-separated-values";
    if (name.endsWith(".json")) return "application/json";
    if (name.endsWith(".md")) return "text/markdown";
    if (name.endsWith(".txt")) return "text/plain";
    if (name.endsWith(".pdf")) return "application/pdf";
    if (name.endsWith(".xlsx"))
        return "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
    if (name.endsWith(".xls")) return "application/vnd.ms-excel";
    return "application/octet-stream";
}

/// Read a `File` as a `data:<mime>;base64,<bytes>` URL. Mirrors the
/// shape `prepareImageForUpload` returns for the image path, minus
/// the downscale (data files go through untouched).
function readFileAsDataUrl(
    file: File,
    mime: string,
): Promise<{ dataUrl: string; sizeBytes: number; mime: string }> {
    return new Promise((resolve, reject) => {
        const reader = new FileReader();
        reader.onload = () => {
            const result = reader.result;
            if (typeof result !== "string") {
                reject(new Error("FileReader returned non-string result"));
                return;
            }
            // Some browsers fill in `application/octet-stream` for
            // unknown types regardless of file extension. Splice in
            // our inferred MIME so the server's acceptlist check
            // doesn't reject a perfectly-valid .csv.
            const fixedDataUrl = result.replace(
                /^data:[^;]+;base64,/,
                `data:${mime};base64,`,
            );
            resolve({
                dataUrl: fixedDataUrl,
                sizeBytes: file.size,
                mime,
            });
        };
        reader.onerror = () =>
            reject(reader.error ?? new Error("FileReader failed"));
        reader.readAsDataURL(file);
    });
}

/// File-icon class for a non-image attachment chip. Mirrors
/// AttachmentCard's icon table (PDF, CSV, JSON, etc.) so the
/// composer chip matches what the operator will see in the message
/// stream once the upload commits.
function fileIconForMime(mime: string): string {
    if (mime === "application/pdf") return "bi bi-file-earmark-pdf";
    if (mime === "text/csv" || mime === "text/tab-separated-values")
        return "bi bi-file-earmark-spreadsheet";
    if (
        mime ===
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" ||
        mime === "application/vnd.ms-excel"
    )
        return "bi bi-file-earmark-excel";
    if (mime === "application/json") return "bi bi-file-earmark-code";
    if (mime === "text/markdown") return "bi bi-markdown";
    if (mime === "text/plain") return "bi bi-file-earmark-text";
    return "bi bi-file-earmark";
}

interface Props {
    disabled?: boolean;
    /**
     * Channel name (e.g. "signal") when this thread is bridged to a
     * non-web transport. Drops the textarea/send/mic chrome entirely
     * and renders a "Thread is managed on …" notice instead — the
     * operator's outbound on a Signal-bridged thread MUST flow
     * through Signal itself, not the web composer.
     *
     * `null` / `undefined` for normal web threads.
     */
    bridgedChannel?: string | null;
    onSend: (
        text: string,
        attachments: InlineAttachment[],
        skillNames: string[],
    ) => Promise<void> | void;
    /**
     * 2026-05-15 — gates the image-attach affordance ( + button +
     * popup menu). When false (text-only backend / probe hasn't
     * landed / backend unreachable), the + button is hidden and
     * pasting an image into the textarea does nothing. The chat
     * shell sources this from `useBackendCapabilities`.
     */
    multimodal?: boolean;
    /**
     * 2026-05-15 — lazy fetcher for the "Attach skill" picker (the
     * second item in the `+` menu). Returns the skills the operator
     * can apply to the next outbound message. Called the first time
     * the picker opens; result is cached locally for the rest of the
     * Composer instance's lifetime. When `undefined` the skill menu
     * item is hidden — settings/embed shells without auth can mount
     * the Composer without surfacing a skill picker. Sourced from
     * `useSkillList()` in the chat shell so Composer stays
     * AuthProvider-independent (matching the same convention as
     * `voiceReadiness`).
     */
    getSkills?: () => Promise<SkillListEntry[]>;
    /**
     * 2026-05-15 — target long-edge dimension for client-side
     * downscale before base64 encode. 0 / undefined ships the bytes
     * as-is. Sourced from the same backend-capability probe as
     * `multimodal`; the server picks the value from detected GPU
     * VRAM (24 GB → 1024, 32–64 GB → 1536, 64+ GB → 2048). Resizing
     * in the browser saves network + base64 overhead without
     * affecting model output, since vLLM's vision pipeline would
     * downsample anything larger anyway.
     */
    recommendedImageEdge?: number;
    /**
     * Phase 13.A — voice-mode wire. Returns true when the bytes
     * were queued on the WebSocket; false drops the chunk. Pass
     * `undefined` to hide the mic button entirely (e.g. routes
     * that don't have a live WebSocket).
     */
    sendVoiceFrame?: (bytes: ArrayBuffer) => boolean;
    /// Phase 13.C — voice control message accessor. Used to fire
    /// `voice_stop` on mic-off so the server flushes Whisper. The
    /// VoiceCaptureButton calls this when its session ends.
    sendVoiceControl?: (payload: object) => boolean;
    /**
     * Phase 14.D — voice backend readiness, sourced from
     * `useVoiceReadiness()` in the parent shell (Chat /
     * WelcomeView). Threading this as a prop instead of
     * calling the hook in Composer keeps the component
     * AuthProvider-independent — tests render Composer in
     * isolation and would otherwise blow up on `useAuth()`.
     * `undefined` means the caller hasn't probed; the mic
     * button defaults to "ready" so existing test setups keep
     * working.
     */
    voiceReadiness?: VoiceReadiness | null;
    /**
     * 2026-04-28 — true while a turn is streaming server-side. When
     * set, the send button is replaced by a stop button that calls
     * `onStop`. Parent shell tracks streaming state from the
     * WebSocket `phase=thinking|awaiting_tool` events. Optional —
     * routes without streaming (e.g. settings) leave it undefined
     * and the stop button never appears.
     */
    busy?: boolean;
    /**
     * 2026-04-28 — invoked when the operator clicks the stop button.
     * Parent calls `postStopTurn(conversationId)`. Optional; when
     * absent the stop button is hidden even if `busy` is true.
     */
    onStop?: () => void;
}

export function Composer({
    disabled,
    bridgedChannel,
    onSend,
    sendVoiceFrame,
    sendVoiceControl,
    voiceReadiness,
    busy,
    onStop,
    multimodal,
    recommendedImageEdge,
    getSkills,
}: Props) {
    // Bridged-channel short-circuit. Render a flat, non-interactive
    // notice in place of the composer chip. Mirrors the chip's
    // outer dimensions (max-width, padding, border-radius via the
    // shared `.execlaw-composer__shell` class) so swapping back to
    // the live composer when the operator opens an unbridged thread
    // doesn't reflow the chat region.
    if (bridgedChannel) {
        const channelLabel =
            bridgedChannel.charAt(0).toUpperCase() + bridgedChannel.slice(1);
        return (
            <div
                className="execlaw-composer__shell execlaw-composer__shell--bridged"
                data-testid="composer-bridged-notice"
                data-channel={bridgedChannel}
                role="status"
                aria-live="polite"
                style={{
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "center",
                    color: "#7d8590",
                    fontSize: "0.875rem",
                    padding: "0.85rem 1rem",
                    cursor: "not-allowed",
                }}
            >
                <i className="bi bi-info-circle" aria-hidden style={{ marginRight: "0.5rem" }} />
                Thread is managed on {channelLabel}
            </div>
        );
    }
    const [text, setText] = useState("");
    const [submitting, setSubmitting] = useState(false);
    const [attachments, setAttachments] = useState<PendingAttachment[]>([]);
    const [attachMenuOpen, setAttachMenuOpen] = useState(false);
    const textareaRef = useRef<HTMLTextAreaElement | null>(null);
    const fileInputRef = useRef<HTMLInputElement | null>(null);
    /// 2026-05-18 — second hidden <input> for the non-image
    /// "Attach file" path. Kept separate from `fileInputRef` (the
    /// image picker) so each has its own `accept` attribute and
    /// the OS picker shows the right MIME group by default.
    const dataFileInputRef = useRef<HTMLInputElement | null>(null);
    const attachMenuRef = useRef<HTMLDivElement | null>(null);
    /// 2026-05-15 — skill picker state. `mode` toggles the dropdown
    /// between the two-item top menu and the skill list view. The
    /// available skills are fetched lazily on first picker open so
    /// the Composer doesn't pay the round-trip on every chat-pane
    /// mount. `selectedSkills` is sticky across keystrokes but
    /// cleared on every send (per-turn semantics).
    const [attachMenuMode, setAttachMenuMode] = useState<"root" | "skills">("root");
    const [availableSkills, setAvailableSkills] = useState<SkillListEntry[] | null>(null);
    const [skillsLoading, setSkillsLoading] = useState(false);
    const [skillsError, setSkillsError] = useState<string | null>(null);
    const [selectedSkills, setSelectedSkills] = useState<SkillListEntry[]>([]);
    /// 2026-05-18 — drag-drop state. `isDragOver` toggles the
    /// composer-shell drop-zone highlight; `dropError` carries
    /// the most recent "unsupported file" message for the
    /// inline ErrorBanner. `dragDepth` solves the dragenter /
    /// dragleave flicker problem: dragenter fires on every child
    /// element transition, dragleave fires before the next
    /// dragenter, so a naive boolean toggles rapidly as the
    /// pointer moves over the textarea / buttons. Counting
    /// enter/leave pairs lets us flip the highlight only when
    /// the cursor truly leaves the shell.
    const [isDragOver, setIsDragOver] = useState(false);
    const [dropError, setDropError] = useState<string | null>(null);
    const dragDepth = useRef(0);

    // Auto-grow.
    useEffect(() => {
        const ta = textareaRef.current;
        if (!ta) return;
        ta.style.height = "auto";
        ta.style.height = `${Math.min(ta.scrollHeight, 192)}px`;
    }, [text]);

    // Click-outside / Escape closes the + menu. Tracked here (not on
    // the trigger) so the menu stays open while the operator hovers
    // its items. Closing always resets back to the root view so the
    // next open shows "Attach photo / Attach skill" rather than the
    // skill list the operator was last in.
    useEffect(() => {
        if (!attachMenuOpen) return;
        const close = () => {
            setAttachMenuOpen(false);
            setAttachMenuMode("root");
        };
        const onPointer = (ev: MouseEvent) => {
            const root = attachMenuRef.current;
            if (root && !root.contains(ev.target as Node)) {
                close();
            }
        };
        const onKey = (ev: globalThis.KeyboardEvent) => {
            if (ev.key === "Escape") close();
        };
        document.addEventListener("mousedown", onPointer);
        document.addEventListener("keydown", onKey);
        return () => {
            document.removeEventListener("mousedown", onPointer);
            document.removeEventListener("keydown", onKey);
        };
    }, [attachMenuOpen]);

    const onPickPhoto = () => {
        setAttachMenuOpen(false);
        setAttachMenuMode("root");
        fileInputRef.current?.click();
    };

    /// 2026-05-18 — third menu item alongside "Attach photo" /
    /// "Attach skill". Opens a separate <input type="file"> with the
    /// `accept` attribute restricted to data files (CSV, JSON, PDF,
    /// etc.) so the OS picker hides binaries by default. The actual
    /// MIME acceptlist still gets re-checked server-side; this is
    /// just the operator-affordance.
    const onPickFile = () => {
        setAttachMenuOpen(false);
        setAttachMenuMode("root");
        dataFileInputRef.current?.click();
    };

    /// Switch the menu into "skill picker" mode and lazy-load the
    /// list on first open. Cached across opens for the rest of the
    /// component's lifetime so the second open is instant. Errors
    /// surface inline in the picker; the operator can dismiss the
    /// menu and try again. Hidden entirely when the parent didn't
    /// pass a `getSkills` callback.
    const onPickSkill = () => {
        if (!getSkills) return;
        setAttachMenuMode("skills");
        if (availableSkills !== null || skillsLoading) return;
        setSkillsLoading(true);
        setSkillsError(null);
        getSkills()
            .then((list) => {
                setAvailableSkills(list);
            })
            .catch((e: unknown) => {
                setSkillsError(
                    e instanceof Error ? e.message : "failed to load skills",
                );
            })
            .finally(() => setSkillsLoading(false));
    };

    /// Add or remove a skill from the staged set. Toggle by name
    /// since the list might re-fetch and produce new object
    /// identities for the same skill.
    const toggleSkill = (skill: SkillListEntry) => {
        setSelectedSkills((prev) => {
            const has = prev.some((s) => s.name === skill.name);
            return has
                ? prev.filter((s) => s.name !== skill.name)
                : [...prev, skill];
        });
    };

    const removeSelectedSkill = (name: string) => {
        setSelectedSkills((prev) => prev.filter((s) => s.name !== name));
    };

    /// Per-file image processor. Reads the bytes, downscales when
    /// the backend asked for it, returns a `PendingAttachment`
    /// suitable for direct append to staged state. Extracted from
    /// the picker handler so the drag-drop path can reuse the
    /// same downscale + chip-shape logic.
    const processImageFile = async (
        f: File,
        idx: number,
    ): Promise<PendingAttachment> => {
        const result = await prepareImageForUpload(f, recommendedImageEdge ?? 0);
        return {
            localId: `${Date.now()}-${idx}-${f.name}`,
            name: f.name,
            mime: result.mime,
            dataUrl: result.dataUrl,
            sizeBytes: result.sizeBytes,
            kind: "image",
        };
    };

    /// Per-file data-file processor. Mirrors `processImageFile`
    /// but for non-image attachments (CSV / PDF / JSON / etc.).
    /// Returns `null` on FileReader failure (rare — browser quota,
    /// permission revoked mid-read); callers filter nulls.
    const processDataFile = async (
        f: File,
        idx: number,
    ): Promise<PendingAttachment | null> => {
        const mime = inferFileMime(f);
        try {
            const result = await readFileAsDataUrl(f, mime);
            return {
                localId: `${Date.now()}-${idx}-${f.name}`,
                name: f.name,
                mime: result.mime,
                dataUrl: result.dataUrl,
                sizeBytes: result.sizeBytes,
                kind: "file",
            };
        } catch (e) {
            console.error("readFileAsDataUrl failed", f.name, e);
            return null;
        }
    };

    const onFilesPicked = async (files: FileList | null) => {
        if (!files) return;
        const next: PendingAttachment[] = [];
        for (let i = 0; i < files.length; i++) {
            const f = files[i];
            if (!f.type.startsWith("image/")) continue;
            next.push(await processImageFile(f, i));
        }
        setAttachments((prev) => [...prev, ...next]);
        // Reset the input so picking the same file twice in a row
        // re-fires onChange (browsers suppress duplicate selections
        // otherwise).
        if (fileInputRef.current) fileInputRef.current.value = "";
    };

    /// 2026-05-18 — non-image picker. Tags the resulting chip as
    /// `kind: "file"` so the preview renders an icon instead of
    /// a thumbnail, and stages the original `file.name` as the
    /// operator-facing filename so python-sandbox hydration can
    /// drop the blob at `/work/<convo>/uploads/<filename>`.
    const onDataFilesPicked = async (files: FileList | null) => {
        if (!files) return;
        const next: PendingAttachment[] = [];
        for (let i = 0; i < files.length; i++) {
            const f = files[i];
            const att = await processDataFile(f, i);
            if (att !== null) next.push(att);
        }
        setAttachments((prev) => [...prev, ...next]);
        if (dataFileInputRef.current) dataFileInputRef.current.value = "";
    };

    /// 2026-05-18 — drag-and-drop path. Classifies every dropped
    /// file (image / data / unsupported), routes the supported
    /// ones through the same per-file processors the pickers use,
    /// and surfaces rejected names to the operator via the
    /// inline ErrorBanner so a wrong-MIME drop doesn't fail
    /// silently or with a server 400.
    ///
    /// Failure modes the operator might hit (and what they see):
    ///   * Drops a .py / .exe / .docx file → ErrorBanner naming
    ///     the rejected files + the accepted MIME groups.
    ///   * Drops a mix of accepted + unsupported → accepted files
    ///     stage as chips, unsupported names go to the banner.
    ///   * Drops a folder (browser surfaces folders as a single
    ///     File with size=0 and empty type) → classified as
    ///     reject because no MIME matches.
    ///   * Drops nothing (drag was just text from another tab) →
    ///     no-op; banner stays clear.
    const handleDroppedFiles = async (files: File[]) => {
        if (files.length === 0) return;
        const images: File[] = [];
        const dataFiles: File[] = [];
        const rejected: string[] = [];
        for (const f of files) {
            switch (classifyDroppedFile(f)) {
                case "image":
                    images.push(f);
                    break;
                case "file":
                    dataFiles.push(f);
                    break;
                case "reject":
                    rejected.push(f.name || "(unnamed)");
                    break;
            }
        }
        // Process both groups in parallel — image downscale is the
        // slow part and there's no inter-file dependency.
        const [imageChips, dataChips] = await Promise.all([
            Promise.all(images.map((f, i) => processImageFile(f, i))),
            Promise.all(
                dataFiles.map((f, i) => processDataFile(f, images.length + i)),
            ),
        ]);
        const accepted: PendingAttachment[] = [
            ...imageChips,
            ...dataChips.filter((a): a is PendingAttachment => a !== null),
        ];
        if (accepted.length > 0) {
            setAttachments((prev) => [...prev, ...accepted]);
        }
        if (rejected.length > 0) {
            // Compact list when many — operators don't need a 20-
            // name dump in the banner.
            const shown = rejected.length <= 3
                ? rejected.join(", ")
                : `${rejected.slice(0, 3).join(", ")} +${rejected.length - 3} more`;
            setDropError(
                `Unsupported file type: ${shown}. Supported: images (PNG, JPEG, WebP, GIF) ` +
                `and data files (CSV, TSV, JSON, TXT, MD, PDF, XLSX, XLS).`,
            );
        } else {
            // Clear any stale banner on a fully-accepted drop.
            setDropError(null);
        }
    };

    const removeAttachment = (localId: string) => {
        setAttachments((prev) => prev.filter((a) => a.localId !== localId));
    };

    const submit = async (e?: FormEvent | KeyboardEvent) => {
        e?.preventDefault();
        const trimmed = text.trim();
        // Allow image-only sends — vision models handle a bare image
        // fine with the implicit "describe this" framing.
        if ((trimmed.length === 0 && attachments.length === 0) || submitting) return;
        setSubmitting(true);
        // Snapshot the staged attachments + clear the input
        // synchronously so the operator sees the send committed even
        // while `onSend` is still streaming the reply.
        const wire: InlineAttachment[] = attachments.map((a) => ({
            mime: a.mime,
            data_url: a.dataUrl,
            // 2026-05-18 — server REQUIRES filename for non-image
            // MIMEs so python-sandbox hydration can land the file
            // at /work/<convo>/uploads/<filename>. Images carry the
            // filename too (harmless metadata), but the server
            // doesn't act on it for the vision-content path.
            filename: a.name,
        }));
        const skillNames = selectedSkills.map((s) => s.name);
        setText("");
        setAttachments([]);
        // Per-turn semantics: the picker clears after each send so
        // the operator's next message doesn't accidentally re-apply
        // the same skill. They can re-select if they want sticky.
        setSelectedSkills([]);
        try {
            await onSend(trimmed, wire, skillNames);
        } finally {
            setSubmitting(false);
        }
    };

    const onKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
        if (e.key === "Enter" && !e.shiftKey) {
            void submit(e);
        }
    };

    const isBusy = !!submitting || !!disabled;
    const trimmedEmpty = text.trim().length === 0;
    // The send button is enabled when there's either typed text or
    // at least one staged image. Image-only sends are valid against
    // a multimodal backend.
    const sendDisabled = isBusy || (trimmedEmpty && attachments.length === 0);

    // ChatGPT-style composer:
    //   * The visible "chip" is the outer `<div>`. It owns the
    //     background fill, the rounded corners, and the focus-within
    //     ring. The textarea inside is fully transparent so it
    //     reads as part of the chip rather than a separate input.
    //   * Tool affordances (mic, future attach / web-search /
    //     model-picker, etc.) anchor on a bottom row inside the
    //     same chip — left-aligned tools, right-aligned send.
    //
    // We hand-roll the layout instead of using Bootstrap's
    // InputGroup because InputGroup is fundamentally a single
    // horizontal row (textarea + buttons inline) and we want a
    // two-row chip (textarea on top, tools below).
    /// 2026-05-18 — drag-drop event wiring on the composer shell.
    ///
    /// The shell is the natural drop zone (it covers the textarea
    /// + the tools row). Wiring details:
    ///
    /// `dragenter` / `dragleave` use a ref-counted depth instead
    /// of a plain boolean. Browsers fire one dragleave + one
    /// dragenter every time the pointer crosses an internal
    /// boundary (e.g. from the textarea onto the + button); a
    /// boolean toggle there causes the highlight to strobe. The
    /// counter increments on enter, decrements on leave, and only
    /// the depth==0 transitions flip the highlight state.
    ///
    /// `dragover` MUST call `preventDefault` or the drop event
    /// never fires (HTML5 drag-drop spec quirk — without the
    /// preventDefault, the browser treats the area as a non-drop
    /// target and renders the "no drop" cursor). We also set
    /// `dropEffect = "copy"` so the OS cursor shows the copy-on-
    /// drop affordance.
    ///
    /// `drop` cancels the default (otherwise the browser
    /// navigates to display the file) and delegates to
    /// `handleDroppedFiles`. Files that aren't accepted MIMEs
    /// get listed in the inline ErrorBanner below the chip row;
    /// supported files stage as chips immediately.
    const onShellDragEnter = (e: DragEvent<HTMLDivElement>) => {
        // Only respond to file drags, not text / DOM-node drags
        // from elsewhere in the SPA. `dataTransfer.types` contains
        // "Files" when an OS file drag is in flight.
        if (!e.dataTransfer.types.includes("Files")) return;
        e.preventDefault();
        dragDepth.current += 1;
        setIsDragOver(true);
    };
    const onShellDragLeave = (e: DragEvent<HTMLDivElement>) => {
        if (!e.dataTransfer.types.includes("Files")) return;
        e.preventDefault();
        dragDepth.current = Math.max(0, dragDepth.current - 1);
        if (dragDepth.current === 0) setIsDragOver(false);
    };
    const onShellDragOver = (e: DragEvent<HTMLDivElement>) => {
        if (!e.dataTransfer.types.includes("Files")) return;
        e.preventDefault();
        e.dataTransfer.dropEffect = "copy";
    };
    const onShellDrop = (e: DragEvent<HTMLDivElement>) => {
        if (!e.dataTransfer.types.includes("Files")) return;
        e.preventDefault();
        dragDepth.current = 0;
        setIsDragOver(false);
        // Snapshot files synchronously — `e.dataTransfer.files`
        // is invalidated as soon as the event handler returns,
        // and our classifier + reader is async.
        const files: File[] = Array.from(e.dataTransfer.files);
        void handleDroppedFiles(files);
    };

    return (
        <form onSubmit={submit} className="execlaw-composer__form">
            <div
                className={
                    isDragOver
                        ? "execlaw-composer__shell execlaw-composer__shell--drag-over"
                        : "execlaw-composer__shell"
                }
                data-testid="composer-shell"
                data-drag-over={isDragOver || undefined}
                onDragEnter={onShellDragEnter}
                onDragLeave={onShellDragLeave}
                onDragOver={onShellDragOver}
                onDrop={onShellDrop}
            >
                {dropError && (
                    <div
                        className="execlaw-composer__drop-error"
                        role="alert"
                        data-testid="composer-drop-error"
                    >
                        <i
                            className="bi bi-exclamation-triangle-fill"
                            aria-hidden
                            style={{ marginRight: "0.4rem" }}
                        />
                        <span style={{ flex: 1 }}>{dropError}</span>
                        <button
                            type="button"
                            className="execlaw-composer__drop-error-dismiss"
                            onClick={() => setDropError(null)}
                            aria-label="Dismiss"
                            data-testid="composer-drop-error-dismiss"
                        >
                            <i className="bi bi-x" aria-hidden />
                        </button>
                    </div>
                )}
                {(attachments.length > 0 || selectedSkills.length > 0) && (
                    <div
                        className="execlaw-composer__attachments"
                        data-testid="composer-attachments"
                    >
                        {attachments.map((a) => (
                            <div
                                key={a.localId}
                                className={
                                    a.kind === "file"
                                        ? "execlaw-composer__attachment-chip execlaw-composer__attachment-chip--file"
                                        : "execlaw-composer__attachment-chip"
                                }
                                data-testid="composer-attachment-chip"
                                data-attachment-kind={a.kind}
                                title={`${a.name} · ${formatBytes(a.sizeBytes)}`}
                            >
                                {a.kind === "image" ? (
                                    <img
                                        className="execlaw-composer__attachment-thumb"
                                        src={a.dataUrl}
                                        alt={a.name}
                                    />
                                ) : (
                                    <span className="execlaw-composer__attachment-file">
                                        <i
                                            className={fileIconForMime(a.mime)}
                                            aria-hidden
                                        />
                                        <span className="execlaw-composer__attachment-filename">
                                            {a.name}
                                        </span>
                                    </span>
                                )}
                                <button
                                    type="button"
                                    className="execlaw-composer__attachment-remove"
                                    onClick={() => removeAttachment(a.localId)}
                                    aria-label={`Remove ${a.name}`}
                                    data-testid="composer-attachment-remove"
                                >
                                    <i className="bi bi-x" aria-hidden />
                                </button>
                            </div>
                        ))}
                        {selectedSkills.map((s) => (
                            <div
                                key={s.name}
                                className="execlaw-composer__skill-chip"
                                data-testid="composer-skill-chip"
                                data-skill-name={s.name}
                                title={s.description}
                            >
                                <i
                                    className="bi bi-stars"
                                    aria-hidden
                                />
                                <span className="execlaw-composer__skill-chip-name">
                                    {s.name}
                                </span>
                                <button
                                    type="button"
                                    className="execlaw-composer__attachment-remove"
                                    onClick={() => removeSelectedSkill(s.name)}
                                    aria-label={`Remove skill ${s.name}`}
                                    data-testid="composer-skill-chip-remove"
                                >
                                    <i className="bi bi-x" aria-hidden />
                                </button>
                            </div>
                        ))}
                    </div>
                )}
                <Form.Control
                    ref={textareaRef}
                    as="textarea"
                    rows={1}
                    placeholder="How can I help?"
                    value={text}
                    onChange={(e) => setText(e.target.value)}
                    onKeyDown={onKeyDown}
                    // 2026-04-28 — only honour the *external* disabled
                    // prop. Previously we also disabled while the
                    // submit await was in flight, which (a) blurred
                    // the textarea (disabled inputs can't hold
                    // focus) and (b) blocked the operator from
                    // composing a follow-up while the agent was
                    // streaming. The double-send guard lives in
                    // `submit()` (`if (submitting) return`); the
                    // send button itself stays disabled (see
                    // `isBusy` below). Keeping the textarea hot
                    // means focus survives Enter and the operator
                    // can queue their next thought immediately.
                    disabled={!!disabled}
                    className="execlaw-composer__input"
                    data-testid="composer-input"
                />
                <div className="execlaw-composer__tools">
                    <div className="execlaw-composer__tools-left">
                        {/*
                          Attach-trigger anchor. 2026-05-18: "Attach
                          file" is always available (data files don't
                          need a vision-capable backend), so the +
                          button is now unconditionally rendered. The
                          submenu items are individually gated on the
                          underlying capability (multimodal for photos,
                          getSkills for skills, always-on for files).
                          The previous `{(multimodal || !!getSkills) &&`
                          wrapper would hide the + button entirely on
                          a text-only backend without skills, which
                          would also hide the file-attach affordance.
                        */}
                        <div
                                ref={attachMenuRef}
                                className="execlaw-composer__attach-anchor"
                            >
                                <Button
                                    type="button"
                                    variant="link"
                                    className="execlaw-composer__attach-btn"
                                    onClick={() => {
                                        setAttachMenuOpen((v) => !v);
                                        // Always reopen on the root view —
                                        // no surprise "still in skill list"
                                        // state from the previous open.
                                        setAttachMenuMode("root");
                                    }}
                                    aria-label="Attach"
                                    aria-haspopup="menu"
                                    aria-expanded={attachMenuOpen}
                                    disabled={isBusy}
                                    data-testid="composer-attach-trigger"
                                >
                                    <i className="bi bi-plus-lg" aria-hidden />
                                </Button>
                                {attachMenuOpen && attachMenuMode === "root" && (
                                    <div
                                        className="execlaw-composer__attach-menu"
                                        role="menu"
                                        data-testid="composer-attach-menu"
                                    >
                                        {multimodal && (
                                            <button
                                                type="button"
                                                role="menuitem"
                                                className="execlaw-composer__attach-menu-item"
                                                onClick={onPickPhoto}
                                                data-testid="composer-attach-photo"
                                            >
                                                <i
                                                    className="bi bi-image"
                                                    aria-hidden
                                                />
                                                <span>Attach photo</span>
                                            </button>
                                        )}
                                        {/*
                                          2026-05-18 — "Attach file"
                                          surfaces the python-sandbox
                                          hydration path. Always shown
                                          (independent of `multimodal`
                                          since data files don't need a
                                          vision-capable backend); the
                                          server-side acceptlist gates
                                          which MIME types actually
                                          land. If the python-sandbox
                                          plugin isn't installed the
                                          file still uploads + becomes
                                          a state_attachments row but
                                          no hydration occurs — that's
                                          fine, the agent just won't
                                          see it surface in
                                          /work/uploads/.
                                        */}
                                        <button
                                            type="button"
                                            role="menuitem"
                                            className="execlaw-composer__attach-menu-item"
                                            onClick={onPickFile}
                                            data-testid="composer-attach-file"
                                        >
                                            <i
                                                className="bi bi-paperclip"
                                                aria-hidden
                                            />
                                            <span>Attach file</span>
                                        </button>
                                        {getSkills && (
                                            <button
                                                type="button"
                                                role="menuitem"
                                                className="execlaw-composer__attach-menu-item"
                                                onClick={onPickSkill}
                                                data-testid="composer-attach-skill"
                                            >
                                                <i
                                                    className="bi bi-stars"
                                                    aria-hidden
                                                />
                                                <span>Attach skill</span>
                                            </button>
                                        )}
                                    </div>
                                )}
                                {attachMenuOpen && attachMenuMode === "skills" && (
                                    <div
                                        className="execlaw-composer__attach-menu execlaw-composer__attach-menu--skills"
                                        role="menu"
                                        data-testid="composer-skill-picker"
                                    >
                                        <div className="execlaw-composer__skill-picker-header">
                                            <button
                                                type="button"
                                                className="execlaw-composer__skill-picker-back"
                                                onClick={() =>
                                                    setAttachMenuMode("root")
                                                }
                                                aria-label="Back to attach menu"
                                                data-testid="composer-skill-picker-back"
                                            >
                                                <i
                                                    className="bi bi-chevron-left"
                                                    aria-hidden
                                                />
                                            </button>
                                            <span>Pick skills for this turn</span>
                                        </div>
                                        {skillsLoading && (
                                            <div
                                                className="execlaw-composer__skill-picker-status"
                                                data-testid="composer-skill-picker-loading"
                                            >
                                                Loading…
                                            </div>
                                        )}
                                        {skillsError && (
                                            <div
                                                className="execlaw-composer__skill-picker-status execlaw-composer__skill-picker-status--error"
                                                data-testid="composer-skill-picker-error"
                                            >
                                                {skillsError}
                                            </div>
                                        )}
                                        {availableSkills &&
                                            availableSkills.length === 0 &&
                                            !skillsLoading &&
                                            !skillsError && (
                                                <div
                                                    className="execlaw-composer__skill-picker-status"
                                                    data-testid="composer-skill-picker-empty"
                                                >
                                                    No skills available. Create one
                                                    on the Skills page.
                                                </div>
                                            )}
                                        {availableSkills &&
                                            availableSkills.map((s) => {
                                                const checked = selectedSkills.some(
                                                    (sel) => sel.name === s.name,
                                                );
                                                return (
                                                    <button
                                                        type="button"
                                                        role="menuitemcheckbox"
                                                        aria-checked={checked}
                                                        key={s.name}
                                                        className={
                                                            "execlaw-composer__skill-picker-item" +
                                                            (checked
                                                                ? " is-checked"
                                                                : "")
                                                        }
                                                        onClick={() =>
                                                            toggleSkill(s)
                                                        }
                                                        data-testid="composer-skill-picker-item"
                                                        data-skill-name={s.name}
                                                    >
                                                        <i
                                                            className={
                                                                checked
                                                                    ? "bi bi-check-square"
                                                                    : "bi bi-square"
                                                            }
                                                            aria-hidden
                                                        />
                                                        <span className="execlaw-composer__skill-picker-name">
                                                            {s.name}
                                                        </span>
                                                        <span className="execlaw-composer__skill-picker-desc">
                                                            {s.description}
                                                        </span>
                                                    </button>
                                                );
                                            })}
                                    </div>
                                )}
                                <input
                                    ref={fileInputRef}
                                    type="file"
                                    accept="image/png,image/jpeg,image/webp,image/gif"
                                    multiple
                                    style={{ display: "none" }}
                                    onChange={(e) =>
                                        void onFilesPicked(e.target.files)
                                    }
                                    data-testid="composer-file-input"
                                />
                                {/*
                                  2026-05-18 — second hidden input for
                                  the non-image "Attach file" path. The
                                  separate `accept` attribute means the
                                  OS picker shows only data files by
                                  default (the operator can override
                                  with "All files" in most pickers).
                                */}
                                <input
                                    ref={dataFileInputRef}
                                    type="file"
                                    accept={FILE_PICKER_ACCEPT}
                                    multiple
                                    style={{ display: "none" }}
                                    onChange={(e) =>
                                        void onDataFilesPicked(e.target.files)
                                    }
                                    data-testid="composer-data-file-input"
                                />
                            </div>
                    </div>
                    <div className="execlaw-composer__tools-right">
                        {/*
                          Mic always renders so the operator sees
                          the affordance even when:
                            * the websocket isn't connected
                              (sendVoiceFrame undefined → null), OR
                            * voice backends aren't configured /
                              healthy yet.
                          The button itself owns the muted-vs-ready
                          state + the tooltip; we just feed it the
                          two relevant signals.
                        */}
                        <VoiceCaptureButton
                            sendBinary={sendVoiceFrame ?? null}
                            sendControl={sendVoiceControl}
                            disabled={isBusy}
                            readiness={voiceReadiness ?? null}
                        />
                        {(submitting || busy) && onStop ? (
                            // 2026-04-28 — replace the send button with a
                            // stop button while a turn is streaming. The
                            // local-model agent can otherwise generate
                            // for minutes with no escape hatch; this
                            // gives the operator the same "stop"
                            // affordance every chat UI ships.
                            //
                            // Triggered by EITHER:
                            //   * `submitting` — the await on `onSend`
                            //     hasn't resolved yet; postMessage
                            //     only returns after the server
                            //     finishes the turn, so this stays
                            //     true for the whole streaming
                            //     window. This is the most reliable
                            //     signal because it doesn't depend
                            //     on any WS phase event landing in
                            //     time.
                            //   * `busy` — external phase signal
                            //     (e.g. a routine fired the turn
                            //     so this tab didn't initiate it).
                            <Button
                                type="button"
                                variant="primary"
                                onClick={(ev) => {
                                    ev.preventDefault();
                                    onStop();
                                }}
                                data-testid="composer-stop"
                                aria-label="Stop generating"
                                className="execlaw-composer__send execlaw-composer__send--stop"
                            >
                                <i className="bi bi-stop-fill" aria-hidden />
                            </Button>
                        ) : (
                            <Button
                                type="submit"
                                variant="primary"
                                disabled={sendDisabled}
                                data-testid="composer-send"
                                aria-label="Send"
                                className="execlaw-composer__send"
                            >
                                {submitting ? (
                                    <Spinner size="sm" animation="border" />
                                ) : (
                                    <i className="bi bi-arrow-up" aria-hidden />
                                )}
                            </Button>
                        )}
                    </div>
                </div>
            </div>
        </form>
    );
}

interface PreparedImage {
    dataUrl: string;
    mime: string;
    sizeBytes: number;
}

/// Prepare a picked image for upload. When `maxEdge > 0` and the
/// image is larger on its long edge, resize via Canvas before
/// base64-encoding. The output mime is set to JPEG for resized photos
/// (saves ~3-5× over PNG for typical phone shots) UNLESS the source
/// is a GIF (preserve animation by skipping resize and passing the
/// original bytes through) or PNG with transparency (PNG bytes
/// passed through to avoid losing the alpha channel).
///
/// Returned `dataUrl` is the wire form the server's
/// `persist_inline_attachments` decodes; `mime` MUST match the
/// `data:` prefix inside it. `sizeBytes` is the post-encode payload
/// size used only for the chip's tooltip — it's the decoded byte
/// count, not the base64-inflated wire length.
async function prepareImageForUpload(
    file: File,
    maxEdge: number,
): Promise<PreparedImage> {
    // No edge target / animated GIF / very small image → ship the
    // original bytes unchanged.
    if (maxEdge <= 0 || file.type === "image/gif") {
        const dataUrl = await readAsDataUrl(file);
        return { dataUrl, mime: file.type, sizeBytes: file.size };
    }
    // Read once into an ImageBitmap so we can measure before deciding
    // whether to resize. createImageBitmap rejects on non-image MIME
    // types; the caller has already filtered to image/* so any
    // rejection here means the file is corrupt — propagate.
    const bmp = await createImageBitmap(file);
    const longEdge = Math.max(bmp.width, bmp.height);
    if (longEdge <= maxEdge) {
        bmp.close();
        const dataUrl = await readAsDataUrl(file);
        return { dataUrl, mime: file.type, sizeBytes: file.size };
    }
    const scale = maxEdge / longEdge;
    const w = Math.max(1, Math.round(bmp.width * scale));
    const h = Math.max(1, Math.round(bmp.height * scale));
    // OffscreenCanvas is widely supported (Chrome 69+ / Firefox 105+
    // / Safari 16.4+); fall back to a regular <canvas> when it isn't
    // available so the resize still works on slightly older browsers.
    let blob: Blob;
    const outMime =
        file.type === "image/png" ? "image/png" : "image/jpeg";
    const quality = outMime === "image/jpeg" ? 0.85 : 1.0;
    if (typeof OffscreenCanvas !== "undefined") {
        const canvas = new OffscreenCanvas(w, h);
        const ctx = canvas.getContext("2d");
        if (!ctx) throw new Error("2D canvas context unavailable");
        ctx.drawImage(bmp, 0, 0, w, h);
        bmp.close();
        blob = await canvas.convertToBlob({ type: outMime, quality });
    } else {
        const canvas = document.createElement("canvas");
        canvas.width = w;
        canvas.height = h;
        const ctx = canvas.getContext("2d");
        if (!ctx) throw new Error("2D canvas context unavailable");
        ctx.drawImage(bmp, 0, 0, w, h);
        bmp.close();
        blob = await new Promise<Blob>((resolve, reject) => {
            canvas.toBlob(
                (b) =>
                    b ? resolve(b) : reject(new Error("canvas toBlob failed")),
                outMime,
                quality,
            );
        });
    }
    const dataUrl = await readAsDataUrl(blob);
    return { dataUrl, mime: outMime, sizeBytes: blob.size };
}

/// Read a File / Blob into a base64 data URL via FileReader. The
/// server expects `data:<mime>;base64,<bytes>` and decodes verbatim,
/// so the browser's native encoding is exactly what we want.
function readAsDataUrl(file: Blob): Promise<string> {
    return new Promise((resolve, reject) => {
        const r = new FileReader();
        r.onload = () => {
            const result = r.result;
            if (typeof result === "string") {
                resolve(result);
            } else {
                reject(new Error("FileReader returned non-string"));
            }
        };
        r.onerror = () => reject(r.error ?? new Error("FileReader error"));
        r.readAsDataURL(file);
    });
}

function formatBytes(bytes: number): string {
    if (bytes < 1024) return `${bytes} B`;
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
    return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}
