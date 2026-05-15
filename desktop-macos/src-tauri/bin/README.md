# Bundled CLI sidecar binary

The Tauri bundler reads `bundle.externalBin = ["bin/execlaw"]` from
`tauri.conf.json` and looks for a per-triple binary at build time —
specifically `bin/execlaw-aarch64-apple-darwin` for an Apple-Silicon
build.

That binary is **not** committed. It's produced by `cargo build
--release -p execlaw` in the root workspace and copied here by
`scripts/build-mac.sh`. The copy step is what lets the `.app`
bundle ship a self-contained server inside `Contents/MacOS/execlaw`.

If you run `cargo tauri build` directly without `build-mac.sh`,
you'll hit an error like:

```
binary not found: bin/execlaw-aarch64-apple-darwin
```

Run `scripts/build-mac.sh` instead (or copy the binary manually).
