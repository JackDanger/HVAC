# Contributing to hvac

Thanks for considering a contribution. The project is small enough that one
person can keep the whole thing in their head — please help us keep it that
way. The guidelines below are short for a reason; they're the ones that
actually matter day-to-day.

## Quick start

```bash
git clone https://github.com/JackDanger/hvac
cd hvac
git config core.hooksPath .githooks    # fmt + clippy on every commit
cargo build
cargo test
```

The pre-commit hook runs `cargo fmt --all -- --check` and
`cargo clippy -- -D warnings`. Both are also enforced by CI.
`HVAC_SKIP_CLIPPY=1` skips the slow check for fix-up commits;
`--no-verify` skips both (use sparingly).

## Where the code lives

```
src/main.rs          — thin orchestrator
src/cli.rs           — Cli struct (clap derive API; no env var reads)
src/ui.rs            — terminal symbols + display helpers
src/pipeline/        — Phase 1–4 of the transcode flow:
    scan.rs            paths → ScanItems (ISO expansion)
    partition.rs       probe + filter → WorkItems
    worker.rs          encode worker + typed RetryDecision state machine
    render.rs          progress UI + auto-ramp driver
    replace.rs         optional --replace pass
src/{config,gpu,iso,probe,scanner,transcode,util}.rs  — domain modules
src/{flags,setup,telemetry}.rs                         — LaunchDarkly wiring
```

`docs/ARCHITECTURE.md` walks through the end-to-end flow for newcomers.

## Pull-request checklist

1. **One topic per PR.** Tag refactors, behaviour changes, and doc updates
   into separate PRs when you can.
2. **Tests for behaviour changes.** Inline `#[cfg(test)]` modules live in
   the same file as the code under test. Cross-file flow is exercised by
   integration tests under `tests/`.
3. **No new `cargo` dependencies without a one-line `why` in the PR body.**
   This project is single-binary; every dep is also in every distro
   package, every install.sh download, every container image.
4. **CI green** (fmt + clippy + tests on Ubuntu and macOS). Local tests
   pass on a stable Rust toolchain; the remote NVENC host is the
   integration environment.
5. **No `cargo` runs locally** if you're touching GPU code paths — those
   are tested on the remote host via `./deploy.sh test`. See
   `.claude/rules/project.md` for the deploy mechanics.

## Style

- Match the surrounding code. Comment density, identifier conventions, error
  message style — read the neighbours before inventing your own.
- Public items get a `///` doc comment. The first sentence should make
  sense on its own (it shows up in the rustdoc index).
- Errors with `anyhow::Result<T>` in the binary; `thiserror` for library
  error types if you add any.
- `log` macros, not `println!` / `eprintln!` for diagnostics. Reserve
  `eprintln!` for user-facing status lines (the run summary, skip
  reasons, etc.).
- Prefer plain functions to traits when there's only one implementation.

## Reporting bugs

[Open an issue](https://github.com/JackDanger/hvac/issues/new/choose) with
the bug-report template. Please include:

- `hvac --version` output (we print the git SHA).
- The exact command you ran.
- `RUST_LOG=debug` output if you can capture it.
- For transcode failures: the relevant ffmpeg stderr lines (hvac surfaces
  these in the error message when possible).

For security-sensitive reports, see [SECURITY.md](SECURITY.md) — don't open
a public issue first.

## Release flow

Maintainers tag a release with `git tag -a vX.Y.Z -m "..."` and `git push
--tags`. The release workflow:

1. Builds binaries for Linux x86_64/aarch64 and macOS x86_64/aarch64.
2. Publishes a GitHub release with tarballs + `.deb` packages.
3. Updates the Homebrew tap formula.
4. Bumps the AUR PKGBUILD.
5. Publishes `hvac-transcoder` to crates.io.

`CHANGELOG.md` follows [Keep a Changelog](https://keepachangelog.com/);
update it in the same PR as the change.

## License

By contributing you agree your work is licensed under the
[MIT License](LICENSE).
