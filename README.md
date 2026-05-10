# Get your TBs back

Point `hvac` at a directory that contains videos — even ones hidden inside `.img` and `.iso` files - and it'll compress them to h.265 (HEVC) using reasonable defaults. You can overwrite these defaults with a small config file.

<p align="center">
  <img src="logo.svg" alt="hvac" width="700"/>
</p>

<p align="center">
  <b>Shrink your entire video library to h265.</b><br/>
</p>

---

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/JackDanger/hvac/main/install.sh | sh
```

You also need `ffmpeg` (`apt install ffmpeg` / `brew install ffmpeg`) and any hardware acceleration — see [below](#gpu-required).

Other ways: `brew install JackDanger/tap/hvac` &middot; `cargo install hvac-transcoder` &middot; [`.deb`, AUR, tarballs](https://github.com/JackDanger/hvac/releases)

---

## Use

```bash
hvac /path/to/movies
```

It scans the directory, skips files that are already h265, and re-encodes the rest. Each new file is ffprobe-verified before the original is replaced. Disc images (`.iso`, `.img`) work too. Re-running picks up where you left off.

```bash
hvac --dry-run /path/to/movies        # preview, change nothing
hvac --no-overwrite /path/to/movies   # write .transcoded.mkv copies, keep originals
```

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

## License

[MIT](LICENSE) &copy; Jack Danger
