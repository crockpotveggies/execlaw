// Tiny execlaw mascot rendered as a spinner: the hair + beard +
// eye outline rotate together around the canvas centre while the
// iris stays fixed (in screen-space terms) and tracks the cursor
// just like `MascotGreeting`'s. Used by `ToolActivityPill` to
// surface "the agent is working" without dragging in a generic
// CSS spinner.
//
// The rotation is pure CSS so we don't pay a JS animation tick
// per frame for a tiny icon. Iris tracking IS JS — same
// mousemove handler shape as MascotGreeting.

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
    const eyeOuterRef = useRef<SVGGElement | null>(null);
    const eyePathRef = useRef<SVGPathElement | null>(null);
    const irisRef = useRef<SVGGElement | null>(null);
    const irisPathRef = useRef<SVGPathElement | null>(null);

    // Iris cursor-tracking — copy of MascotGreeting's logic but
    // operating only on the iris (the rotating outer group keeps
    // the eye outline + iris parent moving in lock-step around
    // the canvas centre, but we counter-rotate the iris translate
    // so the pupil reads as "looking at the cursor" regardless of
    // current rotation).
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
                // Translate the iris in viewBox units. The
                // `irisOffset` puts it back at its rest position
                // INSIDE the eye, then we add the pointer-relative
                // delta on top. We DON'T counter-rotate against
                // the spinner: the iris should rotate with the
                // eye since it's part of the eyeball, but the
                // tracking offset adds on top so the pupil drift
                // matches the cursor in screen-space at the
                // moment of the frame. The brief
                // visual-anti-pattern of the iris tracking through
                // a rotation is acceptable for a 22px icon.
                irisG.setAttribute(
                    "transform",
                    `translate(${irisOffset.x + tx} ${irisOffset.y + ty})`,
                );
            });
        };
        window.addEventListener("mousemove", onMove, { passive: true });
        // Park the iris at its rest position before the first
        // mousemove so it doesn't render at the SVG origin.
        irisG.setAttribute(
            "transform",
            `translate(${irisOffset.x} ${irisOffset.y})`,
        );
        return () => {
            window.removeEventListener("mousemove", onMove);
            if (rafId !== null) cancelAnimationFrame(rafId);
        };
    }, []);

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
            {/* Rotating outer group — hair + beard + eye outline.
                Pivot is the viewBox centre, not the group's
                bounding-box centre: the mascot's hair extends
                higher than the beard extends low, so the bbox
                centre is below-of-eye and rotating around that
                makes the face wobble. We pivot in viewBox-space
                coordinates instead — see the `transform-box:
                view-box` rule on `.execlaw-mini-mascot__rotor`
                in theme.scss. CSS class drives the spin so we
                don't pay a JS animation tick per frame on every
                WS event flush. */}
            <g className="execlaw-mini-mascot__rotor">
                <path d={HAIR_PATH} fill="currentColor" />
                <path d={BEARD_PATH} fill="currentColor" />
                {/* Eye + iris share a parent <g ref> so the
                    iris's `transform` is relative to the eye's
                    coordinate frame. */}
                <g ref={eyeOuterRef}>
                    <path
                        ref={eyePathRef}
                        d={EYE_PATH}
                        fill="currentColor"
                    />
                    <g ref={irisRef} className="execlaw-mini-mascot__iris">
                        <path
                            ref={irisPathRef}
                            d={IRIS_PATH}
                            fill="var(--bs-body-bg, #0d1117)"
                        />
                    </g>
                </g>
            </g>
        </svg>
    );
}
