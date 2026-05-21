import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
// 2026-04-29 — IBM Plex Sans is the body font for agent chat
// responses + chat / page titles. Self-hosted via
// `@fontsource/ibm-plex-sans` so the SPA stays offline-capable
// (execlaw's grounding rule). The sidebar + brand intentionally
// keep the existing system stack — see `theme.scss` for the
// per-element font-family assignments.
import "@fontsource/ibm-plex-sans/400.css";
import "@fontsource/ibm-plex-sans/500.css";
import "@fontsource/ibm-plex-sans/600.css";
import "@fontsource/ibm-plex-sans/700.css";
// 2026-05-21 — Display-only faces for the welcome greeting on the
// new-chat view. MascotGreeting picks one at random per page load
// so the greeting feels fresh between sessions. All four are
// self-hosted (offline-capable) like the Plex body font above.
// Single weights only — these only render the time-of-day line.
import "@fontsource/unica-one/400.css";
import "@fontsource/orbitron/500.css";
import "@fontsource/antonio/400.css";
import "@fontsource/instrument-serif/400.css";
import "@fontsource/instrument-serif/400-italic.css";
import { App } from "./App";
import { initializeI18n } from "./i18n";
import "./styles/theme.scss";

const rootEl = document.getElementById("root");
if (!rootEl) {
    throw new Error("Could not find <div id=\"root\"> — index.html is malformed.");
}

// Kick off i18n boot — we don't await it. The initial render shows
// English defaults (correct fallback when `i18next.language` is still
// the default), and components subscribed via `useT()` re-render once
// the stored / browser-detected language resolves.
void initializeI18n().catch((err) => {
    console.error("Failed to initialize translations", err);
});

createRoot(rootEl).render(
    <StrictMode>
        <App />
    </StrictMode>,
);
