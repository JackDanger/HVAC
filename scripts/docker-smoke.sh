#!/usr/bin/env bash
# Smoke-test a built hvac container image against the contract the README
# and Dockerfile promise users. Runs the image exactly the way the README
# tells people to and asserts every guarantee that a `push: false` build
# can't catch: that the binary actually runs, ffmpeg + the GPU encoders are
# wired in, and the image runs as the documented NAS UID/GID.
#
# Usage:
#   scripts/docker-smoke.sh <image-ref>
#
# <image-ref> is anything `docker run` accepts — a local tag from a
# `load: true` build (hvac:ci) or a pushed digest (ghcr.io/.../hvac@sha256:..).
# The expected version is read from the repo's VERSION file so this stays a
# single source of truth with set-version.sh.
set -euo pipefail

IMAGE="${1:?usage: docker-smoke.sh <image-ref>}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
EXPECTED_VERSION="$(head -1 "$ROOT/VERSION" | sed 's/[[:space:]]*$//')"

fail() { echo "✗ $*" >&2; exit 1; }
ok()   { echo "✓ $*"; }

echo "smoke-testing image: $IMAGE (expecting hvac $EXPECTED_VERSION)"

# 1. The entrypoint runs the binary and reports the version we shipped.
#    Catches a binary that compiles but can't start (missing shared lib,
#    wrong copy path) and version drift between VERSION and the build.
version_out="$(docker run --rm "$IMAGE" --version)"
[ "$version_out" = "hvac $EXPECTED_VERSION" ] \
  || fail "--version: expected 'hvac $EXPECTED_VERSION', got '$version_out'"
ok "--version reports hvac $EXPECTED_VERSION"

# 2. --help exits 0. The default CMD is --help, so this also proves the
#    tini entrypoint forwards the exit code rather than swallowing it.
docker run --rm "$IMAGE" --help >/dev/null \
  || fail "--help did not exit 0"
ok "--help exits 0"

# 3. Runs as the documented NAS UID 1026 / GID 100. The README tells users
#    to drop --user only when their NAS already matches this; if the image
#    silently reverted to root, files written to bind-mounts would be
#    root-owned and the docs would be wrong.
id_out="$(docker run --rm --entrypoint id "$IMAGE")"
echo "$id_out" | grep -q "uid=1026" \
  || fail "expected uid=1026, got: $id_out"
echo "$id_out" | grep -q "gid=100" \
  || fail "expected gid=100, got: $id_out"
ok "runs as uid=1026 gid=100 ($id_out)"

# 4. ffmpeg is present and carries the encoders hvac actually drives. The
#    whole point of the image is "ffmpeg + drivers pre-wired"; assert the
#    GPU encoders the README's Docker examples depend on: VAAPI (Intel
#    iGPU, --device /dev/dri) and NVENC (--gpus all). Both ship on amd64
#    and arm64.
encoders="$(docker run --rm --entrypoint ffmpeg "$IMAGE" -hide_banner -encoders)"
required_encoders="hevc_vaapi hevc_nvenc"

# hevc_qsv (Intel Quick Sync) only exists in Debian's amd64 ffmpeg — it's
# Intel-specific and has no arm64 build. Require it only on amd64 so the
# arm64 image (Synology / QNAP / Apple Silicon) isn't held to an encoder
# it can never have. Detect arch from the image itself rather than the
# host, so the check is correct whichever variant we were handed.
arch="$(docker run --rm --entrypoint uname "$IMAGE" -m)"
case "$arch" in
  x86_64|amd64) required_encoders="$required_encoders hevc_qsv" ;;
esac

for enc in $required_encoders; do
  echo "$encoders" | grep -q "$enc" \
    || fail "ffmpeg is missing the $enc encoder (arch: $arch)"
  ok "ffmpeg has $enc"
done

echo "all docker smoke checks passed for $IMAGE"
