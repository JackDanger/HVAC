---
description: Project-wide rules for tdorr development
globs: "**/*"
---

# tdorr - Rust Tdarr Alternative

## Project Purpose
A Rust CLI tool that scans media directories and transcodes video files to highly-compressed h265 using GPU acceleration (NVIDIA NVENC or Intel QSV).

## Key Constraints
- Never run locally. All compilation and testing happens on the remote host via `./deploy.sh`
- Remote host: `ssh -J neurotic root@10.30.0.199`
- Media directory: `/mnt/media/dumb-tv`
- Never overwrite source files unless `--overwrite` flag is provided. Default is to create copies.
- Must detect GPU (NVIDIA CUDA / Intel QSV) and exit nonzero with clear message if none found.
- Uses ffmpeg for transcoding and isomage (https://github.com/JackDanger/isomage.git) for extracting media from .iso/.img disc images.
- isomage is only used for .iso and .img files. Regular media files go straight to ffmpeg.
- Config via YAML file with target encoding preset (default: high quality h265 compression).

## Architecture
- Single Rust binary CLI using `clap` for arg parsing, `serde`/`serde_yaml` for config
- Probe files with ffprobe JSON output to detect codec, resolution, bitrate
- Smart skip: don't transcode if file already meets target (h265, same or smaller resolution, same or lower bitrate)
- GPU detection at startup, fail fast if no GPU available
- FFmpeg invocation with hevc_nvenc (nvidia) or hevc_vaapi (intel) encoder
- ISO/IMG handling: isomage lists contents -> extracts media to temp dir -> probe/transcode as normal

## Development Workflow
1. Edit code locally
2. Run `./deploy.sh` to scp and build on remote host
3. Run `./deploy.sh test` to run tests on remote host
4. Run `./deploy.sh run -- [args]` to run the binary on remote host

## Remote Host Info
- Debian 12 (bookworm), Linux 6.8
- NVIDIA GeForce RTX 2060 with NVENC, CUDA 12.2
- ffmpeg n7.1 with hevc_nvenc, hevc_vaapi encoders
- Rust 1.91.1, Cargo 1.91.1
