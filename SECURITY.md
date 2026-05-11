# Security policy

## Supported versions

Only the current minor release line on `main` receives security updates.
hvac is a single-binary tool with a small public surface — older tagged
releases are kept around for reproducibility but are not patched in place.

## Reporting a vulnerability

**Do not open a public GitHub issue for security-sensitive reports.**

Email **jack@jackdanger.com** with the subject line `[hvac security]` and
include:

- A description of the vulnerability and the conditions under which it
  fires (versions affected, GPU vendor, container, source-file type, etc.).
- The smallest reproducer you can manage.
- Whether you'd like credit in the release notes, and how to spell your
  name / handle.

We'll acknowledge receipt within **3 working days**. A fix is targeted
within **14 days** for high-severity issues (remote code execution,
filesystem escape, denial of service against shared media stores); lower-
severity issues are batched into the next regular release.

If you don't get an acknowledgement in 3 days, please nudge — we'd rather
get a second email than have a report slip through the cracks.

## Threat model

The realistic threats hvac exposes:

- **Filesystem.** hvac runs with the invoking user's permissions and reads
  paths from `argv`. It writes `.hvac_tmp_*`, `.hvac_writable_check_*`, and
  `.transcoded.<ext>` files in the source's parent or in `--output-dir`. It
  invokes `ffmpeg` and `ffprobe` via `Command` with file paths as arguments
  — these are passed as separate `argv` entries, never shell-interpolated.
- **GPU.** NVENC / VAAPI / VideoToolbox are user-facing APIs; hvac doesn't
  do anything privileged beyond invoking them.
- **LaunchDarkly remote control** (opt-in via `--launchdarkly-sdk-key`).
  The SDK key is **CLI-only**. hvac never reads it from an environment
  variable, so a leaked shell rc can't silently expose your run to remote
  pause/kill. See the "Controlling resource usage during multi-day
  transcodes" section in the README for the rationale.
- **OpenTelemetry export** (active when `--launchdarkly-sdk-key` is set):
  per-run spans tagged with hostname, username, and GPU info are sent to
  `otel.observability.app.launchdarkly.com`. Omit the SDK key to disable.

## Out of scope

- **ffmpeg / ffprobe** vulnerabilities are upstream issues. Report them to
  the ffmpeg project. hvac will absorb upstream fixes through your distro's
  package update mechanism.
- **GPU driver** vulnerabilities are upstream issues. Same channel.
- **Crashes in malformed disc images.** hvac uses the
  [isomage](https://github.com/JackDanger/isomage) crate to read ISOs; a
  crash there should be filed against isomage. A panic in hvac's own
  parsing code on a malformed ISO is in-scope and worth a report.

## Disclosure

We coordinate disclosure with the reporter. Default behaviour: hold details
until a release with the fix is published, then credit the reporter in the
CHANGELOG and (optionally) the release notes.
