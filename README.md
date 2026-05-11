# HVAC — Get your TBs back

[![CI](https://github.com/JackDanger/hvac/actions/workflows/ci.yml/badge.svg)](https://github.com/JackDanger/hvac/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/hvac-transcoder.svg)](https://crates.io/crates/hvac-transcoder)
[![docs.rs](https://img.shields.io/docsrs/hvac-transcoder.svg)](https://docs.rs/hvac-transcoder)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Point `hvac` at a directory that contains videos — even ones hidden inside `.img` and `.iso` files — and it'll compress them to `h.265` (`HEVC`) using reasonable defaults. You can overwrite these defaults with a small config file.

You need a GPU with an HEVC encoder (NVIDIA NVENC, Intel VAAPI, or
Apple VideoToolbox) and an ffmpeg built against it. The installer below
handles both on macOS and Debian/Ubuntu; for everything else there's
[Docker](#docker) or the [NAS-specific notes](docs/NAS.md).

---

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/JackDanger/hvac/main/install.sh | sh
```

Other ways: `brew install JackDanger/tap/hvac` &middot; `cargo install hvac-transcoder` &middot; [`.deb`, AUR, tarballs](https://github.com/JackDanger/hvac/releases) &middot; [Docker](#docker) &middot; [Synology / QNAP / Unraid](docs/NAS.md)

---

## Use

First time on a library you care about, do a dry run:

```bash
hvac --dry-run /path/to/movies        # preview, change nothing
```

Once you've eyeballed the list, drop `--dry-run`:

```bash
hvac /path/to/movies                  # overwrite in place (default)
hvac --no-overwrite /path/to/movies   # write .transcoded.mkv copies, keep originals
```

It scans the directory, skips files that already meet the target, and re-encodes
the rest. Re-running picks up where you left off — there's a sidecar
`.hvac.complete` next to each output that records source size + duration so the
next run knows whether to adopt it or re-encode. Ctrl-C is safe; in-progress
encodes leave a `.hvac_tmp_*` file that the next run sweeps.

---

## Does it actually save space?

Real numbers from one library — public domain films, full bitrate range from pristine remuxes to lo-fi early transfers:

| File | Before | After | Savings |
|------|-------:|------:|--------:|
| Nosferatu (1922) Remux-1080p.mkv | 27.2 GB | 4.1 GB | **-84%** |
| The Blood of a Poet (1932) Bluray-1080p.mkv | 3.3 GB | 1.0 GB | **-69%** |
| Battleship Potemkin (1925) Remux-1080p.mkv | 21.0 GB | 3.5 GB | **-83%** |
| Sherlock Jr. (1924) Remux-1080p.mkv | 22.7 GB | 4.6 GB | **-79%** |
| Way Down East (1920) Remux-1080p.mkv | 17.5 GB | 3.2 GB | **-81%** |
| The Black Pirate (1926) Remux-1080p.mkv | 21.1 GB | 4.7 GB | **-77%** |
| Intolerance (1916) Bluray-1080p.mkv | 13.9 GB | 3.2 GB | **-77%** |
| The Gold Rush (1925) Bluray-1080p.mkv | 15.0 GB | 4.1 GB | **-72%** |
| Metropolis (1927) Remux-1080p.mkv | 798 MB | 120 MB | **-84%** |
| Our Hospitality (1923) Bluray-1080p.mkv | 11.4 GB | 4.2 GB | **-63%** |
| The Boat (1921) Bluray-1080p.mkv | 2.2 GB | 839 MB | **-62%** |
| Safety Last! (1923) WEBDL-1080p.mkv | 6.1 GB | 3.1 GB | **-49%** |
| The Phantom of the Opera (1925) Bluray-1080p.mkv | 4.8 GB | 2.5 GB | **-48%** |
| The Blacksmith (1922) Bluray-1080p.mkv | 1.5 GB | 670 MB | **-55%** |
| The Blot (1921) Bluray-1080p.mkv | 3.9 GB | 2.5 GB | **-34%** |
| The General (1926) Bluray-1080p.mkv | 4.9 GB | 3.4 GB | **-31%** |
| The Navigator (1924) Bluray-1080p.avi | 700 MB | 406 MB | **-42%** |
| The Kid (1921) Remux-1080p.mkv | 5.4 GB | 1.1 GB | **-80%** |
| Strike (1925) Bluray-1080p.mkv | 5.3 GB | 3.3 GB | **-37%** |

**Average across the full library: ~65% smaller.**

---

## GPU required

| GPU | Encoder | Platform |
|-----|---------|----------|
| NVIDIA (Kepler+) | `hevc_nvenc` | Linux |
| Intel (Broadwell+) | `hevc_vaapi` | Linux |
| Apple Silicon / Intel Mac | `hevc_videotoolbox` | macOS |

No GPU, no go — `hvac` exits with a clear message. CPU h265 is too slow to be worth shipping.

---

## Config

The defaults are sensible. To tune quality, presets, max resolution, etc.:

```bash
hvac --dump-config > config.yaml
$EDITOR config.yaml
hvac --config config.yaml /path/to/movies
```

---

## Docker

If you'd rather not install ffmpeg + drivers + the binary on the host —
or if the host is a NAS where those don't behave — there's a container
image with everything pre-wired.

```bash
# Intel iGPU (Broadwell+)
docker run --rm \
  --device /dev/dri:/dev/dri \
  -v /path/to/media:/media \
  ghcr.io/jackdanger/hvac:latest --dry-run /media

# NVIDIA (needs nvidia-container-toolkit on the host)
docker run --rm \
  --gpus all --runtime=nvidia \
  -v /path/to/media:/media \
  ghcr.io/jackdanger/hvac:latest --dry-run /media
```

For compose, copy [`compose.example.yml`](compose.example.yml) and edit
the volume path. The image is built from this repo's
[`Dockerfile`](Dockerfile) — `docker build -t hvac .` works if you'd
rather build locally.

NAS-specific instructions (Synology Container Manager, QNAP Container
Station, Unraid Community Applications, TrueNAS SCALE, OpenMediaVault)
live in [`docs/NAS.md`](docs/NAS.md). If your NAS has no GPU, that doc
also covers the "mount over NFS and transcode off-box" pattern.

---

## Troubleshooting

**"No GPU found for h265 encoding!"**
- macOS: nothing to do — Apple Silicon and all post-2017 Macs have
  VideoToolbox built in. If you still see this, your shell is missing
  `ffmpeg`; `brew install ffmpeg`.
- Linux + Intel iGPU: `ls -la /dev/dri` — if `renderD128` isn't there,
  load the driver (`sudo modprobe i915` on most distros) and install
  `intel-media-va-driver` + `vainfo`.
- Linux + NVIDIA: `nvidia-smi` should print your card. If it doesn't,
  install the proprietary driver and reboot. The open-source `nouveau`
  driver has no NVENC.
- Docker / NAS: pass the device. `--device /dev/dri:/dev/dri` for
  Intel; `--gpus all --runtime=nvidia` for NVIDIA. See
  [`docs/NAS.md`](docs/NAS.md).

**"Can I do CPU encoding instead?"** No, by design. x265 at the quality
the defaults target runs at ~5 fps on a fast desktop CPU. A 2-hour
movie is 6+ hours of wall time vs. 5 minutes on a $50 used Quadro. If
you're on a NAS without a GPU, see [`docs/NAS.md`](docs/NAS.md) for
the off-box pattern.

**"Will it touch my files?"** It overwrites by default — only after
the new encode has passed an ffprobe duration + codec + min-size
check, and only via an atomic rename of a `.hvac_tmp_…` sidecar over
the original. The first run on a library you care about should be
`hvac --dry-run`, then `hvac --no-overwrite`; the latter writes
`.transcoded.<ext>` copies you can compare before committing with
`--replace`.

**"It's stuck on a single file."** ffprobe has a watchdog
(`--probe-timeout`, default 30 s); the directory walk doesn't. If
your media lives on a flaky NFS / SMB mount and the scan hangs, that
hang is on the kernel's mount layer, not hvac. Raise the probe
timeout on slow NAS shares: `hvac --probe-timeout 120 /path`.

**"I want to stop it cleanly."** Ctrl-C once — workers finish their
current file, then exit. Ctrl-C twice — force quit; in-progress
`.hvac_tmp_*` files are swept on the next run. Resume is automatic.

---

## Controlling resource usage during multi-day transcodes

For long-running batch transcodes (a media library of thousands of files takes days) you may want a way to observe progress, push tuning changes, or kill the process from outside without losing partial work. hvac integrates with LaunchDarkly to support that — the binary connects to your project at startup if you pass an SDK key:

**Full disclosure:** At the time of writing, I work at LaunchDarkly. I drive my whole homelab config with it.

1. Provision the LaunchDarkly project once: `hvac --setup-launchdarkly --ld-api-key <YOUR_LD_API_KEY>`
2. Note the SDK key it prints. Pass it on each long-running invocation:
   ```
   hvac --launchdarkly-sdk-key <SDK_KEY> /path/to/media
   ```
3. With a key supplied, hvac connects to LaunchDarkly's evaluation endpoint and exports per-encode OpenTelemetry spans to LaunchDarkly Observability, so you can watch live progress and timing in the LD dashboard.

The three flags that are active during a run:

| Flag | Type | Effect |
|------|------|--------|
| `pause-transcoding` | boolean | Workers finish their current file, then spin until you set it back to false |
| `enable-transcoding` | boolean | Kill-switch — workers stop picking up new files when set to false |
| `max-parallel-jobs` | integer | Override the parallel encoder count on the fly (0 = auto) |

The SDK key is **CLI-only** by design — it does not read from any environment variable. This is deliberate: hvac controls expensive GPU/disk resources, and a key that lives in your shell rc would silently apply to every run. Keep the key in a secure location and pass it explicitly when you want remote observability/control to be active.

---

## Development

Hook the lint checks up once per clone:

```bash
git config core.hooksPath .githooks
```

After that, every commit that touches a `.rs` file runs:

1. `cargo fmt --all -- --check` — sub-second; the commit is rejected if any file would be reformatted. Fix with `cargo fmt --all`.
2. `cargo clippy -- -D warnings` — slower (5-30s cold, <2s incremental); rejected if any lint fires. Both checks mirror exactly what CI enforces.

To bypass the slow check for a quick fix-up commit (you've already run clippy yourself or are about to squash anyway): `HVAC_SKIP_CLIPPY=1 git commit ...`. To bypass both: `git commit --no-verify`.

For more on the code layout, retry tiers, and concurrency model, see [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

---

## Contributing

Pull requests welcome — see [`CONTRIBUTING.md`](CONTRIBUTING.md) for the
PR checklist, code style notes, and release flow. Bugs go on the
[issues tracker](https://github.com/JackDanger/hvac/issues/new/choose);
security-sensitive reports go to the address in
[`SECURITY.md`](SECURITY.md) instead.

The [`CHANGELOG.md`](CHANGELOG.md) tracks user-visible changes in
[Keep a Changelog](https://keepachangelog.com/) format.

---

## License

[MIT](LICENSE) &copy; Jack Danger
