import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";

// Mirror the vite.config.ts react-native alias so the test runner
// resolves `react-native` to `react-native-web` (Reanimated 4 imports
// from `react-native` directly).
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
            {
                find: /^react-native$/,
                replacement: new URL(
                    "./src/shims/react-native.ts",
                    import.meta.url,
                ).pathname.replace(/^\/(\w):/, "$1:"),
            },
        ],
        extensions: [".web.tsx", ".web.ts", ".tsx", ".ts", ".jsx", ".js"],
    },
    define: {
        __DEV__: JSON.stringify(true),
        "process.env.NODE_ENV": JSON.stringify("test"),
    },
    test: {
        environment: "jsdom",
        globals: true,
        setupFiles: ["./src/__tests__/setup.ts"],
        css: false,
        // Test files only — exclude scripts and node_modules.
        include: ["src/**/*.test.{ts,tsx}"],
        server: {
            deps: {
                // RN-web ships ESM shapes that vitest's default
                // optimizer mis-detects as CJS without an explicit
                // inline.
                inline: ["react-native-web", "react-native-reanimated"],
            },
        },
    },
});
