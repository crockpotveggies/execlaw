// MascotGreeting — wraps the execlaw mascot SVG, drives both the
// cursor-tracking iris animation AND a one-shot greeting that
// physically morphs the mascot's existing shapes into bracket-
// like silhouettes and reveals a "welcome <name>" line.
//
// Why one component? The morph reuses the mascot's own paths —
// it rotates and slides them — so the SVG, the refs into each
// piece, and the timeline have to live in the same place.
//
// LAYOUT
//   The mascot SVG is split into three independently-controllable
//   sub-trees, each wrapped in two groups so we can compose
//   transforms cleanly:
//
//       <g outerSlide>
//         <g innerRotate>
//           <path d=… />     ← hair / beard / eye+iris
//         </g>
//       </g>
//
//   The inner group handles rotation around the canvas centre.
//   The outer group handles screen-space translation. Stacking
//   them this way means the slide always moves the shape in
//   viewBox X/Y regardless of how far the inner group has
//   rotated.
//
// SEQUENCE
//   1. 5 s rest — mascot tracks the cursor.
//   2. Morph (~0.8 s) — each sub-tree rotates −90° around the
//      canvas centre. The (originally top) hair lands on the
//      left and slides further left. The (originally bottom)
//      beard lands on the right and slides further right. The
//      eye fades out.
//   3. Welcome line fades in between the two bracket-shapes.
//   4. 5 s hold.
//   5. Reverse — line fades, hair/beard slide back, rotation
//      unwinds, eye fades back in.
//
// REDUCED MOTION
//   The greeting is the brand mark's reason to exist, and the
//   movement is a single ~0.8s rotation + slide rather than
//   large-scale parallax — well below the threshold the
//   preference is meant to suppress. We intentionally do NOT
//   honour `prefers-reduced-motion` here.
//
// 2026-04-28.

import { useEffect, useRef } from "react";
import gsap from "gsap";
import {
    BEARD_PATH,
    CANVAS_CENTER,
    EYE_PATH,
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
    /** Pixel size of the rendered SVG. Defaults to 216. */
    size?: number;
    /** Name to interpolate into the welcome line. Falls back to
     *  "friend" so the line still reads if we don't have a name. */
    userName?: string;
    /** Optional className passthrough for layout/spacing. */
    className?: string;
}

// VIEWBOX, CANVAS_CENTER, IRIS_MAX_TRAVEL_VB, IRIS_SATURATION_PX,
// the mascot path strings, and the iris/eye fallback centres are
// shared with `MiniMascotSpinner` via `./mascotPaths`. Constants
// imported above. Greeting-only knobs stay here.

// Greeting timing.
const REST_DELAY_S = 3;
const HOLD_DURATION_S = 5;
const MORPH_ROTATION_S = 0.8;
const MORPH_SLIDE_S = 0.6;
const FADE_S = 0.4;
// Bracket height multiplier — shapes morph to this many times
// the welcome line's measured pixel height.
const BRACKET_HEIGHT_MULT = 1.3;
// Fraction of the welcome-line width to leave between each
// bracket and the text.
const BRACKET_MARGIN_FRACTION = 0.12;

// Natural bbox dimensions of hair and beard (measured from the
// source SVG). Used as fallbacks when getBBox isn't available
// (jsdom). Width / height are in viewBox units.
const FALLBACK_HAIR_BBOX = { width: 689, height: 504 };
const FALLBACK_BEARD_BBOX = { width: 398, height: 142 };

function readBboxSize(
    el: SVGGraphicsElement | null,
    fallback: { width: number; height: number },
): { width: number; height: number } {
    if (!el || typeof el.getBBox !== "function") return fallback;
    try {
        const b = el.getBBox();
        if (b.width <= 0 || b.height <= 0) return fallback;
        return { width: b.width, height: b.height };
    } catch {
        return fallback;
    }
}

