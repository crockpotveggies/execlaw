// Shared mascot SVG path data + cursor-tracking constants.
//
// Extracted from `MascotGreeting.tsx` so other surfaces (the
// `ToolActivityPill`'s mini spinner, future avatar slots, etc.)
// can render the same character without depending on the heavy
// greeting timeline. MascotGreeting still owns the canonical
// docs on what each path represents — see its top-of-file
// comment for the LAYOUT / SEQUENCE notes.

/** Square viewBox edge length the SVG paths are authored for. */
export const VIEWBOX = 1254;
/** Centre coordinate (both X and Y) inside the viewBox. */
export const CANVAS_CENTER = VIEWBOX / 2;

/** Beard subpath — the lower half of the mascot, originally the
 *  start of the source's chained `m` path. */
export const BEARD_PATH =
    "M 654.15557,975.88339 c -38.88731,-8.46697 -68.44901,-38.61953 -102.94573,-56.92215 -32.49091,-24.96176 -77.36552,-37.20328 -101.56163,-70.94735 -5.68076,-31.92277 20.16397,-59.61023 34.40601,-86.31057 18.23629,-31.60889 57.05504,-61.28168 94.5227,-43.33332 33.00314,14.5489 61.97447,54.11906 101.87893,38.96376 35.09898,-15.10221 65.67858,-56.09129 108.05236,-40.97961 40.91001,16.35199 60.53646,60.40367 79.58094,97.20202 24.23432,36.22277 -18.02399,60.74167 -45.144,74.89116 -46.26156,27.52673 -90.26734,59.30025 -138.00272,84.05777 -9.90737,2.59313 -20.50639,5.652 -30.78686,3.37829 z";

/** Hair subpath — the upper half. Absolute start computed from
 *  the source's chained `m` commands:
 *  (654.15557 − 290, 975.88339 − 184.69708). */
export const HAIR_PATH =
    "M 364.15557,791.18631 c -23.62607,-21.7223 -32.45073,-56.71081 -43.2739,-86.49509 -40.53572,-128.57979 4.48805,-276.92121 106.89777,-363.85884 104.95089,-96.23256 271.0206,-118.34118 396.19401,-49.34125 129.0904,66.96607 209.13815,217.75556 187.52145,362.30502 -6.1483,47.26262 -22.71535,93.17983 -47.83049,133.62639 -32.48229,9.34943 -14.78793,-49.99715 -30.83962,-68.78839 -10.11855,-34.51325 -37.1563,-58.81949 -61.42363,-83.46046 -22.86972,-32.68484 -6.88528,-75.51731 -20.96525,-111.40497 -25.15487,-98.03949 -134.1298,-165.60464 -233.0275,-139.44942 -86.40582,19.0432 -151.14463,101.54579 -152.01106,189.50444 3.11626,32.19608 -8.33193,65.19195 -35.8964,83.70425 -35.99289,32.02096 -49.2266,81.00689 -53.29136,127.3193 -1.86051,4.68904 -7.3054,6.97853 -12.05402,6.33902 z";

/** Eye outline. Filled with currentColor in normal state; left
 *  alone (no animation) in the spinner's rotating outer group. */
export const EYE_PATH =
    "M 643.65557,699.75475 c -66.42845,-8.76517 -115.89555,-78.66217 -100.68539,-144.29273 11.11596,-69.61313 90.16755,-116.82716 156.72805,-92.64361 73.17109,21.45853 106.26577,117.63314 65.07339,180.71597 -24.51852,41.12465 -74.0613,63.09685 -121.11605,56.22037 z";

/** Iris (pupil). Mouse-tracked — translates within
 *  IRIS_MAX_TRAVEL_VB viewBox units relative to the resting
 *  centre. */
export const IRIS_PATH =
    "M 717.89431,552.74422 c 47.70688,-26.59709 -27.14173,-79.76534 -38.24866,-29.23118 -5.16587,21.56539 18.8456,39.35316 38.24866,29.23118 z";

/** Maximum viewBox-units the iris travels off-centre when the
 *  cursor is far from the mascot. */
export const IRIS_MAX_TRAVEL_VB = 50;

/** Pixel distance at which iris travel saturates at the maximum.
 *  Linear ramp 0 → MAX from 0 → SATURATION pixels. */
export const IRIS_SATURATION_PX = 360;

/** Resting centre of the eye outline + iris — used as a
 *  fallback when `getBBox()` isn't available (jsdom). */
export const FALLBACK_EYE_CX = 653;
export const FALLBACK_EYE_CY = 581;
export const FALLBACK_IRIS_CX = 703;
export const FALLBACK_IRIS_CY = 553;

/** Read the centre of an SVG element. Falls back to hardcoded
 *  coordinates when getBBox isn't usable (test environments,
 *  display: none ancestors). */
export function readCenter(
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
        return { cx: b.x + b.width / 2, cy: b.y + b.height / 2 };
    } catch {
        return { cx: fallbackCx, cy: fallbackCy };
    }
}
