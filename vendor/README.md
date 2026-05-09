# vendor/

Patched copies of third-party crates that the workspace needs but
that aren't yet fixed upstream. Each subdirectory is a snapshot of a
crate at a known version, with execlaw-specific patches applied
inline (look for `// execlaw vendoring fix:` markers in the source).

The workspace `Cargo.toml`'s `[patch.crates-io]` block redirects
`crates.io` resolutions to the local copies here.

## Current vendored crates

### `hardware-query` (snapshot of v0.2.1)

Source: <https://github.com/ciresnave/hardware-query> @ tag `v0.2.1`
(commit `b84e708e`).

Patch: `src/npu.rs` widens the `Command` import gate from
`#[cfg(target_os = "linux")]` to `#[cfg(unix)]` so the macOS-cfg-gated
`Command::new("sysctl")` call inside `detect_apple_neural_engine`
finds the symbol in scope. Without the patch, builds on macOS fail
with `error[E0433]: cannot find type Command in this scope`.

Drop the `[patch.crates-io]` entry once upstream cuts a release with
the fix (no 0.2.2 / 0.3.0 published as of 2026-05-09).

## Adding a vendored crate

1. Clone the upstream at the exact version tag into
   `vendor/<crate-name>/`.
2. Drop the `.git` and `.github` directories — they bloat the repo.
3. Drop benches / examples / tests / non-essential docs to keep the
   snapshot reviewable. Keep the LICENSE files (attribution).
4. Apply your fix inline with a `// execlaw vendoring fix:` comment
   that explains the bug + cites the upstream issue (if filed).
5. Add a `[patch.crates-io]` entry in the workspace `Cargo.toml`
   pointing to the local path.
6. Update this README with the crate, the source commit, and the
   reason for the patch.
7. File an issue / PR upstream so the fix lands in a future release
   and we can drop the vendoring.

## Why patch instead of fork-on-GitHub?

A fork would be the more permanent solution but requires owning a
separate repo with its own release cadence. Inline vendoring keeps
the patch reviewable in the same PR as the consumer change, and the
diff against the upstream snapshot is small enough to grok at a
glance.
