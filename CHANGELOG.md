# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and this project follows [Semantic Versioning](https://semver.org/).

`Unreleased` collects work merged to `main` since the last tag; on release
it becomes the new version section and a fresh `Unreleased` is opened.

## [Unreleased]

## [5.2.5] — 2026-05-13

### Fixed
- **.deb package again contains the `hvac` binary.** When an explicit `assets`
  list is present, cargo-deb disables auto-detection entirely (it only runs when
  `assets` is omitted). Previous attempts either hardcoded `target/release/hvac`
  (which breaks cross-compiled builds) or removed the entry expecting auto-
  detection to kick in (which it doesn't). The fix is the explicit `["$auto"]`
  sentinel entry, which restores cargo-deb's built-in binary detection — correct
  target-triple path handling included — while preserving the custom asset
  entries for config, README, and LICENSE.

## [5.2.1] — 2026-05-12

### Fixed
- **apt repository now works.** `cargo deb` was silently producing no `.deb`
  because the `assets` entry hardcoded `target/release/hvac` (the native-build
  path) instead of letting cargo-deb auto-detect the cross-compiled binary at
  `target/<target>/release/hvac`. Added `name = "hvac"` to
  `[package.metadata.deb]` so the output filename matches what the release
  workflow expects (`hvac_*.deb`).

### Added
- **`Dockerfile` + `compose.example.yml`.** Two-stage Debian build with
  ffmpeg + VAAPI userland in the runtime image; non-root user matching
  the typical NAS admin UID/GID (1026:100) so files written back to
  bind mounts aren't root-owned. `intel-media-va-driver` gated on
  amd64 (no arm64 candidate). Tini as PID 1 forwards SIGINT cleanly
  through `docker run`.
- **`.github/workflows/docker.yml`.** Multi-arch (linux/amd64 +
  linux/arm64) build + publish to `ghcr.io/jackdanger/hvac`. main →
  `:main` + `:latest` + `:sha-<short>`; tags → `:<X.Y.Z>` + `:<X.Y>` +
  `:latest`. PR verify runs in a separate, read-only job; only the
  publish job gets `packages: write`.
- **`docs/NAS.md`.** Copy-pasteable per-platform instructions for
  Synology DSM (Container Manager + compose), QNAP QTS (Container
  Station), Unraid (Docker → Add Container + Intel-GPU-TOP /
  Nvidia-Driver plugins), OpenMediaVault (omv-extras compose plugin),
  TrueNAS SCALE (apps catalog + direct Docker), TrueNAS CORE (off-box
  only), plus the NFS/SMB off-box pattern for NAS hosts without a
  usable GPU.
- **README Docker + Troubleshooting sections.** Docker one-liners for
  Intel and NVIDIA (with `--user "$(id -u):$(id -g)"` for non-NAS
  hosts); FAQ covering the four questions every comment thread asks
  (no GPU, CPU encoding, will-it-touch-my-files, how-to-stop).

### Changed
- **`install.sh` recognises Synology, QNAP, Unraid, OpenMediaVault, and
  Alpine.** The previous "other Linux" branch named only Arch and
  Fedora; anyone running the one-liner on a NAS hit a dead end. Each
  appliance now gets a specific hint pointing at the actually-supported
  path (Docker on Synology/QNAP/Unraid, apt+omv-extras on OMV,
  `apk add` on Alpine).
- **Platform-aware "No GPU found" error.** Splits into macOS-specific
  (`brew install ffmpeg`), container-specific (the exact `--device` /
  `--gpus` flag), and generic-Linux branches. Inline tests pin the
  encoder names and the platform branch taken.
- **Bug-report template** asks how hvac is being run (native / Docker /
  NAS) up front and no longer assumes nvidia-smi is available for the
  GPU question.
- **`deny.toml` allows CDLA-Permissive-2.0.** `webpki-roots` 0.26.11+
  and 1.0.7 ship the Mozilla CA bundle under this SPDX-listed
  permissive license; the deny check fails without it.

## [5.2.0] — 2026-05-11

### Added
- **Pipeline refactor.** The 2,235-line `src/main.rs` is now 401 lines of
  orchestration. Phase-by-phase code lives in `src/pipeline/{scan,
  partition, worker, render, replace}.rs`; `Cli` and terminal display
  primitives moved to `src/{cli,ui}.rs`. Worker retry logic is now a
  typed `RetryDecision` state machine — `classify_failure(err_str,
  &state)` is pure and has 8 unit tests pinning the tier ordering.
- **Restored regressed features.** Three behaviours whose commit
  messages claimed they landed but whose source had been overwritten by
  earlier merges: multi-title DVD splitting (PR #11), AACS / BD+
  encrypted-disc skip (PR #21), and the completion-marker adopt gate
  (PR #17). All three are wired through the pipeline modules with
  regression tests.
- **Governance files.** `CONTRIBUTING.md`, `SECURITY.md`,
  `CODE_OF_CONDUCT.md`, and this `CHANGELOG.md`.
- **`install.sh` auto-installs ffmpeg + hardware acceleration** on
  macOS (via Homebrew) and Debian/Ubuntu (via apt). Picks up the VAAPI
  driver stack (`intel-media-va-driver` + `mesa-va-drivers` + `vainfo`)
  unconditionally so the script works in minimal containers without
  `pciutils`. NVIDIA's kernel driver is hinted at rather than
  auto-installed (kernel-module installs need reboots + conflict with
  Optimus / Tesla / container-host setups). Knobs: `HVAC_SKIP_FFMPEG=1`,
  `HVAC_ASSUME_YES=1`.
- **Post-install summary in `install.sh`** — probes
  `ffmpeg -encoders` for `hevc_nvenc` / `hevc_vaapi` / `hevc_videotoolbox`
  and reports which are compiled in, so a misconfigured host (e.g.
  RHEL-clone `ffmpeg-free` without nvenc) is surfaced at install time
  instead of on first run.

### Changed
- **`LdGuard` lifted to a top-level struct** with a `Drop` impl. The
  LaunchDarkly client + OTel exporter now flush on every `main()` exit
  path (early-return for `--dry-run` / empty scan, panics), not just the
  success path.
- **`scanner::detect_network_mount` is wired in.** Was dead code
  previously; now emits a one-line warning at scan start when the target
  path is on an NFS / SMB / CIFS mount.
- **README cleanup.** Dropped the "You also need `ffmpeg`…" line —
  `install.sh` handles it on supported platforms, and the "GPU
  required" matrix below still documents what each platform uses.

### Fixed
- **Pre-commit hook clarity.** README's Development section describes
  both fmt and clippy steps; the "warn once" wording in the hook itself
  corrected to "warn on every commit" (clippy missing is rare enough
  that we don't bother caching state).
- **`install.sh` install-fallback precedence.** The previous
  `install … || cp … && chmod …` parsed as `(install || cp) && chmod`;
  POSIX `set -e` is suppressed for non-final commands of an AND-OR
  list, so a `cp` failure did not exit the script and the trailing
  "Installed hvac" message printed over a missing binary. Now an
  explicit if / elif / else with `err()` on the fall-through.

## [5.1.1] — 2026-05-10

### Added
- **Clearer ffmpeg failure reporting.** `summarize_ffmpeg_error()` scans
  the full stderr stream and surfaces the root-cause line (e.g.
  `No wav codec tag found for codec pcm_dvd`) instead of the muxer's
  generic "Nothing was written into output file" cascade.
- **Audio re-encode auto-retry.** When `-c:a copy` produces a stream the
  target container can't accept (most commonly pcm_dvd into MKV),
  workers automatically retry with `-c:a aac`.

## [5.1.0] — 2026-05-10

### Added
- GA release: install.sh, .deb packaging, AUR publishing, name cleanup.

## [5.0.0] — 2026-04-?? *(pre-changelog, dates approximate)*

### Changed
- **`--overwrite` is the default** for non-disc-image sources;
  `--no-overwrite` writes `.transcoded.<ext>` siblings instead.
- **`--dry-run` previews** the plan without touching anything.
- ISO filename used for transcoded output instead of inner track name
  (`Movie.iso` → `Movie.transcoded.mkv`, not `00000.M2T.transcoded.mkv`).

## [4.1.0] — *(pre-changelog)*

### Added
- `nl_langinfo(CODESET)`-based locale detection (more reliable than env
  var sniffing).

## [4.0.0] — *(pre-changelog)*

### Added
- Direct ISO/IMG streaming to ffmpeg via the `isomage` crate (no
  on-disk extract step).

## [0.6.0] and earlier

Initial public releases. See `git log` for individual commits prior to
the changelog being established.

[Unreleased]: https://github.com/JackDanger/hvac/compare/v5.2.1...HEAD
[5.2.1]: https://github.com/JackDanger/hvac/compare/v5.2.0...v5.2.1
[5.2.0]: https://github.com/JackDanger/hvac/compare/v5.1.1...v5.2.0
[5.1.1]: https://github.com/JackDanger/hvac/compare/v5.1.0...v5.1.1
[5.1.0]: https://github.com/JackDanger/hvac/compare/v5.0.0...v5.1.0
[5.0.0]: https://github.com/JackDanger/hvac/compare/v4.1.0...v5.0.0
[4.1.0]: https://github.com/JackDanger/hvac/compare/v4.0.0...v4.1.0
[4.0.0]: https://github.com/JackDanger/hvac/compare/v0.6.0...v4.0.0
