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
    // Total dist contents — generous because it includes sourcemaps.
    total: 4 * 1024 * 1024,
    // JS only (the cold-load critical path).
    js: 750 * 1024,
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
const total = files.reduce((n, f) => n + f.bytes, 0);
const js = files
    .filter((f) => f.path.endsWith(".js") && !f.path.endsWith(".map"))
    .reduce((n, f) => n + f.bytes, 0);
const css = files
    .filter((f) => f.path.endsWith(".css") && !f.path.endsWith(".map"))
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
