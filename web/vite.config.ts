import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Vite config for the execlaw SPA.
//
// In dev: Vite serves on :5173 and proxies /api → :3030 (Rust server),
// so the SPA looks like a same-origin app and we don't need CORS.
// In prod (Phase 6c-d): the built bundle is embedded in the Rust
// binary via rust-embed; same-origin by construction.
export default defineConfig({
  plugins: [react()],
  resolve: {
    // Cross-platform component layer (§6 of MIGRATION_PLAN.md).
    // Today the web target uses react-bootstrap directly; native
    // targets land later with a parallel component layer (Tamagui /
    // similar). The alias is wired so any code that DOES write
    // `react-native` imports compiles to `react-native-web` on web —
    // we just don't pull react-native-web in until something needs it.
  },
  server: {
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
    // Phase-6 budget: keep the initial JS payload tight so cold loads
    // on bad connections still feel fast. The `npm run size` script
    // checks dist/assets size against an explicit ceiling.
    chunkSizeWarningLimit: 500,
  },
});
