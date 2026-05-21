// MascotGreeting — mascot face + time-of-day greeting revealed by a
// particle constellation emitting from the mascot's eye.
//
// 2026-05-21 — replaced the original full-shape morph (where the
// hair and beard rotated −90° to become `<` and `>` brackets
// around a "Welcome, name!" line) with a composed sequence:
//
//   1. The hair LIFTS off the head (mirrors the MiniMascotSpinner
//      "pop" pattern) — a small upward translate + scale + tilt
//      anchored at the bottom-centre of the hair bbox. The beard
//      squashes inward to "catch" the absent hair, and the eye
//      widens slightly. Reads as "the mascot's mind is opening."
//   2. A swarm of accent-blue PARTICLES bursts from the EXPOSED
//      eye (now the visible center of the face), scatters into a
//      loose cloud, and funnels into a line beneath the mascot.
//   3. As the particles arrive, the time-of-day GREETING fades in
//      ("Good morning, Justin." / afternoon / evening).
//   4. The hair DROPS back with a bounce, the beard rebounds, the
//      eye settles. Residual particles dissolve, leaving the line.
//
// The mascot continues idle motion (slow breath + occasional
// blink + cursor-tracking iris) before, during, and after the
// sequence — so the mascot reads as steadily alive rather than a
// performer that goes still between cues.
//
// REDUCED MOTION
//   `prefers-reduced-motion: reduce` skips the lift, the burst,
//   and the convergence. The greeting just fades in. Breath /
//   blink keep running — they're well below the parallax threshold
//   the preference targets.

import { useGSAP } from "@gsap/react";
import gsap from "gsap";
import { useMemo, useRef } from "react";
import {
    BEARD_PATH,
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
    /** Name to interpolate into the greeting. Falls back to
     *  "friend" so the line still reads if we don't have a name. */
    userName?: string;
    /** Optional className passthrough for layout/spacing. */
    className?: string;
    /**
     * 2026-05-21 — when `true` the mascot transitions to its
     * incognito look: colour shifts from accent-blue to
     * danger-red and the iris (the small pupil blob) cross-
     * fades into an X cross. Default `false`. Reverses
     * cleanly when toggled back off.
     */
    incognito?: boolean;
}

// ---- Tuning knobs ---------------------------------------------------

/** Total particles in the constellation. */
const PARTICLE_COUNT = 14;

/** Delay after mount before the sequence begins. Gives the layout
 *  a beat so the welcome text bbox we measure is stable. */
const REST_DELAY_S = 0.4;

/** Pixel ranges used during the BURST phase (random per particle).
 *  Values land in `.execlaw-mascot-stage__particle`'s CSS-pixel
 *  frame (the particles are HTML divs, not SVG elements). */
const BURST_DIST_MIN = 70;
const BURST_DIST_MAX = 140;

/** Vertical jitter applied to each particle's converge target so
 *  the resting line looks like a constellation rather than a
 *  perfectly straight ruler. */
const CONVERGE_JITTER_Y = 10;

// ---- Hair lift values (CSS pixels via `transform-box: fill-box`) ---
//
// Calibrated against `size = 216`. At smaller renderings the lift
// will look proportionally identical because we're scaling against
// the hair's own bbox via fill-box.
const HAIR_LIFT_Y = -32;
const HAIR_LIFT_SCALE = 1.04;
const HAIR_LIFT_ROT = -4;
const BEARD_SQUASH_Y = 0.9;
const BEARD_SQUASH_X = 1.05;
const EYE_WIDEN_SCALE = 1.06;

// ---- Time-of-day greeting -----------------------------------------

/** Map an hour-of-day into a friendly greeting. Boundaries match
 *  conversational English: morning until noon, afternoon until 5,
 *  evening covers everything from late afternoon to early morning
 *  (no separate "good night" — that implies departure). */
function timeOfDayGreeting(hour: number): string {
    if (hour >= 5 && hour < 12) return "Good morning";
    if (hour >= 12 && hour < 17) return "Good afternoon";
    return "Good evening";
}

// ---- Greeting font rotation ----------------------------------------

