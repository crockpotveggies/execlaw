import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Vite config for the execlaw SPA — plain React + GSAP, no native
// or react-native plumbing.
//
// In dev: Vite serves on 127.0.0.1:5173 and proxies /api → :3030
// (Rust server) so the SPA looks like a same-origin app and we don't
// need CORS. In prod (Phase 6c-d): the built bundle is embedded in
// the Rust binary via rust-embed; same-origin by construction.
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
                target: "http://127.0.0.1:3030",
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
});
