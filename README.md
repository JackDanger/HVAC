<p align="center">
  <img src="logo.svg" alt="hvac" width="700"/>
</p>

<p align="center">
  <a href="#why">Why</a> &bull;
  <a href="#compression-results">Results</a> &bull;
  <a href="#features">Features</a> &bull;
  <a href="#install">Install</a> &bull;
  <a href="#usage">Usage</a> &bull;
  <a href="#config">Config</a> &bull;
  <a href="#license">License</a>
</p>

---

## Why?

I spent an entire evening trying to get [Tdarr](https://github.com/HaveAGitGat/Tdarr) working. Nodes, servers, web UIs, plugins, databases... I just wanted to convert my media library to h265. So I wrote HVAC instead.

**HVAC** is a single binary. Point it at a directory. It finds video files, skips the ones that are already fine, and GPU-transcodes the rest to h265. That's it.

---

## Compression Results

Real numbers from a media library. Public domain films make surprisingly good test data — they span the full range of bitrates, from pristine remuxes to lo-fi early transfers.

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

**Average across a full library:** ~65% smaller.

---

## Features

- **GPU-accelerated h265 encoding** &mdash; NVIDIA NVENC, Intel VAAPI, and Apple VideoToolbox
- **Smart skip** &mdash; won't touch files that are already h265 at or below your target resolution and bitrate
- **Safe by default** &mdash; writes `.transcoded.mkv` copies alongside originals; never overwrites unless `--overwrite` is passed
- **Output validation** &mdash; ffprobe-verifies each output for duration, codec, and minimum file size before finalizing
- **Disc image support** &mdash; extracts and transcodes media from `.iso` and `.img` files (Blu-ray, DVD, AVCHD) using the bundled [isomage](https://github.com/JackDanger/isomage) library — no separate install needed
- **YAML config** &mdash; sensible defaults, fully overridable
- **Resumable** &mdash; re-running skips already-transcoded outputs; ISO progress is preserved too
- **Fast** &mdash; 14× realtime on an RTX 2060 for 720p content

---

## Install

### One-liner (Linux & macOS)

```bash
curl -fsSL https://raw.githubusercontent.com/JackDanger/hvac/main/install.sh | sh
```

Detects your OS and architecture, downloads the matching binary from the
latest GitHub release, verifies its sha256, and installs to `/usr/local/bin`
(falling back to `~/.local/bin` if that's not writable). Pin a version with
`HVAC_VERSION=v5.2.0` or change the install dir with `HVAC_PREFIX=...`.

### Debian / Ubuntu (apt)

A signed `.deb` is published with every release.

```bash
# Install ffmpeg first — the .deb declares it as a dependency
sudo apt update && sudo apt install -y ffmpeg

# x86_64 (most laptops/servers)
curl -fsSLO https://github.com/JackDanger/hvac/releases/latest/download/hvac_5.2.0_amd64.deb
sudo apt install -y ./hvac_5.2.0_amd64.deb

# aarch64 / arm64 (Raspberry Pi, Ampere, Graviton)
curl -fsSLO https://github.com/JackDanger/hvac/releases/latest/download/hvac_5.2.0_arm64.deb
sudo apt install -y ./hvac_5.2.0_arm64.deb
```

`apt install ./file.deb` resolves dependencies (unlike bare `dpkg -i`).

### Arch Linux (AUR)

```bash
yay -S hvac     # or: paru -S hvac, or any AUR helper
```

Or build from the in-tree PKGBUILD without an AUR helper:

```bash
git clone https://github.com/JackDanger/hvac.git
cd hvac/packaging/aur
makepkg -si
```

### Homebrew (macOS & Linuxbrew)

```bash
brew install JackDanger/tap/hvac
```

### Cargo (any platform)

The crate is published as `hvac-transcoder` because the short `hvac` name on
crates.io is held by an unrelated 2018 thermostat crate. The installed
binary is still `hvac`.

```bash
cargo install hvac-transcoder
```

### Pre-built binaries

Download tarballs and `.sha256` sidecars from [GitHub Releases](https://github.com/JackDanger/hvac/releases) for:
- Linux x86\_64 / aarch64 (`.tar.gz` and `.deb`)
- macOS x86\_64 (Intel) / aarch64 (Apple Silicon) (`.tar.gz`)

### From source

```bash
git clone https://github.com/JackDanger/hvac.git
cd hvac
make build
# ./hvac is now symlinked to the release binary
```

### Requirements

- `ffmpeg` with a GPU encoder (`hevc_nvenc`, `hevc_vaapi`, or `hevc_videotoolbox`)
  - Debian/Ubuntu: `sudo apt install ffmpeg`
  - Arch: `sudo pacman -S ffmpeg`
  - Fedora: `sudo dnf install ffmpeg`
  - macOS: `brew install ffmpeg`

---

## Usage

```bash
# Dry run — see what would be transcoded without doing anything
hvac --dry-run --config config.yaml /mnt/media/movies

# Transcode (creates .transcoded.mkv copies alongside originals)
hvac --config config.yaml /mnt/media/movies

# Write outputs to a separate directory tree
hvac --config config.yaml --output-dir /mnt/transcoded /mnt/media/movies

# Overwrite originals in-place
hvac --overwrite --config config.yaml /mnt/media/movies
```

### What it looks like

```
$ hvac --config config.yaml /mnt/media/movies
GPU detected: NVIDIA GeForce RTX 2060 (encoder: hevc_nvenc)
Found 8 media files in "/mnt/media/movies"
▶ Nosferatu (1922)/Nosferatu.mkv (h264, 1920x1080, 18240 kbps)
  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━╸──────────── 78%  ⏳ 0:12 remaining
...

Done: 8 transcoded, 0 skipped, 0 errors (of 8 total)
```

Run it again and everything is skipped:

```
$ hvac --config config.yaml /mnt/media/movies
GPU detected: NVIDIA GeForce RTX 2060 (encoder: hevc_nvenc)
Found 8 media files in "/mnt/media/movies"

Done: 0 transcoded, 8 skipped, 0 errors (of 8 total)
```

---

## Config

```yaml
target:
  codec: hevc
  quality: 28          # CQ value — lower = better quality + bigger file
  preset: slow         # NVENC preset; VAAPI maps automatically
  max_width: 3840      # 4K max (downscale anything wider)
  max_height: 2160
  max_bitrate_kbps: 0  # 0 = no cap; CQ alone controls file size
  container: mkv
  audio_codec: copy    # Don't re-encode audio
  subtitle_codec: copy

media_extensions:
  - mkv
  - mp4
  - avi
  - ts
  - m2ts
  - iso
  - img
```

---

## GPU Requirements

HVAC requires a GPU encoder and will exit with a clear error if none is detected.

| GPU | Encoder | Platform | Detection method |
|-----|---------|----------|-----------------|
| NVIDIA (Kepler+) | `hevc_nvenc` | Linux | `nvidia-smi` + ffmpeg encoder check |
| Intel (Broadwell+) | `hevc_vaapi` | Linux | `/dev/dri/renderD128` + ffmpeg encoder check |
| Apple Silicon / Intel Mac | `hevc_videotoolbox` | macOS | OS detection + ffmpeg encoder check |

CPU h265 encoding is painfully slow and not what HVAC is for. No GPU, no encoding. This is intentional.

---

## License

[MIT](LICENSE) &copy; Jack Danger