export function MascotGreeting({
    size = 216,
    userName = "friend",
    className,
}: Props) {
    const svgRef = useRef<SVGSVGElement | null>(null);

    // Iris tracking refs.
    const eyePathRef = useRef<SVGPathElement | null>(null);
    const irisRef = useRef<SVGGElement | null>(null);

    // Greeting morph refs — outer (slide) + inner (rotate/scale)
    // for each piece. Path refs are also held so we can measure
    // each shape's natural bbox before the timeline mutates it.
    const hairOuterRef = useRef<SVGGElement | null>(null);
    const hairInnerRef = useRef<SVGGElement | null>(null);
    const hairPathRef = useRef<SVGPathElement | null>(null);
    const beardOuterRef = useRef<SVGGElement | null>(null);
    const beardInnerRef = useRef<SVGGElement | null>(null);
    const beardPathRef = useRef<SVGPathElement | null>(null);
    const eyeOuterRef = useRef<SVGGElement | null>(null);
    const eyeInnerRef = useRef<SVGGElement | null>(null);
    // Bracket overlays. The original hair/beard rotate + scale
    // into vertical ribbons at the canvas centre; on Phase B these
    // overlays fade in (replacing the rotated blobs) and slide
    // outward to flank the welcome text, reading as `<` `>`. On
    // the reverse leg they slide back + fade out, then the
    // hair/beard inner groups unwind to their resting state.
    const leftBracketOuterRef = useRef<SVGGElement | null>(null);
    const leftBracketRef = useRef<SVGPathElement | null>(null);
    const rightBracketOuterRef = useRef<SVGGElement | null>(null);
    const rightBracketRef = useRef<SVGPathElement | null>(null);

    // Welcome text (HTML overlay, sits above the SVG centre).
    const welcomeRef = useRef<HTMLSpanElement | null>(null);

    // ---- Iris cursor tracking --------------------------------
    useEffect(() => {
        const svg = svgRef.current;
        const eye = eyePathRef.current;
        const iris = irisRef.current;
        if (!svg || !eye || !iris) return;

        const eyeCenter = readCenter(eye, FALLBACK_EYE_CX, FALLBACK_EYE_CY);
        const irisCenter = readCenter(
            iris,
            FALLBACK_IRIS_CX,
            FALLBACK_IRIS_CY,
        );

        const baseOffsetX = eyeCenter.cx - irisCenter.cx;
        const baseOffsetY = eyeCenter.cy - irisCenter.cy;
        gsap.set(iris, { x: baseOffsetX, y: baseOffsetY });

        const xTo = gsap.quickTo(iris, "x", {
            duration: 0.3,
            ease: "power3.out",
        });
        const yTo = gsap.quickTo(iris, "y", {
            duration: 0.3,
            ease: "power3.out",
        });

        const onMove = (e: MouseEvent) => {
            const rect = svg.getBoundingClientRect();
            if (rect.width === 0 || rect.height === 0) return;
            const cx = rect.left + (eyeCenter.cx / VIEWBOX) * rect.width;
            const cy = rect.top + (eyeCenter.cy / VIEWBOX) * rect.height;
            const dx = e.clientX - cx;
            const dy = e.clientY - cy;
            const dist = Math.hypot(dx, dy);
            if (dist === 0) {
                xTo(baseOffsetX);
                yTo(baseOffsetY);
                return;
            }
            const ratio = Math.min(1, dist / IRIS_SATURATION_PX);
            const travel = IRIS_MAX_TRAVEL_VB * ratio;
            xTo(baseOffsetX + (dx / dist) * travel);
            yTo(baseOffsetY + (dy / dist) * travel);
        };

        window.addEventListener("mousemove", onMove, { passive: true });
        return () => window.removeEventListener("mousemove", onMove);
    }, []);

    // ---- Greeting timeline -----------------------------------
    useEffect(() => {
        const svg = svgRef.current;
        const hairOuter = hairOuterRef.current;
        const hairInner = hairInnerRef.current;
        const hairPath = hairPathRef.current;
        const beardOuter = beardOuterRef.current;
        const beardInner = beardInnerRef.current;
        const beardPath = beardPathRef.current;
        const eyeOuter = eyeOuterRef.current;
        const eyeInner = eyeInnerRef.current;
        const welcome = welcomeRef.current;
        const leftBracketOuter = leftBracketOuterRef.current;
        const leftBracket = leftBracketRef.current;
        const rightBracketOuter = rightBracketOuterRef.current;
        const rightBracket = rightBracketRef.current;
        if (
            !svg ||
            !hairOuter ||
            !hairInner ||
            !hairPath ||
            !beardOuter ||
            !beardInner ||
            !beardPath ||
            !eyeOuter ||
            !eyeInner ||
            !welcome ||
            !leftBracketOuter ||
            !leftBracket ||
            !rightBracketOuter ||
            !rightBracket
        ) {
            return;
        }

        // ---- Geometry ---------------------------------------
        // Measure the welcome line and the SVG's screen size so
        // we can convert between CSS-pixel and viewBox units.
        // Reading the line's bbox before any GSAP transforms
        // touch it gives us the natural rendered size — opacity
        // is 0 via CSS but layout still happens.
        const welcomeRect = welcome.getBoundingClientRect();
        const svgRect = svg.getBoundingClientRect();
        const pxToVb =
            svgRect.width > 0 ? VIEWBOX / svgRect.width : 1;
        const textHeightVb = welcomeRect.height * pxToVb;
        const textWidthVb = welcomeRect.width * pxToVb;

        // Each bracket morphs to BRACKET_HEIGHT_MULT × text
        // height, in viewBox units.
        const targetHeightVb = textHeightVb * BRACKET_HEIGHT_MULT;

        // Natural bboxes (in viewBox units). After a -90° rotation
        // the natural width becomes the screen-vertical extent and
        // the natural height becomes the screen-horizontal extent.
        // Uniform scale is therefore (target / naturalWidth) so
        // the rotated shape ends up exactly `targetHeightVb` tall.
        const hairBbox = readBboxSize(hairPath, FALLBACK_HAIR_BBOX);
        const beardBbox = readBboxSize(beardPath, FALLBACK_BEARD_BBOX);
        const hairScale = targetHeightVb / hairBbox.width;
        const beardScale = targetHeightVb / beardBbox.width;

        // Post-transform half-widths (the screen-X half-extent
        // each bracket occupies once rotated and scaled).
        const hairHalfWidthVb =
            (hairBbox.height * hairScale) / 2;
        const beardHalfWidthVb =
            (beardBbox.height * beardScale) / 2;

        // Slide distances — each bracket sits just outside the
        // text's bounding box, with a margin proportional to the
        // text width.
        const marginVb = textWidthVb * BRACKET_MARGIN_FRACTION;
        const hairSlide = -(
            textWidthVb / 2 +
            marginVb +
            hairHalfWidthVb
        );
        const beardSlide =
            textWidthVb / 2 + marginVb + beardHalfWidthVb;

        // ---- Bracket geometry --------------------------------
        // Brackets are stroked chevrons drawn at the canvas
        // centre, sized to the welcome text height. Their outer
        // groups inherit the same `hairSlide` / `beardSlide`
        // distances the rotated blobs target — so the bracket
        // emerges right where the rotated original disappears.
        const bracketHalfH = targetHeightVb / 2;
        const bracketHalfW = targetHeightVb * 0.28; // chevron aspect
        const bracketStrokeVb = Math.max(targetHeightVb * 0.10, 18);
        const bcx = CANVAS_CENTER;
        const bcy = CANVAS_CENTER;
        // `<` — point on the left, opens to the right.
        const leftBracketD =
            `M ${bcx + bracketHalfW} ${bcy - bracketHalfH} ` +
            `L ${bcx - bracketHalfW} ${bcy} ` +
            `L ${bcx + bracketHalfW} ${bcy + bracketHalfH}`;
        // `>` — point on the right, opens to the left.
        const rightBracketD =
            `M ${bcx - bracketHalfW} ${bcy - bracketHalfH} ` +
            `L ${bcx + bracketHalfW} ${bcy} ` +
            `L ${bcx - bracketHalfW} ${bcy + bracketHalfH}`;
        leftBracket.setAttribute("d", leftBracketD);
        leftBracket.setAttribute("stroke-width", `${bracketStrokeVb}`);
        rightBracket.setAttribute("d", rightBracketD);
        rightBracket.setAttribute("stroke-width", `${bracketStrokeVb}`);
        // Brackets start invisible at the canvas centre.
        gsap.set([leftBracketOuter, rightBracketOuter], { opacity: 0, x: 0 });

        // Welcome line starts hidden.
        gsap.set(welcome, { opacity: 0, scale: 0.95 });

        const origin = `${CANVAS_CENTER} ${CANVAS_CENTER}`;

        const tl = gsap.timeline({ delay: REST_DELAY_S });

        // Phase A — morph each bracket: rotate −90° + uniform
        // scale to the target height, all anchored to the canvas
        // centre. Hair (originally top) lands on the left, beard
        // (originally bottom) lands on the right. The eye rotates
        // along but doesn't scale (it's about to fade).
        tl.to(hairInner, {
            rotation: -90,
            scale: hairScale,
            duration: MORPH_ROTATION_S,
            ease: "power2.inOut",
            svgOrigin: origin,
        });
        tl.to(
            beardInner,
            {
                rotation: -90,
                scale: beardScale,
                duration: MORPH_ROTATION_S,
                ease: "power2.inOut",
                svgOrigin: origin,
            },
            "<",
        );
        tl.to(
            eyeInner,
            {
                rotation: -90,
                duration: MORPH_ROTATION_S,
                ease: "power2.inOut",
                svgOrigin: origin,
            },
            "<",
        );

        // Phase B — bracket morph. The rotated hair/beard blobs
        // fade out at the canvas centre while the bracket
        // overlays fade in at the same spot and slide outward to
        // their calibrated flanking positions. Eye fades too,
        // clearing the centre for the welcome text.
        tl.to(
            [hairInner, beardInner],
            {
                opacity: 0,
                duration: FADE_S,
                ease: "power2.in",
            },
            "-=0.45",
        );
        tl.to(
            leftBracketOuter,
            {
                opacity: 1,
                x: hairSlide,
                duration: MORPH_SLIDE_S,
                ease: "power2.out",
            },
            "<",
        );
        tl.to(
            rightBracketOuter,
            {
                opacity: 1,
                x: beardSlide,
                duration: MORPH_SLIDE_S,
                ease: "power2.out",
            },
            "<",
        );
        tl.to(
            eyeOuter,
            {
                opacity: 0,
                duration: FADE_S,
                ease: "power2.in",
            },
            "<",
        );

        // Phase C — welcome line fades in, vertically centred
        // (the CSS overlay handles the centring; GSAP only
        // animates opacity + a small scale-up).
        tl.to(
            welcome,
            {
                opacity: 1,
                scale: 1,
                duration: FADE_S,
                ease: "power2.out",
            },
            "-=0.15",
        );

        // Phase D — hold.
        tl.to({}, { duration: HOLD_DURATION_S });

        // Phase E — fade the welcome line out.
        tl.to(welcome, {
            opacity: 0,
            scale: 0.95,
            duration: FADE_S,
            ease: "power2.in",
        });

        // Phase F — brackets slide back to centre + fade out;
        // hair/beard fade back in at the centre (still rotated
        // from Phase A — Phase G unwinds that). Eye fades back.
        tl.to(
            leftBracketOuter,
            {
                opacity: 0,
                x: 0,
                duration: MORPH_SLIDE_S,
                ease: "power2.inOut",
            },
            "-=0.2",
        );
        tl.to(
            rightBracketOuter,
            {
                opacity: 0,
                x: 0,
                duration: MORPH_SLIDE_S,
                ease: "power2.inOut",
            },
            "<",
        );
        tl.to(
            [hairInner, beardInner],
            {
                opacity: 1,
                duration: FADE_S,
                ease: "power2.out",
            },
            "<",
        );
        tl.to(
            eyeOuter,
            {
                opacity: 1,
                duration: FADE_S,
                ease: "power2.out",
            },
            "<",
        );

        // Phase G — unwind rotation + scale on hair / beard, and
        // unwind rotation on the eye.
        tl.to(
            hairInner,
            {
                rotation: 0,
                scale: 1,
                duration: MORPH_ROTATION_S,
                ease: "power2.inOut",
                svgOrigin: origin,
            },
            "-=0.3",
        );
        tl.to(
            beardInner,
            {
                rotation: 0,
                scale: 1,
                duration: MORPH_ROTATION_S,
                ease: "power2.inOut",
                svgOrigin: origin,
            },
            "<",
        );
        tl.to(
            eyeInner,
            {
                rotation: 0,
                duration: MORPH_ROTATION_S,
                ease: "power2.inOut",
                svgOrigin: origin,
            },
            "<",
        );

        return () => {
            tl.kill();
        };
    }, []);

    return (
        <div
            className={
                "execlaw-mascot-stage" +
                (className ? " " + className : "")
            }
            style={{ width: size, height: size }}
        >
            <svg
                ref={svgRef}
                className="execlaw-mascot"
                viewBox={`0 0 ${VIEWBOX} ${VIEWBOX}`}
                width={size}
                height={size}
                role="img"
                aria-label="execlaw"
            >
                {/* Hair — outer slide group + inner rotate
                    group. The path ref is read once for getBBox
                    so we can size the bracket to the welcome
                    line. */}
                <g ref={hairOuterRef}>
                    <g ref={hairInnerRef}>
                        <path
                            ref={hairPathRef}
                            d={HAIR_PATH}
                            fill="currentColor"
                        />
                    </g>
                </g>
                {/* Beard. */}
                <g ref={beardOuterRef}>
                    <g ref={beardInnerRef}>
                        <path
                            ref={beardPathRef}
                            d={BEARD_PATH}
                            fill="currentColor"
                        />
                    </g>
                </g>
                {/* Eye + iris. The outer group also handles the
                    opacity fade during the greeting. */}
                <g ref={eyeOuterRef}>
                    <g ref={eyeInnerRef}>
                        <path
                            ref={eyePathRef}
                            d={EYE_PATH}
                            fill="currentColor"
                        />
                        <g
                            ref={irisRef}
                            className="execlaw-mascot__iris"
                        >
                            <path d={IRIS_PATH} />
                        </g>
                    </g>
                </g>
                {/* Bracket overlays — d / stroke-width set at
                    runtime from the welcome line's measured
                    height (see useEffect). Initially hidden;
                    fade in as the rotated hair/beard fades out. */}
                <g ref={leftBracketOuterRef} opacity="0">
                    <path
                        ref={leftBracketRef}
                        d=""
                        fill="none"
                        stroke="currentColor"
                        strokeWidth={20}
                        strokeLinecap="round"
                        strokeLinejoin="round"
                    />
                </g>
                <g ref={rightBracketOuterRef} opacity="0">
                    <path
                        ref={rightBracketRef}
                        d=""
                        fill="none"
                        stroke="currentColor"
                        strokeWidth={20}
                        strokeLinecap="round"
                        strokeLinejoin="round"
                    />
                </g>
            </svg>
            <span
                ref={welcomeRef}
                className="execlaw-mascot-stage__text"
                data-testid="welcome-greeting-text"
            >
                Welcome, {userName}!
            </span>
        </div>
    );
}
