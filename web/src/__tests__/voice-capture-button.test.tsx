// Phase 13.A — VoiceCaptureButton tests.
//
// jsdom doesn't ship `navigator.mediaDevices.getUserMedia` or
// `MediaRecorder` — both have to be stubbed before the component
// mounts. Tests focus on the wiring contract:
//   * Click → request mic → start recorder → stream chunks via the
//     `sendBinary` prop.
//   * Click again → stop recorder → release tracks.
//   * Permission denial surfaces as a banner without crashing.
//   * Unsupported-browser path produces an explanatory error.

import {
    afterEach,
    beforeEach,
    describe,
    expect,
    it,
    vi,
} from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { VoiceCaptureButton } from "../chat/VoiceCaptureButton";

interface MockTrack {
    stop: () => void;
}

class MockMediaRecorder {
    static isTypeSupported = vi.fn().mockReturnValue(true);
    public state: "inactive" | "recording" = "inactive";
    public ondataavailable: ((ev: BlobEvent) => void) | null = null;
    public onstop: (() => void) | null = null;
    private timer: ReturnType<typeof setInterval> | null = null;

    constructor(public stream: unknown, public _opts?: unknown) {}

    start(timesliceMs: number) {
        this.state = "recording";
        // Simulate a single dataavailable event after timeslice.
        // jsdom's Blob doesn't ship `arrayBuffer()` reliably across
        // versions, so synthesize a Blob-shaped object the
        // VoiceCaptureButton can consume directly.
        this.timer = setTimeout(() => {
            const buf = new TextEncoder().encode("frame-bytes").buffer;
            const fakeBlob = {
                size: 11,
                arrayBuffer: () => Promise.resolve(buf),
            };
            this.ondataavailable?.({
                data: fakeBlob,
            } as unknown as BlobEvent);
        }, timesliceMs / 5); // shorter so tests don't drag
    }

    stop() {
        this.state = "inactive";
        if (this.timer) {
            clearTimeout(this.timer);
            this.timer = null;
        }
        this.onstop?.();
    }
}

let mockTracks: MockTrack[];

function installMocks(opts: {
    permissionGranted: boolean;
    recorderConstructorThrows?: boolean;
}) {
    mockTracks = [{ stop: vi.fn() }];
    const stream = {
        getTracks: () => mockTracks,
    } as unknown as MediaStream;
    const getUserMedia = opts.permissionGranted
        ? vi.fn().mockResolvedValue(stream)
        : vi.fn().mockRejectedValue(new Error("permission denied"));
    Object.defineProperty(navigator, "mediaDevices", {
        configurable: true,
        value: { getUserMedia },
    });
    if (opts.recorderConstructorThrows) {
        // @ts-expect-error swap with a constructor that throws
        globalThis.MediaRecorder = function () {
            throw new Error("recorder construction failed");
        };
        // The TS lib types narrow `globalThis.MediaRecorder` away
        // when reassigned to a non-class function above; cast for
        // the static-method assignment.
        (globalThis.MediaRecorder as unknown as {
            isTypeSupported: (s: string) => boolean;
        }).isTypeSupported = vi.fn().mockReturnValue(true);
    } else {
        // @ts-expect-error replace global with our stub class
        globalThis.MediaRecorder = MockMediaRecorder;
    }
}

beforeEach(() => {
    vi.useFakeTimers();
});

afterEach(() => {
    vi.useRealTimers();
    // @ts-expect-error reset
    delete globalThis.MediaRecorder;
    Object.defineProperty(navigator, "mediaDevices", {
        configurable: true,
        value: undefined,
    });
});

