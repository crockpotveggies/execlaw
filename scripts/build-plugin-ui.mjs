#!/usr/bin/env node
/**
 * Build a plugin's UI panel from TypeScript+JSX to a single
 * self-contained ES module.
 *
 * Each plugin that declares `[[ui_panels]]` ships `ui/panel.tsx` (or
 * `ui/panel.ts`) inside its source directory. This script compiles
 * it via esbuild to `ui/panel.js` in the same directory; the
 * resulting JS lands in the plugin's ZIP at package time so the
 * host's `GET /api/admin/plugins/{id}/ui/panel.js` static-asset
 * route can serve it.
 *
 * # Usage
 *
 *     node scripts/build-plugin-ui.mjs <plugin-id>
 *     node scripts/build-plugin-ui.mjs <plugin-id> --watch
 *     node scripts/build-plugin-ui.mjs --all
 *
 * Examples:
 *     node scripts/build-plugin-ui.mjs signal
 *     node scripts/build-plugin-ui.mjs signal --watch
 *     node scripts/build-plugin-ui.mjs --all
 *
 * `--watch` re-runs esbuild on every source change. Combined with
 * a symlinked stage dir (or a quick re-install), this gives
 * plugin authors a hot-reload loop: edit `ui/panel.tsx`, save,
 * navigate away+back in the SPA, see the new panel — no
 * execlaw restart, no SPA rebuild.
 *
 * `--all` builds every plugin whose source tree contains a
 * `ui/panel.tsx`. Used by CI + by the dist-packaging step before
 * each plugin's ZIP is assembled.
 *
 * # What esbuild does
 *
 *   * Compiles `.tsx`/`.ts` → ESM JS (target: ES2022).
 *   * Erases `import type` declarations (which is how plugins reach
 *     `@execlaw/plugin-ui` for types without a runtime dep).
 *   * Marks `react`, `react-dom`, `@execlaw/plugin-ui` as
 *     externals. The plugin's source should not import these at
 *     runtime — React comes from the bridge passed via props
 *     (`bridge.React`) and the types from `@execlaw/plugin-ui`
 *     are type-only. Marking them external is defensive: if a
 *     plugin author accidentally writes `import React from "react"`,
 *     esbuild leaves the import in the output and the dynamic
 *     loader fails with a clear "cannot resolve module" rather
 *     than silently bundling a second React copy.
 *   * JSX is compiled via the classic transform with
 *     `React.createElement` as the factory. Plugin authors put a
 *     `const React = globalThis.execlawHost.React` (or destructure
 *     from `props.bridge`) at module scope so the factory resolves.
 *   * Source map emitted next to the output. The host's
 *     static-asset route already serves `.js.map` with the right
 *     Content-Type.
 *
 * # Externals contract
 *
 * The plugin's output JS MUST NOT contain any unresolved imports.
 * If you see esbuild warn about an external being left in the
 * output, your plugin source imports something it shouldn't.
 * Audit and remove. The bridge is the only runtime surface;
 * everything else is type-only.
 */

import * as esbuild from "esbuild";
import { existsSync, readdirSync, statSync } from "node:fs";
import { join, dirname, resolve, relative } from "node:path";
import { fileURLToPath } from "node:url";

const __filename = fileURLToPath(import.meta.url);
const REPO_ROOT = resolve(dirname(__filename), "..");
const PLUGINS_DIR = join(REPO_ROOT, "plugins");

const args = process.argv.slice(2);
const WATCH = args.includes("--watch");
const ALL = args.includes("--all");
const positional = args.filter((a) => !a.startsWith("--"));

if (!ALL && positional.length !== 1) {
    console.error(
        "usage: node scripts/build-plugin-ui.mjs <plugin-id> [--watch]",
    );
    console.error("       node scripts/build-plugin-ui.mjs --all");
    process.exit(2);
}

/** Plugin IDs that ship a `ui/panel.tsx` source. */
function discoverPluginsWithPanels() {
    const out = [];
    for (const entry of readdirSync(PLUGINS_DIR)) {
        if (entry.startsWith("_") || entry.startsWith(".")) continue;
        const dir = join(PLUGINS_DIR, entry);
        if (!statSync(dir).isDirectory()) continue;
        if (existsSync(join(dir, "ui", "panel.tsx"))) out.push(entry);
        else if (existsSync(join(dir, "ui", "panel.ts"))) out.push(entry);
    }
    return out.sort();
}

const targets = ALL ? discoverPluginsWithPanels() : positional;
if (ALL && targets.length === 0) {
    console.log("no plugin under plugins/*/ui/panel.tsx — nothing to build");
    process.exit(0);
}

/** Resolve the actual source file (tsx preferred over ts). */
function entryFor(pluginId) {
    const dir = join(PLUGINS_DIR, pluginId, "ui");
    const tsx = join(dir, "panel.tsx");
    const ts = join(dir, "panel.ts");
    if (existsSync(tsx)) return tsx;
    if (existsSync(ts)) return ts;
    throw new Error(
        `plugin '${pluginId}' has no ui/panel.tsx or ui/panel.ts under ${dir}`,
    );
}

/** esbuild config shared across all plugins. */
function configFor(pluginId) {
    const entry = entryFor(pluginId);
    const outfile = join(PLUGINS_DIR, pluginId, "ui", "panel.js");
    return {
        entryPoints: [entry],
        outfile,
        bundle: true,
        format: "esm",
        target: "es2022",
        platform: "browser",
        sourcemap: true,
        // Classic JSX transform pointing at the module-scoped
        // `React` const each plugin pulls from the bridge.
        // Matches `plugins/_shared/tsconfig.plugin.json`.
        jsx: "transform",
        jsxFactory: "React.createElement",
        jsxFragment: "React.Fragment",
        // The bridge surface MUST be unbundled. If a plugin
        // accidentally imports any of these at runtime, the
        // resulting output won't resolve and the dynamic loader
        // will surface a clear error.
        external: ["react", "react-dom", "@execlaw/plugin-ui"],
        // Minify in non-watch mode. Watch mode keeps the output
        // readable so the plugin author can debug.
        minify: !WATCH,
        legalComments: "none",
        logLevel: "info",
    };
}

async function buildOnce(pluginId) {
    const cfg = configFor(pluginId);
    const rel = relative(REPO_ROOT, cfg.outfile).replace(/\\/g, "/");
    console.log(`[build-plugin-ui] ${pluginId} → ${rel}`);
    await esbuild.build(cfg);
}

async function watchOne(pluginId) {
    const cfg = configFor(pluginId);
    const ctx = await esbuild.context(cfg);
    await ctx.watch();
    const rel = relative(REPO_ROOT, cfg.outfile).replace(/\\/g, "/");
    console.log(`[build-plugin-ui] watching ${pluginId} → ${rel}`);
    // Hold the process alive for the watcher.
    process.stdin.resume();
}

if (WATCH) {
    if (targets.length !== 1) {
        console.error("--watch requires exactly one plugin id");
        process.exit(2);
    }
    await watchOne(targets[0]);
} else {
    for (const t of targets) {
        await buildOnce(t);
    }
}
