# Bundled CLI sidecar binary

The Tauri bundler reads `bundle.externalBin = ["bin/execlaw"]` from
`tauri.conf.json` and looks for a per-triple binary at build time —
specifically `bin/execlaw-x86_64-unknown-linux-gnu` for an x64
Linux build.

That binary is **not** committed. It's produced by `cargo build
--release -p execlaw --target x86_64-unknown-linux-gnu` in the
root workspace and copied here by `scripts/build-linux.sh`. The
copy step is what lets the .deb ship a self-contained server at
`/usr/bin/execlaw`.

If you run `cargo tauri build` directly without
`build-linux.sh`, you'll hit an error like:

```
binary not found: bin/execlaw-x86_64-unknown-linux-gnu
```

Run `scripts/build-linux.sh` instead (or copy the binary manually).
