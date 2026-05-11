<!--
Thanks for the PR. Please complete the sections below; remove the ones
that don't apply.
-->

## What this changes

<!-- One or two sentences. The "why" matters more than the "what". -->

## Behaviour change?

<!--
[ ] No (refactor, doc, test-only)
[ ] Yes — describe the user-visible effect and update CHANGELOG.md under
    [Unreleased] in the same PR.
-->

## How I tested it

<!--
- cargo test (always)
- ./deploy.sh test on the NVENC host (if you touched encode paths)
- A specific test case I ran manually:
-->

## Anything reviewers should look at first?

<!-- Pointer to the gnarliest function, the suspicious test, the
     unresolved trade-off, the breaking-change candidate. -->

## Checklist

- [ ] `cargo fmt --all` is clean (pre-commit hook enforces this).
- [ ] `cargo clippy -- -D warnings` is clean.
- [ ] `cargo test --bin hvac` passes locally.
- [ ] New behaviour has tests; bug fixes have regression tests.
- [ ] `CHANGELOG.md` updated if user-visible.
- [ ] Public items have `///` doc comments.
- [ ] No new dependencies, or the PR body says why one was added.
