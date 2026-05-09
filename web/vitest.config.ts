import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";

export default defineConfig({
    plugins: [react()],
    test: {
        environment: "jsdom",
        globals: true,
        setupFiles: ["./src/__tests__/setup.ts"],
        css: false,
        // Test files only — exclude scripts and node_modules.
        include: ["src/**/*.test.{ts,tsx}"],
    },
});
