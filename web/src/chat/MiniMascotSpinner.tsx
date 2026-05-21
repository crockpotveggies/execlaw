// Tiny execlaw mascot rendered as a working-indicator.
//
// 2026-05-20 — was a flat CSS 360° spin of the whole face. Replaced
// with a GSAP timeline that reads as "the mascot is thinking":
//
//   * Hair pops up away from the head with a slight tilt, as if the
//     top of the skull lifts off so the brain can buzz.
//   * While suspended, the hair vibrates — quick low-amplitude
//     oscillation to suggest energy / processing.
//   * Hair drops back into place with a bounce. The beard, which
//     squashed inward to "inhale" while the hair was airborne,
//     catches the impact with a tiny rebound.
//   * The eye widens during the lift (subtle scale-up) and relaxes
//     when the hair lands.
//   * A short breath beat closes the cycle before the next pop.
//
// Iris cursor-tracking continues to run as a separate effect so the
// pupil chases the operator's cursor independently of the GSAP
// timeline. The two paint to different elements (eye-outer `<g>` vs
// inner iris translate via setAttribute) so they don't fight for
// the same transform.
//
// `prefers-reduced-motion` callers see a calmer breathing scale on
// the hair only — no lift, no vibration, no bounce.

import { useGSAP } from "@gsap/react";
import gsap from "gsap";
import { useEffect, useRef } from "react";
import {
    EYE_PATH,
    BEARD_PATH,
    FALLBACK_EYE_CX,
    FALLBACK_EYE_CY,
    FALLBACK_IRIS_CX,
    FALLBACK_IRIS_CY,
    HAIR_PATH,
    IRIS_MAX_TRAVEL_VB,
    IRIS_PATH,
    IRIS_SATURATION_PX,
    VIEWBOX,
    readCenter,
} from "./mascotPaths";

interface Props {
    /** Pixel size of the rendered SVG. Defaults to 22 — sized
     *  for inline use next to body text. */
    size?: number;
    /** Optional className passthrough. */
    className?: string;
}

