# Tauri icon set

`tauri.conf.json` references `icons/icon.icns`. Generate the full
icon set from a single 1024×1024 PNG via:

```bash
cd desktop-macos/src-tauri
cargo tauri icon path/to/source.png
```

That command writes `icon.icns` here (plus several `Square*` PNGs
for Windows / Linux targets we don't currently ship).

The source PNG is **not** committed — drop one in temporarily,
generate the set, and only `icons/icon.icns` ends up in the
bundle. The `.gitignore` keeps the generated files untracked.

For the v1 release a placeholder solid-colour PNG is fine; replace
with the real mark before signing.