describe("VoiceCaptureButton", () => {
    it("renders a mic button in the idle state", () => {
        installMocks({ permissionGranted: true });
        render(<VoiceCaptureButton sendBinary={vi.fn().mockReturnValue(true)} />);
        const btn = screen.getByTestId("composer-voice");
        expect(btn).toHaveAttribute("aria-pressed", "false");
        expect(btn).toHaveAttribute("aria-label", "Start voice capture");
    });

    it("starts recording on click and pipes framed chunks through sendBinary", async () => {
        vi.useRealTimers(); // MediaRecorder needs setTimeout to fire
        installMocks({ permissionGranted: true });
        const sent: ArrayBuffer[] = [];
        const sendBinary = vi.fn().mockImplementation((b: ArrayBuffer) => {
            sent.push(b);
            return true;
        });
        render(<VoiceCaptureButton sendBinary={sendBinary} timesliceMs={50} />);
        fireEvent.click(screen.getByTestId("composer-voice"));
        await waitFor(() => {
            expect(screen.getByTestId("composer-voice")).toHaveAttribute(
                "aria-pressed",
                "true",
            );
        });
        // Wait for the simulated dataavailable to fire.
        await waitFor(
            () => {
                expect(sendBinary).toHaveBeenCalled();
            },
            { timeout: 500 },
        );
        // Phase 13.A closure — every chunk is now wrapped in the
        // [u32 header_len][JSON header][payload] envelope. Decode
        // the first frame and assert the header + opus payload
        // round-trip.
        const frame = sent[0];
        expect(frame.byteLength).toBeGreaterThan(4);
        const view = new DataView(frame);
        const headerLen = view.getUint32(0, false);
        const headerBytes = new Uint8Array(frame, 4, headerLen);
        const header = JSON.parse(new TextDecoder().decode(headerBytes));
        expect(typeof header.session).toBe("string");
        expect(header.session.length).toBeGreaterThan(0);
        expect(header.seq).toBe(0);
        expect(header.codec).toBe("opus");
        expect(typeof header.sample_rate).toBe("number");
        const payload = new Uint8Array(frame, 4 + headerLen);
        expect(new TextDecoder().decode(payload)).toBe("frame-bytes");
    });

    it("stops recording + releases mic tracks on second click", async () => {
        vi.useRealTimers();
        installMocks({ permissionGranted: true });
        const sendBinary = vi.fn().mockReturnValue(true);
        render(<VoiceCaptureButton sendBinary={sendBinary} timesliceMs={50} />);
        fireEvent.click(screen.getByTestId("composer-voice"));
        await waitFor(() => {
            expect(screen.getByTestId("composer-voice")).toHaveAttribute(
                "aria-pressed",
                "true",
            );
        });
        fireEvent.click(screen.getByTestId("composer-voice"));
        await waitFor(() => {
            expect(screen.getByTestId("composer-voice")).toHaveAttribute(
                "aria-pressed",
                "false",
            );
        });
        // Mic tracks must be stopped so the browser drops the
        // recording indicator.
        expect(mockTracks[0].stop).toHaveBeenCalled();
    });

    it("surfaces a banner when getUserMedia rejects", async () => {
        vi.useRealTimers();
        installMocks({ permissionGranted: false });
        const sendBinary = vi.fn().mockReturnValue(true);
        render(<VoiceCaptureButton sendBinary={sendBinary} />);
        fireEvent.click(screen.getByTestId("composer-voice"));
        await waitFor(() => {
            expect(
                screen.getByTestId("composer-voice-error"),
            ).toBeInTheDocument();
        });
        expect(
            screen.getByTestId("composer-voice-error"),
        ).toHaveTextContent(/Mic permission denied/i);
        // Stays in idle state — no recording started.
        expect(screen.getByTestId("composer-voice")).toHaveAttribute(
            "aria-pressed",
            "false",
        );
    });

    it("releases the mic stream when MediaRecorder construction throws", async () => {
        vi.useRealTimers();
        installMocks({ permissionGranted: true, recorderConstructorThrows: true });
        const sendBinary = vi.fn().mockReturnValue(true);
        render(<VoiceCaptureButton sendBinary={sendBinary} />);
        fireEvent.click(screen.getByTestId("composer-voice"));
        await waitFor(() => {
            expect(
                screen.getByTestId("composer-voice-error"),
            ).toBeInTheDocument();
        });
        // The acquired-but-couldn't-record stream gets torn down so
        // the mic indicator vanishes.
        expect(mockTracks[0].stop).toHaveBeenCalled();
    });

    it("ignores clicks while disabled", () => {
        installMocks({ permissionGranted: true });
        const sendBinary = vi.fn().mockReturnValue(true);
        render(<VoiceCaptureButton sendBinary={sendBinary} disabled />);
        const btn = screen.getByTestId("composer-voice") as HTMLButtonElement;
        expect(btn.disabled).toBe(true);
    });
});