export function MiniMascotSpinner({ size = 22, className }: Props) {
    const svgRef = useRef<SVGSVGElement | null>(null);
    const hairRef = useRef<SVGPathElement | null>(null);
    const beardRef = useRef<SVGPathElement | null>(null);
    const eyeOuterRef = useRef<SVGGElement | null>(null);
    const eyePathRef = useRef<SVGPathElement | null>(null);
    const irisRef = useRef<SVGGElement | null>(null);
    const irisPathRef = useRef<SVGPathElement | null>(null);

    // Iris cursor tracking — runs every frame the mouse moves.
    // Independent from the GSAP "pop" timeline so the pupil keeps
    // chasing the cursor even mid-bounce.
    useEffect(() => {
        const svg = svgRef.current;
        const irisG = irisRef.current;
        if (!svg || !irisG) return;
        const eyeRest = readCenter(
            eyePathRef.current,
            FALLBACK_EYE_CX,
            FALLBACK_EYE_CY,
        );
        const irisRest = readCenter(
            irisPathRef.current,
            FALLBACK_IRIS_CX,
            FALLBACK_IRIS_CY,
        );
        const irisOffset = {
            x: irisRest.cx - eyeRest.cx,
            y: irisRest.cy - eyeRest.cy,
        };

        let rafId: number | null = null;
        let pendingX = 0;
        let pendingY = 0;
        const onMove = (e: MouseEvent) => {
            pendingX = e.clientX;
            pendingY = e.clientY;
            if (rafId !== null) return;
            rafId = requestAnimationFrame(() => {
                rafId = null;
                const rect = svg.getBoundingClientRect();
                if (rect.width <= 0 || rect.height <= 0) return;
                const eyeScreenX =
                    rect.left + (eyeRest.cx / VIEWBOX) * rect.width;
                const eyeScreenY =
                    rect.top + (eyeRest.cy / VIEWBOX) * rect.height;
                const dx = pendingX - eyeScreenX;
                const dy = pendingY - eyeScreenY;
                const dist = Math.hypot(dx, dy) || 1;
                const ratio = Math.min(1, dist / IRIS_SATURATION_PX);
                const travel = IRIS_MAX_TRAVEL_VB * ratio;
                const tx = (dx / dist) * travel;
                const ty = (dy / dist) * travel;
                irisG.setAttribute(
                    "transform",
                    `translate(${irisOffset.x + tx} ${irisOffset.y + ty})`,
                );
            });
        };
        window.addEventListener("mousemove", onMove, { passive: true });
        irisG.setAttribute(
            "transform",
            `translate(${irisOffset.x} ${irisOffset.y})`,
        );
        return () => {
            window.removeEventListener("mousemove", onMove);
            if (rafId !== null) cancelAnimationFrame(rafId);
        };
    }, []);

    // Pop-and-vibrate timeline. Loops as long as the spinner is
    // mounted. `useGSAP` ties the timeline to the component's
    // lifetime so it cleans up on unmount.
    useGSAP(
        () => {
            const hair = hairRef.current;
            const beard = beardRef.current;
            const eye = eyeOuterRef.current;
            if (!hair || !beard || !eye) return;

            // Honour the operator's motion preference — drop the lift
            // and vibration, keep a gentle breath on the hair so the
            // indicator is still visibly alive.
            const reducedMotion =
                typeof window !== "undefined" &&
                window.matchMedia?.("(prefers-reduced-motion: reduce)")
                    .matches;

            // Pivots: hair sits ABOVE the rest of the face, so when we
            // tilt it we want the pivot at the BOTTOM of the hair
            // bbox (where it would "hinge" off the head). Beard
            // pivots at its TOP so a vertical squash compresses
            // toward the eye like an inhale. Eye stays centred.
            gsap.set(hair, { transformOrigin: "50% 100%" });
            gsap.set(beard, { transformOrigin: "50% 0%" });
            gsap.set(eye, { transformOrigin: "50% 50%" });

            if (reducedMotion) {
                gsap.to(hair, {
                    scale: 1.04,
                    duration: 1.4,
                    repeat: -1,
                    yoyo: true,
                    ease: "sine.inOut",
                });
                return;
            }

            const tl = gsap.timeline({ repeat: -1, defaults: { overwrite: "auto" } });

            // 1. POP — hair leaps up + tilts, beard inhales, eye widens.
            //    Values are in viewBox units (1254 wide); -150 ≈ 12% of
            //    icon height, which reads as a noticeable hop on the
            //    22px render.
            tl.to(
                hair,
                {
                    y: -150,
                    scale: 1.06,
                    rotation: -4,
                    duration: 0.32,
                    ease: "back.out(2)",
                },
                0,
            )
                .to(
                    beard,
                    {
                        scaleY: 0.88,
                        scaleX: 1.06,
                        duration: 0.32,
                        ease: "power2.out",
                    },
                    0,
                )
                .to(
                    eye,
                    {
                        scale: 1.08,
                        duration: 0.32,
                        ease: "power2.out",
                    },
                    0,
                );

            // 2. VIBRATE — hair buzzes in mid-air for ~0.4s. Five fast
            //    yoyo cycles cover the dwell with a perceptible jitter
            //    without descending into a blur.
            tl.to(
                hair,
                {
                    rotation: "+=8",
                    x: 18,
                    y: -158,
                    duration: 0.07,
                    repeat: 5,
                    yoyo: true,
                    ease: "sine.inOut",
                },
                0.34,
            );

            // 3. DROP — hair falls back into place with a bounce.
            tl.to(
                hair,
                {
                    y: 0,
                    x: 0,
                    scale: 1,
                    rotation: 0,
                    duration: 0.45,
                    ease: "bounce.out",
                },
                0.78,
            )
                // Beard catches the impact: tiny over-extension then a
                // springy settle. Timed so the peak of the catch lines
                // up with the moment the hair "lands" (about 0.95s in).
                .to(
                    beard,
                    {
                        scaleY: 1.05,
                        scaleX: 0.97,
                        duration: 0.18,
                        ease: "power3.out",
                    },
                    1.05,
                )
                .to(
                    beard,
                    {
                        scaleY: 1,
                        scaleX: 1,
                        duration: 0.35,
                        ease: "elastic.out(1, 0.45)",
                    },
                    1.23,
                )
                .to(
                    eye,
                    {
                        scale: 1,
                        duration: 0.3,
                        ease: "power2.out",
                    },
                    1.0,
                );

            // 4. BREATH — short hold before the next pop. Keeps the
            //    cycle from feeling frantic; a working-indicator
            //    needs to feel like steady thinking, not panic.
            tl.to({}, { duration: 0.45 }, ">");
        },
        { dependencies: [] },
    );

    return (
        <svg
            ref={svgRef}
            viewBox={`0 0 ${VIEWBOX} ${VIEWBOX}`}
            width={size}
            height={size}
            role="img"
            aria-label="working"
            className={
                "execlaw-mini-mascot" + (className ? " " + className : "")
            }
        >
            <path ref={hairRef} d={HAIR_PATH} fill="currentColor" />
            <path ref={beardRef} d={BEARD_PATH} fill="currentColor" />
            {/* Eye + iris share a parent <g> so iris transform is
                relative to the eye coordinate frame. */}
            <g ref={eyeOuterRef}>
                <path ref={eyePathRef} d={EYE_PATH} fill="currentColor" />
                <g ref={irisRef} className="execlaw-mini-mascot__iris">
                    <path
                        ref={irisPathRef}
                        d={IRIS_PATH}
                        fill="var(--bs-body-bg, #0d1117)"
                    />
                </g>
            </g>
        </svg>
    );
}
