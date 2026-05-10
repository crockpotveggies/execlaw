/// <reference types="vite/client" />

// Side-effect imports of stylesheet subpaths (e.g.
// `@fontsource/ibm-plex-sans/400.css`) need an ambient module
// declaration under TS 6+: the host's package types don't cover
// per-weight subpaths, and the bundler resolves them at build
// time as inert CSS modules. The `*.css` / `*.scss` shapes mirror
// what `vite/client` already publishes for top-level imports.
declare module "*.css";
declare module "*.scss";
