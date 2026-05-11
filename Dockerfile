# syntax=docker/dockerfile:1.7
# hvac container image.
#
# Two-stage build:
#   1. builder — Debian + Rust, compiles the release binary from this repo.
#   2. runtime — Debian-slim + ffmpeg + the VAAPI userland. The binary is
#                copied in, no toolchain in the final image.
#
# Why Debian and not Alpine? hvac transitively depends on glibc-only
# crates (the LaunchDarkly SDK pulls in `ring`), and the Debian apt
# ffmpeg ships every encoder hvac drives — Alpine's ffmpeg is missing
# `hevc_nvenc` and the VAAPI driver story on musl is fragile enough
# that a single-platform image is the better trade.
#
# NVIDIA support: this image does NOT bundle the NVIDIA userspace
# driver. Use `--gpus all --runtime=nvidia` (nvidia-container-toolkit on
# the host) to inject it at run time. That's the standard pattern and
# keeps the image size in the low hundreds of MB rather than ~2 GB.
#
# Build:
#   docker build -t ghcr.io/jackdanger/hvac:dev .
#
# Run (Intel iGPU):
#   docker run --rm --device /dev/dri:/dev/dri \
#     -v /path/to/media:/media \
#     ghcr.io/jackdanger/hvac:dev --dry-run /media
#
# Run (NVIDIA):
#   docker run --rm --gpus all --runtime=nvidia \
#     -v /path/to/media:/media \
#     ghcr.io/jackdanger/hvac:dev --dry-run /media

# ---- builder ----------------------------------------------------------------

FROM rust:1.93-bookworm AS builder

WORKDIR /src
# Copy the manifests first so `cargo fetch` is cached when only sources change.
COPY Cargo.toml Cargo.lock ./
# Touch a stub main so `cargo fetch` resolves without the real sources.
RUN mkdir -p src && echo "fn main(){}" > src/main.rs && \
    cargo fetch --locked && \
    rm -rf src
COPY . .
RUN cargo build --release --locked && \
    strip target/release/hvac

# ---- runtime ----------------------------------------------------------------

FROM debian:bookworm-slim AS runtime

# ffmpeg                  — the encoders hvac drives.
# vainfo + libva-drm2     — VAAPI diagnostics + the run-time userland.
# intel-media-va-driver   — Broadwell+ Intel iGPUs (amd64 only; no arm64
#                           candidate, so we add it conditionally below).
# mesa-va-drivers         — older Intel + AMD.
# ca-certificates         — LaunchDarkly + OTel exporter use HTTPS.
# tini                    — PID 1, forwards signals so Ctrl-C cancels cleanly
#                           rather than getting eaten by Docker's default init.
#
# TARGETARCH is set automatically by buildx in multi-platform builds. It
# resolves to "amd64" / "arm64" / etc — the same names apt uses. We gate
# intel-media-va-driver on amd64 because the package literally does not
# exist on arm64 (it ships Intel-specific shaders) and an unconditional
# install bricks the arm64 image build.
ARG TARGETARCH
RUN set -eux; \
    extra=""; \
    if [ "${TARGETARCH:-amd64}" = "amd64" ]; then \
        extra="intel-media-va-driver"; \
    fi; \
    apt-get update; \
    # shellcheck disable=SC2086
    apt-get install -y --no-install-recommends \
        ffmpeg \
        vainfo \
        libva-drm2 \
        mesa-va-drivers \
        ca-certificates \
        tini \
        $extra; \
    rm -rf /var/lib/apt/lists/*

COPY --from=builder /src/target/release/hvac /usr/local/bin/hvac

# Match the typical NAS admin UID/GID so files written back to bind-mounts
# don't end up root-owned. GID 100 is the pre-existing "users" group on
# Debian (and on Synology / Unraid / OMV — the value was chosen because
# their admin accounts already sit there), so we just reuse it rather
# than create a parallel "hvac" group at the same number.
# Override with `--user $(id -u):$(id -g)` if your NAS uses different values.
RUN useradd -m -u 1026 -g 100 hvac
USER hvac:users
WORKDIR /media

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/hvac"]
CMD ["--help"]
