import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Vite config for the execlaw SPA — plain React + GSAP, no native
// or react-native plumbing.
//
// In dev: Vite serves on 127.0.0.1:5173 and proxies /api → the Rust
// server (default 127.0.0.1:3031, set in `crates/cli/src/service.rs`
// SERVICE_BIND so dev-server, prod service, and the Vite proxy all
// agree on the same port). The proxy target is overridable via
// `VITE_API_TARGET` for one-off ports, e.g.:
//   cargo run -p execlaw -- serve --bind 127.0.0.1:9000 --no-encrypt
//   VITE_API_TARGET=http://127.0.0.1:9000 npm run dev
//
// In prod the built bundle is embedded in the Rust binary via
// rust-embed; same-origin by construction.
const API_TARGET = process.env.VITE_API_TARGET ?? "http://127.0.0.1:3031";

export default defineConfig({
    plugins: [react()],
    server: {
        // Bind explicitly to IPv4 loopback. Vite's default `localhost`
        // resolves to `[::1]` on Windows, which Chrome won't reach
        // when it tries 127.0.0.1 first → ERR_CONNECTION_REFUSED.
        host: "127.0.0.1",
        port: 5173,
        strictPort: true,
        proxy: {
            "/api": {
                target: API_TARGET,
                changeOrigin: false,
                ws: true,
            },
        },
    },
    build: {
        outDir: "dist",
        sourcemap: true,
        target: "es2022",
        // Phase-6 budget: keep the initial JS payload tight so cold
        // loads on bad connections still feel fast. The
        // `npm run size` script checks dist/assets size against an
        // explicit ceiling.
        chunkSizeWarningLimit: 500,
    },
    css: {
        preprocessorOptions: {
            scss: {
                // Sass 1.77+ defaults the legacy JS API and started
                // emitting a deprecation warning for it. Vite still
                // wires through the legacy API by default; opt into
                // the modern compiler so the warning goes away and
                // we get the (much faster) sass-embedded path.
                api: "modern-compiler",
                // Bootstrap 5.3 still uses `@import`, the old global
                // colour functions (`red()`/`green()`/`blue()`/
                // `mix()`/`unit()`), and ships rules where plain
                // declarations follow nested rules. Those won't be
                // fixed until Bootstrap 6. Silence those specific
                // deprecation channels so a real new warning in our
                // own SCSS still surfaces in the dev-server log.
                silenceDeprecations: [
                    "import",
                    "global-builtin",
                    "color-functions",
                    "mixed-decls",
                    "legacy-js-api",
                ],
            },
        },
    },
});
