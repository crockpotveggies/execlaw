import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Vite config for the execlaw SPA.
//
// In dev: Vite serves on :5173 and proxies /api → :3030 (Rust server),
// so the SPA looks like a same-origin app and we don't need CORS.
// In prod (Phase 6c-d): the built bundle is embedded in the Rust
// binary via rust-embed; same-origin by construction.
//
// React-Native interop: react-native-reanimated 4 powers our
// screen-transition animations and imports from `react-native`. On
// the web target we redirect those to `react-native-web` so the
// Flow-annotated upstream RN sources never reach the bundler. The
// reanimated babel plugin is wired through @vitejs/plugin-react so
// worklets compile correctly in dev + prod.
export default defineConfig({
  plugins: [
    react({
      babel: {
        plugins: ["react-native-reanimated/plugin"],
      },
    }),
  ],
  resolve: {
    alias: [
      // Cross-platform component layer (§6 of MIGRATION_PLAN.md):
      // every `react-native` import resolves to a small shim that
      // re-exports react-native-web AND fills in the TurboModule /
      // NativeModules surfaces Reanimated 4 looks for at boot.
      // iOS/Android targets land later with the real `react-native`
      // package (and this alias drops away).
      {
        find: /^react-native$/,
        replacement: new URL("./src/shims/react-native.ts", import.meta.url)
          .pathname.replace(/^\/(\w):/, "$1:"),
      },
    ],
    // react-native-web ships dual ESM/CJS; tell Vite to prefer the
    // browser/web exports condition so we never accidentally pull
    // node-flavored modules.
    extensions: [".web.tsx", ".web.ts", ".tsx", ".ts", ".jsx", ".js"],
  },
  optimizeDeps: {
    // react-native-web pulls in CJS sub-deps (notably
    // `@react-native/normalize-colors`) that the browser ESM loader
    // can't import directly — they need esbuild's CJS-to-ESM wrap.
    // Force pre-bundle for the entire RN-web tree so those sub-deps
    // get wrapped. We omit `react-native` (upstream, Flow-annotated)
    // and `react-native-reanimated` from `include` because the
    // runtime alias redirects `react-native` to our shim and esbuild
    // doesn't honor the regex alias during pre-bundle.
    include: ["react-native-web"],
    exclude: [
      "react-native",
      "react-native-reanimated",
      "react-native-worklets",
    ],
    esbuildOptions: {
      // Some upstream RN-ecosystem sources are CJS with default
      // exports. Tell esbuild to unwrap them automatically.
      mainFields: ["browser", "module", "main"],
    },
  },
  define: {
    // react-native-web reads these at module load.
    __DEV__: JSON.stringify(process.env.NODE_ENV !== "production"),
    "process.env.NODE_ENV": JSON.stringify(process.env.NODE_ENV ?? "development"),
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
