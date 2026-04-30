// Vite dev server with the proxy aimed at the cargo-watch'd server
// on :3031 (Docker Desktop's vpnkit squats :3030 on Windows). Wraps
// `vite` instead of pre-pending `VITE_API_TARGET=…` so the same npm
// script works in PowerShell, cmd, and bash without `cross-env`.
//
// Windows note: Node 20+ refuses to spawn `.cmd` / `.bat` files
// without `shell: true` (CVE-2024-27980 fix — child_process won't
// resolve the shim transparently any more). We pass that flag on
// win32 so the `vite.cmd` shim resolves through cmd.exe as it
// always has on this platform.

import { spawn } from "node:child_process";
import process from "node:process";

const target = process.env.VITE_API_TARGET ?? "http://127.0.0.1:3031";
process.env.VITE_API_TARGET = target;

const isWindows = process.platform === "win32";
const vite = spawn(
    isWindows ? "vite.cmd" : "vite",
    process.argv.slice(2),
    { stdio: "inherit", env: process.env, shell: isWindows },
);

vite.on("exit", (code) => process.exit(code ?? 0));
process.on("SIGINT", () => vite.kill("SIGINT"));
process.on("SIGTERM", () => vite.kill("SIGTERM"));
