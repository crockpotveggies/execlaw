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

interface Props {
    /** Pixel size of the rendered SVG. Defaults to 216. */
    size?: number;
    /** Name to interpolate into the welcome line. Falls back to
     *  "friend" so the line still reads if we don't have a name. */
    userName?: string;
    /** Optional className passthrough for layout/spacing. */
    className?: string;
}

const VIEWBOX = 1254;
const CANVAS_CENTER = VIEWBOX / 2;
// Iris cursor-tracking constants (carried over from the standalone
// Mascot component).
const IRIS_MAX_TRAVEL_VB = 50;
const IRIS_SATURATION_PX = 360;

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

// --- Path data, split out from the source SVG ---------------------
//
// Source has hair + beard in a single subpath set; we split them
// so each can be rotated and translated independently.
//
// Beard subpath (the one that began the source's path data).
const BEARD_PATH =
    "M 654.15557,975.88339 c -38.88731,-8.46697 -68.44901,-38.61953 -102.94573,-56.92215 -32.49091,-24.96176 -77.36552,-37.20328 -101.56163,-70.94735 -5.68076,-31.92277 20.16397,-59.61023 34.40601,-86.31057 18.23629,-31.60889 57.05504,-61.28168 94.5227,-43.33332 33.00314,14.5489 61.97447,54.11906 101.87893,38.96376 35.09898,-15.10221 65.67858,-56.09129 108.05236,-40.97961 40.91001,16.35199 60.53646,60.40367 79.58094,97.20202 24.23432,36.22277 -18.02399,60.74167 -45.144,74.89116 -46.26156,27.52673 -90.26734,59.30025 -138.00272,84.05777 -9.90737,2.59313 -20.50639,5.652 -30.78686,3.37829 z";
// Hair subpath. Absolute start computed from the source's chained
// `m` commands: (654.15557 − 290, 975.88339 − 184.69708).
const HAIR_PATH =
    "M 364.15557,791.18631 c -23.62607,-21.7223 -32.45073,-56.71081 -43.2739,-86.49509 -40.53572,-128.57979 4.48805,-276.92121 106.89777,-363.85884 104.95089,-96.23256 271.0206,-118.34118 396.19401,-49.34125 129.0904,66.96607 209.13815,217.75556 187.52145,362.30502 -6.1483,47.26262 -22.71535,93.17983 -47.83049,133.62639 -32.48229,9.34943 -14.78793,-49.99715 -30.83962,-68.78839 -10.11855,-34.51325 -37.1563,-58.81949 -61.42363,-83.46046 -22.86972,-32.68484 -6.88528,-75.51731 -20.96525,-111.40497 -25.15487,-98.03949 -134.1298,-165.60464 -233.0275,-139.44942 -86.40582,19.0432 -151.14463,101.54579 -152.01106,189.50444 3.11626,32.19608 -8.33193,65.19195 -35.8964,83.70425 -35.99289,32.02096 -49.2266,81.00689 -53.29136,127.3193 -1.86051,4.68904 -7.3054,6.97853 -12.05402,6.33902 z";
const EYE_PATH =
    "M 643.65557,699.75475 c -66.42845,-8.76517 -115.89555,-78.66217 -100.68539,-144.29273 11.11596,-69.61313 90.16755,-116.82716 156.72805,-92.64361 73.17109,21.45853 106.26577,117.63314 65.07339,180.71597 -24.51852,41.12465 -74.0613,63.09685 -121.11605,56.22037 z";
const IRIS_PATH =
    "M 717.89431,552.74422 c 47.70688,-26.59709 -27.14173,-79.76534 -38.24866,-29.23118 -5.16587,21.56539 18.8456,39.35316 38.24866,29.23118 z";

// Resting centres for parent eye and iris (used by cursor
// tracking when getBBox isn't available — jsdom).
const FALLBACK_EYE_CX = 653;
const FALLBACK_EYE_CY = 581;
const FALLBACK_IRIS_CX = 703;
const FALLBACK_IRIS_CY = 553;

function readCenter(
    el: SVGGraphicsElement | null,
    fallbackCx: number,
    fallbackCy: number,
): { cx: number; cy: number } {
    if (!el || typeof el.getBBox !== "function") {
        return { cx: fallbackCx, cy: fallbackCy };
    }
    try {
        const b = el.getBBox();
        if (b.width <= 0 || b.height <= 0) {
            return { cx: fallbackCx, cy: fallbackCy };
        }
        return {
            cx: b.x + b.width / 2,
            cy: b.y + b.height / 2,
        };
    } catch {
        return { cx: fallbackCx, cy: fallbackCy };
    }
}

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
            !welcome
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

        // Phase B — slide outward to the calibrated positions so
        // the text + margin fits exactly between the brackets.
        // Eye fades to clear the centre.
        tl.to(
            hairOuter,
            {
                x: hairSlide,
                duration: MORPH_SLIDE_S,
                ease: "power2.out",
            },
            "-=0.45",
        );
        tl.to(
            beardOuter,
            {
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

        // Phase F — slide hair / beard back to centre and fade
        // the eye back in (overlapping for a smooth return).
        tl.to(
            hairOuter,
            {
                x: 0,
                duration: MORPH_SLIDE_S,
                ease: "power2.inOut",
            },
            "-=0.2",
        );
        tl.to(
            beardOuter,
            {
                x: 0,
                duration: MORPH_SLIDE_S,
                ease: "power2.inOut",
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
            </svg>
            <span
                ref={welcomeRef}
                className="execlaw-mascot-stage__text"
                data-testid="welcome-greeting-text"
            >
                welcome {userName}
            </span>
        </div>
    );
}
