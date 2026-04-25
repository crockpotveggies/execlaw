import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
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
