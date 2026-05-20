// Bundle-size budget enforcer (axiom #14 for the SPA).
//
// Run after `npm run build`. Sums every file under dist/ and dist/assets/,
// then checks against the explicit ceilings below. Fails the CI step
// (non-zero exit) if any threshold is breached.
//
// Tighten these as the SPA grows and we trim dead deps. The point is
// to catch accidental balloons (e.g. a heavyweight icon font) BEFORE
// they ship, not to police the absolute size to the kilobyte.

import { readdir, stat } from "node:fs/promises";
import path from "node:path";

const DIST = path.resolve("dist");
const BUDGETS_BYTES = {
    // User-facing payload only — `.map` files are excluded because
    // sourcemaps don't ship as part of the cold-load path; they're
    // dev-tool artifacts shipped alongside for debugging. Currently
    // ~2.42 MB: ~1.16 MB JS (see js budget below) + ~360 KB CSS +
    // ~310 KB bootstrap-icons fonts + ~600 KB IBM Plex Sans variants
    // (every weight × every subset × woff/woff2). Trim @fontsource
    // subsets to bring this down — that's the cheapest win once we
    // cross 2.85 MB.
    total: 2850 * 1024,
    // JS only (the cold-load critical path). Currently ~1.16 MB
    // across the main app chunk + vendor splits + lazy locale
    // bundles. Tighten via tree-shaking / route-level code-splitting
    // before raising this further. History of raises:
    //   * 900 KB → 1100 KB to absorb the Automations milestone
    //     (M4c ReactFlow canvas + M5 InferencePage observability +
    //     agent-drafted suggestions surface).
    //   * 1100 KB → 1250 KB to absorb (a) the i18n locale bundles
    //     (7 lazy JSON chunks × ~8 KB ≈ 56 KB) and (b) the vendor
    //     split introduced in vite.config.ts — splitting the
    //     single 1.14 MB main bundle into react/bootstrap/xyflow/
    //     gsap/markdown vendor chunks adds ~30 KB of rolldown
    //     bookkeeping but eliminates the build's chunk-size
    //     warning AND gives slow-link operators incremental
    //     updates (code changes don't bust the vendor cache).
    //     Net-neutral on first-load bytes — same modules, sliced
    //     into more cacheable units. Route-level
    //     `React.lazy(() => import("./routes/Automations"))` is
    //     the next lever once we want to actually shrink this.
    js: 1250 * 1024,
    // CSS only. Bootstrap + bootstrap-icons together land near 300 KB
    // out-of-the-box; the budget is cushioned to ~400 KB so we notice
    // when we've added ANOTHER 100 KB of CSS — at that point we should
    // reach for tree-shaking or a slimmer theme.
    css: 400 * 1024,
};

async function walk(dir) {
    const out = [];
    let entries;
    try {
        entries = await readdir(dir, { withFileTypes: true });
    } catch (e) {
        if (e.code === "ENOENT") {
            console.error(`bundle-size: ${dir} not found — run 'npm run build' first.`);
            process.exit(2);
        }
        throw e;
    }
    for (const e of entries) {
        const full = path.join(dir, e.name);
        if (e.isDirectory()) out.push(...(await walk(full)));
        else {
            const s = await stat(full);
            out.push({ path: full, bytes: s.size });
        }
    }
    return out;
}

function fmt(bytes) {
    if (bytes < 1024) return `${bytes} B`;
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
    return `${(bytes / 1024 / 1024).toFixed(2)} MB`;
}

const files = await walk(DIST);
// Exclude `.map` files everywhere — they're debug artifacts, not part
// of the cold-load critical path users actually fetch.
const shipping = files.filter((f) => !f.path.endsWith(".map"));
const total = shipping.reduce((n, f) => n + f.bytes, 0);
const js = shipping
    .filter((f) => f.path.endsWith(".js"))
    .reduce((n, f) => n + f.bytes, 0);
const css = shipping
    .filter((f) => f.path.endsWith(".css"))
    .reduce((n, f) => n + f.bytes, 0);

console.log("execlaw SPA bundle:");
console.log(`  total : ${fmt(total)} (budget ${fmt(BUDGETS_BYTES.total)})`);
console.log(`  js    : ${fmt(js)} (budget ${fmt(BUDGETS_BYTES.js)})`);
console.log(`  css   : ${fmt(css)} (budget ${fmt(BUDGETS_BYTES.css)})`);

let ok = true;
if (total > BUDGETS_BYTES.total) {
    console.error(`FAIL total over budget: ${fmt(total)} > ${fmt(BUDGETS_BYTES.total)}`);
    ok = false;
}
if (js > BUDGETS_BYTES.js) {
    console.error(`FAIL js over budget: ${fmt(js)} > ${fmt(BUDGETS_BYTES.js)}`);
    ok = false;
}
if (css > BUDGETS_BYTES.css) {
    console.error(`FAIL css over budget: ${fmt(css)} > ${fmt(BUDGETS_BYTES.css)}`);
    ok = false;
}
process.exit(ok ? 0 : 1);