/** Display fonts the greeting cycles through on each page load.
 *  Three sci-fi-leaning sans-serifs (rendered ALL CAPS) plus one
 *  editorial italic serif — see `theme.scss` for the per-variant
 *  size / letter-spacing / case-transform tuning that makes each
 *  read as the "cinematic title card" version of itself.
 *
 *  All four are self-hosted via fontsource (imports in `main.tsx`)
 *  so the SPA stays offline-capable. */
const FONT_VARIANTS = [
    "unica-one",
    "orbitron",
    "antonio",
    "instrument-serif",
] as const;
type FontVariant = (typeof FONT_VARIANTS)[number];

const FONT_STORAGE_KEY = "execlaw:welcome-font";

/** Pick a font that's NOT the one we showed last reload. Pure random
 *  would occasionally repeat back-to-back which kills the "fresh
 *  greeting every time" feel; excluding the previous pick guarantees
 *  variation without requiring strict round-robin bookkeeping. */
function pickGreetingFont(): FontVariant {
    let last: string | null = null;
    try {
        last = localStorage.getItem(FONT_STORAGE_KEY);
    } catch {
        /* private mode / storage-disabled — fall through */
    }
    const candidates = FONT_VARIANTS.filter((f) => f !== last);
    const choice =
        candidates[Math.floor(Math.random() * candidates.length)] ??
        FONT_VARIANTS[0];
    try {
        localStorage.setItem(FONT_STORAGE_KEY, choice);
    } catch {
        /* ignore */
    }
    return choice;
}

