#!/usr/bin/env sh
# hvac installer — downloads the latest pre-built binary for your platform
# from https://github.com/JackDanger/hvac/releases and installs it.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/JackDanger/hvac/main/install.sh | sh
#
# Environment overrides:
#   HVAC_VERSION   pin to a specific tag (e.g. v5.1.1); default: latest
#   HVAC_PREFIX    install dir; default: /usr/local/bin if writable, else $HOME/.local/bin
#   HVAC_REPO      override owner/name; default: JackDanger/hvac

set -eu

REPO="${HVAC_REPO:-JackDanger/hvac}"
VERSION="${HVAC_VERSION:-}"

# ---- helpers ---------------------------------------------------------------

err() { printf 'install.sh: error: %s\n' "$*" >&2; exit 1; }
info() { printf '==> %s\n' "$*"; }

need() {
    command -v "$1" >/dev/null 2>&1 || err "'$1' is required but not installed"
}

# Pick a downloader. Avoids forcing curl on minimal Debian/Ubuntu installs.
download() {
    url="$1"; dest="$2"
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL --retry 3 -o "$dest" "$url"
    elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$dest" "$url"
    else
        err "neither curl nor wget found; install one and retry"
    fi
}

# Resolve a version string to a release tag. Uses the redirect from
# /releases/latest so we don't need a JSON parser or a GitHub token.
resolve_latest() {
    if command -v curl >/dev/null 2>&1; then
        location=$(curl -fsSLI -o /dev/null -w '%{url_effective}' \
            "https://github.com/${REPO}/releases/latest")
    else
        # wget --max-redirect=0 prints Location to stderr, but we cannot read
        # it portably. Fall back to the API.
        location=$(wget -q -O - \
            "https://api.github.com/repos/${REPO}/releases/latest" \
            | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n1)
        case "$location" in
            v*) printf '%s\n' "$location"; return ;;
        esac
    fi
    # location looks like https://github.com/OWNER/REPO/releases/tag/v1.2.3
    printf '%s\n' "${location##*/}"
}

# ---- detect platform -------------------------------------------------------

OS=$(uname -s)
ARCH=$(uname -m)

case "$OS" in
    Linux)  os_slug="linux" ;;
    Darwin) os_slug="macos" ;;
    *)      err "unsupported OS: $OS (only Linux and macOS have pre-built binaries)" ;;
esac

case "$ARCH" in
    x86_64|amd64)        arch_slug="x86_64" ;;
    aarch64|arm64)       arch_slug="aarch64" ;;
    *)                   err "unsupported architecture: $ARCH" ;;
esac

# ---- resolve version & target ----------------------------------------------

if [ -z "$VERSION" ]; then
    info "resolving latest release"
    VERSION=$(resolve_latest)
    [ -n "$VERSION" ] || err "could not determine latest release tag"
fi
case "$VERSION" in
    v*) ;;
    *)  VERSION="v${VERSION}" ;;
esac

ASSET="hvac-${os_slug}-${arch_slug}.tar.gz"
URL="https://github.com/${REPO}/releases/download/${VERSION}/${ASSET}"
SHA_URL="${URL}.sha256"

info "selected: $ASSET ($VERSION)"

# ---- pick install dir ------------------------------------------------------

if [ -n "${HVAC_PREFIX:-}" ]; then
    PREFIX="$HVAC_PREFIX"
elif [ -w /usr/local/bin ] 2>/dev/null; then
    PREFIX=/usr/local/bin
elif command -v sudo >/dev/null 2>&1 && [ -d /usr/local/bin ]; then
    PREFIX=/usr/local/bin
    USE_SUDO=1
else
    PREFIX="$HOME/.local/bin"
    mkdir -p "$PREFIX"
fi

# ---- download, verify, install --------------------------------------------

tmp=$(mktemp -d 2>/dev/null || mktemp -d -t hvac)
trap 'rm -rf "$tmp"' EXIT

info "downloading $URL"
download "$URL" "$tmp/$ASSET"

# Optional checksum verification — release pipeline publishes .sha256 sidecars.
# Tolerate its absence so older releases still work.
if download "$SHA_URL" "$tmp/$ASSET.sha256" 2>/dev/null; then
    if command -v sha256sum >/dev/null 2>&1; then
        ( cd "$tmp" && sha256sum -c "$ASSET.sha256" ) >/dev/null \
            || err "sha256 verification failed for $ASSET"
        info "sha256 verified"
    elif command -v shasum >/dev/null 2>&1; then
        # macOS: shasum -c expects the same format
        ( cd "$tmp" && shasum -a 256 -c "$ASSET.sha256" ) >/dev/null \
            || err "sha256 verification failed for $ASSET"
        info "sha256 verified"
    fi
fi

info "extracting"
( cd "$tmp" && tar -xzf "$ASSET" )
[ -f "$tmp/hvac" ] || err "tarball did not contain a 'hvac' binary"
chmod +x "$tmp/hvac"

info "installing to $PREFIX/hvac"
if [ "${USE_SUDO:-0}" = "1" ]; then
    sudo install -m 755 "$tmp/hvac" "$PREFIX/hvac"
else
    install -m 755 "$tmp/hvac" "$PREFIX/hvac" 2>/dev/null \
        || cp "$tmp/hvac" "$PREFIX/hvac" && chmod 755 "$PREFIX/hvac"
fi

# ---- post-install hints ----------------------------------------------------

case ":$PATH:" in
    *":$PREFIX:"*) ;;
    *) printf '\nNote: %s is not on your PATH. Add this to your shell rc:\n  export PATH="%s:$PATH"\n' \
              "$PREFIX" "$PREFIX" ;;
esac

if ! command -v ffmpeg >/dev/null 2>&1; then
    cat <<'EOF'

ffmpeg was not found on PATH. hvac requires ffmpeg with a GPU encoder
(hevc_nvenc, hevc_vaapi, or hevc_videotoolbox). Install it:
  Debian/Ubuntu: sudo apt install ffmpeg
  Arch:          sudo pacman -S ffmpeg
  Fedora:        sudo dnf install ffmpeg
  macOS:         brew install ffmpeg
EOF
fi

printf '\nInstalled %s. Try:\n  hvac --help\n' "$("$PREFIX/hvac" --version 2>/dev/null || echo hvac)"
