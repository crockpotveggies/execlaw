// Vite dev server with the proxy aimed at the cargo-watch'd server
// on :3031 (Docker Desktop's vpnkit squats :3030 on Windows). Wraps
// `vite` instead of pre-pending `VITE_API_TARGET=…` so the same npm
// script works in PowerShell, cmd, and bash without `cross-env`.

import { spawn } from "node:child_process";
import process from "node:process";

const target = process.env.VITE_API_TARGET ?? "http://127.0.0.1:3031";
process.env.VITE_API_TARGET = target;

const vite = spawn(
    process.platform === "win32" ? "vite.cmd" : "vite",
    process.argv.slice(2),
    { stdio: "inherit", env: process.env },
);

vite.on("exit", (code) => process.exit(code ?? 0));
process.on("SIGINT", () => vite.kill("SIGINT"));
process.on("SIGTERM", () => vite.kill("SIGTERM"));
