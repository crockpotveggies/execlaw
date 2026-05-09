## Summary

<!-- One-paragraph "why this PR exists." -->

## Changes

<!-- Bulleted list of what changed. Cite file paths. -->

-

## Tested

<!-- Mark yes/no/n/a. CI will re-run these in the matrix. -->

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo build --workspace --locked`
- [ ] `cargo clippy --workspace --all-targets`
- [ ] `cargo test --workspace --locked`
- [ ] SPA: `cd web && npm test && npm run lint && npm run build`

## Checklist

<!-- All non-N/A items must be checked before merge. -->

- [ ] My commits follow the repo's `<scope>: <subject>` convention.
- [ ] I read `AGENTS.md` §2 (non-negotiable rules) and my change
      doesn't violate any of them.
- [ ] If I added a public API, I documented it (doc comment + at
      least one example).
- [ ] If I added a migration, it's the next sequential number and I
      did not edit any existing migration.
- [ ] If I added a host-side tool dispatch path, I added a test for
      the trust-class gating and the capability-set gating.
- [ ] If I changed a doc-claim (`README.md`, `docs/*.md`,
      `CHANGELOG.md`), I confirmed the new claim is grounded in
      code I've actually read.
- [ ] No emoji in code or commit messages.
- [ ] No cloud-LLM API on any code path (re-grep your diff).
- [ ] No hardcoded plugin id in `crates/` outside of tests /
      fixtures / doc-comment examples.

## Related

<!-- Linked issue numbers, related PRs, or "n/a". -->

Closes #
