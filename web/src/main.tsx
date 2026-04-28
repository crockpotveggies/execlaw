import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
// 2026-04-28 — Fira Sans is the body font for agent chat responses
// + chat / page titles. Self-hosted via `@fontsource/fira-sans` so
// the SPA stays offline-capable (execlaw's grounding rule). The
// sidebar + brand intentionally keep the existing system stack —
// see `theme.scss` for the per-element font-family assignments.
import "@fontsource/fira-sans/400.css";
import "@fontsource/fira-sans/500.css";
import "@fontsource/fira-sans/600.css";
import "@fontsource/fira-sans/700.css";
import { App } from "./App";
import "./styles/theme.scss";

const rootEl = document.getElementById("root");
if (!rootEl) {
    throw new Error("Could not find <div id=\"root\"> — index.html is malformed.");
}

createRoot(rootEl).render(
    <StrictMode>
        <App />
    </StrictMode>,
);
