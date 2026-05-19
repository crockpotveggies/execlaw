# Bundled CLI sidecar binary

The Tauri bundler reads `bundle.externalBin = ["bin/execlaw"]` from
`tauri.conf.json` and looks for a per-triple binary at build time —
specifically `bin/execlaw-x86_64-pc-windows-msvc.exe` for an x64
Windows build.

That binary is **not** committed. It's produced by `cargo build
--release -p execlaw --target x86_64-pc-windows-msvc` in the root
workspace and copied here by `scripts/build-windows.ps1`. The copy
step is what lets the NSIS installer ship a self-contained server
inside `$INSTDIR\execlaw.exe`.

If you run `cargo tauri build` directly without
`build-windows.ps1`, you'll hit an error like:

```
binary not found: bin/execlaw-x86_64-pc-windows-msvc.exe
```

Run `scripts/build-windows.ps1` instead (or copy the binary manually).