export function MascotGreeting({
    size = 216,
    userName = "friend",
    className,
    incognito = false,
}: Props) {
    const stageRef = useRef<HTMLDivElement | null>(null);
    const svgRef = useRef<SVGSVGElement | null>(null);

    // Per-shape refs for the lift sequence + iris cursor tracking.
    const hairRef = useRef<SVGPathElement | null>(null);
    const beardRef = useRef<SVGPathElement | null>(null);
    const eyeOuterRef = useRef<SVGGElement | null>(null);
    const eyePathRef = useRef<SVGPathElement | null>(null);
    const irisRef = useRef<SVGGElement | null>(null);
    // 2026-05-21 — alternate "iris" shape rendered for incognito
    // mode: an X cross stroked over the eye where the iris blob
    // sits in regular mode. The two shapes share the eye-outer
    // group so they inherit the same blink / lift transforms; the
    // incognito useGSAP cross-fades opacity between them.
    const irisXRef = useRef<SVGGElement | null>(null);

    const welcomeRef = useRef<HTMLSpanElement | null>(null);
    const particleRefs = useRef<Array<HTMLDivElement | null>>([]);

    // Greeting is computed once per mount so the line doesn't flick
    // categories at the exact hour boundary while the welcome view
    // is visible.
    const greeting = useMemo(() => {
        const hour = new Date().getHours();
        return `${timeOfDayGreeting(hour)}, ${userName}.`;
    }, [userName]);

    // Random display font per mount. `useMemo` with no deps so we
    // don't re-pick on parent re-renders inside the same mount —
    // the visual identity should stay stable for the life of the
    // welcome view.
    const fontVariant = useMemo<FontVariant>(() => pickGreetingFont(), []);

    // ---- Iris cursor tracking ---------------------------------------
    useGSAP(
        () => {
            const svg = svgRef.current;
            const eye = eyePathRef.current;
            const iris = irisRef.current;
            // 2026-05-21 — the incognito X cross sits at the same rest
            // position as the iris pupil and should track the cursor
            // in lockstep so the swap feels continuous rather than
            // popping between a moving iris and a static X.
            const irisX = irisXRef.current;
            if (!svg || !eye || !iris) return;

            const eyeCenter = readCenter(
                eye,
                FALLBACK_EYE_CX,
                FALLBACK_EYE_CY,
            );
            const irisCenter = readCenter(
                iris,
                FALLBACK_IRIS_CX,
                FALLBACK_IRIS_CY,
            );

            const baseOffsetX = eyeCenter.cx - irisCenter.cx;
            const baseOffsetY = eyeCenter.cy - irisCenter.cy;
            // Park both the iris blob and the X cross at the same
            // rest position. They share `FALLBACK_IRIS_CX/Y` as
            // their natural drawn centre, so the same translate
            // moves both to align with the eye centre.
            gsap.set(iris, { x: baseOffsetX, y: baseOffsetY });
            if (irisX) {
                gsap.set(irisX, { x: baseOffsetX, y: baseOffsetY });
            }

            const xTo = gsap.quickTo(iris, "x", {
                duration: 0.3,
                ease: "power3.out",
            });
            const yTo = gsap.quickTo(iris, "y", {
                duration: 0.3,
                ease: "power3.out",
            });
            // Separate quickTo setters for the X — quickTo only
            // accepts a single target, so we keep two parallel
            // setter pairs and call all four with the same value
            // per mousemove. Tiny perf cost (two extra tweens) for
            // a much simpler structure than wrapping iris + X in a
            // shared `<g>`.
            const xToX = irisX
                ? gsap.quickTo(irisX, "x", {
                      duration: 0.3,
                      ease: "power3.out",
                  })
                : null;
            const yToX = irisX
                ? gsap.quickTo(irisX, "y", {
                      duration: 0.3,
                      ease: "power3.out",
                  })
                : null;

            const onMove = (e: MouseEvent) => {
                const rect = svg.getBoundingClientRect();
                if (rect.width === 0 || rect.height === 0) return;
                const cx =
                    rect.left + (eyeCenter.cx / VIEWBOX) * rect.width;
                const cy =
                    rect.top + (eyeCenter.cy / VIEWBOX) * rect.height;
                const dx = e.clientX - cx;
                const dy = e.clientY - cy;
                const dist = Math.hypot(dx, dy);
                if (dist === 0) {
                    xTo(baseOffsetX);
                    yTo(baseOffsetY);
                    xToX?.(baseOffsetX);
                    yToX?.(baseOffsetY);
                    return;
                }
                const ratio = Math.min(1, dist / IRIS_SATURATION_PX);
                const travel = IRIS_MAX_TRAVEL_VB * ratio;
                const nextX = baseOffsetX + (dx / dist) * travel;
                const nextY = baseOffsetY + (dy / dist) * travel;
                xTo(nextX);
                yTo(nextY);
                xToX?.(nextX);
                yToX?.(nextY);
            };

            window.addEventListener("mousemove", onMove, { passive: true });
            return () => window.removeEventListener("mousemove", onMove);
        },
        { dependencies: [] },
    );

    // ---- Idle breath + blink ----------------------------------------
    //
    // Tiny ambient motion so the mascot doesn't feel frozen.
    //
    //   * Breath — uniform scale pulse on the whole SVG (1 → 1.015,
    //     3.5s yoyo).
    //
    //   * Blink — multi-step squash on the eye-outer group every
    //     5–9s with a randomised delay. The blink passes through
    //     an intermediate `scaleY: 0.4` ("football" / lens) stage
    //     on the way down and on the way back up so the operator
    //     actually SEES the eye close, rather than a sub-frame
    //     vertical flash. Pivot is set via `svgOrigin` pinned to
    //     the eye path's user-space centre — using `transformOrigin
    //     + fill-box` previously sometimes drifted off the visual
    //     centre of the eye because the eye-outer group's bbox
    //     includes the iris, which shifts the union bbox up-right.
    useGSAP(
        () => {
            const svg = svgRef.current;
            const eyeOuter = eyeOuterRef.current;
            const eyePath = eyePathRef.current;
            if (!svg || !eyeOuter || !eyePath) return;

            // Breath.
            gsap.to(svg, {
                scale: 1.015,
                duration: 3.5,
                yoyo: true,
                repeat: -1,
                ease: "sine.inOut",
            });

            // Blink pivot — eye path's actual centre, in viewBox
            // user-space coords.
            const eyeCenter = readCenter(
                eyePath,
                FALLBACK_EYE_CX,
                FALLBACK_EYE_CY,
            );
            const blinkOrigin = `${eyeCenter.cx} ${eyeCenter.cy}`;
            gsap.set(eyeOuter, { svgOrigin: blinkOrigin });

            let cancelled = false;
            const scheduleBlink = () => {
                if (cancelled) return;
                const delay = gsap.utils.random(5, 9);
                const tl = gsap.timeline({
                    delay,
                    // Every step keeps the same pivot so the eye
                    // squishes around its centre instead of
                    // sliding vertically across the head.
                    defaults: { svgOrigin: blinkOrigin },
                    onComplete: scheduleBlink,
                });
                // Open → football (lens-shaped, half-closed). Slowest
                // of the steps — this is where the operator reads
                // "the eye is closing" rather than "the eye blinked."
                tl.to(eyeOuter, {
                    scaleY: 0.4,
                    duration: 0.10,
                    ease: "power2.inOut",
                });
                // Football → closed. Quick snap to a thin line.
                tl.to(eyeOuter, {
                    scaleY: 0.06,
                    duration: 0.04,
                    ease: "power2.in",
                });
                // Briefly held closed (the eyelid "rests").
                tl.to(eyeOuter, {
                    scaleY: 0.06,
                    duration: 0.03,
                });
                // Closed → football. Mirrors the close-snap so the
                // open looks like a deliberate eyelid lift.
                tl.to(eyeOuter, {
                    scaleY: 0.4,
                    duration: 0.04,
                    ease: "power2.out",
                });
                // Football → open. Slowest again — the eyelid
                // settles to fully open.
                tl.to(eyeOuter, {
                    scaleY: 1,
                    duration: 0.09,
                    ease: "power2.out",
                });
            };
            scheduleBlink();
            return () => {
                cancelled = true;
            };
        },
        { dependencies: [] },
    );

    // ---- Greeting sequence: lift → emit → drop → reveal ------------
    useGSAP(
        () => {
            const stage = stageRef.current;
            const svg = svgRef.current;
            const hair = hairRef.current;
            const beard = beardRef.current;
            const eyeOuter = eyeOuterRef.current;
            const eyePath = eyePathRef.current;
            const welcome = welcomeRef.current;
            const particles = particleRefs.current.filter(
                (p): p is HTMLDivElement => p !== null,
            );
            if (
                !stage ||
                !svg ||
                !hair ||
                !beard ||
                !eyeOuter ||
                !eyePath ||
                !welcome ||
                particles.length !== PARTICLE_COUNT
            ) {
                return;
            }

            const reducedMotion =
                typeof window !== "undefined" &&
                window.matchMedia?.("(prefers-reduced-motion: reduce)")
                    .matches;

            gsap.set(welcome, { opacity: 0, y: 6 });

            if (reducedMotion) {
                gsap.to(welcome, {
                    opacity: 1,
                    y: 0,
                    duration: 0.6,
                    delay: REST_DELAY_S,
                    ease: "power2.out",
                });
                return;
            }

            // Pivots. Hair pivots at its OWN bottom-centre so the
            // lift + tilt hinges off the top of the head, not the
            // SVG canvas centre. Beard pivots at its top so a
            // vertical squash compresses toward the eye like an
            // inhale. Eye scales from its user-space centre — same
            // `svgOrigin` the blink uses, so the two effects share
            // a consistent pivot even if the blink fires shortly
            // after the lift settles.
            gsap.set(hair, { transformOrigin: "50% 100%" });
            gsap.set(beard, { transformOrigin: "50% 0%" });
            const liftEyeCenter = readCenter(
                eyePath,
                FALLBACK_EYE_CX,
                FALLBACK_EYE_CY,
            );
            gsap.set(eyeOuter, {
                svgOrigin: `${liftEyeCenter.cx} ${liftEyeCenter.cy}`,
            });

            // ---- Geometry — compute particle origin (the eye centre,
            // in stage-local CSS pixels) and converge targets along
            // the greeting baseline. We use the eye's path bbox so the
            // burst clearly originates from the visible eye shape
            // rather than the SVG-canvas centre.
            const stageRect = stage.getBoundingClientRect();
            const svgRect = svg.getBoundingClientRect();
            const welcomeRect = welcome.getBoundingClientRect();

            const eyeCenter = readCenter(
                eyePath,
                FALLBACK_EYE_CX,
                FALLBACK_EYE_CY,
            );
            const eyeScreenX =
                svgRect.left + (eyeCenter.cx / VIEWBOX) * svgRect.width;
            const eyeScreenY =
                svgRect.top + (eyeCenter.cy / VIEWBOX) * svgRect.height;
            const originX = eyeScreenX - stageRect.left;
            const originY = eyeScreenY - stageRect.top;

            const targetCenterX =
                welcomeRect.left + welcomeRect.width / 2 - stageRect.left;
            const targetCenterY =
                welcomeRect.top + welcomeRect.height / 2 - stageRect.top;
            const targetSpread = welcomeRect.width * 0.85;

            // Per-particle params. Burst angle is biased upward so
            // the cloud hovers ABOVE the (newly exposed) eye rather
            // than ringing it — the lifted hair has cleared that
            // upper space and the cloud reads as "thought rising."
            const setup = particles.map((_, i) => {
                const burstAngle = gsap.utils.random(0, Math.PI * 2);
                const burstDist = gsap.utils.random(
                    BURST_DIST_MIN,
                    BURST_DIST_MAX,
                );
                const burstX = Math.cos(burstAngle) * burstDist;
                // Subtract 40 — additional upward lift on top of the
                // raw radial scatter so the cloud crowns the mascot.
                const burstY = Math.sin(burstAngle) * burstDist - 40;

                const t = (i + 0.5) / PARTICLE_COUNT;
                const targetX =
                    targetCenterX -
                    targetSpread / 2 +
                    t * targetSpread +
                    gsap.utils.random(-4, 4);
                const targetY =
                    targetCenterY +
                    gsap.utils.random(
                        -CONVERGE_JITTER_Y,
                        CONVERGE_JITTER_Y,
                    );

                return { burstX, burstY, targetX, targetY };
            });

            // Each particle's CSS home is at the stage centre
            // (top: 50%; left: 50%). We translate to the eye via a
            // delta against the stage centre, then apply the per-
            // particle burst / converge offsets relative to that.
            const stageCenterX = stageRect.width / 2;
            const stageCenterY = stageRect.height / 2;
            const homeX = originX - stageCenterX;
            const homeY = originY - stageCenterY;
            gsap.set(particles, {
                x: homeX,
                y: homeY,
                scale: 0,
                opacity: 0,
            });

            const tl = gsap.timeline({ delay: REST_DELAY_S });

            // 1. LIFT — hair pops up + tilts, beard inhales, eye
            //    widens. Mirrors the MiniMascotSpinner pop pattern
            //    but at MascotGreeting amplitude (size=216 vs 22px).
            tl.to(
                hair,
                {
                    y: HAIR_LIFT_Y,
                    scale: HAIR_LIFT_SCALE,
                    rotation: HAIR_LIFT_ROT,
                    duration: 0.36,
                    ease: "back.out(1.6)",
                },
                0,
            )
                .to(
                    beard,
                    {
                        scaleY: BEARD_SQUASH_Y,
                        scaleX: BEARD_SQUASH_X,
                        duration: 0.36,
                        ease: "power2.out",
                    },
                    0,
                )
                .to(
                    eyeOuter,
                    {
                        scale: EYE_WIDEN_SCALE,
                        duration: 0.36,
                        ease: "power2.out",
                    },
                    0,
                );

            // 2. BURST — particles fly out from the eye. Starts
            //    around the time the hair lift peaks; the eye is
            //    fully exposed by then.
            tl.to(
                particles,
                {
                    x: (i) => homeX + setup[i].burstX,
                    y: (i) => homeY + setup[i].burstY,
                    scale: 1,
                    opacity: 1,
                    duration: 0.55,
                    ease: "back.out(1.3)",
                    stagger: { each: 0.025, from: "random" },
                },
                0.22,
            );

            // 3. HOVER — tiny loose drift while the mascot holds
            //    the lifted pose. Total dwell here ≈ 0.25s.
            tl.to(particles, {
                x: (i) =>
                    homeX + setup[i].burstX + gsap.utils.random(-6, 6),
                y: (i) =>
                    homeY + setup[i].burstY + gsap.utils.random(-6, 6),
                duration: 0.25,
                ease: "sine.inOut",
            });

            // 4. DROP — hair falls back with a bounce; beard
            //    catches the impact and rebounds with elastic
            //    settle; eye returns to rest scale.
            tl.to(
                hair,
                {
                    y: 0,
                    scale: 1,
                    rotation: 0,
                    duration: 0.5,
                    ease: "bounce.out",
                },
                ">",
            )
                .to(
                    beard,
                    {
                        scaleY: 1.04,
                        scaleX: 0.98,
                        duration: 0.18,
                        ease: "power3.out",
                    },
                    "<+=0.25",
                )
                .to(
                    beard,
                    {
                        scaleY: 1,
                        scaleX: 1,
                        duration: 0.4,
                        ease: "elastic.out(1, 0.45)",
                    },
                    ">",
                )
                .to(
                    eyeOuter,
                    {
                        scale: 1,
                        duration: 0.35,
                        ease: "power2.out",
                    },
                    "<-=0.2",
                );

            // 5. CONVERGE — particles funnel down to the greeting
            //    baseline. Runs in parallel with the hair drop so
            //    the eye gets clear of the descending hair just as
            //    the particles arrive below.
            tl.to(
                particles,
                {
                    x: (i) => setup[i].targetX - stageCenterX,
                    y: (i) => setup[i].targetY - stageCenterY,
                    duration: 0.6,
                    ease: "power2.inOut",
                    stagger: { each: 0.018, from: "random" },
                },
                "<-=0.3",
            );

            // 6. REVEAL — greeting fades in just as the first
            //    particles arrive.
            tl.to(
                welcome,
                {
                    opacity: 1,
                    y: 0,
                    duration: 0.45,
                    ease: "power2.out",
                },
                "-=0.35",
            );

            // 7. DISSOLVE — particles fade + shrink from the edges
            //    inward, leaving a clean line.
            tl.to(
                particles,
                {
                    opacity: 0,
                    scale: 0.4,
                    duration: 0.4,
                    ease: "power2.in",
                    stagger: { each: 0.02, from: "edges" },
                },
                "-=0.2",
            );

            return () => {
                tl.kill();
            };
        },
        { dependencies: [] },
    );

    // ---- Incognito state: red palette + X iris -----------------------
    //
    // Toggling incognito mode on the welcome view animates two
    // changes in lockstep:
    //
    //   * The SVG's `color` (inherited by every `fill="currentColor"`
    //     path — hair, beard, eye) tweens from `$accent` (#4493f8)
    //     to `$danger` (#f85149). GSAP can animate CSS color
    //     properties; SVG paths follow.
    //
    //   * The iris (the blob pupil) cross-fades into an X cross.
    //     Both shapes live inside the eye-outer group so they
    //     inherit the same blink / lift / breath transforms — the
    //     animation is purely an opacity swap.
    //
    // First-render `gsap.set` avoids a flash: if the mascot mounts
    // with `incognito === true` already, we want the red colour +
    // X-iris in place before paint, not animated in from blue.
    const incognitoFirstRunRef = useRef(true);
    useGSAP(
        () => {
            const svg = svgRef.current;
            const iris = irisRef.current;
            const irisX = irisXRef.current;
            if (!svg || !iris || !irisX) return;

            // Brand palette literals — keep these in sync with
            // `$accent` / `$danger` in `styles/theme.scss`. SCSS
            // tokens aren't reachable from runtime JS, so the
            // values live duplicated here. If the theme palette
            // changes, update both.
            const ACCENT = "#4493f8";
            const DANGER = "#f85149";

            if (incognitoFirstRunRef.current) {
                incognitoFirstRunRef.current = false;
                if (incognito) {
                    gsap.set(svg, { color: DANGER });
                    gsap.set(iris, { opacity: 0 });
                    gsap.set(irisX, { opacity: 1 });
                }
                return;
            }

            if (incognito) {
                gsap.to(svg, {
                    color: DANGER,
                    duration: 0.4,
                    ease: "power2.out",
                });
                gsap.to(iris, {
                    opacity: 0,
                    duration: 0.25,
                    ease: "power2.in",
                });
                gsap.to(irisX, {
                    opacity: 1,
                    duration: 0.3,
                    ease: "power2.out",
                    delay: 0.1,
                });
            } else {
                gsap.to(svg, {
                    color: ACCENT,
                    duration: 0.4,
                    ease: "power2.out",
                });
                gsap.to(iris, {
                    opacity: 1,
                    duration: 0.3,
                    ease: "power2.out",
                    delay: 0.1,
                });
                gsap.to(irisX, {
                    opacity: 0,
                    duration: 0.25,
                    ease: "power2.in",
                });
            }
        },
        { dependencies: [incognito] },
    );

    return (
        <div
            ref={stageRef}
            className={
                "execlaw-mascot-stage" +
                (className ? " " + className : "")
            }
        >
            <svg
                ref={svgRef}
                className="execlaw-mascot"
                viewBox={`0 0 ${VIEWBOX} ${VIEWBOX}`}
                width={size}
                height={size}
                role="img"
                aria-label="execlaw"
                // 2026-05-21 — push the mascot 0.5rem down inside the
                // stage so the visible art sits closer to the
                // greeting baseline. Pairs with the negative margin
                // on `__text` below — the two together keep the
                // stage's total height unchanged while reducing the
                // perceived gap between mascot chin and greeting.
                style={{ marginTop: "0.5rem" }}
            >
                <path
                    ref={hairRef}
                    className="execlaw-mascot__hair"
                    d={HAIR_PATH}
                    fill="currentColor"
                />
                <path
                    ref={beardRef}
                    className="execlaw-mascot__beard"
                    d={BEARD_PATH}
                    fill="currentColor"
                />
                <g
                    ref={eyeOuterRef}
                    className="execlaw-mascot__eye-outer"
                >
                    <path
                        ref={eyePathRef}
                        d={EYE_PATH}
                        fill="currentColor"
                    />
                    <g ref={irisRef} className="execlaw-mascot__iris">
                        <path d={IRIS_PATH} />
                    </g>
                    {/* Incognito iris — an X cross stroked over the
                        eye in the same colour the iris blob uses
                        ($bg-deep), so it reads as "punched out" of
                        the eye in either colour state. Starts at
                        opacity 0; the incognito useGSAP fades it in
                        when the toggle activates. The two `<line>`
                        elements are centred on the iris rest
                        position (FALLBACK_IRIS_CX/Y) so the cross
                        sits exactly where the pupil would be. */}
                    <g
                        ref={irisXRef}
                        className="execlaw-mascot__iris-x"
                        opacity="0"
                    >
                        <line
                            x1={FALLBACK_IRIS_CX - 22}
                            y1={FALLBACK_IRIS_CY - 22}
                            x2={FALLBACK_IRIS_CX + 22}
                            y2={FALLBACK_IRIS_CY + 22}
                            stroke="var(--bs-body-bg, #0d1117)"
                            strokeWidth={14}
                            strokeLinecap="round"
                        />
                        <line
                            x1={FALLBACK_IRIS_CX - 22}
                            y1={FALLBACK_IRIS_CY + 22}
                            x2={FALLBACK_IRIS_CX + 22}
                            y2={FALLBACK_IRIS_CY - 22}
                            stroke="var(--bs-body-bg, #0d1117)"
                            strokeWidth={14}
                            strokeLinecap="round"
                        />
                    </g>
                </g>
            </svg>

            {/* Particles overlay — HTML divs so we can co-ordinate
                CSS-pixel positions with the HTML greeting line. */}
            <div
                className="execlaw-mascot-stage__particles"
                aria-hidden
            >
                {Array.from({ length: PARTICLE_COUNT }).map((_, i) => (
                    <div
                        key={i}
                        ref={(el) => {
                            particleRefs.current[i] = el;
                        }}
                        className="execlaw-mascot-stage__particle"
                    />
                ))}
            </div>

            <span
                ref={welcomeRef}
                className={
                    "execlaw-mascot-stage__text " +
                    `execlaw-mascot-stage__text--${fontVariant}`
                }
                data-testid="welcome-greeting-text"
                data-font={fontVariant}
            >
                {greeting}
            </span>
        </div>
    );
}
